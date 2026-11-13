//! Owned by the repo-status task: status, commit changes, file contents.

use super::Repo;
use crate::{FileChange, Oid, Result, WorktreeStatus};

impl Repo {
    pub fn status(&self) -> Result<WorktreeStatus> {
        todo!("repo-status task")
    }

    /// Files changed by `commit` relative to its first parent (or the empty tree).
    pub fn commit_changes(&self, _commit: Oid) -> Result<Vec<FileChange>> {
        todo!("repo-status task")
    }

    /// Content of `path` at `commit`; `None` if the path does not exist there.
    pub fn file_at_commit(&self, _commit: Oid, _path: &str) -> Result<Option<Vec<u8>>> {
        todo!("repo-status task")
    }

    /// Content of `path` in the index (stage 0); `None` if not present.
    pub fn file_in_index(&self, _path: &str) -> Result<Option<Vec<u8>>> {
        todo!("repo-status task")
    }

    /// Content of `path` in the working tree; `None` if missing.
    pub fn file_in_worktree(&self, _path: &str) -> Result<Option<Vec<u8>>> {
        todo!("repo-status task")
    }
}
