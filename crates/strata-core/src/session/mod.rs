//! One open repository, orchestrated for a UI.
//!
//! `Session` ties the read modules (`repo`, `graph`, `diff`, `highlight`),
//! the write module (`git_cli`, `patch`) and `watch` together, and publishes
//! immutable [`RepoSnapshot`]s. The UI never touches git directly: it renders
//! the latest snapshot and sends [`Operation`]s and [`DiffRequest`]s.
//!
//! Threading: all work runs on session-owned background threads. Results are
//! delivered through the `on_event` callback, also on a background thread; the
//! UI forwards them to its own main thread. Nothing here depends on a UI
//! toolkit or an async runtime.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::diff::FileDiff;
use crate::git_cli::{CommandRecord, Git};
use crate::graph::GraphRow;
use crate::highlight::LineHighlights;
use crate::patch::LineSelection;
use crate::{CommitInfo, HeadInfo, Oid, RefInfo, Result, WorktreeStatus};

/// Commits are published in chunks so a growing list can be shared with the UI
/// without copying it on every append.
pub const COMMIT_CHUNK: usize = 4096;

/// Append-only list of commits with their graph rows, in display order.
/// Cloning is cheap: chunks are shared.
#[derive(Debug, Clone, Default)]
pub struct CommitList {
    chunks: Vec<Arc<CommitChunk>>,
    len: usize,
    /// True once the walk reached the end of history.
    pub complete: bool,
}

#[derive(Debug, Default)]
pub struct CommitChunk {
    pub commits: Vec<CommitInfo>,
    pub rows: Vec<GraphRow>,
}

impl CommitList {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, index: usize) -> Option<(&CommitInfo, &GraphRow)> {
        let chunk = self.chunks.get(index / COMMIT_CHUNK)?;
        let offset = index % COMMIT_CHUNK;
        Some((chunk.commits.get(offset)?, chunk.rows.get(offset)?))
    }

    /// Appends rows; only the last chunk is copied (at most COMMIT_CHUNK items).
    pub fn extend(&mut self, _commits: Vec<CommitInfo>, _rows: Vec<GraphRow>) {
        todo!("session task: fill the last partial chunk, then push new chunks")
    }
}

/// Everything the UI shows for one repository at one point in time.
#[derive(Debug, Clone)]
pub struct RepoSnapshot {
    /// Increases on every publish; lets the UI skip redundant re-renders.
    pub generation: u64,
    pub head: HeadInfo,
    pub refs: Arc<Vec<RefInfo>>,
    /// Indices into `refs`, for drawing ref badges next to commits.
    pub refs_by_commit: Arc<HashMap<Oid, Vec<usize>>>,
    pub commits: CommitList,
    /// Row index of each loaded commit, for "jump to commit" and selection.
    pub commit_index: Arc<HashMap<Oid, usize>>,
    pub status: Arc<WorktreeStatus>,
    /// Operation currently running, if any (the UI disables conflicting actions).
    pub busy: Option<OperationId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OperationId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DiffTicket(pub u64);

/// Mutations. Each runs through the git CLI, one at a time, in submission order.
#[derive(Debug, Clone)]
pub enum Operation {
    StagePaths(Vec<String>),
    UnstagePaths(Vec<String>),
    DiscardPaths(Vec<String>),
    DeleteUntracked(Vec<String>),
    /// Stage selected lines of the current index -> worktree diff of `path`.
    StageLines {
        path: String,
        selection: LineSelection,
    },
    /// Unstage selected lines of the current HEAD -> index diff of `path`.
    UnstageLines {
        path: String,
        selection: LineSelection,
    },
    /// Discard selected lines of the current index -> worktree diff of `path`.
    DiscardLines {
        path: String,
        selection: LineSelection,
    },
    Commit {
        message: String,
        amend: bool,
    },
    Checkout {
        branch: String,
    },
    CreateBranch {
        name: String,
        start_point: Option<String>,
        checkout: bool,
    },
    DeleteBranch {
        name: String,
        force: bool,
    },
    Fetch {
        remote: Option<String>,
    },
    Pull {
        rebase: Option<bool>,
    },
    Push {
        remote: Option<String>,
        branch: Option<String>,
        set_upstream: bool,
    },
    StashPush {
        message: Option<String>,
        include_untracked: bool,
    },
    StashPop {
        index: usize,
    },
}

/// Which pair of sides to diff.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DiffRequest {
    /// `path` in `commit` vs its first parent.
    Commit { commit: Oid, path: String },
    /// HEAD vs index.
    Staged { path: String },
    /// Index vs worktree (worktree converted to git form, see `Repo::file_in_worktree_as_git`).
    Unstaged { path: String },
}

/// A diff ready to render. Highlights are per line of the full old/new text,
/// indexed by `DiffLine::old_lineno` / `new_lineno` minus one.
#[derive(Debug, Clone)]
pub struct DiffView {
    pub request: DiffRequest,
    pub diff: FileDiff,
    /// Empty until highlighting finishes; a second `DiffReady` event follows.
    pub old_highlights: Arc<Vec<LineHighlights>>,
    pub new_highlights: Arc<Vec<LineHighlights>>,
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// A new snapshot is available via `Session::snapshot`.
    SnapshotUpdated {
        generation: u64,
    },
    OperationStarted {
        id: OperationId,
    },
    /// `error` is a user-facing message (git's stderr for CLI failures).
    OperationFinished {
        id: OperationId,
        error: Option<String>,
    },
    /// Sent once without highlights, then again with them.
    DiffReady {
        ticket: DiffTicket,
        view: Arc<DiffView>,
    },
    DiffFailed {
        ticket: DiffTicket,
        error: String,
    },
    /// A git command finished; for the command log panel.
    CommandLogged(CommandRecord),
    /// Background loading failed (e.g. repository deleted).
    LoadFailed {
        error: String,
    },
}

pub struct Session {
    _private: (),
}

impl Session {
    /// Opens the repository at `path` and starts loading. Returns immediately;
    /// the first snapshot (possibly with only part of the history) arrives as
    /// `SnapshotUpdated`.
    pub fn open(
        _path: impl AsRef<Path>,
        _on_event: impl Fn(SessionEvent) + Send + Sync + 'static,
    ) -> Result<Self> {
        todo!("session task")
    }

    /// Latest published snapshot. Cheap; call it on every render.
    pub fn snapshot(&self) -> Arc<RepoSnapshot> {
        todo!("session task")
    }

    /// Queues a mutation. The session refreshes affected state afterwards.
    pub fn run(&self, _op: Operation) -> OperationId {
        todo!("session task")
    }

    /// Computes a diff in the background. Newer requests do not cancel older
    /// ones automatically; call `cancel_diff` for a selection the user left.
    pub fn request_diff(&self, _request: DiffRequest) -> DiffTicket {
        todo!("session task")
    }

    pub fn cancel_diff(&self, _ticket: DiffTicket) {
        todo!("session task")
    }

    /// Forces a full reload (refs, history, status).
    pub fn reload(&self) {
        todo!("session task")
    }

    pub fn git(&self) -> &Git {
        todo!("session task")
    }
}
