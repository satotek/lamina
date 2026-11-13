//! Owned by the repo-log task: commit walk, refs, HEAD.

use super::Repo;
use crate::{CommitInfo, HeadInfo, RefInfo, Result};

/// Streams commits reachable from HEAD and all refs, in topological order
/// (children before parents), newest first among independent commits.
pub struct CommitWalk {
    _private: (),
}

impl Iterator for CommitWalk {
    type Item = Result<CommitInfo>;

    fn next(&mut self) -> Option<Self::Item> {
        todo!("repo-log task")
    }
}

impl Repo {
    pub fn head(&self) -> Result<HeadInfo> {
        todo!("repo-log task")
    }

    pub fn refs(&self) -> Result<Vec<RefInfo>> {
        todo!("repo-log task")
    }

    pub fn walk_commits(&self) -> Result<CommitWalk> {
        todo!("repo-log task")
    }
}
