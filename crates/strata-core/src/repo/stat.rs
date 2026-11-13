//! Per-commit line statistics.
//!
//! A commit is compared with its first parent, or with the empty tree for a
//! root commit. The result matches `git diff-tree -r --numstat --no-renames`:
//! a rename is a deletion plus an addition, and a binary file is flagged with
//! no line counts.
//!
//! Counting never materialises hunks or line text: only the edit script's
//! ranges are read. The tree diff a stat needs is the same one the commit's
//! file list needs, so it is cached per commit.

use std::borrow::Cow;
use std::sync::{Arc, MutexGuard, PoisonError};

use gix::bstr::{BString, ByteSlice};
use gix::object::tree::EntryMode;

use super::Repo;
use crate::{ChangeKind, CommitStat, Error, FileStat, Oid, Result};

/// A text diff above this size is skipped: counting the lines of a huge blob
/// costs more than the whole commit view is worth, so the file is reported the
/// way the UI shows "no line counts" (like a binary) instead.
const MAX_STAT_BYTES: u64 = 4 * 1024 * 1024;

/// How many commit diffs to keep. The UI looks at one commit at a time, and a
/// couple of neighbours make walking up and down the log cheap.
const DIFF_CACHE_COMMITS: usize = 4;

impl Repo {
    /// Added and removed lines of `commit` against its first parent, with the
    /// counts per file.
    ///
    /// Binary files are flagged and contribute nothing to the totals, as are
    /// files too large to count that `git` would have diffed.
    pub fn commit_stat(&self, commit: Oid) -> Result<CommitStat> {
        // Any cached diff of this commit is usable: a rename-aware one is
        // counted as a whole deletion plus a whole addition, which is what the
        // no-rename tree diff would have listed.
        let diff = self.commit_stat_diff(commit)?;
        let repo = self.local();

        let mut stat = CommitStat::default();
        for file in &diff.files {
            for file in file.no_rename_sides(diff.renames) {
                let file = file_stat(&repo, &file)?;
                stat.added += file.added;
                stat.removed += file.removed;
                stat.files.push(file);
            }
        }
        stat.files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(stat)
    }

    /// The first-parent tree diff of `commit`.
    ///
    /// `renames` asks for rewrite detection, which is what `commit_changes`
    /// reports. Both variants are cached by commit id: the UI asks for the file
    /// list and the line counts of the same commit, and the tree walk is the
    /// expensive half of both.
    pub(crate) fn commit_diff(&self, commit: Oid, renames: bool) -> Result<Arc<CommitDiff>> {
        if let Some(diff) = self.commit_diffs().get(commit, renames) {
            return Ok(diff);
        }

        let diff = Arc::new(self.diff_first_parent(commit, renames)?);
        self.commit_diffs().insert(commit, &diff);
        Ok(diff)
    }

    /// The tree diff a line count needs: whichever variant of `commit` is
    /// cached, because both carry the same paths and blob ids.
    fn commit_stat_diff(&self, commit: Oid) -> Result<Arc<CommitDiff>> {
        if let Some(diff) = self.commit_diffs().get_any(commit) {
            return Ok(diff);
        }

        let diff = Arc::new(self.diff_first_parent(commit, false)?);
        self.commit_diffs().insert(commit, &diff);
        Ok(diff)
    }

    fn diff_first_parent(&self, commit: Oid, renames: bool) -> Result<CommitDiff> {
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
            // A root commit added everything, so the empty tree is the other side.
            None => None,
        };

        let options = if renames {
            gix::diff::Options::default().with_rewrites(Some(gix::diff::Rewrites::default()))
        } else {
            gix::diff::Options::default()
        };
        let changes = repo
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(options))
            .map_err(Error::gix)?;

        Ok(CommitDiff {
            renames,
            files: changes.into_iter().filter_map(changed_file).collect(),
        })
    }

    fn commit_diffs(&self) -> MutexGuard<'_, CommitDiffCache> {
        self.commit_diffs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// One first-parent tree diff. `renames` records whether rewrite detection ran,
/// because that decides how a rewrite is counted.
pub(crate) struct CommitDiff {
    pub(crate) renames: bool,
    pub(crate) files: Vec<ChangedFile>,
}

/// Recent commit diffs, at most one entry per commit and rewrite mode.
#[derive(Default)]
pub(crate) struct CommitDiffCache {
    entries: Vec<(Oid, bool, Arc<CommitDiff>)>,
}

impl CommitDiffCache {
    fn get(&mut self, commit: Oid, renames: bool) -> Option<Arc<CommitDiff>> {
        self.get_by(|id, with_renames| *id == commit && with_renames == renames)
    }

    /// Any cached diff of `commit`, regardless of rewrite detection.
    fn get_any(&mut self, commit: Oid) -> Option<Arc<CommitDiff>> {
        self.get_by(|id, _| *id == commit)
    }

    fn get_by(&mut self, predicate: impl Fn(&Oid, bool) -> bool) -> Option<Arc<CommitDiff>> {
        let index = self
            .entries
            .iter()
            .position(|(id, renames, _)| predicate(id, *renames))?;
        // A hit becomes the most recently used entry.
        let entry = self.entries.remove(index);
        let diff = Arc::clone(&entry.2);
        self.entries.push(entry);
        Some(diff)
    }

    fn insert(&mut self, commit: Oid, diff: &Arc<CommitDiff>) {
        self.entries
            .retain(|(id, renames, _)| !(*id == commit && *renames == diff.renames));
        self.entries.push((commit, diff.renames, Arc::clone(diff)));
        if self.entries.len() > DIFF_CACHE_COMMITS {
            self.entries.remove(0);
        }
    }
}

/// One file of a first-parent tree diff, before its lines are counted.
///
/// Shared with `commit_changes`, which reports the same entries as git's
/// `--name-status` does.
#[derive(Clone)]
pub(crate) struct ChangedFile {
    pub(crate) path: String,
    pub(crate) old_path: Option<String>,
    pub(crate) old_id: Option<Oid>,
    pub(crate) new_id: Option<Oid>,
    pub(crate) old_mode: Option<EntryMode>,
    pub(crate) new_mode: Option<EntryMode>,
    pub(crate) kind: ChangeKind,
}

impl ChangedFile {
    /// A submodule has no blob content to count lines in.
    fn uncountable(&self) -> bool {
        !countable(self.old_mode) || !countable(self.new_mode)
    }

    /// The entries a no-rename diff would list for this one.
    ///
    /// A rewrite hides a deletion and an addition: the file list shows one
    /// renamed path, while `--numstat --no-renames` counts the old and the new
    /// content in full, on two paths. A copy keeps its source, so only the
    /// added side is counted.
    fn no_rename_sides(&self, renames: bool) -> Vec<Cow<'_, ChangedFile>> {
        match (renames, self.kind) {
            (true, ChangeKind::Renamed) => {
                let mut sides = Vec::with_capacity(2);
                if let Some(old_path) = &self.old_path {
                    sides.push(Cow::Owned(ChangedFile {
                        path: old_path.clone(),
                        old_path: None,
                        old_id: self.old_id,
                        new_id: None,
                        old_mode: self.old_mode,
                        new_mode: None,
                        kind: ChangeKind::Deleted,
                    }));
                }
                sides.push(Cow::Owned(ChangedFile {
                    path: self.path.clone(),
                    old_path: None,
                    old_id: None,
                    new_id: self.new_id,
                    old_mode: None,
                    new_mode: self.new_mode,
                    kind: ChangeKind::Added,
                }));
                sides
            }
            (true, ChangeKind::Copied) => vec![Cow::Owned(ChangedFile {
                path: self.path.clone(),
                old_path: None,
                old_id: None,
                new_id: self.new_id,
                old_mode: None,
                new_mode: self.new_mode,
                kind: ChangeKind::Added,
            })],
            _ => vec![Cow::Borrowed(self)],
        }
    }
}

/// The parsed tree change of one entry; `None` for a directory entry, whose
/// children carry the changes.
fn changed_file(change: gix::object::tree::diff::ChangeDetached) -> Option<ChangedFile> {
    use gix::object::tree::diff::ChangeDetached;

    let (path, old_path, old_id, new_id, old_mode, new_mode, kind) = match change {
        ChangeDetached::Addition {
            location,
            entry_mode,
            id,
            ..
        } => {
            // Directories are reported through their children.
            if entry_mode.is_tree() {
                return None;
            }
            (
                location,
                None,
                None,
                Some(id),
                None,
                Some(entry_mode),
                ChangeKind::Added,
            )
        }
        ChangeDetached::Deletion {
            location,
            entry_mode,
            id,
            ..
        } => {
            if entry_mode.is_tree() {
                return None;
            }
            (
                location,
                None,
                Some(id),
                None,
                Some(entry_mode),
                None,
                ChangeKind::Deleted,
            )
        }
        ChangeDetached::Modification {
            location,
            previous_entry_mode,
            previous_id,
            entry_mode,
            id,
        } => {
            if entry_mode.is_tree() && previous_entry_mode.is_tree() {
                // A changed directory is reported through its children.
                return None;
            } else if entry_mode.is_tree() {
                // A file replaced by a directory is a deletion of the file.
                (
                    location,
                    None,
                    Some(previous_id),
                    None,
                    Some(previous_entry_mode),
                    None,
                    ChangeKind::Deleted,
                )
            } else if previous_entry_mode.is_tree() {
                // A directory replaced by a file is an addition of the file.
                (
                    location,
                    None,
                    None,
                    Some(id),
                    None,
                    Some(entry_mode),
                    ChangeKind::Added,
                )
            } else {
                let kind = if kind_changed(previous_entry_mode, entry_mode) {
                    ChangeKind::TypeChanged
                } else {
                    ChangeKind::Modified
                };
                (
                    location,
                    None,
                    Some(previous_id),
                    Some(id),
                    Some(previous_entry_mode),
                    Some(entry_mode),
                    kind,
                )
            }
        }
        ChangeDetached::Rewrite {
            location,
            source_location,
            source_entry_mode,
            source_id,
            entry_mode,
            id,
            copy,
            ..
        } => {
            if entry_mode.is_tree() {
                return None;
            }
            (
                location,
                Some(source_location),
                Some(source_id),
                Some(id),
                Some(source_entry_mode),
                Some(entry_mode),
                if copy {
                    ChangeKind::Copied
                } else {
                    ChangeKind::Renamed
                },
            )
        }
    };

    Some(ChangedFile {
        path: to_path(&path),
        old_path: old_path.map(|path| to_path(&path)),
        old_id,
        new_id,
        old_mode,
        new_mode,
        kind,
    })
}

fn file_stat(repo: &gix::Repository, change: &ChangedFile) -> Result<FileStat> {
    if change.uncountable() {
        return Ok(binary_stat(change));
    }

    // A missing side is empty content: an addition or a deletion, counted in
    // full. Identical blobs only changed mode, which has no lines.
    let (added, removed) = match (change.old_id, change.new_id) {
        (None, None) => return Ok(binary_stat(change)),
        (old, new) if old == new => (0, 0),
        (old, new) => {
            if is_large(repo, old) || is_large(repo, new) {
                return Ok(binary_stat(change));
            }
            let old = read_blob(repo, old)?;
            let new = read_blob(repo, new)?;
            let old = old.as_deref().unwrap_or_default();
            let new = new.as_deref().unwrap_or_default();
            if is_binary(old) || is_binary(new) {
                return Ok(binary_stat(change));
            }
            match (change.old_id, change.new_id) {
                (None, Some(_)) => (count_lines(new), 0),
                (Some(_), None) => (0, count_lines(old)),
                _ => count_changed_lines(old, new),
            }
        }
    };

    Ok(FileStat {
        path: change.path.clone(),
        old_path: change.old_path.clone(),
        added,
        removed,
        binary: false,
    })
}

/// A binary or uncountable file: flagged, with no line counts.
fn binary_stat(change: &ChangedFile) -> FileStat {
    FileStat {
        path: change.path.clone(),
        old_path: change.old_path.clone(),
        added: 0,
        removed: 0,
        binary: true,
    }
}

/// The lines a blob adds or removes as a whole file.
fn count_lines(data: &[u8]) -> u64 {
    let lines = data.iter().filter(|byte| **byte == b'\n').count();
    let lines = if data.is_empty() || data.ends_with(b"\n") {
        lines
    } else {
        // A final line without a terminator still counts.
        lines + 1
    };
    lines as u64
}

/// Added and removed lines of two text blobs.
///
/// Only the edit script's ranges are read: building hunks, line text and
/// emphasis would mean allocating a copy of every changed line, which is what
/// made counting a large merge as slow as rendering it.
fn count_changed_lines(old: &[u8], new: &[u8]) -> (u64, u64) {
    let input = imara_diff::InternedInput::new(old, new);
    let diff = imara_diff::Diff::compute(imara_diff::Algorithm::Histogram, &input);
    let mut added = 0;
    let mut removed = 0;
    for hunk in diff.hunks() {
        removed += u64::from(hunk.before.end - hunk.before.start);
        added += u64::from(hunk.after.end - hunk.after.start);
    }
    (added, removed)
}

/// Whether a blob looks binary: a NUL byte near its start, as git decides.
///
/// Only the head is inspected, which keeps the check cheap for a large file.
fn is_binary(data: &[u8]) -> bool {
    const PROBE: usize = 8_000;
    data[..data.len().min(PROBE)].contains(&0)
}

/// `None` is the absent side of an addition or deletion: empty content, which
/// is countable. A submodule entry is not a blob.
fn countable(mode: Option<EntryMode>) -> bool {
    match mode {
        None => true,
        Some(mode) => mode.is_blob_or_symlink(),
    }
}

/// Whether a blob is too large to count, read from its header so the content is
/// never inflated for it.
fn is_large(repo: &gix::Repository, id: Option<Oid>) -> bool {
    let Some(id) = id else {
        return false;
    };
    match repo.try_find_header(id) {
        Ok(Some(header)) => header.size() > MAX_STAT_BYTES,
        _ => false,
    }
}

fn read_blob(repo: &gix::Repository, id: Option<Oid>) -> Result<Option<Vec<u8>>> {
    let Some(id) = id else {
        return Ok(None);
    };
    let blob = repo.find_blob(id).map_err(Error::gix)?;
    Ok(Some(blob.data.clone()))
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

fn to_path(path: &BString) -> String {
    path.to_str_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffOptions, LineKind, diff_text};

    /// The counts of the full text diff, which used to back the stat.
    fn text_counts(old: &[u8], new: &[u8]) -> (u64, u64) {
        let diff = diff_text(old, new, &DiffOptions::default());
        let mut added = 0;
        let mut removed = 0;
        for line in diff.hunks.iter().flat_map(|hunk| &hunk.lines) {
            match line.kind {
                LineKind::Added => added += 1,
                LineKind::Removed => removed += 1,
                LineKind::Context => {}
            }
        }
        (added, removed)
    }

    #[test]
    fn counting_matches_the_full_text_diff() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"", b""),
            (b"", b"one\n"),
            (b"one\n", b""),
            (b"one\ntwo\n", b"one\nthree\n"),
            // A move that the slider heuristic can place differently.
            (b"a\nb\na\nb\n", b"b\na\nb\na\n"),
            // A dropped terminator and a CRLF switch are changes.
            (b"a\nb", b"a\nb\n"),
            (b"a\r\nb\r\n", b"a\nb\n"),
            (b"x\ny\nx\ny\n", b"x\nx\ny\n"),
            (b"one\ntwo\n", b"one\ntwo\nthree\n"),
        ];
        for (old, new) in cases {
            assert_eq!(
                count_changed_lines(old, new),
                text_counts(old, new),
                "old={old:?} new={new:?}"
            );
        }
    }

    #[test]
    fn counting_a_whole_file_keeps_a_final_line_without_terminator() {
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"\n"), 1);
        assert_eq!(count_lines(b"one\n"), 1);
        assert_eq!(count_lines(b"one\ntwo"), 2);
        assert_eq!(count_lines(b"one\n\n"), 2);
    }
}
