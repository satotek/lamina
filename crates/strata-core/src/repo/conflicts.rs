//! Repository state and conflicted paths. Owned by the conflicts task.

use super::Repo;
use crate::{ConflictEntry, ConflictSides, RepoState, Result};

impl Repo {
    /// Which multi-step operation, if any, is in progress.
    pub fn state(&self) -> Result<RepoState> {
        todo!("conflicts task")
    }

    /// Unmerged paths in the index, sorted by path.
    pub fn conflicts(&self) -> Result<Vec<ConflictEntry>> {
        todo!("conflicts task")
    }

    /// Contents of the index stages of a conflicted `path`.
    pub fn conflict_sides(&self, _path: &str) -> Result<ConflictSides> {
        todo!("conflicts task")
    }
}
