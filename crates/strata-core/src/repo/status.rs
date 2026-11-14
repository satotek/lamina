//! Owned by the repo-status task: status, commit changes, file contents.

use std::collections::BTreeSet;
use std::io::{ErrorKind, Read};
use std::path::{Component, Path, PathBuf};

use gix::bstr::{BStr, BString, ByteSlice};
use gix::object::tree::EntryMode;
use gix::status::plumbing::index_as_worktree::{Change, EntryStatus};
use gix::status::tree_index::TrackRenames;

use super::Repo;
use crate::{ChangeKind, Error, FileChange, Oid, Result, WorktreeStatus};

/// Maximum number of changed paths (or affected index entries under a
/// changed directory) handled incrementally before falling back to a full scan.
pub const MAX_INCREMENTAL_PATHS: usize = 1000;

impl Repo {
    /// Compare `HEAD` to the index and the index to the working tree.
    ///
    /// Conflicts live in `unstaged`: an unmerged path is not something that can be
    /// committed as-is, and listing it in both halves would duplicate it in the UI.
    pub fn status(&self) -> Result<WorktreeStatus> {
        let repo = self.local();

        // `gix` checks file metadata first, so unchanged files are not read.
        let iter = repo
            .status(gix::progress::Discard)
            .map_err(Error::gix)?
            .untracked_files(gix::status::UntrackedFiles::Files)
            .tree_index_track_renames(TrackRenames::Given(gix::diff::Rewrites::default()))
            .into_iter(Vec::new())
            .map_err(Error::gix)?;

        let mut staged = Vec::new();
        let mut unstaged = Vec::new();
        let mut conflicted = std::collections::BTreeSet::new();

        for item in iter {
            match item.map_err(Error::gix)? {
                gix::status::Item::TreeIndex(change) => {
                    if let Some(change) = staged_change(change) {
                        staged.push(change);
                    }
                }
                gix::status::Item::IndexWorktree(item) => {
                    if let Some(change) = unstaged_change(item) {
                        if change.kind == ChangeKind::Conflicted {
                            conflicted.insert(change.path.clone());
                        }
                        unstaged.push(change);
                    }
                }
            }
        }

        // The tree-index diff also looks at stage entries of an unmerged path; keep
        // only the single `Conflicted` entry from the index-worktree side.
        staged.retain(|change| !conflicted.contains(&change.path));

        staged.sort_by(|a, b| a.path.cmp(&b.path));
        unstaged.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(WorktreeStatus { staged, unstaged })
    }

    /// Recompute worktree status only for `paths` and merge into `prev`.
    ///
    /// Callers must only invoke this when `.git/index`, `HEAD`, and `refs` have
    /// not changed since `prev` was produced. Returns `Ok(None)` when a full
    /// scan is required (too many paths, worktree root, `.gitignore` /
    /// `.gitattributes` / `.gitmodules` edits, or directories with large or
    /// unknown contents).
    pub fn status_incremental(
        &self,
        prev: &WorktreeStatus,
        paths: &[PathBuf],
    ) -> Result<Option<WorktreeStatus>> {
        let Some(workdir) = self.workdir() else {
            return Ok(None);
        };
        if paths.len() > MAX_INCREMENTAL_PATHS {
            return Ok(None);
        }

        let workdir_abs = std::path::absolute(workdir).ok();
        let workdir_canon = workdir.canonicalize().ok();
        let mut targets = BTreeSet::new();

        for path in paths {
            let Some(rel) = normalize_relative(
                path,
                workdir,
                workdir_abs.as_deref(),
                workdir_canon.as_deref(),
            ) else {
                return Ok(None);
            };
            if rel.is_empty() || touches_git_or_rules(&rel) {
                return Ok(None);
            }
            targets.insert(rel);
        }

        if targets.is_empty() {
            return Ok(Some(prev.clone()));
        }
        if targets.len() > MAX_INCREMENTAL_PATHS {
            return Ok(None);
        }

        let repo = self.local();
        let index = repo.index_or_empty().map_err(Error::gix)?;

        // If a file was replaced by a directory `a/b.txt`, also re-check any
        // tracked or previously-dirty ancestor (`a`) so its deletion/type-change
        // is picked up even if the watcher only reported the child path.
        let initial_targets: Vec<String> = targets.iter().cloned().collect();
        for rel in &initial_targets {
            let mut rest = rel.as_str();
            while let Some((parent, _)) = rest.rsplit_once('/') {
                if index.entry_by_path(BStr::new(parent)).is_some()
                    || prev.unstaged.iter().any(|change| change.path == parent)
                {
                    targets.insert(parent.to_owned());
                }
                rest = parent;
            }
        }

        // Guard against directories that span many index entries or many files
        // on disk; falling back to a full scan is safer and avoids scanning a
        // large subtree through narrow pathspecs.
        for rel in &targets {
            let abs = workdir.join(rel);
            match std::fs::symlink_metadata(&abs) {
                Ok(meta) if meta.is_dir() => {
                    let is_submodule = index
                        .entry_by_path(BStr::new(rel.as_str()))
                        .is_some_and(|entry| entry.mode.is_submodule());
                    if !is_submodule {
                        let prefix = format!("{rel}/");
                        let tracked_count = index
                            .prefixed_entries_range(BStr::new(prefix.as_str()))
                            .map_or(0, |range| range.len());
                        if tracked_count > MAX_INCREMENTAL_PATHS {
                            return Ok(None);
                        }
                    }
                }
                Err(err) if err.kind() == ErrorKind::NotFound => {
                    let prefix = format!("{rel}/");
                    let tracked_count = index
                        .prefixed_entries_range(BStr::new(prefix.as_str()))
                        .map_or(0, |range| range.len());
                    if tracked_count > MAX_INCREMENTAL_PATHS {
                        return Ok(None);
                    }
                }
                _ => {}
            }
        }

        // Collapse child paths when an ancestor directory is already targeted.
        let collapsed = collapse_nested_targets(&targets);
        let patterns: Vec<BString> = collapsed
            .iter()
            .map(|rel| {
                let mut pattern = BString::from(":(literal,top)");
                pattern.extend_from_slice(rel.as_bytes());
                pattern
            })
            .collect();

        let iter = repo
            .status(gix::progress::Discard)
            .map_err(Error::gix)?
            .untracked_files(gix::status::UntrackedFiles::Files)
            .into_index_worktree_iter(patterns)
            .map_err(Error::gix)?;

        let mut targeted_unstaged = Vec::new();
        for item in iter {
            let item = item.map_err(Error::gix)?;
            if let Some(change) = unstaged_change(item) {
                if touches_git_or_rules(&change.path) {
                    return Ok(None);
                }
                targeted_unstaged.push(change);
                if targeted_unstaged.len() > MAX_INCREMENTAL_PATHS {
                    return Ok(None);
                }
            }
        }

        let mut unstaged = prev.unstaged.clone();
        unstaged.retain(|change| {
            !matches_any_target(&change.path, &collapsed)
                && !change
                    .old_path
                    .as_deref()
                    .is_some_and(|old| matches_any_target(old, &collapsed))
        });
        unstaged.extend(targeted_unstaged);
        unstaged.sort_by(|a, b| a.path.cmp(&b.path));
        unstaged.dedup();

        Ok(Some(WorktreeStatus {
            staged: prev.staged.clone(),
            unstaged,
        }))
    }

    /// Asserts that an incremental status result matches a full scan.
    ///
    /// Intended for debug checks and tests where the full scan acts as the
    /// single source of truth.
    pub fn assert_incremental_matches_full(&self, incremental: &WorktreeStatus) -> Result<()> {
        let full = self.status()?;
        assert_eq!(
            incremental, &full,
            "incremental status diverged from full status scan"
        );
        Ok(())
    }

    /// Files changed by `commit` relative to its first parent (or the empty tree).
    ///
    /// Renames are detected. The tree walk is shared with
    /// [`commit_stat`](Self::commit_stat): whichever of the two requests arrives
    /// first computes it, and only the file list runs rename detection over it.
    pub fn commit_changes(&self, commit: Oid) -> Result<Vec<FileChange>> {
        let diff = self.commit_files(commit)?;
        let mut changes: Vec<FileChange> = diff
            .iter()
            .map(|file| FileChange {
                path: file.path.clone(),
                old_path: file.old_path.clone(),
                kind: file.kind,
            })
            .collect();
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(changes)
    }

    /// Content of `path` at `commit`; `None` if the path does not exist there.
    pub fn file_at_commit(&self, commit: Oid, path: &str) -> Result<Option<Vec<u8>>> {
        let repo = self.local();
        let tree = repo
            .find_commit(commit)
            .map_err(Error::gix)?
            .tree()
            .map_err(Error::gix)?;

        let Some(entry) = tree.lookup_entry_by_path(path).map_err(Error::gix)? else {
            return Ok(None);
        };
        if !entry.mode().is_blob_or_symlink() {
            // Submodules are commits, not file content.
            return Ok(None);
        }
        let blob = repo.find_blob(entry.object_id()).map_err(Error::gix)?;
        Ok(Some(blob.data.clone()))
    }

    /// Content of `path` in the index (stage 0); `None` if not present.
    pub fn file_in_index(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let repo = self.local();
        let index = repo.index_or_empty().map_err(Error::gix)?;
        let Some(entry) =
            index.entry_by_path_and_stage(BStr::new(path), gix::index::entry::Stage::Unconflicted)
        else {
            return Ok(None);
        };
        if entry.mode.is_submodule() || entry.mode.is_sparse() {
            return Ok(None);
        }
        let blob = repo.find_blob(entry.id).map_err(Error::gix)?;
        Ok(Some(blob.data.clone()))
    }

    /// Content of `path` in the working tree as stored on disk; `None` if missing.
    ///
    /// Symbolic links are followed, so their content is the target's content.
    ///
    /// These are the raw bytes, which is what the UI renders and what a discard
    /// writes back. They are *not* comparable to the index: on Windows, and
    /// wherever `core.autocrlf` or a `text`/`eol` attribute applies, the checkout
    /// holds CRLF while the index holds LF. Compare with
    /// [`file_in_worktree_as_git`](Self::file_in_worktree_as_git) instead.
    pub fn file_in_worktree(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let Some(workdir) = self.workdir() else {
            return Ok(None);
        };
        // `Path::join` treats both `/` and `\` as separators on Windows.
        let absolute = workdir.join(path);
        match std::fs::metadata(&absolute) {
            Ok(metadata) if !metadata.is_file() => return Ok(None),
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(Error::Io(err)),
        }
        Ok(Some(std::fs::read(&absolute)?))
    }

    /// Content of `path` in the working tree converted to the form git keeps in
    /// the index (git's "clean" direction); `None` if missing.
    ///
    /// Diffing the index against the worktree needs this side. git converts the
    /// worktree bytes before comparing, so a file whose checkout only differs by
    /// line endings (`core.autocrlf=true`, the Windows default, or a
    /// `.gitattributes` `text`/`eol` setting) is unchanged for git. Raw bytes from
    /// [`file_in_worktree`](Self::file_in_worktree) would show every line as
    /// changed, and line-level patches built from that diff would not apply.
    ///
    /// The conversion covers line endings (`core.autocrlf`, `core.eol` and the
    /// `text`/`eol`/`-text`/`binary` attributes), `working-tree-encoding` and the
    /// `ident` filter. External clean filters such as `git-lfs` are deliberately
    /// not run: reading a file must not spawn helper programs, and their output is
    /// not what a text diff wants. A file whose git form is only defined by such a
    /// filter therefore comes back as the attribute-converted disk content. When a
    /// file cannot be converted at all, its raw bytes are returned rather than an
    /// error, so the diff still renders.
    pub fn file_in_worktree_as_git(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let Some(raw) = self.file_in_worktree(path)? else {
            return Ok(None);
        };
        Ok(Some(self.worktree_to_git(path, &raw).unwrap_or(raw)))
    }

    /// Apply git's to-git conversion for `path` to `raw` worktree bytes.
    fn worktree_to_git(&self, path: &str, raw: &[u8]) -> Result<Vec<u8>> {
        use gix::filter::plumbing::pipeline::convert::ToGitOutcome;
        use gix::objs::Find;

        let repo = self.local();
        let index = repo.index_or_empty().map_err(Error::gix)?;
        // Attributes come from the worktree and fall back to the index, as git does.
        let mut attributes = repo
            .attributes_only(
                &index,
                gix::worktree::stack::state::attributes::Source::WorktreeThenIdMapping,
            )
            .map_err(Error::gix)?
            .detach();
        // Same eol/ident/encoding configuration as `git add`, but without the
        // external drivers, which would run `git-lfs` and friends on a read.
        let mut options = gix::filter::Pipeline::options(&repo).map_err(Error::gix)?;
        options.drivers.clear();
        let mut pipeline = gix::filter::plumbing::Pipeline::new(
            repo.command_context().map_err(Error::gix)?,
            repo.object_hash(),
            options,
        );

        let rela_path = std::path::Path::new(path);
        let index_entry = index.entry_by_path(BStr::new(path));
        // The entry mode lets the attribute walk resolve the same way as for `git add`.
        let platform = attributes.at_path(
            rela_path,
            index_entry.map(|entry| entry.mode),
            &repo.objects,
        )?;
        // With `text=auto` the eol filter asks for the index version to avoid
        // converting a file that already has CRLF in git, so answer it the way git
        // would. A missing or unreadable blob just means "no such object".
        let index_blob = index_entry.map(|entry| entry.id);
        let mut index_object = |buf: &mut Vec<u8>| -> gix::error::Result<Option<()>> {
            let Some(id) = index_blob else {
                return Ok(None);
            };
            match repo.objects.try_find(&id, buf) {
                Ok(Some(object)) if object.kind == gix::object::Kind::Blob => Ok(Some(())),
                Ok(_) => Ok(None),
                Err(_) => Err(gix::error::Message::new("could not read the index blob").into()),
            }
        };

        let outcome = pipeline
            .convert_to_git(
                raw,
                rela_path,
                &mut |_, attrs| {
                    platform.matching_attributes(attrs);
                },
                &mut index_object,
            )
            .map_err(Error::gix)?;

        let mut converted = Vec::with_capacity(raw.len());
        match outcome {
            ToGitOutcome::Unchanged(mut src) => {
                src.read_to_end(&mut converted)?;
            }
            ToGitOutcome::Buffer(buf) => converted.extend_from_slice(buf),
            ToGitOutcome::Process(mut src) => {
                src.read_to_end(&mut converted)?;
            }
        }
        Ok(converted)
    }
}

fn staged_change(change: gix::diff::index::Change) -> Option<FileChange> {
    use gix::diff::index::Change;

    let (path, old_path, kind) = match change {
        Change::Addition {
            location,
            entry_mode,
            ..
        } => {
            if entry_mode.is_sparse() {
                return None;
            }
            (location, None, ChangeKind::Added)
        }
        Change::Deletion {
            location,
            entry_mode,
            ..
        } => {
            if entry_mode.is_sparse() {
                return None;
            }
            (location, None, ChangeKind::Deleted)
        }
        Change::Modification {
            location,
            previous_entry_mode,
            entry_mode,
            ..
        } => {
            // A file replaced by a directory shows up as one modification on the
            // file's path, with the directory's children reported separately.
            if entry_mode.is_sparse() {
                (location, None, ChangeKind::Deleted)
            } else if previous_entry_mode.is_sparse() {
                (location, None, ChangeKind::Added)
            } else if index_kind_changed(previous_entry_mode, entry_mode) {
                (location, None, ChangeKind::TypeChanged)
            } else {
                (location, None, ChangeKind::Modified)
            }
        }
        Change::Rewrite {
            location,
            source_location,
            copy,
            ..
        } => (
            location,
            Some(source_location),
            if copy {
                ChangeKind::Copied
            } else {
                ChangeKind::Renamed
            },
        ),
    };

    Some(FileChange {
        path: to_path(&path),
        old_path: old_path.map(|path| to_path(&path)),
        kind,
    })
}

fn unstaged_change(item: gix::status::index_worktree::Item) -> Option<FileChange> {
    use gix::status::index_worktree::Item;

    let (path, old_path, kind) = match item {
        Item::Modification {
            status, rela_path, ..
        } => {
            let kind = match status {
                EntryStatus::Conflict { .. } => ChangeKind::Conflicted,
                EntryStatus::Change(change) => match change {
                    Change::Removed => ChangeKind::Deleted,
                    Change::Type { .. } => ChangeKind::TypeChanged,
                    Change::Modification { .. } | Change::SubmoduleModification(_) => {
                        ChangeKind::Modified
                    }
                },
                // An intent-to-add entry is an empty promise of an addition, so it
                // behaves like an untracked file for the staging UI.
                EntryStatus::IntentToAdd => ChangeKind::Untracked,
                EntryStatus::NeedsUpdate(_) => return None,
            };
            (rela_path, None, kind)
        }
        Item::DirectoryContents { entry, .. } => {
            if !matches!(entry.status, gix::dir::entry::Status::Untracked) {
                return None;
            }
            (entry.rela_path, None, ChangeKind::Untracked)
        }
        Item::Rewrite {
            source,
            dirwalk_entry,
            copy,
            ..
        } => (
            dirwalk_entry.rela_path,
            Some(source.rela_path().to_owned()),
            if copy {
                ChangeKind::Copied
            } else {
                ChangeKind::Renamed
            },
        ),
    };

    Some(FileChange {
        path: to_path(&path),
        old_path: old_path.map(|path| to_path(&path)),
        kind,
    })
}

fn to_path(path: &[u8]) -> String {
    path.to_str_lossy().into_owned()
}

/// Compare modes by file kind, ignoring the executable bit.
fn index_kind_changed(before: gix::index::entry::Mode, after: gix::index::entry::Mode) -> bool {
    match (before.to_tree_entry_mode(), after.to_tree_entry_mode()) {
        (Some(before), Some(after)) => kind_changed(before, after),
        _ => false,
    }
}

fn kind_changed(before: EntryMode, after: EntryMode) -> bool {
    fn family(mode: EntryMode) -> u8 {
        match mode.kind() {
            gix::object::tree::EntryKind::Blob | gix::object::tree::EntryKind::BlobExecutable => 0,
            gix::object::tree::EntryKind::Link => 1,
            gix::object::tree::EntryKind::Commit => 2,
            gix::object::tree::EntryKind::Tree => 3,
        }
    }
    family(before) != family(after)
}

fn normalize_relative(
    path: &Path,
    workdir: &Path,
    workdir_abs: Option<&Path>,
    workdir_canon: Option<&Path>,
) -> Option<String> {
    let rel = if path.is_relative() {
        path.to_path_buf()
    } else if let Ok(rel) = path.strip_prefix(workdir) {
        rel.to_path_buf()
    } else if let Some(abs) = workdir_abs
        && let Ok(rel) = path.strip_prefix(abs)
    {
        rel.to_path_buf()
    } else if let Some(canon) = workdir_canon
        && let Ok(rel) = path.strip_prefix(canon)
    {
        rel.to_path_buf()
    } else if let Some(canon) = workdir_canon
        && let Ok(path_canon) = path.canonicalize()
        && let Ok(rel) = path_canon.strip_prefix(canon)
    {
        rel.to_path_buf()
    } else {
        return None;
    };

    let mut out = String::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(&part.to_string_lossy());
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

fn touches_git_or_rules(rel: &str) -> bool {
    rel.split('/').any(|part| part == ".git")
        || matches!(
            rel.rsplit('/').next(),
            Some(".gitignore" | ".gitattributes" | ".gitmodules")
        )
}

fn collapse_nested_targets(targets: &BTreeSet<String>) -> Vec<String> {
    let mut collapsed: Vec<String> = Vec::with_capacity(targets.len());
    for target in targets {
        let covered_by_prev = collapsed.last().is_some_and(|prev| {
            target == prev
                || target
                    .strip_prefix(prev.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        });
        if !covered_by_prev {
            collapsed.push(target.clone());
        }
    }
    collapsed
}

fn matches_any_target(path: &str, targets: &[String]) -> bool {
    targets.iter().any(|target| {
        path == target
            || path
                .strip_prefix(target.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

#[cfg(test)]
mod tests {
    use crate::ChangeKind;
    use crate::test_support::shallow_clone_fixture;

    #[test]
    fn status_of_a_shallow_clone_reads_head_and_the_index() {
        let (_source, clone) = shallow_clone_fixture(&["--depth", "2"]);
        let repo = clone.open();
        assert_eq!(
            repo.status().expect("clean status"),
            Default::default(),
            "a fresh shallow clone is clean"
        );

        clone.write("dir/new.txt", "one\nTWO\nthree\nfour\nchanged\n");
        let status = repo.status().expect("dirty status");
        assert_eq!(status.staged, Vec::new());
        assert_eq!(status.unstaged.len(), 1);
        assert_eq!(status.unstaged[0].path, "dir/new.txt");
        assert_eq!(status.unstaged[0].kind, ChangeKind::Modified);
    }
}
