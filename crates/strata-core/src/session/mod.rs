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

mod blame;
mod content_search;
mod diffs;
mod history;
mod operations;
mod publish;
mod queries;
mod refresh;
mod signatures;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::diff::FileDiff;
use crate::encoding::TextEncoding;
use crate::git_cli::{CommandRecord, Git, RebaseStep};
use crate::graph::{GraphRow, GraphRows};
use crate::highlight::LineHighlights;
use crate::merge::ContentHash;
use crate::patch::LineSelection;
use crate::repo::{CommitGraphAdvice, Repo, SubmoduleInfo, WorktreeInfo};
use crate::watch::RepoWatcher;
use crate::{
    CommitInfo, CommitStat, ConflictEntry, Error, HeadInfo, Oid, RefInfo, ReflogEntry, RemoteInfo,
    RepoState, Result, StashInfo, UpstreamInfo, WorktreeStatus,
};

pub use crate::diff::DiffOptions;
pub use crate::repo::{
    Blame, BlameCommit, BlameHunk, CommitSignature, CompareFile, CompareMode, Comparison,
    ContentSearchEntry, ContentSearchMode, ContentSearchProgress, ContentSearchQuery,
    InProgressCommit, InProgressDetails, RebaseOnto, SelectionRange, SelectionRangeKind,
    SelectionRangeMode, SignatureFormat, SignatureValidity,
};

use blame::BlameJob;
use content_search::ContentSearchJob;

use diffs::DiffJob;
use history::LoadCommand;
use operations::OperationJob;
use publish::{EventSink, Shared};
use queries::QueryJob;
use refresh::RefreshStatus;
use signatures::{SignatureCache, SignatureJob};

/// Commits are published in chunks so a growing list can be shared with the UI
/// without copying it on every append.
pub const COMMIT_CHUNK: usize = 4096;

/// The first publish holds only a few rows: the newest commits are what the user
/// sees first, and waiting for a full chunk makes the log stay empty while a
/// large history is read. Later publishes wait for a full [`COMMIT_CHUNK`].
pub const FIRST_COMMIT_CHUNK: usize = 200;

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
    pub fn extend(&mut self, mut commits: Vec<CommitInfo>, mut rows: GraphRows) {
        debug_assert_eq!(commits.len(), rows.len(), "one graph row per commit");
        // A caller that disagrees about the lengths loses the extra items
        // instead of building a chunk with commits but no rows.
        let count = commits.len().min(rows.len());
        if count == 0 {
            return;
        }

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

        // Fast path: the loader produces full chunks directly, so move the
        // vectors instead of copying every CommitInfo and PassThrough slice.
        if tail.is_none() && count <= COMMIT_CHUNK && commits.len() == count && rows.len() == count
        {
            commits.shrink_to_fit();
            rows.shrink_to_fit();
            self.len += count;
            self.chunks.push(Arc::new(CommitChunk { commits, rows }));
            return;
        }

        let mut commits = commits.into_iter();
        let mut next_row = 0;

        while next_row < count {
            let mut chunk = tail.take().unwrap_or_default();
            let take = (COMMIT_CHUNK - chunk.commits.len()).min(count - next_row);
            chunk.commits.extend(commits.by_ref().take(take));
            chunk.rows.extend_from(&rows, next_row..next_row + take);
            next_row += take;
            // A copied chunk was already counted when it was first published.
            self.len += take;
            chunk.commits.shrink_to_fit();
            chunk.rows.shrink_to_fit();
            self.chunks.push(Arc::new(chunk));
        }
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.chunks.shrink_to_fit();
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

/// One commit returned by [`Session::path_history_streaming`], paired with the
/// repository-relative path the file had at that commit (which differs from the
/// queried path before a rename).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathHistoryCommit {
    pub commit: CommitInfo,
    pub path: String,
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
    /// `git branch -m <name> <new_name>`.
    ///
    /// With `rename_upstream`, the branch's remote branch is renamed to match:
    /// the new name is pushed, the old remote branch deleted, and the local
    /// branch pointed at the new one. `rename_upstream` holds the remote and
    /// remote branch the upstream currently names.
    RenameBranch {
        name: String,
        new_name: String,
        rename_upstream: Option<RemoteBranch>,
    },
    /// `git branch --set-upstream-to=<remote>/<branch> <branch>`.
    SetBranchUpstream {
        branch: String,
        remote: String,
        /// Branch name on the remote.
        remote_branch: String,
    },
    /// `git branch --unset-upstream <branch>`.
    UnsetBranchUpstream {
        branch: String,
    },
    /// `git remote add <name> <url>`, optionally fetching it right away.
    AddRemote {
        name: String,
        url: String,
        fetch: bool,
    },
    /// `git remote rename <name> <new_name>`. Branch upstreams and the
    /// remote-tracking refs follow the new name.
    RenameRemote {
        name: String,
        new_name: String,
    },
    /// `git remote set-url <name> <url>`, plus the push URL when the remote
    /// pushes somewhere else than it fetches.
    SetRemoteUrl {
        name: String,
        fetch_url: String,
        push_url: Option<String>,
    },
    /// `git remote remove <name>`; its remote-tracking branches go with it.
    RemoveRemote {
        name: String,
    },
    Fetch {
        remote: Option<String>,
        prune: bool,
    },
    /// `git fetch --unshallow`: fetch the history below a shallow clone's
    /// boundary, turning the truncated roots into ordinary commits.
    FetchUnshallow,
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
        /// Fingerprint of the worktree file the editor loaded. The write is
        /// refused when the file on disk no longer matches it; `None` skips
        /// the check, for a path that had no worktree file when it was loaded.
        expected: Option<ContentHash>,
    },
    /// `git cherry-pick <commit>` (`-m 1` for a merge). Conflicts stop it like a merge.
    CherryPick {
        commit: Oid,
    },
    /// `git cherry-pick <commit>...` in the order given, oldest first.
    ///
    /// Picks whose change the branch already has are skipped and reported as
    /// progress output instead of stopping the sequence; a real conflict stops
    /// it with `state == CherryPicking`, like [`Operation::CherryPick`].
    CherryPickMany {
        commits: Vec<Oid>,
    },
    /// `git revert --no-edit <commit>` (`-m 1` for a merge).
    Revert {
        commit: Oid,
    },
    /// Replay `onto..HEAD` in the order of `steps`, rewording, squashing,
    /// fixing up or dropping commits (`git rebase -i` with a prepared todo).
    /// Conflicts stop it with `state == Rebasing`, like a merge.
    RebaseEdit {
        onto: Oid,
        steps: Vec<RebaseStep>,
    },
    /// Replay the current branch onto `onto`: `git rebase <onto>`, or
    /// `git rebase --onto <onto> <upstream>` when an upstream is given.
    /// Conflicts stop it with `state == Rebasing`, like a merge.
    Rebase {
        onto: Oid,
        upstream: Option<Oid>,
        /// Keep a dirty working tree in a stash for the duration.
        autostash: bool,
    },
    /// `git reset --<mode> <target>` on the current branch.
    Reset {
        target: Oid,
        mode: ResetMode,
    },
    /// Restore the state before a reflog entry: `None` undoes the newest HEAD
    /// action strata can restore, `Some(entry)` restores that row. A branch
    /// move runs `git reset --keep`; a checkout switches back to its source.
    UndoReflog {
        entry: Option<ReflogEntry>,
    },
    /// Re-applies an undone action: the state the undo moved away from. A
    /// branch move runs `git reset --keep` back to the entry's new target; a
    /// checkout switches to the branch or commit it left. Refused once the
    /// branch has moved again, because the recorded ids no longer describe
    /// what the user last saw.
    RedoReflog {
        entry: ReflogEntry,
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
    /// `git submodule update [--init] -- <paths>`; an empty `paths` updates
    /// every submodule.
    SubmoduleUpdate {
        paths: Vec<String>,
        init: bool,
    },
    /// `git worktree add` at `path`: an existing `branch`, a `new_branch`
    /// starting at `commit`, or `commit` alone for a detached checkout.
    WorktreeAdd {
        path: PathBuf,
        branch: Option<String>,
        new_branch: Option<String>,
        commit: Option<Oid>,
    },
    /// `git worktree remove [--force] <path>`.
    WorktreeRemove {
        path: PathBuf,
        force: bool,
    },
    /// `git worktree prune`.
    WorktreePrune,
    /// `git lfs track <path>`: add `filter=lfs` for the file's pattern to
    /// `.gitattributes`.
    LfsTrack {
        path: String,
    },
    /// `git lfs lock <path>`.
    LfsLock {
        path: String,
    },
    /// `git lfs unlock <path>`.
    LfsUnlock {
        path: String,
    },
    /// `git lfs pull --include=<path>`: download the real object.
    LfsPull {
        path: String,
    },
    /// `git commit-graph write --reachable --changed-paths`: make future
    /// history walks faster. Never started without an explicit user action.
    WriteCommitGraph,
}

/// A branch on a remote, as `branch.<name>.remote` and `.merge` record it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBranch {
    pub remote: String,
    /// Branch name on the remote, without `refs/heads/`.
    pub branch: String,
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
    /// `path` in the `head` tree vs the same file in the `base` tree, for the
    /// per-file diff of a comparison of two commits (see
    /// [`Session::request_compare`]). `base` is `None` when the two commits
    /// share no history, so the left side is empty. `old_path` names the file
    /// on the left side when a rename moved it.
    Compare {
        base: Option<Oid>,
        head: Oid,
        old_path: Option<String>,
        path: String,
    },
}

/// A diff ready to render. Highlights are per line of the full old/new text,
/// indexed by `DiffLine::old_lineno` / `new_lineno` minus one.
///
/// The two texts are kept so a view can reveal the lines the diff leaves out
/// without reading the blobs again or running the diff a second time;
/// [`DiffView::gaps`] names those runs. A view that only renders the hunks can
/// ignore both.
#[derive(Debug, Clone)]
pub struct DiffView {
    pub request: DiffRequest,
    pub diff: FileDiff,
    /// Empty until highlighting finishes; a second `DiffReady` event follows.
    pub old_highlights: Arc<Vec<LineHighlights>>,
    pub new_highlights: Arc<Vec<LineHighlights>>,
    /// LFS metadata when either side is a pointer. `diff` then holds the real
    /// content when both objects are local, or no hunks when they are not.
    pub lfs: Option<crate::lfs::LfsDiffSummary>,
    /// The old side of the diff, decoded for display.
    pub old_text: Arc<str>,
    /// The new side of the diff, decoded for display.
    pub new_text: Arc<str>,
    /// The encoding `old_text` was decoded from. A view shows a label when it
    /// is not UTF-8, so the reader knows the file is in a legacy encoding.
    pub old_encoding: TextEncoding,
    /// The encoding `new_text` was decoded from.
    pub new_encoding: TextEncoding,
    /// Runs of unchanged lines the diff left out, indexed by the hunk they
    /// precede; the last entry follows the last hunk. Empty gaps sit where two
    /// hunks or a hunk and a file edge meet directly.
    pub gaps: Arc<[crate::diff::Gap]>,
    /// Both sides of a picture, for an image path or an SVG. A raster image
    /// has no text diff, so `diff` then holds no hunks; an SVG keeps its text
    /// diff and carries the rendering beside it.
    pub image: Option<crate::image::ImageDiff>,
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
    /// A network command is waiting for a credential or passphrase. Answer it
    /// with [`Session::answer_askpass`]; answering `None` cancels, which makes
    /// git report the authentication failure.
    AskpassPrompt {
        id: OperationId,
        /// git's question, e.g. `Username for 'https://example.com': `.
        prompt: String,
        /// Passwords and passphrases must be masked while typed.
        secret: bool,
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
    /// Two commits compared, for a ticket from [`Session::request_compare`].
    CompareReady {
        ticket: QueryTicket,
        comparison: Arc<Comparison>,
    },
    /// Partial line attribution from an in-flight `git blame --incremental`
    /// run for [`Session::request_blame`]. `lines` is populated once when the
    /// file text is first read via gix; subsequent events carry `lines: None`,
    /// newly attributed `hunks`, and only newly seen `commits`.
    BlameProgress {
        ticket: QueryTicket,
        lines: Option<Arc<Vec<String>>>,
        hunks: Vec<BlameHunk>,
        commits: HashMap<Oid, BlameCommit>,
    },
    /// Line attribution of a file, for a ticket from
    /// [`Session::request_blame`]. A stale or cancelled ticket emits nothing.
    BlameReady {
        ticket: QueryTicket,
        blame: Arc<Blame>,
    },
    QueryFailed {
        ticket: QueryTicket,
        error: String,
    },
    /// One batch of a streaming path history, in walk order.
    ///
    /// Unlike [`QueryReady`](Self::QueryReady), this walk runs independently of
    /// graph snapshots, so incoming history chunks never invalidate its rows.
    PathHistoryBatch {
        ticket: QueryTicket,
        entries: Arc<Vec<PathHistoryCommit>>,
    },
    /// The streaming path walk finished; every entry has already arrived.
    PathHistoryDone {
        ticket: QueryTicket,
    },
    /// One batch of a streaming content search (`-S` / `-G`), in `--date-order`,
    /// paired with overall scan progress.
    ContentSearchBatch {
        ticket: QueryTicket,
        matches: Arc<Vec<ContentSearchEntry>>,
        scanned: usize,
        total: usize,
    },
    /// Periodic progress update while scanning history without new matches.
    ContentSearchProgress {
        ticket: QueryTicket,
        scanned: usize,
        total: usize,
    },
    /// The streaming content search finished; every match has already arrived.
    ContentSearchDone {
        ticket: QueryTicket,
        scanned: usize,
        total: usize,
    },
    /// Submodules and worktrees for a ticket from
    /// [`Session::request_submodules_and_worktrees`].
    SubmodulesAndWorktreesReady {
        ticket: QueryTicket,
        submodules: Arc<Vec<SubmoduleInfo>>,
        worktrees: Arc<Vec<WorktreeInfo>>,
    },
    /// HEAD's and the local branches' reflogs, newest first, for a ticket from
    /// [`Session::request_reflog`].
    ReflogReady {
        ticket: QueryTicket,
        entries: Arc<Vec<ReflogEntry>>,
    },
    /// Details of an unfinished git operation for a ticket from
    /// [`Session::request_in_progress_details`].
    InProgressDetailsReady {
        ticket: QueryTicket,
        details: Option<InProgressDetails>,
    },
    /// The history walk found that a commit-graph is missing or stale, so
    /// writing one would make later loads faster. The UI offers the write;
    /// nothing is written until the user answers.
    CommitGraphSuggested {
        advice: CommitGraphAdvice,
    },
    /// Cheap `gix` detection found signature headers for uncached commits in a
    /// [`Session::request_commit_signatures`] batch while `git` verification runs.
    CommitSignaturesDetected {
        ticket: QueryTicket,
        formats: Arc<HashMap<Oid, SignatureFormat>>,
    },
    /// Verified signatures (from `git log` or the session cache) for a
    /// [`Session::request_commit_signatures`] batch. Unsigned commits are omitted.
    CommitSignaturesReady {
        ticket: QueryTicket,
        signatures: Arc<HashMap<Oid, CommitSignature>>,
    },
    /// Cached signature verification results were cleared (e.g. after a config
    /// change or reload); visible commits should be re-requested.
    SignaturesInvalidated,
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
    blame: Sender<BlameJob>,
    content_search: Sender<ContentSearchJob>,
    signatures: Sender<SignatureJob>,
    signature_cache: Arc<SignatureCache>,
    /// Ticket counter and cancellation set shared with the query workers.
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
        let (blame_tx, blame_rx) = mpsc::channel();
        let (content_search_tx, content_search_rx) = mpsc::channel();
        let (sig_tx, sig_rx) = mpsc::channel();
        let signature_cache = Arc::new(SignatureCache::new());
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
                let signature_cache = Arc::clone(&signature_cache);
                move |changes| on_watch(&shared, &load, &status, &signature_cache, &changes)
            },
        )?;

        let mut threads = Vec::with_capacity(7 + DIFF_WORKERS);
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
        // Blame walks far more history than the other queries, so it must not
        // share their worker or a large file would stall every answer.
        threads.push(spawn("strata-session-blame", {
            let shared = Arc::clone(&shared);
            let state = Arc::clone(&query_state);
            move || blame::run(shared, state, blame_rx)
        })?);
        // Content search diffs trees across history, so it also gets its own
        // worker thread and never delays commit stats or snapshot searches.
        threads.push(spawn("strata-session-content-search", {
            let shared = Arc::clone(&shared);
            let state = Arc::clone(&query_state);
            move || content_search::run(shared, state, content_search_rx)
        })?);
        // One signature worker caps git CLI verification concurrency at 1 so
        // fast scrolling through a large history never floods git processes.
        threads.push(spawn("strata-session-signatures", {
            let shared = Arc::clone(&shared);
            let state = Arc::clone(&query_state);
            let cache = Arc::clone(&signature_cache);
            move || signatures::run(shared, state, cache, sig_rx)
        })?);

        let session = Self {
            shared,
            senders: Some(Senders {
                load: load_tx,
                status: status_tx,
                operations: ops_tx,
                diffs: diff_tx,
                queries: query_tx,
                blame: blame_tx,
                content_search: content_search_tx,
                signatures: sig_tx,
                signature_cache,
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

    /// Answers the pending [`SessionEvent::AskpassPrompt`] of `id`.
    ///
    /// `None` cancels: the helper exits non-zero and git fails with its own
    /// authentication message, exactly as it would with no credential source.
    /// Returns false when no prompt is waiting, because it was answered
    /// already, timed out, or the operation ended.
    pub fn answer_askpass(&self, id: OperationId, answer: Option<String>) -> bool {
        self.shared.answer_askpass(id, answer)
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

    /// Streams commits that changed `path` (file or directory) while the walk
    /// runs: `PathHistoryBatch` events in walk order, then `PathHistoryDone`.
    ///
    /// Runs its own path walk directly against the repository rather than
    /// indexing the loaded [`RepoSnapshot`], so snapshot updates during the
    /// initial history walk never invalidate or restart it. Cancelling the
    /// ticket stops the walk and suppresses further batches and the done event.
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

    /// Compares two commits off the UI thread: the files that differ between
    /// their trees (with renames and line counts), and how many commits each
    /// side has that the other does not. Answers with
    /// [`SessionEvent::CompareReady`], or `QueryFailed` with the same ticket
    /// when a commit or its offset cannot be read.
    ///
    /// In [`CompareMode::ThreeDot`] the left tree is the merge base of the two
    /// commits, so the files are what `b` added since they diverged.
    pub fn request_compare(&self, a: Oid, b: Oid, mode: CompareMode) -> QueryTicket {
        self.queue_query(queries::QueryKind::Compare { a, b, mode })
    }

    /// Like [`request_compare`](Self::request_compare), with an explicit left
    /// side: `base` names the commit, or is `None` for the empty tree, which is
    /// what a range that starts at a root commit uses. The answer is the same
    /// [`SessionEvent::CompareReady`], so callers that only need a ticket guard
    /// handle both alike.
    pub fn request_compare_from(&self, base: Option<Oid>, head: Oid) -> QueryTicket {
        self.queue_query(queries::QueryKind::CompareFrom { base, head })
    }

    /// Lists submodules and linked worktrees off the UI thread; answers with
    /// [`SessionEvent::SubmodulesAndWorktreesReady`], or `QueryFailed` with the
    /// same ticket when the repository cannot be read.
    ///
    /// Neither list is part of [`RepoSnapshot`], so re-request it after every
    /// `SnapshotUpdated`, including the refreshes the filesystem watcher
    /// triggers. Both reads only touch the repository, which keeps that
    /// re-request as cheap as the refresh itself.
    pub fn request_submodules_and_worktrees(&self) -> QueryTicket {
        self.queue_query(queries::QueryKind::SubmodulesAndWorktrees)
    }

    /// Reads HEAD's reflog and each local branch's reflog off the UI thread,
    /// newest first, at most `limit` entries per reflog. Answers with
    /// [`SessionEvent::ReflogReady`], or `QueryFailed` with the same ticket when
    /// the repository cannot be read.
    ///
    /// Not part of [`RepoSnapshot`]: the reflog view asks again when it opens
    /// and whenever HEAD moves while it is open.
    pub fn request_reflog(&self, limit: usize) -> QueryTicket {
        self.queue_query(queries::QueryKind::Reflog(limit))
    }

    /// Reads details of an unfinished git operation off the UI thread; answers
    /// with [`SessionEvent::InProgressDetailsReady`], or `QueryFailed` with the
    /// same ticket when the repository cannot be read.
    ///
    /// These details sit outside [`RepoSnapshot`], so the UI re-requests them
    /// whenever `SnapshotUpdated` arrives with a non-normal state.
    pub fn request_in_progress_details(&self) -> QueryTicket {
        self.queue_query(queries::QueryKind::InProgressDetails)
    }

    /// Attributes each line of `path` at `commit` to the commit that last
    /// changed it, following renames of the file.
    ///
    /// Runs on the blame worker, so a large file never delays a search, a path
    /// history or a diff. Answers with [`SessionEvent::BlameReady`], or
    /// `QueryFailed` with the same ticket when the path or commit cannot be
    /// read. Cancelling the ticket before the answer is ready emits nothing.
    pub fn request_blame(&self, commit: Oid, path: impl Into<String>) -> QueryTicket {
        let Some(senders) = &self.senders else {
            // Only reachable once the session is shutting down, when no event
            // could be delivered anyway.
            return QueryTicket(0);
        };
        let ticket = senders.query_state.next_ticket();
        let _ = senders.blame.send(BlameJob {
            ticket,
            commit,
            path: path.into(),
        });
        ticket
    }

    /// Streams commits whose first-parent diff matches `query` (`-S` or `-G`,
    /// with optional path limit), in `--date-order`, respecting the session's
    /// current [`HistoryScope`].
    ///
    /// Delivers [`SessionEvent::ContentSearchBatch`] and
    /// [`SessionEvent::ContentSearchProgress`] events while scanning, followed
    /// by [`SessionEvent::ContentSearchDone`], or
    /// [`SessionEvent::QueryFailed`] on error. Cancelling the ticket stops the
    /// walk and suppresses further events.
    pub fn content_search_streaming(&self, query: ContentSearchQuery) -> QueryTicket {
        let Some(senders) = &self.senders else {
            return QueryTicket(0);
        };
        let ticket = senders.query_state.next_ticket();
        let _ = senders
            .content_search
            .send(ContentSearchJob { ticket, query });
        ticket
    }

    /// Detects signatures cheaply via `gix` and verifies signed commits on
    /// demand via the `git` CLI off the UI thread.
    ///
    /// Answers with [`SessionEvent::CommitSignaturesDetected`] (for uncached
    /// signed commits) followed by [`SessionEvent::CommitSignaturesReady`], or
    /// [`SessionEvent::QueryFailed`] on error. Cached results for already-checked
    /// commits return without spawning `git`. Cancelling the returned ticket
    /// stops any in-flight `git` process and suppresses remaining events.
    pub fn request_commit_signatures(&self, commits: Vec<Oid>) -> QueryTicket {
        let Some(senders) = &self.senders else {
            return QueryTicket(0);
        };
        let ticket = senders.query_state.next_ticket();
        let _ = senders.signatures.send(SignatureJob { ticket, commits });
        ticket
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
            if senders.signature_cache.invalidate() {
                self.shared.emit(SessionEvent::SignaturesInvalidated);
            }
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

    /// Ref names kept out of the full history walk. Their refs stay in the
    /// snapshot; only commits reachable from them alone leave the graph.
    ///
    /// The next load drops the hidden tips, and [`Session::reload`] is what
    /// makes that load happen.
    pub fn set_hidden_refs(&self, refs: Vec<String>) {
        self.shared.set_hidden_refs(refs);
        self.reload();
    }

    /// The ref names the current or next history load hides.
    pub fn hidden_refs(&self) -> Vec<String> {
        self.shared.hidden_refs()
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
    signature_cache: &SignatureCache,
    changes: &crate::watch::Changes,
) {
    // Our own git commands write the index and refs; refreshing mid-operation
    // would publish a half-applied state and make the UI flicker.
    if shared.operation_running() {
        shared.hold_refresh(changes);
        return;
    }

    if changes.config && signature_cache.invalidate() {
        shared.emit(SessionEvent::SignaturesInvalidated);
    }

    if changes.refs || changes.head {
        let generation = shared.begin_reload();
        let _ = load.send(LoadCommand { generation });
    }
    // A history reload re-reads the configuration anyway; the extra refresh is
    // only needed for a config edit that no reload covers.
    let config = changes.config && !(changes.refs || changes.head);
    if config || changes.refs || changes.head || changes.index || changes.worktree || changes.rescan
    {
        let paths = if !changes.rescan
            && !changes.index
            && !changes.refs
            && !changes.head
            && changes.worktree
            && !changes.paths.is_empty()
        {
            Some(changes.paths.clone())
        } else {
            None
        };
        let _ = status.send(RefreshStatus {
            config,
            paths,
            ..RefreshStatus::default()
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::graph::Half;
    use crate::test_support::shallow_clone_fixture;

    /// Opens a session on `path`, returning the event stream it publishes to.
    fn open_session(path: &std::path::Path) -> (Session, Receiver<SessionEvent>) {
        let (tx, rx) = mpsc::channel();
        let session = Session::open(path, move |event| {
            let _ = tx.send(event);
        })
        .expect("open session");
        (session, rx)
    }

    /// Waits until history and refs are published, failing on a load error.
    fn wait_for_complete(session: &Session, events: &Receiver<SessionEvent>) -> Arc<RepoSnapshot> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let snapshot = session.snapshot();
            if snapshot.commits.complete && snapshot.head.is_some() {
                return snapshot;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match events.recv_timeout(remaining) {
                Ok(SessionEvent::LoadFailed { error }) => panic!("session load failed: {error}"),
                Ok(_) => {}
                Err(error) => panic!("snapshot did not complete: {error}"),
            }
        }
    }

    /// The next event matching `wanted`, failing on any load, query or diff error.
    fn next_matching(
        events: &Receiver<SessionEvent>,
        mut wanted: impl FnMut(&SessionEvent) -> bool,
    ) -> SessionEvent {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match events.recv_timeout(remaining) {
                Ok(event) => match &event {
                    SessionEvent::LoadFailed { error } => panic!("session load failed: {error}"),
                    SessionEvent::QueryFailed { error, .. } => panic!("query failed: {error}"),
                    SessionEvent::DiffFailed { error, .. } => panic!("diff failed: {error}"),
                    _ if wanted(&event) => return event,
                    _ => {}
                },
                Err(error) => panic!("timed out waiting for an event: {error}"),
            }
        }
    }

    #[test]
    fn a_credential_prompt_is_answered_through_the_session() {
        let (_source, clone) = shallow_clone_fixture(&["--depth", "1"]);
        let (session, events) = open_session(clone.path());
        wait_for_complete(&session, &events);

        let id = session.shared.next_operation_id();
        let waiting = {
            let shared = Arc::clone(&session.shared);
            std::thread::spawn(move || {
                shared.request_answer(
                    id,
                    crate::askpass::Prompt::new("Password for 'https://example.com': "),
                )
            })
        };

        let event = next_matching(
            &events,
            |event| matches!(event, SessionEvent::AskpassPrompt { id: ready, .. } if *ready == id),
        );
        let SessionEvent::AskpassPrompt { prompt, secret, .. } = event else {
            unreachable!()
        };
        assert_eq!(prompt, "Password for 'https://example.com': ");
        assert!(secret, "a password prompt must be masked");

        assert!(
            session.answer_askpass(id, Some("hunter2".to_owned())),
            "the waiting prompt takes the answer"
        );
        let answer = waiting.join().unwrap().expect("the session answered");
        assert_eq!(answer.as_str(), Some("hunter2"));
        assert!(
            !session.answer_askpass(id, Some("again".to_owned())),
            "a prompt is answered only once"
        );
    }

    #[test]
    fn cancelling_a_credential_prompt_reaches_the_command() {
        let (_source, clone) = shallow_clone_fixture(&["--depth", "1"]);
        let (session, events) = open_session(clone.path());
        wait_for_complete(&session, &events);

        let id = session.shared.next_operation_id();
        let waiting = {
            let shared = Arc::clone(&session.shared);
            std::thread::spawn(move || {
                shared.request_answer(
                    id,
                    crate::askpass::Prompt::new("Username for 'https://example.com': "),
                )
            })
        };

        let event = next_matching(
            &events,
            |event| matches!(event, SessionEvent::AskpassPrompt { id: ready, .. } if *ready == id),
        );
        let SessionEvent::AskpassPrompt { secret, .. } = event else {
            unreachable!()
        };
        assert!(!secret, "a username prompt must stay readable");

        assert!(session.answer_askpass(id, None), "cancel is an answer");
        assert!(
            waiting.join().unwrap().is_none(),
            "cancelling reaches the helper as no answer"
        );
    }

    #[test]
    fn a_shallow_repository_publishes_a_complete_snapshot() {
        let (_source, clone) = shallow_clone_fixture(&["--depth", "2"]);
        let (session, events) = open_session(clone.path());
        let snapshot = wait_for_complete(&session, &events);

        let mut expected = clone.rev_list("HEAD");
        expected.reverse();
        let ids: Vec<Oid> = (0..snapshot.commits.len())
            .map(|index| snapshot.commits.get(index).expect("row").0.id)
            .collect();
        assert_eq!(
            ids, expected,
            "opening a shallow clone lists exactly the commits within the depth"
        );
        assert_eq!(snapshot.commits.len(), 2);

        let (boundary, row) = snapshot
            .commits
            .get(snapshot.commits.len() - 1)
            .expect("boundary row");
        assert!(
            boundary.parents.is_empty(),
            "the boundary must be a root, not a missing parent id"
        );
        assert!(
            row.edges.iter().all(|segment| segment.half != Half::Bottom),
            "no parent edge may be drawn from a boundary: {row:?}"
        );
        assert_eq!(
            snapshot.head,
            Some(HeadInfo::Branch {
                short_name: "main".into(),
                target: Some(clone.head()),
            })
        );
        assert!(
            snapshot.upstreams.contains_key("main"),
            "the tracking branch is still listed"
        );
    }

    #[test]
    fn a_shallow_repository_answers_commit_queries() {
        let (_source, clone) = shallow_clone_fixture(&["--depth", "2"]);
        let (session, events) = open_session(clone.path());
        wait_for_complete(&session, &events);

        let head = clone.head();
        let boundary = clone.rev("HEAD^1");

        let ticket = session.request_commit_stat(boundary);
        let event = next_matching(
            &events,
            |event| matches!(event, SessionEvent::CommitStatReady { ticket: ready, .. } if *ready == ticket),
        );
        let SessionEvent::CommitStatReady { stat, .. } = event else {
            unreachable!()
        };
        assert_eq!(stat.added, 7, "the boundary stat is against the empty tree");

        let ticket = session.request_diff(DiffRequest::Commit {
            commit: boundary,
            path: "dir/old.txt".into(),
        });
        let event = next_matching(
            &events,
            |event| matches!(event, SessionEvent::DiffReady { ticket: ready, .. } if *ready == ticket),
        );
        let SessionEvent::DiffReady { view, .. } = event else {
            unreachable!()
        };
        assert!(
            !view.diff.hunks.is_empty(),
            "the boundary diff is against the empty tree"
        );

        let ticket = session.path_history_streaming("dir/new.txt".into());
        let mut history = Vec::new();
        loop {
            let event = next_matching(&events, |event| {
                matches!(
                    event,
                    SessionEvent::PathHistoryBatch { ticket: ready, .. }
                        | SessionEvent::PathHistoryDone { ticket: ready }
                        if *ready == ticket
                )
            });
            match event {
                SessionEvent::PathHistoryBatch { entries, .. } => {
                    history.extend(entries.iter().map(|entry| entry.commit.id));
                }
                SessionEvent::PathHistoryDone { .. } => break,
                _ => unreachable!(),
            }
        }
        assert_eq!(
            history,
            [head, boundary],
            "the rename is followed back to the boundary, where the walk stops"
        );

        let ticket = session.request_blame(head, "dir/new.txt");
        let event = next_matching(
            &events,
            |event| matches!(event, SessionEvent::BlameReady { ticket: ready, .. } if *ready == ticket),
        );
        let SessionEvent::BlameReady { blame, .. } = event else {
            unreachable!()
        };
        assert_eq!(blame.lines, ["one", "TWO", "three", "four", "five"]);
        assert!(
            blame
                .hunks
                .iter()
                .all(|hunk| hunk.commit == head || hunk.commit == boundary),
            "blame stops at the boundary"
        );
    }

    fn commit(index: usize) -> CommitInfo {
        CommitInfo {
            id: format!("{index:040x}").parse().expect("hex object id"),
            parents: Vec::new(),
            summary: format!("commit {index}"),
            author_name: "Strata Test".into(),
            author_email: "test@example.invalid".into(),
            author_time: index as i64,
            committer_time: index as i64,
            shallow_boundary: false,
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

    #[test]
    fn extend_moves_aligned_chunks_directly() {
        let mut list = CommitList::default();
        let (commits, rows) = batch(0..COMMIT_CHUNK);
        list.extend(commits, rows);
        let first_chunk = Arc::clone(&list.chunks[0]);

        let (commits, rows) = batch(COMMIT_CHUNK..COMMIT_CHUNK * 2);
        list.extend(commits, rows);

        assert_eq!(list.chunks.len(), 2);
        assert!(Arc::ptr_eq(&first_chunk, &list.chunks[0]));
        assert_eq!(list.len(), COMMIT_CHUNK * 2);
        assert_eq!(
            list.get(COMMIT_CHUNK)
                .expect("second chunk start")
                .0
                .summary,
            format!("commit {COMMIT_CHUNK}")
        );
    }
}
