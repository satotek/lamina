//! Read-only repository access through gix.
//!
//! `log.rs` (commit walk, refs, HEAD), `status.rs` (status, commit changes,
//! blob/file content), `conflicts.rs` (repository state, conflicted paths),
//! `upstream.rs` (tracking branches), `history.rs` (path-limited history),
//! `message.rs` (commit and merge messages) and `remotes.rs` (configured
//! remotes) are owned by separate tasks. `stat.rs` counts the lines a commit
//! added and removed. `submodules.rs` and `worktrees.rs` list the checkouts a
//! repository owns beyond its own working tree.

mod conflicts;
mod history;
mod log;
mod message;
mod remotes;
mod stat;
mod status;
mod submodules;
mod tree;
mod upstream;
mod worktrees;

pub use history::{PathHistory, PathHistoryEntries, PathHistoryEntry};
pub use log::CommitWalk;
pub use submodules::{SubmoduleInfo, SubmoduleStatus};
pub use worktrees::WorktreeInfo;

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Cheap to clone; safe to share across threads.
#[derive(Clone)]
pub struct Repo {
    inner: gix::ThreadSafeRepository,
    workdir: Option<PathBuf>,
}

impl Repo {
    /// Discover the repository containing `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let inner = gix::ThreadSafeRepository::discover(path)
            .map_err(|_| Error::NotARepository(path.to_path_buf()))?;
        let workdir = inner.work_dir().map(Path::to_path_buf);
        Ok(Self { inner, workdir })
    }

    pub fn workdir(&self) -> Option<&Path> {
        self.workdir.as_deref()
    }

    pub fn git_dir(&self) -> &Path {
        self.inner.git_dir()
    }

    /// A thread-local handle for gix calls.
    pub(crate) fn local(&self) -> gix::Repository {
        self.inner.to_thread_local()
    }
}
