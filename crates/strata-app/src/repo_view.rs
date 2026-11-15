//! The repository view model and the bridge from session events to GPUI.
//!
//! [`Session`] calls its `on_event` callback on background threads. [`open`]
//! moves those events into an unbounded async channel, and one foreground task
//! drains it, updates this entity and calls `cx.notify()`. No git call ever
//! runs on the UI thread: file lists and diffs are requested through the
//! session, whose own workers do the work.

mod blame_view;
mod changed_counts;
mod chrome;
mod commit_graph;
mod commit_list;
mod commit_selection;
mod common;
pub(crate) mod compare;
mod custom_commands;
mod details;
mod diff_pane;
mod edit_dialog;
mod editor;
mod history_ops;
mod in_progress;
mod navigation;
mod rebase_dialog;
pub(crate) mod rebase_onto;
mod rebase_plan;
mod reflog;
mod selection_diff;
mod sidebar;
mod signatures;
mod split;
mod status_bar;
mod title_bar;
mod tree_view;
mod uncommitted;
mod uncommitted_diff;
mod worktrees;

pub(crate) use history_ops::history_filter_menu;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use futures::StreamExt;
use futures::channel::mpsc::{self, UnboundedReceiver};
use gpui::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, FocusHandle, Focusable as _,
    ScrollStrategy, Subscription, Task, UniformListScrollHandle, WeakEntity, Window,
};
use gpui_component::input::InputState;
use strata_core::diff::GapAction;
use strata_core::git_cli::CommandRecord;
use strata_core::lfs::{LfsInfo, LfsLock};
use strata_core::repo::{SubmoduleInfo, WorktreeInfo};
use strata_core::session::{
    DiffOptions, DiffRequest, DiffTicket, DiffView, Operation, OperationId, QueryTicket, Session,
    SessionEvent, SessionOptions,
};
use strata_core::{FileChange, HeadInfo, Oid, RefKind};

use crate::branch_switcher::BranchSwitcher;
use crate::branches::{BranchStart, Branches};
use crate::data::{self, LaneWidth, Source};
use crate::debug_launch::{self, DebugDialog, DebugLaunch, SnapshotRevMatch, TreeTarget};
use crate::diff_view::{self, DiffContext, Row as DiffRow};
use crate::fake::{self, FakeRepo};
use crate::file_history::FileHistory;
use crate::image_diff;
use crate::metrics::FrameMetrics;
use crate::pull_requests::{PullRequests, RefreshCause};
use crate::remote::Remote;
use crate::search::SearchState;
use crate::worktree_panel::{MainMode, WorktreePanel};
use commit_selection::CommitSelection;

use in_progress::{FinishedOperation, InProgress};

/// How many rows a `PageUp`/`PageDown` moves.
const PAGE: isize = 15;
/// The smoke run scrolls this far every frame.
const SMOKE_STEP: usize = 3;
const SMOKE_FRAMES: u64 = 300;

/// What the window should show when it opens.
///
/// Built once per window, never collected, so the session variant's size costs
/// nothing worth an extra allocation to even out.
#[allow(clippy::large_enum_variant)]
pub enum Launch {
    /// `--fake [N]`: synthetic commits for the render benchmark.
    Fake { repo: Rc<FakeRepo> },
    /// A real repository, already loading in the background.
    Repo {
        // Boxed: on macOS a Session is large enough that the other variants
        // would carry its size for nothing.
        session: Box<Session>,
        name: String,
        title: String,
        events: UnboundedReceiver<SessionEvent>,
    },
    /// Opening the path failed; the window shows the error instead of exiting.
    Error { path: String, error: String },
}

impl Launch {
    pub fn fake(count: usize) -> Self {
        let started = std::time::Instant::now();
        let repo = Rc::new(FakeRepo::generate(count));
        eprintln!(
            "strata: generated {} synthetic commits and graph rows in {:?}",
            repo.commits.len(),
            started.elapsed()
        );
        Self::Fake { repo }
    }

    /// Opens the repository containing `path` (defaulting to the current dir).
    ///
    /// The command line opens a repository before a window, and so an `App`,
    /// exists, so the git executable comes from the saved settings file rather
    /// than the settings global.
    pub fn open(path: &Path) -> Self {
        Self::open_with(path, crate::settings::saved_git_executable())
    }

    /// [`Launch::open`] with the git executable the caller read from the
    /// settings; a repository keeps the binary it was opened with.
    pub fn open_with(path: &Path, git_executable: Option<PathBuf>) -> Self {
        let (sender, events) = mpsc::unbounded();
        match Session::open_with(path, SessionOptions { git_executable }, move |event| {
            // A closed receiver only means the window is gone; the session is
            // shutting down anyway.
            let _ = sender.unbounded_send(event);
        }) {
            Ok(session) => {
                let name = repo_name(&session);
                let head = session.repo().head().ok();
                let title = data::window_title(&name, head.as_ref());
                Self::Repo {
                    session: Box::new(session),
                    name,
                    title,
                    events,
                }
            }
            Err(error) => Self::Error {
                path: display_path(path),
                error: error.to_string(),
            },
        }
    }

    /// Test helper: opens a real [`Session`] whose background workers do not
    /// wake tasks scheduled on GPUI's deterministic `TestScheduler`.
    #[cfg(test)]
    pub(crate) fn open_quiet(path: &Path, git_executable: Option<PathBuf>) -> Self {
        let (_, events) = mpsc::unbounded();
        match Session::open_with(path, SessionOptions { git_executable }, |_| {}) {
            Ok(session) => {
                let name = repo_name(&session);
                let head = session.repo().head().ok();
                let title = data::window_title(&name, head.as_ref());
                Self::Repo {
                    session: Box::new(session),
                    name,
                    title,
                    events,
                }
            }
            Err(error) => Self::Error {
                path: display_path(path),
                error: error.to_string(),
            },
        }
    }

    pub fn title(&self) -> String {
        match self {
            Launch::Fake { .. } => t!("history.window_synthetic").to_string(),
            Launch::Repo { title, .. } => title.clone(),
            Launch::Error { path, .. } => format!("Strata — {path}"),
        }
    }
}

/// `<dir>` for the window title: the working tree root, or the git dir for a
/// bare repository.
fn repo_name(session: &Session) -> String {
    let repo = session.repo();
    let path = repo
        .workdir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo.git_dir().to_path_buf());
    // Opening `.` yields a relative workdir with no file name of its own.
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| display_path(&path))
}

fn display_path(path: &Path) -> String {
    // The absolute path can be long; a relative one is what the user typed.
    std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(cwd).ok().map(Path::to_path_buf))
        .unwrap_or_else(|| path.to_path_buf())
        .display()
        .to_string()
}

/// The diff request that is currently on screen.
///
/// A ticket only becomes the current one when its request is the latest, so a
/// worker that finishes after the user moved on cannot replace the diff with
/// something else. The session is told to cancel the ticket it replaced.
/// Which pane's rows a background rebuild belongs to.
///
/// Expansion rebuilds rows off the UI thread; the pane the user clicked can
/// have moved on by the time the rows are ready, so the result names its pane
/// and its revision instead of a captured field.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowsTarget {
    History,
    Compare,
    FileHistory,
}

/// The same "only the latest ticket wins" slot as the history diff, so a stale
/// worker cannot replace the diff after the user moved on.
#[derive(Default)]
struct DiffSlot {
    ticket: Option<DiffTicket>,
    view: Option<Arc<DiffView>>,
}

impl DiffSlot {
    /// Makes `ticket` current and returns the ticket it replaced, if any.
    fn begin(&mut self, ticket: DiffTicket) -> Option<DiffTicket> {
        self.view = None;
        self.ticket.replace(ticket)
    }

    fn is_current(&self, ticket: DiffTicket) -> bool {
        self.ticket == Some(ticket)
    }

    fn accept(&mut self, ticket: DiffTicket, view: Arc<DiffView>) -> bool {
        if !self.is_current(ticket) {
            return false;
        }
        self.view = Some(view);
        true
    }

    /// True when the failure belongs to the diff on screen.
    fn fail(&mut self, ticket: DiffTicket) -> bool {
        self.is_current(ticket)
    }

    /// Clears the slot and returns the ticket the session should cancel.
    fn cancel(&mut self) -> Option<DiffTicket> {
        self.view = None;
        self.ticket.take()
    }

    fn view(&self) -> Option<&Arc<DiffView>> {
        self.view.as_ref()
    }
}

/// The multi-pick this view started, kept until the operation reports back so
/// the commits core skipped can be shown in the status bar.
#[derive(Default)]
struct PendingMultiPick {
    total: usize,
    skipped: Vec<String>,
}

/// The submodule and worktree lists the sidebar shows.
///
/// Neither list is part of a [`RepoSnapshot`], so they are read through the
/// session's query worker after every publish. A ticket only becomes the answer
/// on screen when it is the one the latest request asked for, so a slow read of
/// a state the user moved on from cannot replace the rows. The generation of
/// the accepted answer is part of the sidebar's cache key, because a new answer
/// changes rows without a new snapshot.
#[derive(Default)]
struct ExtraLists {
    /// The request in flight and the snapshot generation it answers for.
    pending: Option<(QueryTicket, u64)>,
    /// Snapshot generation of the answer on screen.
    generation: u64,
    submodules: Arc<Vec<SubmoduleInfo>>,
    worktrees: Arc<Vec<WorktreeInfo>>,
}

impl ExtraLists {
    fn begin(&mut self, ticket: QueryTicket, generation: u64) {
        self.pending = Some((ticket, generation));
    }

    /// Forgets the request in flight and returns its ticket, so the session can
    /// stop the read.
    fn cancel(&mut self) -> Option<QueryTicket> {
        self.pending.take().map(|(ticket, _)| ticket)
    }

    fn is_current(&self, ticket: QueryTicket) -> bool {
        matches!(self.pending, Some((pending, _)) if pending == ticket)
    }

    fn accept(
        &mut self,
        ticket: QueryTicket,
        submodules: Arc<Vec<SubmoduleInfo>>,
        worktrees: Arc<Vec<WorktreeInfo>>,
    ) -> bool {
        let Some((_, generation)) = self.pending.filter(|(pending, _)| *pending == ticket) else {
            return false;
        };
        self.pending = None;
        self.generation = generation;
        self.submodules = submodules;
        self.worktrees = worktrees;
        true
    }

    /// Drops the request `ticket` belongs to, if it is still the current one.
    fn fail(&mut self, ticket: QueryTicket) -> bool {
        if !self.is_current(ticket) {
            return false;
        }
        self.pending = None;
        true
    }
}

/// The options history diffs are compared with, from the settings.
///
/// The working-tree/staging diff in `worktree_panel` deliberately keeps
/// `DiffOptions::default()`: staging maps the diff's lines back to real file
/// line numbers, so ignoring whitespace or shrinking the context there would
/// make the pane stage the wrong lines.
pub(crate) fn history_diff_options(cx: &App) -> DiffOptions {
    let settings = crate::settings::Settings::get(cx);
    DiffOptions {
        context_lines: settings.diff.context_lines,
        ignore_whitespace: settings.diff.ignore_whitespace,
    }
}

pub struct RepoView {
    pub(crate) session: Option<Session>,
    fake: Option<Rc<FakeRepo>>,
    error: Option<(String, String)>,
    name: Option<String>,
    /// The workspace hosting this view, weak so swapping the view releases it.
    workspace: Option<WeakEntity<crate::workspace::Workspace>>,
    /// Name of the parent repository on the navigation stack, if this view was
    /// opened from a superproject or sibling worktree.
    parent_name: Option<String>,

    /// Which view fills the area right of the sidebar.
    pub(crate) mode: MainMode,
    /// Staging state and rendering, in its own module.
    pub(crate) worktree: WorktreePanel,
    /// Handle to this view for the render helpers that receive an `App`
    /// rather than a `Context<Self>`, such as a uniform list's or a row's
    /// callback. Weak: a view that keeps a strong handle to itself is never
    /// released when the workspace replaces it.
    pub(crate) entity: WeakEntity<RepoView>,
    /// Branch sections, dialogs and the operations they started.
    pub(crate) branches: Branches,
    /// The branch switcher under the title bar's chip.
    pub(crate) branch_switcher: BranchSwitcher,
    /// Submodules and linked worktrees, which are not part of a snapshot and so
    /// are re-read after every one.
    extra_lists: ExtraLists,
    /// Fetch/pull/push state, progress and the dialogs that decide them.
    pub(crate) remote: Remote,
    /// The branch badges' pull-request cache and the checkout dialog.
    pub(crate) pull_requests: PullRequests,
    /// Commit search over the loaded history, with its text field.
    pub(crate) search: SearchState,
    /// The file-history view while it is open.
    pub(crate) file_history: Option<FileHistory>,
    /// The comparison of two commits while it is open.
    pub(crate) compare: Option<compare::Compare>,
    /// The job in flight and the banner for what git left in progress.
    in_progress: InProgress,
    /// The reflog rows and the undo this view is waiting for.
    reflog: reflog::ReflogState,
    /// The commit jumps Back and Forward walk through.
    navigation: navigation::NavigationHistory,
    /// The history loader suggested writing a commit-graph and the user has
    /// not answered yet.
    commit_graph_offer: bool,
    /// True while the offered write runs, so a reload cannot re-show it.
    commit_graph_writing: bool,
    /// The user chose "Not now"; later reloads stay quiet until the repository
    /// is opened again.
    commit_graph_dismissed: bool,
    /// Canonical working-tree root, the key "Don't ask" is stored under.
    pub(crate) repository_key: Option<PathBuf>,
    /// Full ref names the history graph leaves out. The refs themselves stay in
    /// the sidebar; only the commits reachable from them alone leave the walk.
    pub(crate) hidden_refs: Vec<String>,

    /// Running maximum of graph widths, updated as chunks arrive.
    lanes: LaneWidth,
    /// The focused row; the details pane and the single commit actions read it.
    pub(crate) selected: Option<usize>,
    /// True when the synthetic uncommitted row at visual index 0 is selected.
    pub(crate) uncommitted_selected: bool,
    /// Stacked working-tree diff state while the uncommitted row is selected.
    pub(crate) uncommitted_diff: uncommitted_diff::UncommittedDiffState,
    /// The rows selected in the commit list. Its focused row mirrors
    /// `selected`, so single selection works exactly as it did before.
    selection: CommitSelection,
    /// Commit whose changed files were requested; guards against reloading the
    /// same selection on every snapshot publish.
    selection_commit: Option<Oid>,
    /// Position the edited commit should have in the history a rebase edit
    /// publishes; cleared once the selection follows it there.
    edit_selection: Option<history_ops::PendingSelection>,
    /// Commit of the ref the sidebar last revealed. Its row keeps the revealed
    /// tint so the target of a branch, tag or stash stays visible; selecting
    /// another commit clears it.
    revealed_commit: Option<Oid>,
    commit_scroll: UniformListScrollHandle,
    diff_scroll: UniformListScrollHandle,
    file_scroll: UniformListScrollHandle,
    /// Measured extents of the two draggable History boundaries, refreshed
    /// after every layout pass.
    splits: split::SplitExtents,
    focus: FocusHandle,

    files: Option<Vec<FileChange>>,
    /// LFS metadata for the changed files of the selected commit, keyed by
    /// path. Read with the file list so a badge never blocks a frame.
    files_lfs: Arc<HashMap<String, LfsInfo>>,
    /// Directories the details pane's tree view has collapsed, by path. Kept
    /// across commits so stepping through history keeps the same folders open.
    file_tree_collapsed: std::collections::HashSet<String>,
    /// LFS metadata for the worktree rows, keyed by path.
    pub(crate) worktree_lfs: Arc<HashMap<String, LfsInfo>>,
    worktree_lfs_epoch: u64,
    files_epoch: u64,
    files_error: Option<String>,
    selected_file: Option<usize>,

    /// Remote LFS locks, for the worktree context menu.
    pub(crate) lfs_locks: Rc<HashMap<String, LfsLock>>,
    pub(crate) lfs_locking_available: bool,
    lfs_locks_loaded: bool,
    /// The one-time warning shown when the repository uses LFS but git-lfs is
    /// not installed.
    lfs_notice: Option<String>,
    lfs_notice_dismissed: bool,

    /// Commit-wide added/removed counts; the details pane keeps the selected
    /// file's counts until they arrive.
    stat: details::CommitStatSlot,
    /// The range a multi-selection compares, and its answer.
    selection_diff: selection_diff::SelectionDiff,
    signatures: signatures::SignatureTracker,
    show_signatures: bool,
    changed_counts: changed_counts::ChangedFileCountTracker,
    show_changed_file_count: bool,
    /// The selected commit's file tree while it replaces the diff pane.
    tree: tree_view::TreeBrowser,
    /// The blame pane while it replaces the diff pane.
    blame: blame_view::BlameView,

    diff: DiffSlot,
    /// The options `diff` was requested with, so a settings change that does
    /// not touch them does not re-request the same diff.
    diff_options: DiffOptions,
    diff_rows: Rc<Vec<DiffRow>>,
    /// Which hidden lines of the diff on screen are revealed.
    diff_context: DiffContext,
    diff_error: Option<String>,
    /// The mode, zoom and swipe position of the image diff on screen.
    pub(crate) image_diff: image_diff::State,
    /// Pre-flattened synthetic diffs so `--fake` does not rebuild one per frame.
    fake_diffs: Rc<Vec<Rc<Vec<DiffRow>>>>,

    last_command: Option<String>,
    pub(crate) status_error: Option<String>,
    /// The multi-pick in flight, kept to report the commits core skipped.
    multi_pick: Option<PendingMultiPick>,
    /// A transient message that is not a failure, such as which copy of a file
    /// the editor opened. Shown in the status bar while no error is.
    pub(crate) status_note: Option<String>,

    title: String,
    target_frame_ms: f32,
    overlay: bool,
    smoke: bool,
    finished: bool,
    metrics: FrameMetrics,
    debug_launch: Option<DebugLaunchRunner>,

    _appearance: Subscription,
    /// Notifies the view when the settings change, so sizes and colours that
    /// come from them take effect without a restart.
    _settings: Subscription,
    /// Held so the event loop is cancelled with the view.
    _bridge: Option<Task<()>>,
    /// False while this repository sits in an inactive tab: session watcher
    /// keeps running in the background, but the view neither repaints nor polls.
    active: bool,
    /// Snapshot arrived while inactive; re-sync diffs/lists on re-activation.
    pending_sync: bool,
    /// Conflicts appeared when an operation finished while inactive.
    pending_reveal_conflicts: bool,
    window_handle: Option<AnyWindowHandle>,
    autofetch: Option<Task<()>>,
    last_autofetch: Instant,
    #[cfg(feature = "terminal")]
    pub(crate) terminal: Option<Entity<crate::terminal_panel::TerminalPanel>>,
    #[cfg(feature = "terminal")]
    pub(crate) terminal_open: bool,
    #[cfg(feature = "terminal")]
    pub(crate) terminal_top_fraction: f32,
    #[cfg(feature = "terminal")]
    pub(crate) terminal_split: Rc<std::cell::Cell<Option<crate::ui::SplitExtent>>>,
    #[cfg(all(test, feature = "terminal"))]
    pub(crate) quiet_terminal: bool,
}

/// Hands a session to a background thread that drops it.
///
/// `Session::drop` joins its worker threads, which only stop at a cancellation
/// point: a walk checks between commits, but a status refresh or a diff runs to
/// completion. Joining them on the UI thread would freeze the window for as
/// long as that git work takes.
pub(crate) fn drop_session_in_background(session: Session) {
    drop_in_background(session);
}

pub(crate) fn drop_in_background<T: Send + 'static>(value: T) {
    if let Err(error) = std::thread::Builder::new()
        .name("strata-session-drop".to_owned())
        .spawn(move || drop(value))
    {
        // Only resource exhaustion gets here, and the failed spawn already
        // dropped the closure, and the session with it, on this thread.
        log::warn!("could not start the session drop thread: {error}");
    }
}

impl Drop for RepoView {
    fn drop(&mut self) {
        // The swap in `Workspace` takes the session out before this runs, so
        // this covers the other ways a view goes away (window or app teardown)
        // without ever joining the workers on the UI thread.
        if let Some(session) = self.detach_session() {
            drop_session_in_background(session);
        }
    }
}

impl RepoView {
    #[cfg(test)]
    pub fn new(launch: Launch, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_in_workspace(launch, None, None, window, cx)
    }

    pub(crate) fn new_in_workspace(
        launch: Launch,
        workspace: Option<WeakEntity<crate::workspace::Workspace>>,
        parent_name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_in_workspace_active(launch, workspace, parent_name, true, window, cx)
    }

    pub(crate) fn new_in_workspace_active(
        launch: Launch,
        workspace: Option<WeakEntity<crate::workspace::Workspace>>,
        parent_name: Option<String>,
        active: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        if active {
            window.focus(&focus, cx);
        }
        let appearance = window
            .observe_window_appearance(|window, cx| crate::settings::apply_theme(Some(window), cx));
        let settings = cx.observe_global::<crate::settings::Settings>(|this, cx| {
            this.settings_changed(cx);
            this.notify_if_active(cx);
        });
        let smoke = std::env::var_os("STRATA_SMOKE").is_some();
        let title = launch.title();
        let entity = cx.weak_entity();
        let diff_options = history_diff_options(cx);

        let mut view = Self {
            session: None,
            fake: None,
            error: None,
            name: None,
            workspace,
            parent_name,
            mode: MainMode::History,
            worktree: WorktreePanel::default(),
            entity,
            branches: Branches::default(),
            branch_switcher: BranchSwitcher::default(),
            extra_lists: ExtraLists::default(),
            remote: Remote::default(),
            pull_requests: PullRequests::default(),
            search: SearchState::default(),
            file_history: None,
            compare: None,
            in_progress: InProgress::default(),
            reflog: reflog::ReflogState::default(),
            navigation: navigation::NavigationHistory::default(),
            commit_graph_offer: false,
            commit_graph_writing: false,
            commit_graph_dismissed: false,
            repository_key: None,
            hidden_refs: Vec::new(),
            lanes: LaneWidth::default(),
            selected: None,
            uncommitted_selected: false,
            uncommitted_diff: uncommitted_diff::UncommittedDiffState::default(),
            selection: CommitSelection::default(),
            selection_commit: None,
            edit_selection: None,
            revealed_commit: None,
            commit_scroll: UniformListScrollHandle::default(),
            diff_scroll: UniformListScrollHandle::default(),
            file_scroll: UniformListScrollHandle::default(),
            splits: split::SplitExtents::default(),
            focus,
            files: None,
            files_lfs: Arc::new(HashMap::new()),
            file_tree_collapsed: std::collections::HashSet::new(),
            worktree_lfs: Arc::new(HashMap::new()),
            worktree_lfs_epoch: 0,
            files_epoch: 0,
            files_error: None,
            selected_file: None,
            lfs_locks: Rc::new(HashMap::new()),
            lfs_locking_available: false,
            lfs_locks_loaded: false,
            lfs_notice: None,
            lfs_notice_dismissed: false,
            stat: details::CommitStatSlot::default(),
            selection_diff: selection_diff::SelectionDiff::default(),
            signatures: signatures::SignatureTracker::default(),
            show_signatures: crate::settings::Settings::get(cx)
                .history
                .show_commit_signatures,
            changed_counts: changed_counts::ChangedFileCountTracker::default(),
            show_changed_file_count: crate::settings::Settings::get(cx)
                .history
                .show_changed_file_count,
            tree: tree_view::TreeBrowser::default(),
            blame: blame_view::BlameView::default(),
            diff: DiffSlot::default(),
            diff_options,
            diff_rows: Rc::new(Vec::new()),
            diff_context: DiffContext::default(),
            diff_error: None,
            image_diff: image_diff::State::new(),
            fake_diffs: Rc::new(Vec::new()),
            last_command: None,
            status_error: None,
            multi_pick: None,
            status_note: None,
            title,
            target_frame_ms: 1000.0 / 60.0,
            overlay: smoke,
            smoke,
            finished: false,
            metrics: FrameMetrics::default(),
            debug_launch: None,
            _appearance: appearance,
            _settings: settings,
            _bridge: None,
            active,
            pending_sync: false,
            pending_reveal_conflicts: false,
            window_handle: Some(window.window_handle()),
            autofetch: None,
            last_autofetch: Instant::now(),
            #[cfg(feature = "terminal")]
            terminal: None,
            #[cfg(feature = "terminal")]
            terminal_open: false,
            #[cfg(feature = "terminal")]
            terminal_top_fraction: Self::DEFAULT_TERMINAL_TOP_FRACTION,
            #[cfg(feature = "terminal")]
            terminal_split: Rc::new(std::cell::Cell::new(None)),
            #[cfg(all(test, feature = "terminal"))]
            quiet_terminal: false,
        };
        view.branches.set_dialog_focus(cx.focus_handle());
        view.branches.set_list_focus(cx.focus_handle());

        match launch {
            Launch::Fake { repo } => {
                view.fake_diffs = Rc::new(
                    (0..8u32)
                        .map(|seed| Rc::new(diff_view::flatten(&fake::sample_diff(seed))))
                        .collect::<Vec<_>>(),
                );
                // The synthetic graph knows its widest row; a real one learns it
                // from the chunks as they arrive.
                view.lanes.observe_max(repo.max_width);
                view.fake = Some(repo);
                view.selected = Some(0);
            }
            Launch::Repo {
                session,
                name,
                title,
                mut events,
            } => {
                view.name = Some(name);
                view.title = title;
                let window_handle = window.window_handle();
                // Every session event is delivered here, on the foreground
                // executor: the session callback only pushes into the channel.
                view._bridge = Some(cx.spawn(async move |this, cx| {
                    while let Some(event) = events.next().await {
                        let is_snapshot = matches!(event, SessionEvent::SnapshotUpdated { .. });
                        let Ok(run_debug) = this.update(cx, |this, cx| {
                            this.on_event(event, cx);
                            is_snapshot && this.active && this.debug_launch.is_some()
                        }) else {
                            break;
                        };
                        if run_debug {
                            let _ = window_handle.update(cx, |_, window, cx| {
                                let _ = this.update(cx, |this, cx| {
                                    this.step_debug_launch(window, cx);
                                });
                            });
                        }
                    }
                }));
                view.repository_key = Some(commit_graph::repository_key(session.repo()));
                view.session = Some(*session);
                if let Some(key) = view.repository_key.clone() {
                    view.hidden_refs = crate::settings::Settings::get(cx).history.hidden_refs(&key);
                }
                if !view.hidden_refs.is_empty()
                    && let Some(session) = view.session.as_ref()
                {
                    session.set_hidden_refs(view.hidden_refs.clone());
                }
                view.search.init_input(window, cx);
                // The title bar's undo tooltip names the last action, so the
                // rows are read before the reflog pane is ever opened.
                view.refresh_reflog(cx);
                view.sync_autofetch(cx);
                view.check_lfs_notice(cx);
                if view.active {
                    view.publish_repository(cx);
                }
            }
            Launch::Error { path, error } => view.error = Some((path, error)),
        }

        view
    }

    #[cfg(test)]
    pub(crate) fn parent_name(&self) -> Option<&str> {
        self.parent_name.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn has_session(&self) -> bool {
        self.session.is_some()
    }

    pub(crate) fn repo_display_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub(crate) fn window_title(&self) -> &str {
        &self.title
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    pub(crate) fn has_operation_in_progress(&self) -> bool {
        self.is_busy() || self.remote.is_running() || self.in_progress.is_running()
    }

    pub(crate) fn record_autofetch(&mut self, at: Instant) {
        self.last_autofetch = at;
    }

    pub(crate) fn notify_if_active(&self, cx: &mut Context<Self>) {
        if self.active {
            cx.notify();
        }
    }

    fn notify_tab_state(&self, cx: &mut Context<Self>) {
        if self.active {
            cx.notify();
        } else if let Some(workspace) = self.workspace.as_ref().and_then(|ws| ws.upgrade()) {
            workspace.update(cx, |workspace, cx| workspace.refresh_tab_strip(cx));
        }
    }

    fn sync_autofetch(&mut self, cx: &mut Context<Self>) {
        let should_run = self.active
            && self.session.is_some()
            && crate::settings::Settings::get(cx)
                .behaviour
                .auto_fetch_minutes
                > 0;
        if should_run {
            if self.autofetch.is_none()
                && let Some(window) = self.window_handle
            {
                self.autofetch = Some(crate::autofetch::start(window, self.last_autofetch, cx));
            }
        } else {
            self.autofetch = None;
        }
    }

    /// Tells the config viewer which repository the next command opens.
    fn publish_repository(&self, cx: &mut App) {
        let Some(session) = &self.session else {
            return;
        };
        let repo = session.repo();
        let path = repo
            .workdir()
            .unwrap_or_else(|| repo.git_dir())
            .to_path_buf();
        crate::git_config_view::set_repository(path, session.git().log_handle(), cx);
    }

    pub(crate) fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active == active {
            return;
        }
        self.active = active;
        self.sync_autofetch(cx);
        if active {
            self.publish_repository(cx);
            if std::mem::take(&mut self.pending_sync) {
                self.sync_after_snapshot(cx);
            }
            if std::mem::take(&mut self.pending_reveal_conflicts) {
                self.reveal_conflicts(cx);
            }
            self.settings_changed(cx);
            cx.notify();
        }
    }

    fn sync_after_snapshot(&mut self, cx: &mut Context<Self>) {
        self.sync_selection(cx);
        self.settle_edit_selection(cx);
        self.sync_worktree(cx);
        if self.uncommitted_selected {
            self.sync_uncommitted_diff(cx);
        }
        if self.mode == MainMode::Worktree {
            self.load_worktree_lfs(cx);
            self.load_lfs_locks(cx);
        }
        self.search_on_snapshot(cx);
        self.file_history_on_snapshot(cx);
        self.reflog_on_snapshot(cx);
        // Branches and their upstreams move with the snapshot, so the pull
        // requests matched to them have to be recomputed even when the gh
        // cache itself did not change.
        self.recompute_pull_request_map(cx);
        // The submodule and worktree lists sit outside the snapshot, so
        // every publish — including a watcher refresh — re-reads them.
        self.refresh_extra_lists();
        self.refresh_in_progress_details();
    }

    /// The view's own focus handle, for panels that hand focus back on close.
    pub(crate) fn view_focus(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// The strong handle a render helper needs when all it gets is an `App`.
    ///
    /// Everything that calls this is either rendering or an element callback
    /// of this view's own tree, so the entity is alive: once it is gone, the
    /// elements carrying those callbacks are gone too.
    pub(crate) fn entity(&self) -> Entity<RepoView> {
        self.entity
            .upgrade()
            .expect("the view renders only while it is alive")
    }

    /// The handler behind every hidden-lines bar of the diff panes.
    ///
    /// One handler serves the history, file-history and compare panes: only
    /// one of them is on screen, and [`Self::expand_diff_gap`] picks the one
    /// whose rows it should rebuild.
    pub(crate) fn gap_handler(&self) -> Option<diff_view::GapHandler> {
        if self.fake.is_some() {
            return None;
        }
        let entity = self.entity();
        Some(Rc::new(move |gap, action, _window, cx| {
            entity.update(cx, |this, cx| this.expand_diff_gap(gap, action, cx));
        }))
    }

    /// Takes the session and stops forwarding its events to this view.
    ///
    /// The workspace calls this when it replaces the view: a late event from
    /// the old session must not reach the window any more, where its
    /// notification or dialog would land over the new repository.
    pub(crate) fn detach_session(&mut self) -> Option<Session> {
        self.autofetch = None;
        self.cancel_file_history();
        self.discard_blame();
        self.cancel_diff();
        self.drop_selection_diff();
        self.uncommitted_diff.cancel_all(self.session.as_ref());
        self.files_epoch = self.files_epoch.wrapping_add(1);
        self.tree.close();
        let worktree_diff = self.worktree.cancel_diff();
        let extra = self.extra_lists.cancel();
        let reflog = self.reflog.cancel();
        let search = self.search.reset();
        let stat = self.stat.begin(None);
        let in_progress = self.in_progress.clear_details();
        let signatures: Vec<_> = self.signatures.reset().collect();
        if let Some(session) = self.session.as_ref() {
            if let Some(ticket) = worktree_diff {
                session.cancel_diff(ticket);
            }
            for ticket in [extra, reflog, search, stat, in_progress]
                .into_iter()
                .flatten()
                .chain(signatures)
            {
                session.cancel_query(ticket);
            }
        }
        self._bridge.take();
        #[cfg(feature = "terminal")]
        {
            self.terminal.take();
            self.terminal_open = false;
        }
        self.session.take()
    }

    #[cfg(feature = "terminal")]
    pub(crate) const DEFAULT_TERMINAL_TOP_FRACTION: f32 = 0.68;
    #[cfg(feature = "terminal")]
    pub(crate) const MAIN_MIN_HEIGHT: f32 = 160.0;
    #[cfg(feature = "terminal")]
    pub(crate) const TERMINAL_MIN_HEIGHT: f32 = 120.0;

    #[cfg(feature = "terminal")]
    pub(crate) fn terminal_working_dir(&self) -> Option<PathBuf> {
        self.work_dir().or_else(|| {
            self.session
                .as_ref()
                .map(|s| s.repo().git_dir().to_path_buf())
        })
    }

    #[cfg(feature = "terminal")]
    pub(crate) fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal_open {
            self.close_terminal(window, cx);
            return;
        }
        if self.error.is_some() {
            return;
        }
        if self.terminal.is_none() {
            let Some(dir) = self.terminal_working_dir() else {
                return;
            };
            let parent_focus = self.focus.clone();
            let weak = self.entity.clone();
            let on_close: crate::terminal_panel::CloseHandler = Rc::new(move |window, cx| {
                let _ = weak.update(cx, |this, cx| this.close_terminal(window, cx));
            });
            #[cfg(test)]
            let quiet = self.quiet_terminal;
            let panel = cx.new(|cx| {
                #[cfg(test)]
                if quiet {
                    return crate::terminal_panel::TerminalPanel::new_quiet(
                        dir,
                        parent_focus,
                        Some(on_close),
                        window,
                        cx,
                    );
                }
                crate::terminal_panel::TerminalPanel::new(
                    dir,
                    parent_focus,
                    Some(on_close),
                    window,
                    cx,
                )
            });
            self.terminal = Some(panel);
        }
        self.terminal_open = true;
        if let Some(panel) = &self.terminal {
            let handle = gpui::Focusable::focus_handle(panel.read(cx), cx);
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    #[cfg(feature = "terminal")]
    pub(crate) fn close_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.terminal_open {
            return;
        }
        self.terminal_open = false;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    #[cfg(feature = "terminal")]
    pub(crate) fn set_terminal_top_fraction(
        &mut self,
        fraction: f32,
        total: f32,
        cx: &mut Context<Self>,
    ) {
        let clamped = crate::ui::clamp_fraction(
            fraction,
            total,
            Self::MAIN_MIN_HEIGHT,
            Self::TERMINAL_MIN_HEIGHT,
        );
        if (self.terminal_top_fraction - clamped).abs() < f32::EPSILON {
            return;
        }
        self.terminal_top_fraction = clamped;
        cx.notify();
    }

    #[cfg(feature = "terminal")]
    pub(crate) fn reset_terminal_top_fraction(&mut self, cx: &mut Context<Self>) {
        if (self.terminal_top_fraction - Self::DEFAULT_TERMINAL_TOP_FRACTION).abs() < f32::EPSILON {
            return;
        }
        self.terminal_top_fraction = Self::DEFAULT_TERMINAL_TOP_FRACTION;
        cx.notify();
    }

    pub(crate) fn source(&self) -> Option<Source> {
        if let Some(fake) = &self.fake {
            Some(Source::Fake(Rc::clone(fake)))
        } else {
            self.session
                .as_ref()
                .map(|session| Source::Repo(session.snapshot()))
        }
    }

    pub fn commit_count(&self) -> usize {
        self.source().map_or(0, |source| source.len())
    }

    fn on_event(&mut self, event: SessionEvent, cx: &mut Context<Self>) {
        match event {
            SessionEvent::SnapshotUpdated { .. } => {
                if !self.active {
                    self.pending_sync = true;
                    return;
                }
                self.sync_after_snapshot(cx);
                cx.notify();
            }
            SessionEvent::DiffReady { ticket, view } => {
                if self.file_history_diff_ready(ticket, &view, cx) {
                    return;
                }
                if self.compare_diff_ready(ticket, &view, cx) {
                    return;
                }
                if self.uncommitted_diff_ready(ticket, &view, cx) {
                    return;
                }
                if self.worktree.on_diff_ready(ticket, &view) {
                    self.notify_if_active(cx);
                    return;
                }
                if !self.diff.accept(ticket, Arc::clone(&view)) {
                    return;
                }
                // Flattening copies the line text once per version instead of
                // rebuilding it on every frame. Another file starts collapsed.
                self.diff_context.reset();
                self.diff_rows = Rc::new(self.diff_context.rows(&view));
                self.diff_error = None;
                self.notify_if_active(cx);
            }
            SessionEvent::DiffFailed { ticket, error } => {
                if self.file_history_diff_failed(ticket, &error, cx) {
                    return;
                }
                if self.compare_diff_failed(ticket, &error, cx) {
                    return;
                }
                if self.uncommitted_diff_failed(ticket, &error, cx) {
                    return;
                }
                if self.worktree.on_diff_failed(ticket, &error) {
                    self.notify_if_active(cx);
                    return;
                }
                if !self.diff.fail(ticket) {
                    return;
                }
                self.diff_rows = Rc::new(Vec::new());
                self.diff_error = Some(error);
                self.notify_if_active(cx);
            }
            SessionEvent::QueryReady {
                ticket,
                generation,
                rows,
            } => {
                self.search_ready(ticket, generation, &rows, cx);
            }
            SessionEvent::QueryFailed { ticket, error } => {
                // A failed re-read only leaves the previous lists in place; the
                // next snapshot asks again.
                if self.extra_lists.fail(ticket) {
                    return;
                }
                if !self.search_failed(ticket, &error, cx)
                    && !self.file_history_failed(ticket, &error, cx)
                    && !self.reflog_failed(ticket, &error, cx)
                    && !self.selection_comparison_failed(ticket, &error, cx)
                    && !self.compare_failed(ticket, &error, cx)
                {
                    self.blame_failed(ticket, &error, cx);
                }
            }
            SessionEvent::CompareReady { ticket, comparison } => {
                if !self.selection_comparison_ready(ticket, &comparison, cx) {
                    self.compare_ready(ticket, &comparison, cx);
                }
            }
            SessionEvent::CommitStatReady { ticket, stat } => {
                if self.stat.accept(ticket, stat) {
                    self.notify_if_active(cx);
                }
            }
            SessionEvent::PathHistoryBatch { ticket, entries } => {
                self.file_history_batch(ticket, &entries, cx);
            }
            SessionEvent::PathHistoryDone { ticket } => {
                self.file_history_done(ticket, cx);
            }
            SessionEvent::ContentSearchBatch {
                ticket,
                matches,
                scanned,
                total,
            } => {
                self.content_search_batch(ticket, &matches, scanned, total, cx);
            }
            SessionEvent::ContentSearchProgress {
                ticket,
                scanned,
                total,
            } => {
                self.content_search_progress(ticket, scanned, total, cx);
            }
            SessionEvent::ContentSearchDone {
                ticket,
                scanned,
                total,
            } => {
                self.content_search_done(ticket, scanned, total, cx);
            }
            SessionEvent::BlameProgress {
                ticket,
                lines,
                hunks,
                commits,
            } => self.blame_progress(ticket, lines, hunks, commits, cx),
            SessionEvent::BlameReady { ticket, blame } => self.blame_ready(ticket, &blame, cx),
            SessionEvent::SubmodulesAndWorktreesReady {
                ticket,
                submodules,
                worktrees,
            } => {
                if self.extra_lists.accept(ticket, submodules, worktrees) {
                    self.notify_if_active(cx);
                }
            }
            SessionEvent::InProgressDetailsReady { ticket, details } => {
                self.on_in_progress_details_ready(ticket, details, cx);
            }
            SessionEvent::ReflogReady { ticket, entries } => {
                self.reflog_ready(ticket, &entries, cx);
            }
            SessionEvent::CommandLogged(record) => {
                // Probes stay in the command log, but only a command the user
                // asked for belongs in the status line.
                if let Some(line) = status_command(&record) {
                    self.last_command = Some(line);
                    self.notify_if_active(cx);
                }
            }
            SessionEvent::LoadFailed { error } => {
                if self.debug_launch.is_some()
                    && self
                        .session
                        .as_ref()
                        .is_none_or(|session| session.snapshot().head.is_none())
                {
                    eprintln!("debug-launch: error: repository load failed: {error}");
                    self.debug_launch = None;
                }
                self.status_error = Some(error);
                self.notify_if_active(cx);
            }
            SessionEvent::OperationFinished { id, error } => {
                // The session refreshed the status before it reported the end,
                // so this snapshot already has the conflicts the command left.
                let conflicts = self
                    .session
                    .as_ref()
                    .map_or(0, |session| session.snapshot().conflicts.len());
                if self.in_progress.finished(id, conflicts) {
                    if self.active {
                        self.reveal_conflicts(cx);
                    } else {
                        self.pending_reveal_conflicts = true;
                    }
                }
                if self.commit_graph_writing {
                    self.commit_graph_writing = false;
                    // A successful write reloads the history and the offer is
                    // answered; a failed one leaves it up so the user can retry.
                    if error.is_none() {
                        self.commit_graph_offer = false;
                        self.commit_graph_dismissed = true;
                    }
                }
                self.worktree.on_operation_finished(id, error.as_deref());
                let remote_finished =
                    self.remote
                        .on_finished(id, error.as_deref(), cx, self.active);
                if remote_finished {
                    // `on_finished` reports a fetch and a push the same way;
                    // the push notification leaves a marker, and a push is the
                    // one remote command whose refresh must not be throttled.
                    let cause = if self.pull_requests.take_pending_push() {
                        RefreshCause::AfterPush
                    } else {
                        RefreshCause::Automatic
                    };
                    self.refresh_pull_requests(cause, cx);
                }
                // The label records which operation failed, so the status bar
                // names it instead of showing a bare git message.
                let label = self.branches.finish(id, error.as_deref());
                self.reflog_operation_finished(id, error.is_some());
                let multi_pick = self.multi_pick.take();
                if let Some(error) = error {
                    // A refused or failed edit left the selection where it was.
                    self.edit_selection = None;
                    self.status_error = Some(match label {
                        Some(label) => format!("{label}: {}", one_line(&error)),
                        None => one_line(&error),
                    });
                } else if let Some(pending) = multi_pick
                    && let Some(note) =
                        commit_selection::applied_note(pending.total, &pending.skipped)
                {
                    // The skipped picks are the one outcome git reports only in
                    // the command output, so the status bar keeps it visible.
                    self.status_note = Some(note);
                }
                if self.active
                    && let Some(retry) = self.branches.take_force_retry()
                {
                    // The offer opens from an operation result, which has no
                    // window of its own, so it is routed through the active one.
                    crate::branches::offer_force(cx.entity(), retry, cx);
                }
                self.notify_tab_state(cx);
            }
            SessionEvent::OperationStarted { id } => {
                // The snapshot published with `busy` still holds the status
                // from before the command, which is what the switch compares
                // against.
                let conflicts = self
                    .session
                    .as_ref()
                    .map_or(0, |session| session.snapshot().conflicts.len());
                // Only a user action queues a job; a refresh just publishes.
                self.in_progress.started(id, conflicts, true);
                self.remote.on_started(id, cx, self.active);
                if !self.active {
                    self.notify_tab_state(cx);
                }
            }
            SessionEvent::OperationProgress { id, text, percent } => {
                if let Some(pending) = &mut self.multi_pick
                    && let Some(skipped) = commit_selection::parse_skipped(&text)
                {
                    pending.skipped = skipped;
                }
                self.remote.on_progress(id, &text, percent, cx, self.active)
            }
            SessionEvent::AskpassPrompt { id, prompt, secret } => {
                self.prompt_for_credentials(id, prompt, secret, cx);
            }
            SessionEvent::CommitGraphSuggested { .. } => {
                if !self.commit_graph_writing
                    && !self.commit_graph_dismissed
                    && self.commit_graph_allowed(cx)
                {
                    self.commit_graph_offer = true;
                    self.notify_if_active(cx);
                }
            }
            SessionEvent::CommitSignaturesDetected { ticket, formats } => {
                self.signatures_detected(ticket, &formats, cx);
            }
            SessionEvent::CommitSignaturesReady { ticket, signatures } => {
                self.signatures_ready(ticket, &signatures, cx);
            }
            SessionEvent::SignaturesInvalidated => {
                self.signatures_invalidated(cx);
            }
        }
    }

    /// Shows git's credential question in a modal and answers the session.
    ///
    /// Cancelling, or a window that is already gone, answers `None`: the helper
    /// exits non-zero and the command fails with git's own message instead of
    /// waiting for a prompt nobody can see.
    fn prompt_for_credentials(
        &mut self,
        id: OperationId,
        prompt: String,
        secret: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let Some(window) = self.window_handle else {
            session.answer_askpass(id, None);
            return;
        };
        let entity = self.entity.clone();
        let opened = window.update(cx, |_, window, cx| {
            crate::ui::askpass_prompt(
                window,
                cx,
                t!("history.authentication").to_string(),
                prompt,
                secret,
                move |answer, _window, cx| {
                    if let Some(entity) = entity.upgrade() {
                        entity.update(cx, |view, _cx| {
                            if let Some(session) = view.session.as_ref() {
                                session.answer_askpass(id, answer);
                            }
                        });
                    }
                },
            );
        });
        if opened.is_err() {
            session.answer_askpass(id, None);
        }
    }

    /// True while the session is running an operation or one this view started
    /// has not reported back yet.
    pub(crate) fn is_busy(&self) -> bool {
        self.branches.is_running()
            || self
                .session
                .as_ref()
                .is_some_and(|session| session.snapshot().busy.is_some())
    }

    /// Re-requests the submodule and worktree lists for the snapshot just
    /// published.
    ///
    /// The request it replaces is cancelled: its answer would describe a
    /// repository state the sidebar no longer shows.
    fn refresh_extra_lists(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        if let Some(ticket) = self.extra_lists.cancel() {
            session.cancel_query(ticket);
        }
        let generation = session.snapshot().generation;
        let ticket = session.request_submodules_and_worktrees();
        self.extra_lists.begin(ticket, generation);
    }

    /// Runs a mutation through the session.
    ///
    /// The branch UI is disabled while an operation runs, and `--fake` has no
    /// session at all, so both cases return without doing anything. `label` is
    /// what a failure is reported as in the status bar.
    pub(crate) fn run_operation(
        &mut self,
        operation: Operation,
        label: impl Into<String>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<OperationId> {
        if self.session.is_none() || self.is_busy() {
            return None;
        }
        // A new edit makes the recorded redo point at a state the user has
        // left; only the undo and redo themselves keep the stack.
        if !matches!(
            operation,
            Operation::UndoReflog { .. } | Operation::RedoReflog { .. }
        ) {
            self.clear_redo();
        }
        let Some(session) = &self.session else {
            return None;
        };
        let label = label.into();
        let retry = match &operation {
            Operation::DeleteBranch { name, force } => Some(crate::branches::Retry::DeleteBranch {
                name: name.clone(),
                forced: *force,
            }),
            Operation::WorktreeRemove { path, force } => {
                Some(crate::branches::Retry::RemoveWorktree {
                    path: path.clone(),
                    forced: *force,
                })
            }
            _ => None,
        };
        if let Operation::CherryPickMany { commits } = &operation {
            self.multi_pick = Some(PendingMultiPick {
                total: commits.len(),
                skipped: Vec::new(),
            });
        }
        let kind = FinishedOperation::of(&operation);
        let id = session.run(operation);
        self.in_progress.submitted(id, kind);
        self.branches.start(id, label, retry);
        cx.notify();
        Some(id)
    }

    /// The create-branch shortcut: starts from HEAD.
    pub(crate) fn create_branch_from_head(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_none() || self.is_busy() || self.branches.dialog_open() {
            return;
        }
        let label = match self.source().and_then(|source| source.head().cloned()) {
            Some(HeadInfo::Branch { short_name, .. }) => {
                t!("history.from_branch", name = short_name).to_string()
            }
            Some(HeadInfo::Detached { .. }) => t!("history.from_current").to_string(),
            None => t!("history.from_head").to_string(),
        };
        self.open_create_dialog(None, label, window, cx);
    }

    /// Opens the create dialog, focused on its name field. `start_point` is a
    /// ref or commit id the branch is created from, or HEAD when absent.
    pub(crate) fn open_create_dialog(
        &mut self,
        start_point: Option<String>,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session.is_none() {
            return;
        }
        self.branches.set_viewport(window.viewport_size());
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("branch.placeholder.name").to_string())
        });
        let handle = input.focus_handle(cx);
        self.branches.open_create(
            input,
            BranchStart {
                point: start_point,
                label,
            },
        );
        // The dialog renders on the next frame; focus it once it is there.
        window.defer(cx, move |window, cx| handle.focus(window, cx));
        cx.notify();
    }

    /// Asks before deleting a branch, then runs the delete.
    ///
    /// This is a dialog layer, not a card drawn over the sidebar, so it holds
    /// focus and `Esc` cancels it instead of reaching the view behind it.
    pub(crate) fn confirm_delete_branch(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session.is_none() {
            return;
        }
        // The dialog restores focus to whatever had it when it opened; the
        // context menu that offered the delete is closing, so hand it the
        // view's own handle instead.
        self.focus_root(window, cx);
        let entity = cx.weak_entity();
        crate::ui::confirm(
            window,
            cx,
            crate::ui::Confirm::new(
                t!("history.delete_branch", name = name).to_string(),
                t!("history.delete_branch_detail").to_string(),
            )
            .ok_label(t!("common.delete").to_string()),
            move |window, cx| {
                let name = name.clone();
                let _ = entity.update(cx, |view, cx| view.delete_branch(name, false, window, cx));
            },
        );
    }

    /// Runs the delete; `force` is the recovery offered after git refused an
    /// unmerged branch.
    pub(crate) fn delete_branch(
        &mut self,
        name: String,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = format!("delete branch {name}");
        self.run_operation(Operation::DeleteBranch { name, force }, label, window, cx);
    }

    /// Confirms whichever branch, remote or upstream dialog is open.
    pub(crate) fn submit_branch_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(request) = self.branches.dialog_request(cx) else {
            return;
        };
        let (operation, label) = match request {
            Ok(crate::branches::DialogRequest::Create(request)) => (
                Operation::CreateBranch {
                    name: request.name.clone(),
                    start_point: request.start_point,
                    checkout: request.checkout,
                },
                format!("create branch {}", request.name),
            ),
            Ok(crate::branches::DialogRequest::Rename(request)) => (
                Operation::RenameBranch {
                    name: request.name.clone(),
                    new_name: request.new_name.clone(),
                    rename_upstream: request.rename_upstream,
                },
                format!("rename branch {} to {}", request.name, request.new_name),
            ),
            Ok(crate::branches::DialogRequest::SetUpstream {
                branch,
                remote,
                remote_branch,
            }) => (
                Operation::SetBranchUpstream {
                    branch: branch.clone(),
                    remote: remote.clone(),
                    remote_branch: remote_branch.clone(),
                },
                format!("set upstream of {branch} to {remote}/{remote_branch}"),
            ),
            Ok(crate::branches::DialogRequest::AddRemote(request)) => (
                Operation::AddRemote {
                    name: request.name.clone(),
                    url: request.url,
                    fetch: request.fetch,
                },
                format!("add remote {}", request.name),
            ),
            Ok(crate::branches::DialogRequest::RenameRemote { name, new_name }) => (
                Operation::RenameRemote {
                    name: name.clone(),
                    new_name: new_name.clone(),
                },
                format!("rename remote {name} to {new_name}"),
            ),
            Ok(crate::branches::DialogRequest::EditRemoteUrl(request)) => (
                Operation::SetRemoteUrl {
                    name: request.name.clone(),
                    fetch_url: request.fetch_url,
                    push_url: request.push_url,
                },
                format!("change the URL of {}", request.name),
            ),
            Err(message) => {
                self.branches.set_dialog_error(message);
                cx.notify();
                return;
            }
        };
        self.branches.close_dialog();
        self.focus_root(window, cx);
        self.run_operation(operation, label, window, cx);
    }

    /// Returns focus from a dialog to the commit list, which owns the keys.
    pub(crate) fn focus_root(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus, cx);
    }

    /// Whether the synthetic uncommitted row sits at the top of the commit list.
    pub(crate) fn has_uncommitted_row(&self, cx: &App) -> bool {
        if self.smoke || self.fake.is_some() || self.search.filter().is_some() {
            return false;
        }
        if !crate::settings::Settings::get(cx)
            .history
            .show_uncommitted_changes
        {
            return false;
        }
        self.session
            .as_ref()
            .is_some_and(|session| uncommitted::should_show_row(&session.snapshot()))
    }

    pub(crate) fn uncommitted_summary(&self) -> Option<String> {
        let session = self.session.as_ref()?;
        let snapshot = session.snapshot();
        let details = self.in_progress.details_for(snapshot.state);
        Some(uncommitted::summary_text(&snapshot, details))
    }

    pub(crate) fn uncommitted_counts(&self) -> uncommitted::UncommittedCounts {
        let Some(session) = self.session.as_ref() else {
            return uncommitted::UncommittedCounts::default();
        };
        uncommitted::counts(&session.snapshot())
    }

    pub(crate) fn uncommitted_graph(&self) -> Option<crate::graph_canvas::UncommittedGraph> {
        let session = self.session.as_ref()?;
        let snapshot = session.snapshot();
        let details = self.in_progress.details_for(snapshot.state);
        Some(uncommitted::graph_info(&snapshot, details))
    }

    /// Maps a commit index in the unfiltered history list to its visual row
    /// position in `uniform_list`.
    pub(crate) fn visual_commit_position(&self, index: usize, cx: &App) -> usize {
        index + usize::from(self.has_uncommitted_row(cx))
    }

    /// Selects the synthetic uncommitted row at visual position 0.
    pub(crate) fn select_uncommitted(&mut self, cx: &mut Context<Self>) {
        if !self.has_uncommitted_row(cx) {
            return;
        }
        if self.uncommitted_selected {
            return;
        }
        self.branches.clear_revealed();
        self.branches.clear_selected_extra();
        self.uncommitted_selected = true;
        self.selected = None;
        self.selection.clear();
        self.selection_commit = None;
        self.revealed_commit = None;
        self.selected_file = None;
        self.files = None;
        self.files_lfs = Arc::new(HashMap::new());
        self.files_error = None;
        self.diff_scroll = UniformListScrollHandle::default();
        self.tree.close();
        self.discard_blame();
        self.cancel_diff();
        self.drop_selection_diff();
        self.sync_uncommitted_diff(cx);
        self.commit_scroll.scroll_to_item(0, ScrollStrategy::Center);
        self.notify_if_active(cx);
    }

    /// Keeps the selection valid as commits arrive or the history is replaced.
    fn sync_selection(&mut self, cx: &mut Context<Self>) {
        let has_uncommitted = self.has_uncommitted_row(cx);
        if self.uncommitted_selected {
            if has_uncommitted {
                return;
            }
            self.uncommitted_selected = false;
            self.uncommitted_diff.cancel_all(self.session.as_ref());
        }
        let Some(source) = self.source() else {
            return;
        };
        if source.is_empty() {
            if has_uncommitted {
                self.select_uncommitted(cx);
            }
            return;
        }
        if self.selection.is_multi() {
            // A reload that rewrote history moves the rows under the
            // selection; remapping would be guesswork, so it collapses onto
            // the focused commit and the single-selection rules below apply.
            let focused_valid = self.selection_commit.is_some()
                && self.selected.and_then(|index| source.commit_id(index)) == self.selection_commit;
            let rows_valid = self.selection.rows().all(|index| index < source.len());
            if focused_valid && rows_valid {
                return;
            }
            self.selection.clear();
        }
        match self.selected {
            None => self.select(Some(0), cx),
            Some(index) if index >= source.len() => self.select(Some(source.len() - 1), cx),
            Some(index) => {
                // A reload can put a different commit at the same index, and a
                // collapsed multi-selection has to be restored to one row.
                let commit = source.commit_id(index);
                let single = self.selection.len() == 1 && self.selection.contains(index);
                if commit.is_some() && (commit != self.selection_commit || !single) {
                    self.select(Some(index), cx);
                }
            }
        }
    }

    pub(crate) fn select(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        let Some(source) = self.source() else {
            return;
        };
        let index = index.filter(|index| *index < source.len());
        let commit = index.and_then(|index| source.commit_id(index));
        let single = match index {
            Some(index) => self.selection.len() == 1 && self.selection.contains(index),
            None => self.selection.len() == 0,
        };
        if !self.uncommitted_selected
            && index == self.selected
            && commit == self.selection_commit
            && single
        {
            return;
        }
        self.uncommitted_selected = false;
        self.selection.select_one(index);
        self.focus_commit(index, commit, cx);
    }

    /// Points the details pane and the single commit actions at `index`,
    /// without touching the selected rows. A multi-selection changes its
    /// focused row through this instead of [`Self::select`], which collapses
    /// the selection.
    fn focus_commit(&mut self, index: Option<usize>, commit: Option<Oid>, cx: &mut Context<Self>) {
        // Selecting a commit is the user moving off the sidebar ref they had
        // revealed; the reveal tint belongs to that jump only.
        self.branches.clear_revealed();
        self.branches.clear_selected_extra();
        self.uncommitted_selected = false;
        self.uncommitted_diff.cancel_all(self.session.as_ref());
        self.selected = index;
        self.selection_commit = commit;
        // The reveal mark belongs to the ref the sidebar showed, not to
        // whatever the user selected next.
        if self.revealed_commit != commit {
            self.revealed_commit = None;
        }
        self.selected_file = None;
        self.files = None;
        self.files_lfs = Arc::new(HashMap::new());
        self.files_error = None;
        self.diff_scroll = UniformListScrollHandle::default();
        // The tree shows one commit's contents, so another selection leaves it.
        self.tree.close();
        self.cancel_diff();

        // A multi-selection compares its range instead of the focused commit:
        // the summary block and the range's file list take the pane.
        let multi = self.selection.is_multi();
        if multi {
            self.sync_selection_diff(cx);
        } else {
            self.drop_selection_diff();
        }

        // The smoke run scrolls as fast as it can; loading a file list and a
        // diff for every row would measure git instead of the renderer.
        if let Some(commit) = commit
            && !self.smoke
            && !multi
        {
            self.load_files(commit, cx);
        }
        // The range replaces the commit-wide totals, so the focused commit has
        // none to show while several rows are selected.
        self.load_commit_stat(if multi { None } else { commit }, cx);
        self.load_selected_signature(commit, cx);
        self.notify_if_active(cx);
    }

    /// A click on a commit row. Without a modifier it is exactly the old
    /// single selection; Cmd/Ctrl and Shift act on the selected set.
    pub(crate) fn click_commit(
        &mut self,
        index: usize,
        position: usize,
        shift: bool,
        secondary: bool,
        order: &[usize],
        cx: &mut Context<Self>,
    ) {
        if !shift && !secondary {
            self.select_result(index, position, cx);
            return;
        }
        self.uncommitted_selected = false;
        let previous = self.selected;
        self.selection.click(index, shift, secondary, order);
        let focused = self.selection.focused();
        if focused == previous {
            // Only the selected set changed; the focused commit and its diff
            // stay on screen, but the range the set describes did not.
            self.sync_selection_diff(cx);
            cx.notify();
            return;
        }
        let commit =
            focused.and_then(|focused| self.source().and_then(|source| source.commit_id(focused)));
        self.focus_commit(focused, commit, cx);
    }

    /// Right-clicking a row outside the selection moves the selection there
    /// before its menu opens, so the menu acts on what the user pointed at.
    pub(crate) fn ensure_commit_selection(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.selection.contains(index) {
            return;
        }
        self.select(Some(index), cx);
    }

    /// Shift+Up/Down: moves the focused row and extends the selected range.
    pub(crate) fn extend_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.commit_count();
        if len == 0 {
            return;
        }
        if self.uncommitted_selected {
            if delta > 0 {
                self.select(Some(0), cx);
            }
            return;
        }
        // A filtered list ranges over its visible rows, in the order displayed.
        let order: Vec<usize> = match self.search.filter() {
            Some(rows) => rows.as_ref().clone(),
            None => (0..len).collect(),
        };
        let previous = self.selected;
        let Some(focused) = self.selection.extend_by(delta, &order) else {
            return;
        };
        if let Some(position) = order.iter().position(|row| *row == focused) {
            let visual = if self.search.filter().is_some() {
                position
            } else {
                self.visual_commit_position(position, cx)
            };
            self.scroll_to_position(visual);
        }
        if Some(focused) == previous {
            self.sync_selection_diff(cx);
            cx.notify();
            return;
        }
        let commit = self.source().and_then(|source| source.commit_id(focused));
        self.focus_commit(Some(focused), commit, cx);
    }

    fn load_files(&mut self, commit: Oid, cx: &mut Context<Self>) {
        self.files_epoch += 1;
        let epoch = self.files_epoch;
        let Some(session) = &self.session else {
            return;
        };
        // `commit_changes` is a gix read; the repo handle is cheap to clone and
        // safe off the UI thread.
        let repo = session.repo().clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let files = repo.commit_changes(commit)?;
                    let mut lfs = HashMap::new();
                    for change in &files {
                        if let Ok(Some(info)) = repo.lfs_info_at(commit, &change.path) {
                            lfs.insert(change.path.clone(), info);
                        }
                    }
                    Ok::<_, strata_core::Error>((files, lfs))
                })
                .await;
            this.update(cx, |this, cx| {
                if this.files_epoch != epoch {
                    return;
                }
                match result {
                    Ok((files, lfs)) => {
                        let empty = files.is_empty();
                        let preferred = this
                            .search
                            .matched_path_for(commit)
                            .and_then(|path| files.iter().position(|file| file.path == path))
                            .unwrap_or(0);
                        this.files = Some(files);
                        this.files_lfs = Arc::new(lfs);
                        this.files_error = None;
                        if empty {
                            this.notify_if_active(cx);
                        } else {
                            // Prefer the file that matched an active content
                            // search; otherwise show the first file.
                            this.select_file(preferred, cx);
                        }
                    }
                    Err(error) => {
                        this.files_error = Some(error.to_string());
                        this.notify_if_active(cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Reads LFS metadata for the worktree rows off the UI thread.
    fn load_worktree_lfs(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let snapshot = session.snapshot();
        let mut paths = Vec::new();
        for change in snapshot
            .status
            .staged
            .iter()
            .chain(snapshot.status.unstaged.iter())
        {
            if !paths.contains(&change.path) {
                paths.push(change.path.clone());
            }
        }
        self.worktree_lfs_epoch = self.worktree_lfs_epoch.wrapping_add(1);
        let epoch = self.worktree_lfs_epoch;
        let repo = session.repo().clone();
        cx.spawn(async move |this, cx| {
            let map = cx
                .background_executor()
                .spawn(async move {
                    let mut map = HashMap::new();
                    for path in paths {
                        let info = repo
                            .lfs_info_in_worktree(&path)
                            .ok()
                            .flatten()
                            .or_else(|| repo.lfs_info_in_index(&path).ok().flatten());
                        if let Some(info) = info {
                            map.insert(path, info);
                        }
                    }
                    map
                })
                .await;
            this.update(cx, |this, cx| {
                if this.worktree_lfs_epoch == epoch {
                    this.worktree_lfs = Arc::new(map);
                    this.notify_if_active(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Asks the remote for its LFS locks once per view, which also decides
    /// whether the Lock and Unlock entries belong in the context menu.
    fn load_lfs_locks(&mut self, cx: &mut Context<Self>) {
        if self.lfs_locks_loaded {
            return;
        }
        let Some(session) = &self.session else {
            return;
        };
        self.lfs_locks_loaded = true;
        let git = session.git().clone();
        cx.spawn(async move |this, cx| {
            let (available, locks) = cx
                .background_executor()
                .spawn(async move {
                    let available = git.lfs_available();
                    let locks = git.lfs_locks().ok();
                    (available, locks)
                })
                .await;
            this.update(cx, |this, cx| {
                this.lfs_locking_available = available && locks.is_some();
                this.lfs_locks = Rc::new(
                    locks
                        .map(|locks| {
                            locks
                                .into_iter()
                                .map(|lock| (lock.path.clone(), lock))
                                .collect()
                        })
                        .unwrap_or_default(),
                );
                this.notify_if_active(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Shows the install hint once when the repository tracks LFS files but
    /// the `git-lfs` command is missing.
    fn check_lfs_notice(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let git = session.git().clone();
        let repo = session.repo().clone();
        cx.spawn(async move |this, cx| {
            let missing = cx
                .background_executor()
                .spawn(async move { !git.lfs_available() && repo.uses_lfs() })
                .await;
            this.update(cx, |this, cx| {
                if missing && !this.lfs_notice_dismissed && this.lfs_notice.is_none() {
                    this.lfs_notice = Some(crate::ui::install_hint());
                    this.notify_if_active(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn select_file(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(change) = self
            .files
            .as_ref()
            .and_then(|files| files.get(index))
            .cloned()
        else {
            return;
        };
        let Some(session) = self.session.as_ref() else {
            return;
        };

        // A multi-selection shows the range at the pair the details summary
        // names, so the same file diff pane serves both cases.
        let request = if let Some((base, head)) = self.selection_diff.range() {
            DiffRequest::Compare {
                base,
                head,
                old_path: change.old_path.clone(),
                path: change.path.clone(),
            }
        } else {
            // The commit on screen, not the selected history row: the reflog
            // pane shows a commit the history selection may not point at.
            let Some(commit) = self.selection_commit else {
                return;
            };
            DiffRequest::Commit {
                commit,
                path: change.path.clone(),
            }
        };

        self.selected_file = Some(index);
        self.diff_error = None;
        self.diff_rows = Rc::new(Vec::new());
        self.diff_context.reset();
        let options = history_diff_options(cx);
        let ticket = session.request_diff_with(request, options);
        self.diff_options = options;
        if let Some(previous) = self.diff.begin(ticket) {
            session.cancel_diff(previous);
        }
        self.notify_if_active(cx);
    }

    /// Re-requests the history diff when the options it was computed with
    /// changed, so a new context size or ignore-whitespace shows at once.
    fn settings_changed(&mut self, cx: &mut Context<Self>) {
        self.sync_autofetch(cx);
        self.pull_requests_setting_changed(crate::settings::Settings::get(cx).tools.gh.enabled, cx);
        let show_signatures = crate::settings::Settings::get(cx)
            .history
            .show_commit_signatures;
        if show_signatures != self.show_signatures {
            self.show_signatures = show_signatures;
            self.sync_signature_setting(cx);
        }
        let show_changed_file_count = crate::settings::Settings::get(cx)
            .history
            .show_changed_file_count;
        if show_changed_file_count != self.show_changed_file_count {
            self.show_changed_file_count = show_changed_file_count;
            self.sync_changed_file_count_setting(cx);
        }
        let show_uncommitted = crate::settings::Settings::get(cx)
            .history
            .show_uncommitted_changes;
        if !show_uncommitted && self.uncommitted_selected {
            self.uncommitted_selected = false;
            self.uncommitted_diff.cancel_all(self.session.as_ref());
            self.sync_selection(cx);
        }
        if !self.active {
            return;
        }
        let options = history_diff_options(cx);
        if options == self.diff_options {
            self.notify_if_active(cx);
            return;
        }
        self.diff_options = options;
        if self.uncommitted_selected {
            self.sync_uncommitted_diff(cx);
        }
        if self.file_history.is_some() {
            self.request_history_diff(cx);
        } else if self.compare.is_some() {
            self.request_compare_diff(cx);
        } else if let Some(index) = self.selected_file {
            self.select_file(index, cx);
        }
    }

    fn cancel_diff(&mut self) {
        self.diff_rows = Rc::new(Vec::new());
        self.diff_context.reset();
        self.diff_error = None;
        let ticket = self.diff.cancel();
        if let (Some(session), Some(ticket)) = (self.session.as_ref(), ticket) {
            session.cancel_diff(ticket);
        }
    }

    /// Applies one press on a hidden-lines bar of the diff on screen.
    pub(crate) fn expand_diff_gap(
        &mut self,
        gap: usize,
        action: GapAction,
        cx: &mut Context<Self>,
    ) {
        if self.file_history.is_some() {
            self.expand_file_history_gap(gap, action, cx);
            return;
        }
        if self.compare.is_some() {
            self.expand_compare_gap(gap, action, cx);
            return;
        }
        let Some(view) = self.diff.view().cloned() else {
            return;
        };
        if self.diff_context.expand(gap, action, &view) {
            let context = self.diff_context.clone();
            let revision = context.revision();
            let build_view = Arc::clone(&view);
            self.rebuild_rows_in_background(RowsTarget::History, revision, cx, move || {
                context.rows(&build_view)
            });
        }
        self.notify_if_active(cx);
    }

    /// The "Whole file" toggle, for whichever diff is on screen.
    pub(crate) fn toggle_whole_file(&mut self, cx: &mut Context<Self>) {
        if self.file_history.is_some() {
            self.toggle_file_history_whole_file(cx);
            return;
        }
        if self.compare.is_some() {
            self.toggle_compare_whole_file(cx);
            return;
        }
        if self.mode == MainMode::Worktree {
            self.toggle_worktree_whole_file(cx);
            return;
        }
        let Some(view) = self.diff.view().cloned() else {
            return;
        };
        if self.diff_context.toggle_whole_file(&view) {
            let context = self.diff_context.clone();
            let revision = context.revision();
            let build_view = Arc::clone(&view);
            self.rebuild_rows_in_background(RowsTarget::History, revision, cx, move || {
                context.rows(&build_view)
            });
        }
        self.notify_if_active(cx);
    }

    /// Rebuilds a pane's rows off the UI thread.
    ///
    /// Revealing context turns into one row per shown line, and the whole-file
    /// view can be a whole file, so the allocation does not belong in a frame.
    pub(crate) fn rebuild_rows_in_background(
        &mut self,
        target: RowsTarget,
        revision: u64,
        cx: &mut Context<Self>,
        build: impl FnOnce() -> Vec<DiffRow> + Send + 'static,
    ) {
        cx.spawn(async move |this, cx| {
            let rows = cx.background_executor().spawn(async move { build() }).await;
            this.update(cx, |this, cx| {
                if this.accept_rows(target, revision, rows) {
                    this.notify_if_active(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Installs a rebuild unless the pane moved on while it ran.
    fn accept_rows(&mut self, target: RowsTarget, revision: u64, rows: Vec<DiffRow>) -> bool {
        match target {
            RowsTarget::History => {
                if self.diff_context.revision() != revision {
                    return false;
                }
                self.diff_rows = Rc::new(rows);
                true
            }
            RowsTarget::Compare => {
                let Some(compare) = self.compare.as_mut() else {
                    return false;
                };
                if compare.context.revision() != revision {
                    return false;
                }
                compare.diff_rows = Rc::new(rows);
                true
            }
            RowsTarget::FileHistory => {
                let Some(history) = self.file_history.as_mut() else {
                    return false;
                };
                if history.context.revision() != revision {
                    return false;
                }
                history.diff_rows = Rc::new(rows);
                true
            }
        }
    }

    fn toggle_frame_stats(&mut self, cx: &mut Context<Self>) {
        self.overlay = !self.overlay;
        cx.notify();
    }

    fn select_first(&mut self, cx: &mut Context<Self>) {
        self.jump_visual(0, cx);
    }

    fn select_last(&mut self, cx: &mut Context<Self>) {
        let count = self.commit_count();
        if count == 0 {
            if self.has_uncommitted_row(cx) {
                self.select_uncommitted(cx);
            }
            return;
        }
        self.jump(count - 1, cx);
    }

    pub(crate) fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        // The compare view has no commit rows; the same keys walk its files.
        if self.compare.is_some() {
            self.step_compare_file(delta, cx);
            return;
        }
        let has_uncommitted = self.has_uncommitted_row(cx);
        let total = self.commit_count() + usize::from(has_uncommitted);
        if total == 0 {
            return;
        }
        let current = if self.uncommitted_selected && has_uncommitted {
            0
        } else {
            self.selected
                .map_or(0, |index| index + usize::from(has_uncommitted))
        } as isize;
        let next = (current + delta).clamp(0, total as isize - 1) as usize;
        self.jump_visual(next, cx);
    }

    /// Selects and scrolls to the commit a sidebar entry points at. Nothing
    /// is checked out: every ref's history is already in the list.
    pub(crate) fn reveal_ref(
        &mut self,
        kind: RefKind,
        short_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let snapshot = session.snapshot();
        let target = snapshot
            .refs
            .iter()
            .find(|info| info.kind == kind && info.short_name == short_name)
            .map(|info| info.target);
        self.reveal_commit(target, window, cx);
        // After the jump, because selecting the commit clears the highlight.
        self.branches.clear_selected_extra();
        self.branches.reveal_ref(kind, short_name);
        cx.notify();
    }

    /// Like [`Self::reveal_ref`] for `stash@{index}`.
    pub(crate) fn reveal_stash(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let target = session
            .snapshot()
            .stashes
            .iter()
            .find(|stash| stash.index == index)
            .map(|stash| stash.id);
        self.reveal_commit(target, window, cx);
        self.branches.clear_selected_extra();
        self.branches.reveal_stash(index);
        cx.notify();
    }

    fn reveal_commit(&mut self, target: Option<Oid>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = target.and_then(|id| {
            self.session
                .as_ref()
                .and_then(|session| session.snapshot().commit_index.get(&id).copied())
        }) else {
            // Not loaded yet (the walk is still streaming) or outside the
            // walked refs; there is nothing to scroll to.
            return;
        };
        if self.mode != MainMode::History {
            self.close_worktree(window, cx);
        }
        // The jump addresses the full list, so neither a file history nor a
        // search filter may stay in front of it.
        self.close_file_history(window, cx);
        if self.search.is_active() {
            self.clear_search(window, cx);
        }
        self.jump(index, cx);
        // Set after the jump: `select` clears a mark that points elsewhere.
        self.revealed_commit = target;
    }

    /// Scrolls the commit list to a row without changing the selection. While a
    /// search filters the list the argument is a visible position, not a commit
    /// index.
    pub(crate) fn scroll_to_position(&mut self, position: usize) {
        self.commit_scroll
            .scroll_to_item(position, ScrollStrategy::Center);
    }

    fn jump_visual(&mut self, position: usize, cx: &mut Context<Self>) {
        let has_uncommitted = self.has_uncommitted_row(cx);
        if has_uncommitted && position == 0 {
            self.select_uncommitted(cx);
            return;
        }
        let index = position.saturating_sub(usize::from(has_uncommitted));
        self.commit_scroll
            .scroll_to_item(position, ScrollStrategy::Center);
        self.select(Some(index), cx);
    }

    fn jump(&mut self, index: usize, cx: &mut Context<Self>) {
        let position = self.visual_commit_position(index, cx);
        self.commit_scroll
            .scroll_to_item(position, ScrollStrategy::Center);
        self.select(Some(index), cx);
    }

    /// Selects several rows for `--select a,b`, the same way Cmd-clicking them
    /// builds a multi-selection. `rows` is in display order, newest first.
    fn debug_multi_select(&mut self, rows: &[usize], cx: &mut Context<Self>) {
        let order: Vec<usize> = (0..self.commit_count()).collect();
        self.uncommitted_selected = false;
        self.selection.clear();
        // Oldest first so the newest row, which the details pane follows, ends
        // up focused.
        for &row in rows.iter().rev() {
            if row < order.len() {
                self.selection.click(row, false, true, &order);
            }
        }
        let focused = self.selection.focused();
        let position = self.visual_commit_position(rows[0], cx);
        self.commit_scroll
            .scroll_to_item(position, ScrollStrategy::Center);
        let commit =
            focused.and_then(|index| self.source().and_then(|source| source.commit_id(index)));
        self.focus_commit(focused, commit, cx);
    }

    fn advance_smoke(&mut self, cx: &mut Context<Self>) {
        let len = self.commit_count();
        if len > 0 {
            let next = (self.selected.unwrap_or(0) + SMOKE_STEP) % len;
            self.commit_scroll
                .scroll_to_item(next, ScrollStrategy::Center);
            // Selection only: the smoke run measures scrolling, not git.
            self.selected = Some(next);
        }
        if self.metrics.frames() >= SMOKE_FRAMES {
            self.finish_smoke_run(cx);
        }
    }

    fn finish_smoke_run(&mut self, cx: &mut Context<Self>) {
        if self.finished {
            return;
        }
        self.finished = true;
        let stats = self.metrics.stats();
        eprintln!(
            "smoke: {} frames, {:.2} ms/frame mean, {:.1} fps, p95 {:.2} ms, build {:.2} ms, target {:.1} ms, {} commits",
            self.metrics.frames(),
            stats.frame_ms,
            stats.fps,
            stats.p95_ms,
            stats.render_ms,
            self.target_frame_ms,
            self.commit_count()
        );
        cx.quit();
    }

    fn sync_title(&mut self, window: &mut Window) {
        let title = if self.fake.is_some() {
            t!("history.window_synthetic").to_string()
        } else if let Some(name) = &self.name {
            let head: Option<HeadInfo> = self.source().and_then(|source| source.head().cloned());
            data::window_title(name, head.as_ref())
        } else {
            t!("app.name").to_string()
        };
        if title != self.title {
            self.title = title.clone();
            window.set_window_title(&title);
        }
    }

    /// The display's refresh interval, or 60 Hz when the platform does not
    /// report one.
    fn target_frame_ms(window: &Window, cx: &App) -> f32 {
        window
            .refresh_interval(cx)
            .map(|interval| interval.as_secs_f32() * 1000.0)
            .filter(|interval| *interval > 0.0)
            .unwrap_or(1000.0 / 60.0)
    }

    /// Configures debug launch flags to be applied once the repository's first
    /// snapshot has loaded (or reports why they cannot apply for `--fake` / error views).
    pub(crate) fn set_debug_launch(
        &mut self,
        debug: DebugLaunch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !debug.is_active() {
            return;
        }
        if self.session.is_none() {
            if debug.has_repo_actions() {
                if self.fake.is_some() {
                    eprintln!(
                        "debug-launch: error: repository options require a real repository (cannot be used with --fake)"
                    );
                } else {
                    eprintln!(
                        "debug-launch: error: repository failed to open, skipping repository debug options"
                    );
                }
            }
            if let Some(dialog) = debug.dialog
                && !dialog.needs_repo()
            {
                debug_launch::open_window_dialog(window.window_handle(), dialog, cx);
            }
            return;
        }

        self.debug_launch = Some(DebugLaunchRunner::new(debug));
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.snapshot().head.is_some())
        {
            self.step_debug_launch(window, cx);
        }
    }

    fn step_debug_launch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            self.debug_launch = None;
            return;
        };
        let snapshot = session.snapshot();
        // Status refreshes can publish an empty snapshot before history::load
        // reads HEAD and the first commit chunk.
        if snapshot.head.is_none() {
            return;
        }
        let Some(mut runner) = self.debug_launch.take() else {
            return;
        };

        let mut select_ready = true;
        if let Some(list) = runner.launch.select.clone() {
            for (index, rev) in debug_launch::select_revs(&list).into_iter().enumerate() {
                if !runner.resolve_rev(
                    rev,
                    RevSlot::Select(index),
                    &snapshot,
                    session.repo().clone(),
                    window,
                    cx,
                ) {
                    select_ready = false;
                }
            }
        }
        let tree_ready = if let Some(target) = runner.launch.tree.clone() {
            runner.resolve_rev(
                target.rev(),
                RevSlot::Tree,
                &snapshot,
                session.repo().clone(),
                window,
                cx,
            )
        } else {
            true
        };
        let blame_ready = if let Some(target) = runner.launch.blame.clone() {
            runner.resolve_rev(
                &target.rev,
                RevSlot::Blame,
                &snapshot,
                session.repo().clone(),
                window,
                cx,
            )
        } else {
            true
        };

        if !select_ready || !tree_ready || !blame_ready {
            self.debug_launch = Some(runner);
            return;
        }

        let launch = runner.launch;
        if let Some(list) = &launch.select {
            let revs = debug_launch::select_revs(list);
            let mut rows = Vec::new();
            for (index, rev) in revs.iter().enumerate() {
                match &runner.select_oids[index] {
                    Some(Ok(oid)) => match snapshot.commit_index.get(oid).copied() {
                        Some(row) => rows.push(row),
                        None => eprintln!(
                            "debug-launch: error: --select '{rev}' resolved to {}, which is not in the loaded history",
                            data::short_oid(*oid)
                        ),
                    },
                    Some(Err(err)) => {
                        eprintln!("debug-launch: error: --select '{rev}' failed: {err}");
                    }
                    None => {}
                }
            }
            // A higher row is an older commit, so sorting puts the newest
            // selection first, which is the order the details pane reads.
            rows.sort_unstable();
            rows.dedup();
            match rows.as_slice() {
                [] => {}
                [row] => {
                    self.jump(*row, cx);
                    eprintln!("debug-launch: applied --select {list} (row {row})");
                }
                _ => {
                    self.debug_multi_select(&rows, cx);
                    eprintln!("debug-launch: applied --select {list} (rows {rows:?})");
                }
            }
        }

        if let Some(target) = &launch.tree {
            let rev = target.rev();
            let flag = match target {
                TreeTarget::Directory { .. } => "--tree",
                TreeTarget::File { .. } => "--tree-file",
            };
            match runner.tree_oid {
                Some(Ok(oid)) => match snapshot.commit_index.get(&oid).copied() {
                    Some(index) => {
                        self.jump(index, cx);
                        match target {
                            TreeTarget::Directory { dir: None, .. } => {
                                self.open_tree(window, cx);
                                eprintln!(
                                    "debug-launch: applied --tree {rev} -> {}",
                                    data::short_oid(oid)
                                );
                            }
                            TreeTarget::Directory { dir: Some(dir), .. } => {
                                self.open_tree_at(Some(dir.clone()), None, window, cx);
                                eprintln!(
                                    "debug-launch: applied --tree {rev} -> {} (dir '{dir}')",
                                    data::short_oid(oid)
                                );
                            }
                            TreeTarget::File { path, .. } => {
                                self.open_tree_at(None, Some(path.clone()), window, cx);
                                eprintln!(
                                    "debug-launch: applied --tree-file {rev}:{path} -> {}",
                                    data::short_oid(oid)
                                );
                            }
                        }
                    }
                    None => eprintln!(
                        "debug-launch: error: {flag} '{rev}' resolved to {}, which is not in the loaded history",
                        data::short_oid(oid)
                    ),
                },
                Some(Err(err)) => {
                    eprintln!("debug-launch: error: {flag} '{rev}' failed: {err}");
                }
                None => {}
            }
        }

        if let Some(path) = launch.file_history {
            self.open_file_history(path.clone(), window, cx);
            eprintln!("debug-launch: applied --file-history {path}");
        }

        if launch.worktree {
            self.open_worktree(window, cx);
            eprintln!("debug-launch: applied --worktree");
        }

        if let Some(target) = &launch.blame {
            let rev = target.rev.as_str();
            match runner.blame_oid {
                Some(Ok(oid)) => {
                    // The blamed commit need not be one of the loaded rows, so
                    // the pane opens either way; selecting it is a nicety.
                    if let Some(index) = snapshot.commit_index.get(&oid).copied() {
                        self.jump(index, cx);
                    }
                    self.open_blame(oid, target.path.clone(), window, cx);
                    eprintln!(
                        "debug-launch: applied --blame {rev}:{} -> {}",
                        target.path,
                        data::short_oid(oid)
                    );
                }
                Some(Err(err)) => {
                    eprintln!("debug-launch: error: --blame '{rev}' failed: {err}");
                }
                None => {}
            }
        }

        if let Some(dialog) = launch.dialog {
            let view = cx.weak_entity();
            window.on_next_frame(move |window, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.apply_debug_dialog(dialog, window, cx);
                });
            });
            window.refresh();
        }
    }

    fn apply_debug_dialog(
        &mut self,
        dialog: DebugDialog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match dialog {
            DebugDialog::CreateBranch => {
                self.create_branch_from_head(window, cx);
                eprintln!("debug-launch: applied --dialog create-branch");
            }
            DebugDialog::Stash => {
                self.stash_changes(window, cx);
                eprintln!("debug-launch: applied --dialog stash");
            }
            DebugDialog::AddWorktree => {
                self.open_add_worktree(window, cx);
                eprintln!("debug-launch: applied --dialog add-worktree");
            }
            DebugDialog::CreateTag => {
                let head_commit = self.session.as_ref().and_then(|session| {
                    session
                        .snapshot()
                        .head
                        .as_ref()
                        .and_then(|head| match head {
                            HeadInfo::Branch { target, .. } => *target,
                            HeadInfo::Detached { target } => Some(*target),
                        })
                });
                let target = self.selection_commit.or(head_commit);
                if let Some(commit) = target {
                    self.open_tag_dialog(commit, data::short_oid(commit), window, cx);
                    eprintln!("debug-launch: applied --dialog create-tag");
                } else {
                    eprintln!(
                        "debug-launch: error: --dialog create-tag requires a commit (HEAD is unborn)"
                    );
                }
            }
            DebugDialog::OpenRepository => {
                window.dispatch_action(Box::new(crate::actions::OpenRepository), cx);
                eprintln!("debug-launch: applied --dialog open-repository");
            }
            DebugDialog::Settings | DebugDialog::Palette => {
                debug_launch::dispatch_window_dialog(dialog, window, cx);
            }
        }
    }

    fn finish_debug_rev_lookup(
        &mut self,
        slot: RevSlot,
        resolved: Result<Oid, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(runner) = self.debug_launch.as_mut() else {
            return;
        };
        match slot {
            RevSlot::Select(index) => {
                runner.select_resolving = false;
                runner.select_oids[index] = Some(resolved);
            }
            RevSlot::Tree => {
                runner.tree_resolving = false;
                runner.tree_oid = Some(resolved);
            }
            RevSlot::Blame => {
                runner.blame_resolving = false;
                runner.blame_oid = Some(resolved);
            }
        }
        self.step_debug_launch(window, cx);
    }
}

#[derive(Clone, Copy)]
enum RevSlot {
    Select(usize),
    Tree,
    Blame,
}

struct DebugLaunchRunner {
    launch: DebugLaunch,
    /// One slot per comma-separated `--select` revision; a multi-selection
    /// needs every revision in the loaded history before it can be applied.
    select_oids: Vec<Option<Result<Oid, String>>>,
    select_resolving: bool,
    tree_oid: Option<Result<Oid, String>>,
    tree_resolving: bool,
    blame_oid: Option<Result<Oid, String>>,
    blame_resolving: bool,
}

impl DebugLaunchRunner {
    fn new(launch: DebugLaunch) -> Self {
        let select_oids = launch
            .select
            .as_deref()
            .map(debug_launch::select_revs)
            .map(|revs| vec![None; revs.len()])
            .unwrap_or_default();
        Self {
            launch,
            select_oids,
            select_resolving: false,
            tree_oid: None,
            tree_resolving: false,
            blame_oid: None,
            blame_resolving: false,
        }
    }

    /// Returns `true` when this revision is settled (either resolved and present
    /// in `snapshot.commit_index`, or resolved to a final error / missing after
    /// `snapshot.commits.complete`).
    fn resolve_rev(
        &mut self,
        rev: &str,
        which: RevSlot,
        snapshot: &strata_core::session::RepoSnapshot,
        repo: strata_core::repo::Repo,
        window: &mut Window,
        cx: &mut Context<RepoView>,
    ) -> bool {
        let (slot, resolving) = match which {
            RevSlot::Select(index) => (&mut self.select_oids[index], &mut self.select_resolving),
            RevSlot::Tree => (&mut self.tree_oid, &mut self.tree_resolving),
            RevSlot::Blame => (&mut self.blame_oid, &mut self.blame_resolving),
        };

        if slot.is_none() {
            if *resolving {
                return false;
            }
            match debug_launch::resolve_in_snapshot(snapshot, rev) {
                SnapshotRevMatch::Found(oid) => *slot = Some(Ok(oid)),
                SnapshotRevMatch::Ambiguous(count) => {
                    *slot = Some(Err(format!(
                        "ambiguous revision '{rev}' ({count} matching commits)"
                    )));
                }
                SnapshotRevMatch::NotFound => {
                    *resolving = true;
                    let window_handle = window.window_handle();
                    let rev_owned = rev.to_owned();
                    cx.spawn(async move |this, cx| {
                        let resolved = cx
                            .background_executor()
                            .spawn(async move {
                                let mut walk = repo
                                    .walk_commits_from_refs(std::slice::from_ref(&rev_owned))
                                    .map_err(|err| err.to_string())?;
                                match walk.next() {
                                    Some(Ok(commit)) => Ok(commit.id),
                                    Some(Err(err)) => Err(err.to_string()),
                                    None => Err(format!(
                                        "revision '{rev_owned}' did not resolve to a commit"
                                    )),
                                }
                            })
                            .await;
                        let _ = window_handle.update(cx, |_, window, cx| {
                            let _ = this.update(cx, |this, cx| {
                                this.finish_debug_rev_lookup(which, resolved, window, cx);
                            });
                        });
                    })
                    .detach();
                    return false;
                }
            }
        }

        match slot {
            Some(Ok(oid)) => snapshot.commit_index.contains_key(oid) || snapshot.commits.complete,
            Some(Err(_)) => true,
            None => false,
        }
    }
}

/// git's stderr can span several lines, but the status bar is one line tall.
fn one_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The status line's entry for a finished command, or `None` for a capability
/// probe. A probe such as `git lfs version` fails whenever that optional tool
/// is missing, and showing it as the user's last command reads like an error
/// they caused.
fn status_command(record: &CommandRecord) -> Option<String> {
    (!record.internal).then(|| command_line(record))
}

/// `git <args> · 12.3 ms`, with a non-zero exit code kept visible.
fn command_line(record: &CommandRecord) -> String {
    let mut line = String::from("git");
    for arg in &record.args {
        line.push(' ');
        line.push_str(arg);
    }
    if let Some(code) = record.exit_code
        && code != 0
    {
        line.push_str(&format!(" (exit {code})"));
    }
    let ms = record.duration.as_secs_f64() * 1000.0;
    format!("{line} · {ms:.1} ms")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};
    use strata_core::diff::FileDiff;

    fn oid(byte: u8) -> Oid {
        format!("{byte:02x}")
            .repeat(20)
            .parse()
            .expect("hex object id")
    }

    fn view(commit: Oid) -> Arc<DiffView> {
        Arc::new(DiffView {
            request: DiffRequest::Commit {
                commit,
                path: "src/lib.rs".into(),
            },
            diff: FileDiff::default(),
            old_highlights: Arc::new(Vec::new()),
            new_highlights: Arc::new(Vec::new()),
            lfs: None,
            old_text: Arc::from(""),
            new_text: Arc::from(""),
            old_encoding: strata_core::encoding::TextEncoding::UTF_8,
            new_encoding: strata_core::encoding::TextEncoding::UTF_8,
            gaps: Arc::from(Vec::new()),
            image: None,
        })
    }

    /// The workspace replaces a repository view when the user opens another
    /// one, and a window releases its root view when it closes; in both cases
    /// the view must actually go. It can only do that if nothing it owns keeps
    /// a strong handle to it, which is where a stored `Entity<RepoView>` of
    /// itself would leak the whole view (commit rows, graph, diffs, caches).
    #[gpui::test]
    fn closing_a_window_releases_its_repo_view(cx: &mut gpui::TestAppContext) {
        // Closing the last window must not shut the test app down before the
        // assertions run.
        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            cx.set_global(crate::settings::Settings::default());
        });
        let window = cx.add_window(|window, cx| {
            RepoView::new(
                Launch::Error {
                    path: "not-a-repository".to_owned(),
                    error: "not a repository".to_owned(),
                },
                window,
                cx,
            )
        });

        let view = window.root(cx).expect("the window's root view");
        let released = view.downgrade();
        assert!(
            released.upgrade().is_some(),
            "the window root must hold the view"
        );
        drop(view);

        window
            .update(cx, |_, window, _| window.remove_window())
            .expect("the window is still open");
        cx.run_until_parked();

        assert!(
            released.upgrade().is_none(),
            "the closed window must release the repository view"
        );
    }

    #[test]
    fn a_background_drop_does_not_block_the_caller() {
        use std::sync::mpsc;

        // Stands in for a session whose worker threads take a while to join.
        struct SlowDrop {
            started: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        }

        impl Drop for SlowDrop {
            fn drop(&mut self) {
                let _ = self.started.send(());
                let _ = self.release.recv_timeout(Duration::from_secs(5));
            }
        }

        let (started, drop_started) = mpsc::channel();
        let (release, drop_release) = mpsc::channel();
        let handoff = std::time::Instant::now();
        drop_in_background(SlowDrop {
            started,
            release: drop_release,
        });
        assert!(
            handoff.elapsed() < Duration::from_millis(500),
            "the caller must not wait for the session's threads"
        );

        drop_started
            .recv_timeout(Duration::from_secs(5))
            .expect("the background thread must start the drop");
        release
            .send(())
            .expect("the background thread must still be dropping");
    }

    #[test]
    fn a_late_diff_ready_for_an_old_ticket_is_ignored() {
        let mut slot = DiffSlot::default();
        assert_eq!(slot.begin(DiffTicket(1)), None);
        assert_eq!(slot.begin(DiffTicket(2)), Some(DiffTicket(1)));

        assert!(!slot.accept(DiffTicket(1), view(oid(1))));
        assert!(slot.view().is_none(), "the stale view must not be stored");

        assert!(slot.accept(DiffTicket(2), view(oid(2))));
        assert_eq!(
            slot.view().expect("current view").request,
            DiffRequest::Commit {
                commit: oid(2),
                path: "src/lib.rs".into()
            }
        );
    }

    #[test]
    fn starting_a_new_diff_clears_the_previous_view() {
        let mut slot = DiffSlot::default();
        slot.begin(DiffTicket(1));
        assert!(slot.accept(DiffTicket(1), view(oid(1))));
        assert!(slot.view().is_some());

        slot.begin(DiffTicket(2));
        assert!(slot.view().is_none(), "the old diff is not shown meanwhile");
    }

    #[test]
    fn only_the_current_ticket_can_fail_or_be_cancelled() {
        let mut slot = DiffSlot::default();
        slot.begin(DiffTicket(7));

        assert!(!slot.fail(DiffTicket(6)));
        assert!(slot.fail(DiffTicket(7)));
        assert_eq!(slot.cancel(), Some(DiffTicket(7)));
        assert_eq!(slot.cancel(), None);
        assert!(!slot.fail(DiffTicket(7)), "cancelled tickets are stale");
    }

    #[test]
    fn a_late_submodule_and_worktree_answer_for_an_old_ticket_is_ignored() {
        let mut lists = ExtraLists::default();
        lists.begin(QueryTicket(2), 7);

        assert!(
            !lists.accept(QueryTicket(1), Arc::new(Vec::new()), Arc::new(Vec::new())),
            "an answer requested before the latest snapshot must not replace it"
        );
        assert_eq!(lists.generation, 0, "nothing was accepted");

        assert!(lists.accept(QueryTicket(2), Arc::new(Vec::new()), Arc::new(Vec::new())));
        assert_eq!(
            lists.generation, 7,
            "the answer carries the generation it was requested for"
        );
    }

    #[test]
    fn a_new_request_supersedes_the_read_in_flight() {
        let mut lists = ExtraLists::default();
        lists.begin(QueryTicket(1), 4);
        assert_eq!(
            lists.cancel(),
            Some(QueryTicket(1)),
            "the replaced read is cancelled, not kept"
        );

        lists.begin(QueryTicket(2), 5);
        assert!(
            !lists.accept(QueryTicket(1), Arc::new(Vec::new()), Arc::new(Vec::new())),
            "the cancelled read's answer is stale too"
        );
        assert!(lists.accept(QueryTicket(2), Arc::new(Vec::new()), Arc::new(Vec::new())));
        assert_eq!(lists.generation, 5);
    }

    #[test]
    fn a_failure_only_clears_the_request_it_belongs_to() {
        let mut lists = ExtraLists::default();
        lists.begin(QueryTicket(3), 9);

        assert!(!lists.fail(QueryTicket(2)), "an old failure is not ours");
        assert!(lists.fail(QueryTicket(3)));
        assert!(
            !lists.accept(QueryTicket(3), Arc::new(Vec::new()), Arc::new(Vec::new())),
            "a failed read cannot still answer"
        );
    }

    #[test]
    fn a_multi_line_git_error_fits_the_status_bar() {
        assert_eq!(
            one_line(
                "error: the branch 'topic' is not fully merged\n\nhint: run 'git branch -D topic'"
            ),
            "error: the branch 'topic' is not fully merged · hint: run 'git branch -D topic'"
        );
        assert_eq!(one_line("  fatal: one line  "), "fatal: one line");
    }

    #[test]
    fn commands_are_logged_with_their_duration_and_failure() {
        let record = CommandRecord {
            args: vec!["status".into(), "--porcelain=v2".into()],
            started_at: SystemTime::now(),
            duration: Duration::from_millis(12),
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            internal: false,
        };
        assert_eq!(command_line(&record), "git status --porcelain=v2 · 12.0 ms");

        let failed = CommandRecord {
            exit_code: Some(128),
            ..record
        };
        assert_eq!(
            command_line(&failed),
            "git status --porcelain=v2 (exit 128) · 12.0 ms"
        );
    }

    #[test]
    fn a_capability_probe_is_logged_but_never_becomes_the_last_command() {
        let probe = CommandRecord {
            args: vec!["lfs".into(), "version".into()],
            started_at: SystemTime::now(),
            duration: Duration::from_millis(6),
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "git: 'lfs' is not a git command".into(),
            internal: true,
        };
        assert_eq!(status_command(&probe), None);

        // The same argv from the user (a custom command, say) still shows.
        let asked = CommandRecord {
            internal: false,
            ..probe
        };
        assert_eq!(
            status_command(&asked).as_deref(),
            Some("git lfs version (exit 1) · 6.0 ms")
        );
    }

    #[test]
    fn paths_render_relative_to_the_current_directory() {
        let cwd = std::env::current_dir().expect("current dir");
        let nested = cwd.join("crates/strata-app");
        assert_eq!(display_path(&nested), "crates/strata-app");

        let elsewhere = PathBuf::from("/definitely/not/here");
        assert_eq!(display_path(&elsewhere), "/definitely/not/here");
    }

    #[cfg(feature = "terminal")]
    mod terminal_tests {
        use super::*;
        use crate::actions::ToggleTerminal;
        use gpui::{QuitMode, VisualTestContext};
        use gpui_component::ThemeMode;
        use strata_term::{TermKey, TermKeyAction, TermKeyEvent, TermModifiers};

        struct ScratchRepo {
            dir: tempfile::TempDir,
            config: PathBuf,
        }

        impl ScratchRepo {
            fn new() -> Self {
                let dir = tempfile::TempDir::new().expect("temporary directory");
                let config = dir.path().join("gitconfig");
                std::fs::write(
                    &config,
                    "[user]\n\tname = Strata Test\n\temail = strata@example.invalid\n\
                     [commit]\n\tgpgsign = false\n[init]\n\tdefaultBranch = main\n",
                )
                .expect("write test git config");
                let scratch = Self { dir, config };
                std::fs::create_dir(scratch.work()).expect("work directory");
                scratch.git(scratch.work(), &["init", "-q"]);
                std::fs::write(scratch.work().join("README.md"), "hello\n").expect("write file");
                scratch.git(scratch.work(), &["add", "README.md"]);
                scratch.git(scratch.work(), &["commit", "-q", "-m", "initial"]);
                scratch
            }

            fn work(&self) -> PathBuf {
                self.dir.path().join("work")
            }

            fn add_worktree(&self, name: &str) -> PathBuf {
                let wt = self.dir.path().join(name);
                self.git(
                    self.work(),
                    &[
                        "worktree",
                        "add",
                        "-q",
                        "-b",
                        name,
                        wt.to_str().expect("utf8 path"),
                    ],
                );
                wt
            }

            fn git(&self, cwd: PathBuf, args: &[&str]) -> String {
                let output = std::process::Command::new("git")
                    .args(args)
                    .current_dir(cwd)
                    .env("GIT_CONFIG_GLOBAL", &self.config)
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .output()
                    .expect("run git");
                assert!(
                    output.status.success(),
                    "git {args:?} failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                String::from_utf8_lossy(&output.stdout).into_owned()
            }
        }

        fn prepare_app(cx: &mut App) {
            cx.set_quit_mode(QuitMode::Explicit);
            gpui_component::init(cx);
            // Only Unix overrides the shell, so Windows never mutates it.
            #[cfg_attr(not(unix), allow(unused_mut))]
            let mut settings = crate::settings::Settings::default();
            #[cfg(unix)]
            {
                settings.tools.embedded_shell = Some("/bin/sh".to_owned());
            }
            cx.set_global(settings);
            crate::actions::bind(cx);
            crate::terminal_panel::bind(cx);
        }

        #[gpui::test]
        fn toggle_terminal_is_lazy_focuses_panel_keeps_session_and_ignores_escape(
            cx: &mut gpui::TestAppContext,
        ) {
            cx.update(prepare_app);
            let repo = ScratchRepo::new();
            let wt_path = repo.add_worktree("linked-wt");
            let window = cx.add_window(|window, cx| {
                RepoView::new(Launch::open_quiet(&wt_path, None), window, cx)
            });
            let view = window.root(cx).expect("the window's root view");
            let mut vcx = VisualTestContext::from_window(window.into(), cx);
            vcx.update(|_, cx| {
                view.update(cx, |view, _| view.quiet_terminal = true);
            });

            // Session must be created lazily: nothing spawned until toggled open.
            assert!(
                view.read_with(&vcx, |view, _| !view.terminal_open
                    && view.terminal.is_none()),
                "terminal session must stay uncreated before first open"
            );

            vcx.dispatch_action(ToggleTerminal);
            let panel = vcx.update(|window, cx| {
                window.draw(cx).clear(cx);
                let repo_view = view.read(cx);
                assert!(repo_view.terminal_open);
                let panel = repo_view
                    .terminal
                    .clone()
                    .expect("terminal created on open");
                let actual = std::fs::canonicalize(panel.read(cx).working_dir())
                    .expect("canonical worktree dir");
                let expected = std::fs::canonicalize(&wt_path).expect("canonical expected");
                assert_eq!(actual, expected, "terminal opens in linked worktree root");
                assert!(panel.read(cx).has_session());
                assert!(panel.read(cx).snapshot().cols >= 2);
                assert!(
                    panel.read(cx).focus_handle(cx).is_focused(window),
                    "opening the terminal must move focus into it"
                );
                panel
            });

            // Pressing Escape while the terminal is focused must NOT close it
            // or trigger RepoView's SearchDismiss handler.
            vcx.simulate_keystrokes("escape");
            assert!(
                view.read_with(&vcx, |view, _| view.terminal_open),
                "Escape must leave the terminal panel open"
            );
            assert!(view.read_with(&vcx, |view, _| {
                let mapped = crate::terminal_panel::keystroke_to_term_event(
                    &gpui::Keystroke::parse("escape").expect("escape"),
                )
                .expect("mapped escape");
                assert_eq!(
                    mapped,
                    TermKeyEvent {
                        action: TermKeyAction::Press,
                        key: TermKey::Escape,
                        modifiers: TermModifiers::default(),
                        text: None,
                        ..TermKeyEvent::default()
                    }
                );
                view.terminal_open
            }));

            // Resize height and close; closing must preserve both entity and fraction
            // and return focus to RepoView.
            vcx.update(|_, cx| {
                view.update(cx, |view, cx| {
                    view.set_terminal_top_fraction(0.5, 600.0, cx);
                });
            });
            let saved_fraction = view.read_with(&vcx, |view, _| view.terminal_top_fraction);

            vcx.dispatch_action(ToggleTerminal);
            vcx.update(|window, cx| {
                assert!(!view.read(cx).terminal_open);
                assert!(
                    view.read(cx).focus.is_focused(window),
                    "closing the terminal must return focus to RepoView"
                );
            });

            // Reopening reuses the exact same TerminalPanel entity and saved height.
            vcx.dispatch_action(ToggleTerminal);
            vcx.update(|window, cx| {
                let current = view.read(cx).terminal.as_ref().expect("reused panel");
                assert_eq!(current.entity_id(), panel.entity_id());
                assert!((view.read(cx).terminal_top_fraction - saved_fraction).abs() < 1e-6);
                assert!(current.read(cx).focus_handle(cx).is_focused(window));
                view.update(cx, |view, cx| view.reset_terminal_top_fraction(cx));
                assert!(
                    (view.read(cx).terminal_top_fraction - RepoView::DEFAULT_TERMINAL_TOP_FRACTION)
                        .abs()
                        < 1e-6
                );
            });

            // Theme changes update the terminal panel's mapped TerminalTheme.
            let initial_theme = panel.read_with(&vcx, |p, _| p.last_theme().cloned());
            vcx.update(|window, cx| {
                let target_mode = if gpui_component::ActiveTheme::theme(cx).is_dark() {
                    ThemeMode::Light
                } else {
                    ThemeMode::Dark
                };
                gpui_component::Theme::change(target_mode, Some(window), cx);
                panel.update(cx, |p, cx| p.sync_appearance(Some(window), cx));
                window.draw(cx).clear(cx);
            });
            let updated_theme = panel.read_with(&vcx, |p, _| p.last_theme().cloned());
            assert_ne!(
                initial_theme, updated_theme,
                "light/dark theme change must update TerminalTheme"
            );

            // Closing the window must release both RepoView and TerminalPanel
            // without creating reference cycles.
            let released_view = view.downgrade();
            let released_panel = panel.downgrade();
            drop(panel);
            drop(view);
            vcx.update(|window, _| window.remove_window());
            vcx.run_until_parked();
            assert!(released_view.upgrade().is_none(), "RepoView leaked");
            assert!(released_panel.upgrade().is_none(), "TerminalPanel leaked");
        }

        #[gpui::test]
        fn terminal_button_toggles_embedded_panel_and_formats_live_shortcut(
            cx: &mut gpui::TestAppContext,
        ) {
            use std::cell::RefCell;
            use std::rc::Rc;

            use gpui::{Bounds, MouseButton, ParentElement as _, Pixels, Styled as _, point};
            use gpui_component::Root;

            cx.update(prepare_app);
            let repo = ScratchRepo::new();
            let repo_work = repo.work();
            let repo_view = cx.update(|cx| {
                cx.open_window(Default::default(), |window, cx| {
                    cx.new(|cx| RepoView::new(Launch::open_quiet(&repo_work, None), window, cx))
                })
                .expect("repo window")
            });
            let view = repo_view.root(cx).expect("the window's root view");
            let mut rcx = VisualTestContext::from_window(repo_view.into(), cx);
            rcx.update(|window, cx| {
                view.update(cx, |v, _| v.quiet_terminal = true);
                window.draw(cx).clear(cx);
            });
            rcx.run_until_parked();

            // After RepoView has drawn once with its key context, the tooltip
            // reflects the live ToggleTerminal keybinding.
            let bound_tip = rcx.update(|window, cx| view.read(cx).terminal_toggle_tooltip(window));
            assert!(
                bound_tip.starts_with("Toggle terminal panel (") && bound_tip.ends_with(')'),
                "expected bound shortcut in tooltip, got {bound_tip:?}"
            );

            // Clearing bindings falls back to the unbound label.
            rcx.update(|window, cx| {
                cx.clear_key_bindings();
                let unbound_tip = view.read(cx).terminal_toggle_tooltip(window);
                assert_eq!(unbound_tip, "Toggle terminal");
            });

            // Host the title bar's terminal_button directly in a test root to
            // capture its bounds and click it with real pointer events.
            struct ButtonProbe {
                repo: Entity<RepoView>,
                bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
            }
            impl gpui::Render for ButtonProbe {
                fn render(
                    &mut self,
                    window: &mut gpui::Window,
                    cx: &mut gpui::Context<Self>,
                ) -> impl gpui::IntoElement {
                    let bounds_slot = self.bounds.clone();
                    let btn = self
                        .repo
                        .read(cx)
                        .terminal_button(&crate::style::Design::default(), window);
                    gpui::div().size_full().child(
                        gpui::div().flex().child(btn).on_children_prepainted(
                            move |children, _, _| {
                                *bounds_slot.borrow_mut() = children.first().copied();
                            },
                        ),
                    )
                }
            }

            let button_bounds: Rc<RefCell<Option<Bounds<Pixels>>>> = Rc::new(RefCell::new(None));
            let probe_bounds = button_bounds.clone();
            let probe_repo = view.clone();
            let probe_win = rcx.update(|_, cx| {
                cx.open_window(Default::default(), |window, cx| {
                    let probe = cx.new(|_| ButtonProbe {
                        repo: probe_repo,
                        bounds: probe_bounds,
                    });
                    cx.new(|cx| Root::new(probe, window, cx))
                })
                .expect("probe window")
            });
            let mut pcx = VisualTestContext::from_window(probe_win.into(), cx);
            pcx.update(|window, cx| window.draw(cx).clear(cx));

            let bounds = button_bounds
                .borrow()
                .expect("terminal button bounds recorded");
            let center = point(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            );
            assert!(!view.read_with(&pcx, |v, _| v.terminal_open));

            pcx.simulate_mouse_move(center, None, Default::default());
            pcx.simulate_mouse_down(center, MouseButton::Left, Default::default());
            pcx.simulate_mouse_up(center, MouseButton::Left, Default::default());
            pcx.run_until_parked();
            assert!(
                view.read_with(&pcx, |v, _| v.terminal_open),
                "clicking the title bar terminal button must open the embedded terminal"
            );

            pcx.simulate_mouse_down(center, MouseButton::Left, Default::default());
            pcx.simulate_mouse_up(center, MouseButton::Left, Default::default());
            pcx.run_until_parked();
            assert!(
                !view.read_with(&pcx, |v, _| v.terminal_open),
                "clicking the title bar terminal button again must close the embedded terminal"
            );
        }
    }
}
