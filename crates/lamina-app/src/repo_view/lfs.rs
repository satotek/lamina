//! LFS metadata for the surfaces that show it.
//!
//! The commit file list, the worktree rows, the remote locks and the install
//! hint all describe the same optional tool but arrive at different times and
//! are keyed by path. Caching them together keeps them off the render path and
//! gives the view one place to reset or refresh LFS state.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use lamina_core::lfs::{LfsInfo, LfsLock};

#[derive(Default)]
pub(crate) struct LfsState {
    /// LFS metadata for the changed files of the selected commit, keyed by
    /// path. Read with the file list so a badge never blocks a frame.
    pub(super) files: Arc<HashMap<String, LfsInfo>>,
    /// LFS metadata for the worktree rows, keyed by path.
    pub(crate) worktree: Arc<HashMap<String, LfsInfo>>,
    /// Bumped per read so a stale answer cannot replace a newer one.
    pub(super) worktree_epoch: u64,
    /// Remote LFS locks, for the worktree context menu.
    pub(crate) locks: Rc<HashMap<String, LfsLock>>,
    /// Whether the remote answered, which decides whether Lock and Unlock are
    /// offered at all.
    pub(crate) locking_available: bool,
    /// The locks were asked for once; a failed read is not retried per snapshot.
    pub(super) locks_loaded: bool,
    /// The one-time warning shown when the repository uses LFS but git-lfs is
    /// not installed.
    pub(super) notice: Option<String>,
    pub(super) notice_dismissed: bool,
}
