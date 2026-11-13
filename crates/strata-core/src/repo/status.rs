//! Owned by the repo-status task: status, commit changes, file contents.

use std::io::ErrorKind;

use gix::bstr::{BStr, ByteSlice};
use gix::object::tree::EntryMode;
use gix::status::plumbing::index_as_worktree::{Change, EntryStatus};
use gix::status::tree_index::TrackRenames;

use super::Repo;
use crate::{ChangeKind, Error, FileChange, Oid, Result, WorktreeStatus};

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

    /// Files changed by `commit` relative to its first parent (or the empty tree).
    pub fn commit_changes(&self, commit: Oid) -> Result<Vec<FileChange>> {
        let repo = self.local();
        let commit = repo.find_commit(commit).map_err(Error::gix)?;
        let tree = commit.tree().map_err(Error::gix)?;
        let parent_tree = match commit.parent_ids().next() {
            Some(parent) => Some(
                repo.find_commit(parent.detach())
                    .map_err(Error::gix)?
                    .tree()
                    .map_err(Error::gix)?,
            ),
            None => None,
        };

        let changes = repo
            .diff_tree_to_tree(
                parent_tree.as_ref(),
                Some(&tree),
                Some(
                    gix::diff::Options::default()
                        .with_rewrites(Some(gix::diff::Rewrites::default())),
                ),
            )
            .map_err(Error::gix)?;

        let mut changes: Vec<FileChange> = changes.into_iter().filter_map(tree_change).collect();
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

    /// Content of `path` in the working tree; `None` if missing.
    ///
    /// Symbolic links are followed, so their content is the target's content.
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

fn tree_change(change: gix::object::tree::diff::ChangeDetached) -> Option<FileChange> {
    use gix::object::tree::diff::ChangeDetached;

    let (path, old_path, kind) = match change {
        ChangeDetached::Addition {
            location,
            entry_mode,
            ..
        } => {
            // Directories are reported through their children.
            if entry_mode.is_tree() {
                return None;
            }
            (location, None, ChangeKind::Added)
        }
        ChangeDetached::Deletion {
            location,
            entry_mode,
            ..
        } => {
            if entry_mode.is_tree() {
                return None;
            }
            (location, None, ChangeKind::Deleted)
        }
        ChangeDetached::Modification {
            location,
            previous_entry_mode,
            entry_mode,
            ..
        } => {
            if entry_mode.is_tree() {
                (location, None, ChangeKind::Deleted)
            } else if previous_entry_mode.is_tree() {
                (location, None, ChangeKind::Added)
            } else if kind_changed(previous_entry_mode, entry_mode) {
                (location, None, ChangeKind::TypeChanged)
            } else {
                (location, None, ChangeKind::Modified)
            }
        }
        ChangeDetached::Rewrite {
            location,
            source_location,
            entry_mode,
            copy,
            ..
        } => {
            if entry_mode.is_tree() {
                return None;
            }
            (
                location,
                Some(source_location),
                if copy {
                    ChangeKind::Copied
                } else {
                    ChangeKind::Renamed
                },
            )
        }
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
