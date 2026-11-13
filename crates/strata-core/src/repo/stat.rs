//! Per-commit line statistics.
//!
//! A commit is compared with its first parent, or with the empty tree for a
//! root commit. The result matches `git diff-tree -r --numstat --no-renames`:
//! a rename is a deletion plus an addition, and a binary file is flagged with
//! no line counts.

use gix::bstr::{BString, ByteSlice};
use gix::object::tree::EntryMode;

use super::Repo;
use crate::diff::{DiffOptions, LineKind, diff_text};
use crate::{CommitStat, Error, FileStat, Oid, Result};

impl Repo {
    /// Added and removed lines of `commit` against its first parent, with the
    /// counts per file.
    ///
    /// Binary files are flagged and contribute nothing to the totals.
    pub fn commit_stat(&self, commit: Oid) -> Result<CommitStat> {
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

        let changes = repo
            .diff_tree_to_tree(
                parent_tree.as_ref(),
                Some(&tree),
                Some(gix::diff::Options::default()),
            )
            .map_err(Error::gix)?;

        let mut stat = CommitStat::default();
        for change in changes {
            let Some(change) = changed_file(change) else {
                continue;
            };
            let file = file_stat(&repo, change)?;
            stat.added += file.added;
            stat.removed += file.removed;
            stat.files.push(file);
        }
        stat.files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(stat)
    }
}

/// The tree entries of one changed file, before their contents are compared.
struct ChangedFile {
    path: String,
    old_path: Option<String>,
    old_id: Option<Oid>,
    new_id: Option<Oid>,
    /// A submodule has no blob content to count lines in.
    uncountable: bool,
}

fn changed_file(change: gix::object::tree::diff::ChangeDetached) -> Option<ChangedFile> {
    use gix::object::tree::diff::ChangeDetached;

    let (path, old_path, old_id, new_id, old_mode, new_mode) = match change {
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
            (location, None, None, Some(id), None, Some(entry_mode))
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
            (location, None, Some(id), None, Some(entry_mode), None)
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
                )
            } else if previous_entry_mode.is_tree() {
                // A directory replaced by a file is an addition of the file.
                (location, None, None, Some(id), None, Some(entry_mode))
            } else {
                (
                    location,
                    None,
                    Some(previous_id),
                    Some(id),
                    Some(previous_entry_mode),
                    Some(entry_mode),
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
            )
        }
    };

    Some(ChangedFile {
        path: to_path(&path),
        old_path: old_path.map(|path| to_path(&path)),
        old_id,
        new_id,
        uncountable: !countable(old_mode) || !countable(new_mode),
    })
}

fn file_stat(repo: &gix::Repository, change: ChangedFile) -> Result<FileStat> {
    let ChangedFile {
        path,
        old_path,
        old_id,
        new_id,
        uncountable,
    } = change;

    if uncountable {
        return Ok(FileStat {
            path,
            old_path,
            added: 0,
            removed: 0,
            binary: true,
        });
    }

    let old = read_blob(repo, old_id)?;
    let new = read_blob(repo, new_id)?;
    let diff = diff_text(
        old.as_deref().unwrap_or_default(),
        new.as_deref().unwrap_or_default(),
        &DiffOptions::default(),
    );
    if diff.binary {
        return Ok(FileStat {
            path,
            old_path,
            added: 0,
            removed: 0,
            binary: true,
        });
    }

    let mut added = 0;
    let mut removed = 0;
    for hunk in &diff.hunks {
        for line in &hunk.lines {
            match line.kind {
                LineKind::Added => added += 1,
                LineKind::Removed => removed += 1,
                LineKind::Context => {}
            }
        }
    }
    Ok(FileStat {
        path,
        old_path,
        added,
        removed,
        binary: false,
    })
}

/// `None` is the absent side of an addition or deletion: empty content, which
/// is countable. A submodule entry is not a blob.
fn countable(mode: Option<EntryMode>) -> bool {
    match mode {
        None => true,
        Some(mode) => mode.is_blob_or_symlink(),
    }
}

fn read_blob(repo: &gix::Repository, id: Option<Oid>) -> Result<Option<Vec<u8>>> {
    let Some(id) = id else {
        return Ok(None);
    };
    let blob = repo.find_blob(id).map_err(Error::gix)?;
    Ok(Some(blob.data.clone()))
}

fn to_path(path: &BString) -> String {
    path.to_str_lossy().into_owned()
}
