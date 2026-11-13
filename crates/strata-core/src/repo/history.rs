//! History limited to a path. Owned by the history task.

use super::Repo;
use crate::{Oid, Result};

/// Streams ids of commits that changed `path` (a file, or a directory prefix),
/// in the same order as [`Repo::walk_commits`].
pub struct PathHistory {
    _private: (),
}

impl Iterator for PathHistory {
    type Item = Result<Oid>;

    fn next(&mut self) -> Option<Self::Item> {
        todo!("history task")
    }
}

impl Repo {
    /// Like `git log --all --date-order -- <path>`, without rename following.
    pub fn path_history(&self, _path: &str) -> Result<PathHistory> {
        todo!("history task")
    }
}
