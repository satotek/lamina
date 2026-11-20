//! Tree listings and blob reads at a commit.

use gix::bstr::ByteSlice;
use gix::object::tree::EntryKind;
use gix::objs::TreeRefIter;

use super::Repo;
use crate::lfs::{LfsPointer, MAX_POINTER_BYTES};
use crate::{Error, Oid, Result, TreeEntry, TreeEntryKind};

impl Repo {
    /// One level of the tree at `commit`, starting at `dir_path` (`""` or `/`
    /// for the repository root).
    ///
    /// Sorted directories first, then by name, so a file list can be rendered
    /// without checking kinds in the UI. A path that does not name a directory
    /// is an error rather than an empty list.
    pub fn tree_entries(&self, commit: Oid, dir_path: &str) -> Result<Vec<TreeEntry>> {
        let repo = self.local();
        let root = repo
            .find_commit(commit)
            .map_err(Error::gix)?
            .tree()
            .map_err(Error::gix)?;
        let directory = match directory_tree_id(&root, dir_path)? {
            Some(id) => repo.find_tree(id).map_err(Error::gix)?,
            None => root,
        };

        let prefix = normalize(dir_path);
        let mut entries = Vec::new();
        for entry in TreeRefIter::from_bytes(&directory.data, directory.id.kind()) {
            let entry = entry.map_err(Error::gix)?;
            let kind = tree_entry_kind(entry.mode.kind());
            let name = entry.filename.to_str_lossy().into_owned();
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let size = match kind {
                TreeEntryKind::File | TreeEntryKind::Symlink => {
                    blob_size(&repo, entry.oid.to_owned())
                }
                TreeEntryKind::Directory | TreeEntryKind::Submodule => None,
            };
            entries.push(TreeEntry {
                name,
                path,
                kind,
                mode: entry.mode.value(),
                size,
            });
        }

        entries.sort_by(|a, b| {
            directory_rank(a)
                .cmp(&directory_rank(b))
                .then_with(|| a.name.as_bytes().cmp(b.name.as_bytes()))
        });
        Ok(entries)
    }

    /// Content of the blob at `path` in `commit`.
    ///
    /// Unlike [`file_at_commit`](Self::file_at_commit), a missing path, a
    /// directory or a submodule is an error: callers that asked for a blob
    /// should not have to tell "empty" apart from "absent".
    pub fn blob_at(&self, commit: Oid, path: &str) -> Result<Vec<u8>> {
        self.file_at_commit(commit, path)?
            .ok_or_else(|| Error::other(format!("no blob at {path} in {commit}")))
    }
}

/// The tree id `dir_path` names inside `root`, or `None` for the root itself.
fn directory_tree_id(root: &gix::Tree<'_>, dir_path: &str) -> Result<Option<Oid>> {
    let normalized = normalize(dir_path);
    if normalized.is_empty() {
        return Ok(None);
    }
    match root.lookup_entry_by_path(&normalized).map_err(Error::gix)? {
        Some(entry) if entry.mode().is_tree() => Ok(Some(entry.object_id())),
        Some(_) => Err(Error::other(format!("{dir_path} is not a directory"))),
        None => Err(Error::other(format!("no such directory: {dir_path}"))),
    }
}

fn tree_entry_kind(kind: EntryKind) -> TreeEntryKind {
    match kind {
        EntryKind::Tree => TreeEntryKind::Directory,
        EntryKind::Link => TreeEntryKind::Symlink,
        EntryKind::Commit => TreeEntryKind::Submodule,
        // Blob and BlobExecutable; the executable bit is carried by the mode.
        EntryKind::Blob | EntryKind::BlobExecutable => TreeEntryKind::File,
    }
}

fn directory_rank(entry: &TreeEntry) -> u8 {
    u8::from(entry.kind != TreeEntryKind::Directory)
}

/// Blob size from the object header, which avoids inflating the content.
///
/// An LFS pointer's header carries the metadata file's size; the listing wants
/// the real object's size, so pointers small enough to be one are inflated and
/// parsed.
fn blob_size(repo: &gix::Repository, id: Oid) -> Option<u64> {
    match repo.try_find_header(id) {
        Ok(Some(header)) if header.kind() == gix::objs::Kind::Blob => {
            if header.size() <= MAX_POINTER_BYTES as u64
                && let Ok(blob) = repo.find_blob(id)
                && let Some(pointer) = LfsPointer::parse(&blob.data)
            {
                return Some(pointer.size);
            }
            Some(header.size())
        }
        _ => None,
    }
}

/// Split and re-join a directory path so repeated, trailing and `.` components
/// do not change which tree is named.
fn normalize(path: &str) -> String {
    path.split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .collect::<Vec<_>>()
        .join("/")
}
