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
mod queries;
mod refresh;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::diff::FileDiff;
use crate::git_cli::{CommandRecord, Git};
use crate::graph::{GraphRow, GraphRows};
use crate::highlight::LineHighlights;
use crate::patch::LineSelection;
use crate::repo::Repo;
use crate::watch::RepoWatcher;
use crate::{
    CommitInfo, CommitStat, ConflictEntry, Error, HeadInfo, Oid, RefInfo, RemoteInfo, RepoState,
    Result, StashInfo, UpstreamInfo, WorktreeStatus,
};

pub use crate::diff::DiffOptions;

use diffs::DiffJob;
use history::LoadCommand;
use operations::OperationJob;
use publish::{EventSink, Shared};
use queries::QueryJob;
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
    pub rows: GraphRows,
}

impl CommitList {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, index: usize) -> Option<(&CommitInfo, GraphRow<'_>)> {
        let chunk = self.chunks.get(index / COMMIT_CHUNK)?;
        let offset = index % COMMIT_CHUNK;
        Some((chunk.commits.get(offset)?, chunk.rows.get(offset)?))
    }

    /// Appends rows; only the last chunk is copied (at most COMMIT_CHUNK items).
    pub fn extend(&mut self, commits: Vec<CommitInfo>, rows: GraphRows) {
        debug_assert_eq!(commits.len(), rows.len(), "one graph row per commit");
        // A caller that disagrees about the lengths loses the extra items
        // instead of building a chunk with commits but no rows.
        let count = commits.len().min(rows.len());
        if count == 0 {
            return;
        }
        let mut commits = commits.into_iter();
        let mut next_row = 0;

        // Chunks are shared with published snapshots, so completing the last
        // partial one means copying it instead of mutating it in place.
        let mut tail = match self.chunks.last() {
            Some(last) if last.commits.len() < COMMIT_CHUNK => {
                let tail = CommitChunk {
                    commits: last.commits.clone(),
                    rows: last.rows.clone(),
                };
                self.chunks.pop();
                Some(tail)
            }
            _ => None,
        };

        while next_row < count {
            let mut chunk = tail.take().unwrap_or_default();
            let take = (COMMIT_CHUNK - chunk.commits.len()).min(count - next_row);
            chunk.commits.extend(commits.by_ref().take(take));
            chunk.rows.extend_from(&rows, next_row..next_row + take);
            next_row += take;
            // A copied chunk was already counted when it was first published.
            self.len += take;
            if chunk.commits.len() == COMMIT_CHUNK {
                chunk.rows.shrink_to_fit();
            }
            self.chunks.push(Arc::new(chunk));
        }
    }

    /// Approximate heap bytes of the graph rows, for memory budgeting.
    pub fn graph_heap_bytes(&self) -> usize {
        self.chunks
            .iter()
            .map(|chunk| chunk.rows.heap_bytes())
            .sum()
    }
}

/// Everything the UI shows for one repository at one point in time.
#[derive(Debug, Clone)]
pub struct RepoSnapshot {
    /// Increases on every publish; lets the UI skip redundant re-renders.
    pub generation: u64,
    /// `None` until the first load has read HEAD.
    pub head: Option<HeadInfo>,
    pub refs: Arc<Vec<RefInfo>>,
    /// Indices into `refs`, for drawing ref badges next to commits.
    pub refs_by_commit: Arc<HashMap<Oid, Vec<usize>>>,
    pub commits: CommitList,
    /// Row index of each loaded commit, for "jump to commit" and selection.
    pub commit_index: Arc<HashMap<Oid, usize>>,
    pub status: Arc<WorktreeStatus>,
    /// Newest first, as `stash@{0}`, `stash@{1}`, ...
    pub stashes: Arc<Vec<StashInfo>>,
    /// Upstream tracking per local branch short name; refreshed with refs.
    pub upstreams: Arc<HashMap<String, UpstreamInfo>>,
    /// Configured remotes, sorted by name. Includes remotes that have no
    /// remote-tracking branches yet.
    pub remotes: Vec<RemoteInfo>,
    /// A merge/rebase/... left in progress; refreshed with status.
    pub state: RepoState,
    /// Unmerged paths; refreshed with status. Non-empty only while `state` is not Normal.
    pub conflicts: Arc<Vec<ConflictEntry>>,
    /// Operation currently running, if any (the UI disables conflicting actions).
    pub busy: Option<OperationId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OperationId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DiffTicket(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QueryTicket(pub u64);

/// Filter over loaded history. Text matching is case-insensitive substring
/// unless it looks like a hash prefix (4+ hex chars), which also matches ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitQuery {
    pub text: String,
    pub in_summary: bool,
    pub in_author: bool,
    pub in_hash: bool,
}

/// Which starting points the commit walk uses.
///
/// History loading reloads from this scope, so a UI that offers "show only this
/// branch" changes one setting instead of filtering an already loaded list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum HistoryScope {
    /// HEAD plus every user-facing ref: local branches, remote-tracking
    /// branches, tags and the stash. The default.
    #[default]
    All,
    /// Only these refs or commits, and everything they reach. Each entry is a
    /// ref (`refs/heads/topic`, `origin/main`) or a revision git can parse, such
    /// as an object id.
    Refs(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConflictSide {
    Ours,
    Theirs,
}

/// Mutations. Each runs through the git CLI, one at a time, in submission order.
#[derive(Debug, Clone)]
pub enum Operation {
    StagePaths(Vec<String>),
    UnstagePaths(Vec<String>),
    DiscardPaths(Vec<String>),
    /// Discard the staged and the worktree changes at `paths` in one step,
    /// restoring the index and the worktree from HEAD.
    ///
    /// A path that is only in the index (added, absent from HEAD) is removed
    /// from the index and deleted from the worktree instead, because HEAD has
    /// no content to restore. On an unborn branch that applies to every path.
    DiscardStagedPaths {
        paths: Vec<String>,
    },
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
        prune: bool,
    },
    Pull {
        rebase: Option<bool>,
    },
    Push {
        remote: Option<String>,
        branch: Option<String>,
        set_upstream: bool,
        /// `--force-with-lease`: refuses to overwrite remote work you have not fetched.
        force_with_lease: bool,
    },
    StashPush {
        message: Option<String>,
        include_untracked: bool,
    },
    /// `git stash push -- <paths>`: stash only the changes at `paths` and
    /// leave the rest of the worktree alone. An empty `paths` stashes
    /// everything, like [`Operation::StashPush`].
    StashPushPaths {
        message: Option<String>,
        include_untracked: bool,
        paths: Vec<String>,
    },
    StashPop {
        index: usize,
    },
    StashApply {
        index: usize,
    },
    StashDrop {
        index: usize,
    },
    /// `git merge <branch>`; stops with `state == Merging` on conflicts.
    Merge {
        branch: String,
        no_ff: bool,
    },
    /// `git merge --abort`.
    MergeAbort,
    /// Take one side for a conflicted path and mark it resolved.
    ResolveConflict {
        path: String,
        side: ConflictSide,
    },
    /// `git add` for paths the user resolved by editing.
    MarkResolved(Vec<String>),
    /// Commit the merge with git's prepared message.
    MergeContinue,
    /// Run the user's configured `git mergetool` for one path; blocks until it exits.
    OpenMergetool {
        path: String,
    },
    /// Write `content` as the resolved worktree file and stage it. Used when
    /// conflicts are resolved region by region in the app.
    ResolveWithContent {
        path: String,
        content: Vec<u8>,
    },
    /// `git cherry-pick <commit>` (`-m 1` for a merge). Conflicts stop it like a merge.
    CherryPick {
        commit: Oid,
    },
    /// `git revert --no-edit <commit>` (`-m 1` for a merge).
    Revert {
        commit: Oid,
    },
    /// `git reset --<mode> <target>` on the current branch.
    Reset {
        target: Oid,
        mode: ResetMode,
    },
    /// A lightweight tag when `message` is `None`, an annotated one otherwise.
    CreateTag {
        name: String,
        target: Oid,
        message: Option<String>,
    },
    DeleteTag {
        name: String,
    },
    /// `--continue` for whichever of merge, rebase, cherry-pick, revert or
    /// `am` is in progress (see `RepoSnapshot::state`).
    ContinueInProgress,
    /// `--abort` for whichever operation is in progress.
    AbortInProgress,
}

/// How far `Operation::Reset` rewinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResetMode {
    /// Move the branch only; index and worktree keep the changes staged.
    Soft,
    /// Move the branch and reset the index; worktree changes stay unstaged.
    Mixed,
    /// Move the branch and discard index and worktree changes.
    Hard,
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
    /// Progress output of a running fetch/pull/push (git's `--progress` lines).
    OperationProgress {
        id: OperationId,
        text: String,
        /// Parsed from lines like `Receiving objects:  45% (450/1000)`.
        percent: Option<u8>,
    },
    /// Rows (indices into `snapshot.commits` of `generation`) matching a query,
    /// in display order. Rows not loaded yet at that generation are not included.
    QueryReady {
        ticket: QueryTicket,
        generation: u64,
        rows: Arc<Vec<usize>>,
    },
    /// Line counts of a commit against its first parent, for a ticket from
    /// [`Session::request_commit_stat`].
    CommitStatReady {
        ticket: QueryTicket,
        stat: Arc<CommitStat>,
    },
    QueryFailed {
        ticket: QueryTicket,
        error: String,
    },
    /// One batch of a streaming path history, in walk order. `entries` are rows
    /// into `snapshot.commits` at `generation`, exactly like `QueryReady`.
    PathHistoryBatch {
        ticket: QueryTicket,
        generation: u64,
        entries: Arc<Vec<usize>>,
    },
    /// The streaming path walk finished; every entry has already arrived.
    PathHistoryDone {
        ticket: QueryTicket,
        generation: u64,
    },
}

/// How a session talks to `git`.
#[derive(Debug, Clone, Default)]
pub struct SessionOptions {
    /// The `git` executable used for every write. `None` uses the `git` found
    /// on `PATH` when the session opened.
    pub git_executable: Option<PathBuf>,
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
    queries: Sender<QueryJob>,
    /// Ticket counter and cancellation set shared with the query worker.
    query_state: Arc<queries::QueryState>,
}

impl Session {
    /// Opens the repository at `path` and starts loading. Returns immediately;
    /// the first snapshot (possibly with only part of the history) arrives as
    /// `SnapshotUpdated`.
    pub fn open(
        path: impl AsRef<Path>,
        on_event: impl Fn(SessionEvent) + Send + Sync + 'static,
    ) -> Result<Self> {
        Self::open_with(path, SessionOptions::default(), on_event)
    }

    /// Like [`open`](Self::open), with control over the environment the
    /// session works in.
    pub fn open_with(
        path: impl AsRef<Path>,
        options: SessionOptions,
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
        // Choosing the executable here, not per command, keeps every write on
        // the same binary even when `PATH` changes later.
        let git = match options.git_executable {
            Some(binary) => git.with_git_binary(binary),
            None => git,
        };

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
        let (query_tx, query_rx) = mpsc::channel();
        let query_state = Arc::new(queries::QueryState::new());

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

        let mut threads = Vec::with_capacity(4 + DIFF_WORKERS);
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
        // One query worker: a path walk and a search both read a snapshot, and
        // the UI only needs the latest answer.
        threads.push(spawn("strata-session-queries", {
            let shared = Arc::clone(&shared);
            let state = Arc::clone(&query_state);
            move || queries::run(shared, state, query_rx)
        })?);

        let session = Self {
            shared,
            senders: Some(Senders {
                load: load_tx,
                status: status_tx,
                operations: ops_tx,
                diffs: diff_tx,
                queries: query_tx,
                query_state,
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

    /// Computes a diff in the background with [`DiffOptions::default`]. Newer
    /// requests do not cancel older ones automatically; call `cancel_diff` for
    /// a selection the user left.
    pub fn request_diff(&self, request: DiffRequest) -> DiffTicket {
        self.request_diff_with(request, DiffOptions::default())
    }

    /// Like [`request_diff`](Self::request_diff), with the comparison options
    /// the UI is configured for. The options travel with the job, so queued
    /// requests each keep their own.
    pub fn request_diff_with(&self, request: DiffRequest, options: DiffOptions) -> DiffTicket {
        let ticket = self.shared.next_ticket();
        if let Some(senders) = &self.senders {
            let _ = senders.diffs.send(DiffJob {
                ticket,
                request,
                options,
            });
        }
        ticket
    }

    pub fn cancel_diff(&self, ticket: DiffTicket) {
        self.shared.cancel_diff(ticket);
    }

    /// Filters loaded history in the background; answers with `QueryReady`.
    pub fn search_commits(&self, query: CommitQuery) -> QueryTicket {
        self.queue_query(queries::QueryKind::Search(query))
    }

    /// Commits that changed `path` (file or directory), as rows of the current
    /// snapshot; answers with `QueryReady`.
    pub fn path_history(&self, path: String) -> QueryTicket {
        self.queue_query(queries::QueryKind::Path(path))
    }

    /// Like [`path_history`](Self::path_history), but delivers the rows while
    /// the walk runs: `PathHistoryBatch` events in walk order, then
    /// `PathHistoryDone`. Concatenating the batches gives the same rows
    /// `path_history` answers with. Cancelling the ticket stops the walk and
    /// suppresses further batches and the done event.
    pub fn path_history_streaming(&self, path: String) -> QueryTicket {
        self.queue_query(queries::QueryKind::StreamingPath(path))
    }

    /// Added and removed line counts of `commit` against its first parent (or
    /// the empty tree for a root commit), computed off the UI thread; answers
    /// with [`SessionEvent::CommitStatReady`], or `QueryFailed` with the same
    /// ticket when the commit cannot be read.
    pub fn request_commit_stat(&self, commit: Oid) -> QueryTicket {
        self.queue_query(queries::QueryKind::CommitStat(commit))
    }

    pub fn cancel_query(&self, ticket: QueryTicket) {
        if let Some(senders) = &self.senders {
            senders.query_state.cancel(ticket);
        }
    }

    fn queue_query(&self, kind: queries::QueryKind) -> QueryTicket {
        let Some(senders) = &self.senders else {
            // Only reachable once the session is shutting down, when no event
            // could be delivered anyway.
            return QueryTicket(0);
        };
        let ticket = senders.query_state.next_ticket();
        let _ = senders.queries.send(QueryJob { ticket, kind });
        ticket
    }

    /// Forces a full reload (refs, history, status).
    pub fn reload(&self) {
        if let Some(senders) = &self.senders {
            let generation = self.shared.begin_reload();
            let _ = senders.load.send(LoadCommand { generation });
            let _ = senders.status.send(RefreshStatus::default());
        }
    }

    /// Reloads history from `scope`, e.g. only one branch. `All` is the
    /// default. The resulting snapshots still carry the same commit and graph
    /// types; only the set of walked starting points changes.
    pub fn set_history_scope(&self, scope: HistoryScope) {
        self.shared.set_history_scope(scope);
        self.reload();
    }

    /// The scope the current or next history load uses.
    pub fn history_scope(&self) -> HistoryScope {
        self.shared.history_scope()
    }

    pub fn git(&self) -> &Git {
        &self.shared.git
    }

    /// Read-only repository handle for small on-demand reads the snapshot does
    /// not carry (e.g. the file list of a selected commit). Cheap to clone and
    /// safe to use from any thread; run it off the UI thread.
    pub fn repo(&self) -> &Repo {
        &self.shared.repo
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
        head: None,
        refs: Arc::new(Vec::new()),
        refs_by_commit: Arc::new(HashMap::new()),
        commits: CommitList::default(),
        commit_index: Arc::new(HashMap::new()),
        status: Arc::new(WorktreeStatus::default()),
        stashes: Arc::new(Vec::new()),
        upstreams: Arc::new(HashMap::new()),
        remotes: Vec::new(),
        state: RepoState::Normal,
        conflicts: Arc::new(Vec::new()),
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
fn first_parent(repo: &Repo, commit: Oid) -> Result<Option<Oid>> {
    Ok(repo.parents(commit)?.into_iter().next())
}

/// The worktree side of a diff in the form git hashes into a blob, so CRLF
/// checkouts (autocrlf, eol attributes) do not show every line as changed.
fn worktree_as_git(repo: &Repo, path: &str) -> Result<Option<Vec<u8>>> {
    repo.file_in_worktree_as_git(path)
}

/// Watcher callback: a ref or HEAD move needs a new walk, a worktree or index
/// change only a status refresh, and a configuration edit only the
/// configuration-derived remotes and upstreams.
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
    // A history reload re-reads the configuration anyway; the extra refresh is
    // only needed for a config edit that no reload covers.
    let config = changes.config && !(changes.refs || changes.head);
    if config || changes.refs || changes.head || changes.index || changes.worktree {
        let _ = status.send(RefreshStatus {
            config,
            ..RefreshStatus::default()
        });
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

    fn rows(range: std::ops::Range<usize>) -> GraphRows {
        // A linear history: each commit's only parent is the next one.
        let mut layout = crate::graph::GraphLayout::new();
        let mut rows = GraphRows::new();
        for index in range {
            let parent = commit(index + 1).id;
            layout.push(commit(index).id, &[parent], &mut rows);
        }
        rows
    }

    fn batch(range: std::ops::Range<usize>) -> (Vec<CommitInfo>, GraphRows) {
        (range.clone().map(commit).collect(), rows(range))
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

        list.extend(Vec::new(), GraphRows::new());

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
