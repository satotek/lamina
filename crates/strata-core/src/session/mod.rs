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

mod diffs;
mod history;
mod operations;
mod publish;
mod refresh;

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::diff::FileDiff;
use crate::git_cli::{CommandRecord, Git};
use crate::graph::GraphRow;
use crate::highlight::LineHighlights;
use crate::patch::LineSelection;
use crate::repo::Repo;
use crate::watch::RepoWatcher;
use crate::{CommitInfo, Error, HeadInfo, Oid, RefInfo, Result, WorktreeStatus};

use diffs::DiffJob;
use history::LoadCommand;
use operations::OperationJob;
use publish::{EventSink, Shared};
use refresh::RefreshStatus;

/// Commits are published in chunks so a growing list can be shared with the UI
/// without copying it on every append.
pub const COMMIT_CHUNK: usize = 4096;

/// Quiet window of the filesystem watcher. Long enough for one git command's
/// burst of writes to collapse into a single refresh.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(150);

/// One diff thread would serialize a pinned diff behind the current selection.
const DIFF_WORKERS: usize = 2;

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
    pub fn extend(&mut self, commits: Vec<CommitInfo>, rows: Vec<GraphRow>) {
        debug_assert_eq!(commits.len(), rows.len(), "one graph row per commit");
        if commits.is_empty() {
            return;
        }
        let mut commits = commits.into_iter();
        let mut rows = rows.into_iter();

        // Chunks are shared with published snapshots, so completing the last
        // partial one means copying it instead of mutating it in place.
        let mut tail = match self.chunks.last() {
            Some(last) if last.commits.len() < COMMIT_CHUNK => Some(CommitChunk {
                commits: last.commits.clone(),
                rows: last.rows.clone(),
            }),
            _ => None,
        };
        if tail.is_some() {
            self.chunks.pop();
        }

        loop {
            let mut chunk = tail.take().unwrap_or_default();
            // A copied chunk was already counted when it was first published.
            let existing = chunk.commits.len();
            while chunk.commits.len() < COMMIT_CHUNK {
                // A caller that disagrees about the lengths loses the extra
                // commits instead of building a chunk without rows.
                let (Some(commit), Some(row)) = (commits.next(), rows.next()) else {
                    break;
                };
                chunk.commits.push(commit);
                chunk.rows.push(row);
            }
            let added = chunk.commits.len() - existing;
            if added == 0 {
                // Only possible when the commit and row lists disagree; the
                // copied chunk must go back so the list stays readable.
                if existing > 0 {
                    self.chunks.push(Arc::new(chunk));
                }
                break;
            }
            let full = chunk.commits.len() == COMMIT_CHUNK;
            self.len += added;
            self.chunks.push(Arc::new(chunk));
            if !full {
                break;
            }
        }
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
    shared: Arc<Shared>,
    /// Held so that dropping the session closes every worker's channel.
    senders: Option<Senders>,
    /// Kept last-in, first-dropped: its callback holds channel senders.
    watcher: Option<RepoWatcher>,
    threads: Vec<JoinHandle<()>>,
}

/// Channel ends the session (and its watcher) use to hand work to the threads.
struct Senders {
    load: Sender<LoadCommand>,
    status: Sender<RefreshStatus>,
    operations: Sender<OperationJob>,
    diffs: Sender<DiffJob>,
}

impl Session {
    /// Opens the repository at `path` and starts loading. Returns immediately;
    /// the first snapshot (possibly with only part of the history) arrives as
    /// `SnapshotUpdated`.
    pub fn open(
        path: impl AsRef<Path>,
        on_event: impl Fn(SessionEvent) + Send + Sync + 'static,
    ) -> Result<Self> {
        let repo = Repo::open(path)?;
        // A bare repository has no working tree; git commands fail there with a
        // message the operation reports, which is better than no session at all.
        let git_workdir = repo
            .workdir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| repo.git_dir().to_path_buf());
        let git = Git::new(git_workdir);

        let events: EventSink = Arc::new(on_event);
        {
            let events = Arc::clone(&events);
            git.set_log_subscriber(move |record| {
                events(SessionEvent::CommandLogged(record.clone()));
            });
        }

        let (load_tx, load_rx) = mpsc::channel();
        let (status_tx, status_rx) = mpsc::channel();
        let (ops_tx, ops_rx) = mpsc::channel();
        let (diff_tx, diff_rx) = mpsc::channel();
        let diff_rx = Arc::new(Mutex::new(diff_rx));

        let shared = Arc::new(Shared::new(repo, git, events, empty_snapshot()));

        // Start the watcher before the workers: a failure here must not leave
        // threads behind, and the callback only needs the channel senders.
        let watcher = RepoWatcher::new(
            shared.repo.workdir(),
            shared.repo.git_dir(),
            WATCH_DEBOUNCE,
            {
                let shared = Arc::clone(&shared);
                let load = load_tx.clone();
                let status = status_tx.clone();
                move |changes| on_watch(&shared, &load, &status, &changes)
            },
        )?;

        let mut threads = Vec::with_capacity(3 + DIFF_WORKERS);
        threads.push(spawn("strata-session-history", {
            let shared = Arc::clone(&shared);
            move || history::run(shared, load_rx)
        })?);
        threads.push(spawn("strata-session-status", {
            let shared = Arc::clone(&shared);
            move || refresh::run(shared, status_rx)
        })?);
        threads.push(spawn("strata-session-operations", {
            let shared = Arc::clone(&shared);
            let status = status_tx.clone();
            let load = load_tx.clone();
            move || operations::run(shared, ops_rx, status, load)
        })?);
        for index in 0..DIFF_WORKERS {
            threads.push(spawn(&format!("strata-session-diff-{index}"), {
                let shared = Arc::clone(&shared);
                let jobs = Arc::clone(&diff_rx);
                move || diffs::run(shared, jobs)
            })?);
        }

        let session = Self {
            shared,
            senders: Some(Senders {
                load: load_tx,
                status: status_tx,
                operations: ops_tx,
                diffs: diff_tx,
            }),
            watcher: Some(watcher),
            threads,
        };
        session.reload();
        Ok(session)
    }

    /// Latest published snapshot. Cheap; call it on every render.
    pub fn snapshot(&self) -> Arc<RepoSnapshot> {
        self.shared.snapshot()
    }

    /// Queues a mutation. The session refreshes affected state afterwards.
    pub fn run(&self, op: Operation) -> OperationId {
        let id = self.shared.next_operation_id();
        if let Some(senders) = &self.senders {
            let _ = senders.operations.send(OperationJob { id, operation: op });
        }
        id
    }

    /// Computes a diff in the background. Newer requests do not cancel older
    /// ones automatically; call `cancel_diff` for a selection the user left.
    pub fn request_diff(&self, request: DiffRequest) -> DiffTicket {
        let ticket = self.shared.next_ticket();
        if let Some(senders) = &self.senders {
            let _ = senders.diffs.send(DiffJob { ticket, request });
        }
        ticket
    }

    pub fn cancel_diff(&self, ticket: DiffTicket) {
        self.shared.cancel_diff(ticket);
    }

    /// Forces a full reload (refs, history, status).
    pub fn reload(&self) {
        if let Some(senders) = &self.senders {
            let generation = self.shared.begin_reload();
            let _ = senders.load.send(LoadCommand { generation });
            let _ = senders.status.send(RefreshStatus::default());
        }
    }

    pub fn git(&self) -> &Git {
        &self.shared.git
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shared.begin_shutdown();
        // Stop the watcher first: its callback holds channel senders, and a
        // refresh delivered while the workers are joined would only be dropped.
        drop(self.watcher.take());
        // Closing the channels is what wakes every idle worker.
        drop(self.senders.take());

        let current = thread::current().id();
        for thread in std::mem::take(&mut self.threads) {
            // A callback that drops its own session runs on a worker thread.
            if thread.thread().id() != current {
                let _ = thread.join();
            }
        }
    }
}

fn spawn(name: &str, run: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(run)
        .map_err(|error| Error::other(format!("failed to start {name}: {error}")))
}

/// A snapshot the UI can render before the first load lands.
fn empty_snapshot() -> RepoSnapshot {
    RepoSnapshot {
        generation: 0,
        // Replaced by the first load; `snapshot()` must never block or panic.
        head: HeadInfo::Branch {
            short_name: String::new(),
            target: None,
        },
        refs: Arc::new(Vec::new()),
        refs_by_commit: Arc::new(HashMap::new()),
        commits: CommitList::default(),
        commit_index: Arc::new(HashMap::new()),
        status: Arc::new(WorktreeStatus::default()),
        busy: None,
    }
}

/// The commit HEAD points at, or `None` on an unborn branch.
fn head_commit(repo: &Repo) -> Result<Option<Oid>> {
    Ok(match repo.head()? {
        HeadInfo::Branch { target, .. } => target,
        HeadInfo::Detached { target } => Some(target),
    })
}

/// Content of `path` at HEAD; `None` for an unborn branch or a path absent there.
fn head_file(repo: &Repo, path: &str) -> Result<Option<Vec<u8>>> {
    match head_commit(repo)? {
        Some(head) => repo.file_at_commit(head, path),
        None => Ok(None),
    }
}

/// First parent of `commit`, for the commit-vs-parent diff.
///
/// `Repo` has no parent accessor yet, so this is the session's only direct gix
/// read; it belongs in `repo` once that module grows one.
fn first_parent(repo: &Repo, commit: Oid) -> Result<Option<Oid>> {
    let local = repo.local();
    let commit = local.find_commit(commit).map_err(Error::gix)?;
    Ok(commit.parent_ids().next().map(|id| id.detach()))
}

/// The worktree side of a diff in the form git hashes into a blob.
///
/// Another task is adding `Repo::file_in_worktree_as_git` (autocrlf and
/// attributes): swap this call for it once it lands.
fn worktree_as_git(repo: &Repo, path: &str) -> Result<Option<Vec<u8>>> {
    repo.file_in_worktree(path)
}

/// Watcher callback: a ref or HEAD move needs a new walk, a worktree or index
/// change only a status refresh.
fn on_watch(
    shared: &Shared,
    load: &Sender<LoadCommand>,
    status: &Sender<RefreshStatus>,
    changes: &crate::watch::Changes,
) {
    // Our own git commands write the index and refs; refreshing mid-operation
    // would publish a half-applied state and make the UI flicker.
    if shared.operation_running() {
        shared.hold_refresh(changes);
        return;
    }

    if changes.refs || changes.head {
        let generation = shared.begin_reload();
        let _ = load.send(LoadCommand { generation });
    }
    if changes.refs || changes.head || changes.index || changes.worktree {
        let _ = status.send(RefreshStatus::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(index: usize) -> CommitInfo {
        CommitInfo {
            id: format!("{index:040x}").parse().expect("hex object id"),
            parents: Vec::new(),
            summary: format!("commit {index}"),
            author_name: "Strata Test".into(),
            author_email: "test@example.invalid".into(),
            author_time: index as i64,
            committer_time: index as i64,
        }
    }

    fn rows(count: usize) -> Vec<GraphRow> {
        (0..count)
            .map(|_| GraphRow {
                node_lane: 0,
                node_color: 0,
                segments: Vec::new(),
                width: 1,
            })
            .collect()
    }

    fn batch(range: std::ops::Range<usize>) -> (Vec<CommitInfo>, Vec<GraphRow>) {
        let count = range.len();
        (range.map(commit).collect(), rows(count))
    }

    #[test]
    fn extend_fills_the_partial_chunk_without_recounting_it() {
        let mut list = CommitList::default();
        let (commits, rows) = batch(0..2_000);
        list.extend(commits, rows);
        assert_eq!(list.len(), 2_000);

        // A snapshot taken now shares the first chunk with the growing list.
        let published = list.clone();
        let (commits, rows) = batch(2_000..6_096);
        list.extend(commits, rows);

        assert_eq!(
            list.len(),
            6_096,
            "the copied tail must not be counted twice"
        );
        assert_eq!(list.chunks.len(), 2);
        assert_eq!(list.chunks[0].commits.len(), COMMIT_CHUNK);
        assert_eq!(list.chunks[1].commits.len(), 2_000);
        assert_eq!(published.len(), 2_000, "clones are append-only");
        assert!(
            !Arc::ptr_eq(&published.chunks[0], &list.chunks[0]),
            "the partial chunk must be copied, not shared after appending"
        );

        for (index, expected) in [(0, 0), (1_999, 1_999), (2_000, 2_000), (6_095, 6_095)] {
            let (commit, _) = list.get(index).expect("commit in range");
            assert_eq!(commit.summary, format!("commit {expected}"));
        }
        assert!(list.get(6_096).is_none());
    }

    #[test]
    fn extend_of_nothing_keeps_the_tail_chunk() {
        let mut list = CommitList::default();
        let (commits, rows) = batch(0..2_000);
        list.extend(commits, rows);
        let tail = Arc::clone(&list.chunks[0]);

        list.extend(Vec::new(), Vec::new());

        assert_eq!(list.len(), 2_000);
        assert_eq!(list.chunks.len(), 1);
        assert!(Arc::ptr_eq(&tail, &list.chunks[0]));
        assert_eq!(
            list.get(1_999).expect("last commit").0.summary,
            "commit 1999"
        );
        assert!(list.get(2_000).is_none());
    }

    #[test]
    fn extend_spans_several_chunks() {
        let mut list = CommitList::default();
        for start in (0..10_000).step_by(1_000) {
            let (commits, rows) = batch(start..start + 1_000);
            list.extend(commits, rows);
        }
        assert_eq!(list.len(), 10_000);
        assert_eq!(list.chunks.len(), 3);
        assert!(list.get(9_999).is_some());
        assert!(list.get(10_000).is_none());
        for index in [0, 999, 1_000, 4_095, 4_096, 8_191, 8_192, 9_999] {
            assert_eq!(
                list.get(index).expect("commit in range").0.summary,
                format!("commit {index}")
            );
        }
    }
}
