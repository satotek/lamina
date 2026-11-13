//! The repository view model and the bridge from session events to GPUI.
//!
//! [`Session`] calls its `on_event` callback on background threads. [`open`]
//! moves those events into an unbounded async channel, and one foreground task
//! drains it, updates this entity and calls `cx.notify()`. No git call ever
//! runs on the UI thread: file lists and diffs are requested through the
//! session, whose own workers do the work.

mod blame_view;
mod chrome;
mod commit_list;
mod common;
mod details;
mod diff_pane;
mod edit_dialog;
mod history_ops;
mod in_progress;
mod rebase_dialog;
mod rebase_plan;
mod sidebar;
mod status_bar;
mod title_bar;
mod tree_view;
mod worktrees;

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use futures::StreamExt;
use futures::channel::mpsc::{self, UnboundedReceiver};
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable as _, ScrollStrategy,
    Subscription, Task, UniformListScrollHandle, WeakEntity, Window,
};
use gpui_component::input::InputState;
use strata_core::git_cli::CommandRecord;
use strata_core::repo::{SubmoduleInfo, WorktreeInfo};
use strata_core::session::{
    DiffOptions, DiffRequest, DiffTicket, DiffView, Operation, QueryTicket, Session, SessionEvent,
    SessionOptions,
};
use strata_core::{FileChange, HeadInfo, Oid, RefKind};

use crate::branches::{BranchStart, Branches};
use crate::data::{self, LaneWidth, Source};
use crate::debug_launch::{self, DebugDialog, DebugLaunch, SnapshotRevMatch, TreeTarget};
use crate::diff_view::{self, Row as DiffRow};
use crate::fake::{self, FakeRepo};
use crate::file_history::FileHistory;
use crate::metrics::FrameMetrics;
use crate::remote::Remote;
use crate::search::SearchState;
use crate::worktree_panel::{MainMode, WorktreePanel};

use in_progress::{FinishedOperation, InProgress};

/// Height of the diff pane below the commit list.
const DIFF_HEIGHT: f32 = 360.0;
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

    pub fn title(&self) -> String {
        match self {
            Launch::Fake { .. } => "Strata — synthetic (main)".into(),
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
    /// Submodules and linked worktrees, which are not part of a snapshot and so
    /// are re-read after every one.
    extra_lists: ExtraLists,
    /// Fetch/pull/push state, progress and the dialogs that decide them.
    pub(crate) remote: Remote,
    /// Commit search over the loaded history, with its text field.
    pub(crate) search: SearchState,
    /// The file-history view while it is open.
    pub(crate) file_history: Option<FileHistory>,
    /// The job in flight and the banner for what git left in progress.
    in_progress: InProgress,

    /// Running maximum of graph widths, updated as chunks arrive.
    lanes: LaneWidth,
    pub(crate) selected: Option<usize>,
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
    focus: FocusHandle,

    files: Option<Vec<FileChange>>,
    files_epoch: u64,
    files_error: Option<String>,
    selected_file: Option<usize>,

    /// Commit-wide added/removed counts; the details pane keeps the selected
    /// file's counts until they arrive.
    stat: details::CommitStatSlot,
    /// The selected commit's file tree while it replaces the diff pane.
    tree: tree_view::TreeBrowser,
    /// The blame pane while it replaces the diff pane.
    blame: blame_view::BlameView,

    diff: DiffSlot,
    /// The options `diff` was requested with, so a settings change that does
    /// not touch them does not re-request the same diff.
    diff_options: DiffOptions,
    diff_rows: Rc<Vec<DiffRow>>,
    diff_error: Option<String>,
    /// Pre-flattened synthetic diffs so `--fake` does not rebuild one per frame.
    fake_diffs: Rc<Vec<Rc<Vec<DiffRow>>>>,

    last_command: Option<String>,
    status_error: Option<String>,

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

fn drop_in_background<T: Send + 'static>(value: T) {
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
    pub fn new(launch: Launch, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let appearance = window
            .observe_window_appearance(|window, cx| crate::settings::apply_theme(Some(window), cx));
        let settings = cx.observe_global::<crate::settings::Settings>(|this, cx| {
            this.settings_changed(cx);
            cx.notify();
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
            mode: MainMode::History,
            worktree: WorktreePanel::default(),
            entity,
            branches: Branches::default(),
            extra_lists: ExtraLists::default(),
            remote: Remote::default(),
            search: SearchState::default(),
            file_history: None,
            in_progress: InProgress::default(),
            lanes: LaneWidth::default(),
            selected: None,
            selection_commit: None,
            edit_selection: None,
            revealed_commit: None,
            commit_scroll: UniformListScrollHandle::default(),
            diff_scroll: UniformListScrollHandle::default(),
            file_scroll: UniformListScrollHandle::default(),
            focus,
            files: None,
            files_epoch: 0,
            files_error: None,
            selected_file: None,
            stat: details::CommitStatSlot::default(),
            tree: tree_view::TreeBrowser::default(),
            blame: blame_view::BlameView::default(),
            diff: DiffSlot::default(),
            diff_options,
            diff_rows: Rc::new(Vec::new()),
            diff_error: None,
            fake_diffs: Rc::new(Vec::new()),
            last_command: None,
            status_error: None,
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
        };
        view.branches.set_dialog_focus(cx.focus_handle());

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
                        if this
                            .update(cx, |this, cx| this.on_event(event, cx))
                            .is_err()
                        {
                            break;
                        }
                        if is_snapshot {
                            let _ = window_handle.update(cx, |_, window, cx| {
                                let _ = this.update(cx, |this, cx| {
                                    this.step_debug_launch(window, cx);
                                });
                            });
                        }
                    }
                }));
                view.session = Some(*session);
                view.search.init_input(window, cx);
                crate::autofetch::start(window.window_handle(), cx);
            }
            Launch::Error { path, error } => view.error = Some((path, error)),
        }

        view
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

    /// Takes the session and stops forwarding its events to this view.
    ///
    /// The workspace calls this when it replaces the view: a late event from
    /// the old session must not reach the window any more, where its
    /// notification or dialog would land over the new repository.
    pub(crate) fn detach_session(&mut self) -> Option<Session> {
        self.cancel_file_history();
        self._bridge.take();
        self.session.take()
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
                self.sync_selection(cx);
                self.settle_edit_selection(cx);
                self.sync_worktree(cx);
                self.search_on_snapshot(cx);
                self.file_history_on_snapshot(cx);
                // The submodule and worktree lists sit outside the snapshot, so
                // every publish — including a watcher refresh — re-reads them.
                self.refresh_extra_lists();
                self.refresh_in_progress_details();
                cx.notify();
            }
            SessionEvent::DiffReady { ticket, view } => {
                if self.file_history_diff_ready(ticket, &view, cx) {
                    return;
                }
                if self.worktree.on_diff_ready(ticket, &view) {
                    cx.notify();
                    return;
                }
                if !self.diff.accept(ticket, Arc::clone(&view)) {
                    return;
                }
                // Flattening copies the line text once per version instead of
                // rebuilding it on every frame.
                self.diff_rows = Rc::new(diff_view::flatten(&view.diff));
                self.diff_error = None;
                cx.notify();
            }
            SessionEvent::DiffFailed { ticket, error } => {
                if self.file_history_diff_failed(ticket, &error, cx) {
                    return;
                }
                if self.worktree.on_diff_failed(ticket, &error) {
                    cx.notify();
                    return;
                }
                if !self.diff.fail(ticket) {
                    return;
                }
                self.diff_rows = Rc::new(Vec::new());
                self.diff_error = Some(error);
                cx.notify();
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
                {
                    self.blame_failed(ticket, &error, cx);
                }
            }
            SessionEvent::CommitStatReady { ticket, stat } => {
                if self.stat.accept(ticket, stat) {
                    cx.notify();
                }
            }
            SessionEvent::PathHistoryBatch { ticket, entries } => {
                self.file_history_batch(ticket, &entries, cx);
            }
            SessionEvent::PathHistoryDone { ticket } => {
                self.file_history_done(ticket, cx);
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
                    cx.notify();
                }
            }
            SessionEvent::InProgressDetailsReady { ticket, details } => {
                self.on_in_progress_details_ready(ticket, details, cx);
            }
            SessionEvent::CommandLogged(record) => {
                self.last_command = Some(command_line(&record));
                cx.notify();
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
                cx.notify();
            }
            SessionEvent::OperationFinished { id, error } => {
                // The session refreshed the status before it reported the end,
                // so this snapshot already has the conflicts the command left.
                let conflicts = self
                    .session
                    .as_ref()
                    .map_or(0, |session| session.snapshot().conflicts.len());
                if self.in_progress.finished(id, conflicts) {
                    self.reveal_conflicts(cx);
                }
                self.worktree.on_operation_finished(id, error.as_deref());
                self.remote.on_finished(id, error.as_deref(), cx);
                // The label records which operation failed, so the status bar
                // names it instead of showing a bare git message.
                let label = self.branches.finish(id, error.as_deref());
                if let Some(error) = error {
                    // A refused or failed edit left the selection where it was.
                    self.edit_selection = None;
                    self.status_error = Some(match label {
                        Some(label) => format!("{label}: {}", one_line(&error)),
                        None => one_line(&error),
                    });
                }
                if let Some(retry) = self.branches.take_force_retry() {
                    // The offer opens from an operation result, which has no
                    // window of its own, so it is routed through the active one.
                    crate::branches::offer_force(cx.entity(), retry, cx);
                }
                cx.notify();
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
                self.remote.on_started(id, cx)
            }
            SessionEvent::OperationProgress { id, text, percent } => {
                self.remote.on_progress(id, &text, percent, cx)
            }
        }
    }

    /// True while the session is running an operation or one this view started
    /// has not reported back yet.
    fn is_busy(&self) -> bool {
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
    ) {
        if self.session.is_none() || self.is_busy() {
            return;
        }
        let Some(session) = &self.session else {
            return;
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
        let kind = FinishedOperation::of(&operation);
        let id = session.run(operation);
        self.in_progress.submitted(id, kind);
        self.branches.start(id, label, retry);
        cx.notify();
    }

    /// The create-branch shortcut: starts from HEAD.
    pub(crate) fn create_branch_from_head(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_none() || self.is_busy() || self.branches.dialog_open() {
            return;
        }
        let label = match self.source().and_then(|source| source.head().cloned()) {
            Some(HeadInfo::Branch { short_name, .. }) => format!("from {short_name}"),
            Some(HeadInfo::Detached { .. }) => "from the current commit".to_owned(),
            None => "from HEAD".to_owned(),
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
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("branch name"));
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
                format!("Delete branch {name}?"),
                "Its commits stay in the repository until they are garbage collected.",
            )
            .ok_label("Delete"),
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

    /// Confirms whichever branch dialog is open.
    pub(crate) fn submit_branch_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(request) = self.branches.create_request(cx) {
            match request {
                Ok(request) => {
                    let label = format!("create branch {}", request.name);
                    self.branches.close_dialog();
                    self.focus_root(window, cx);
                    self.run_operation(
                        Operation::CreateBranch {
                            name: request.name,
                            start_point: request.start_point,
                            checkout: request.checkout,
                        },
                        label,
                        window,
                        cx,
                    );
                }
                Err(message) => {
                    self.branches.set_create_error(message);
                    cx.notify();
                }
            }
        }
    }

    /// Returns focus from a dialog to the commit list, which owns the keys.
    pub(crate) fn focus_root(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus, cx);
    }

    /// Keeps the selection valid as commits arrive or the history is replaced.
    fn sync_selection(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.source() else {
            return;
        };
        if source.is_empty() {
            return;
        }
        match self.selected {
            None => self.select(Some(0), cx),
            Some(index) if index >= source.len() => self.select(Some(source.len() - 1), cx),
            Some(index) => {
                // A reload can put a different commit at the same index.
                let commit = source.commit_id(index);
                if commit.is_some() && commit != self.selection_commit {
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
        if index == self.selected && commit == self.selection_commit {
            return;
        }

        // Selecting a commit is the user moving off the sidebar ref they had
        // revealed; the reveal tint belongs to that jump only.
        self.branches.clear_revealed();
        self.selected = index;
        self.selection_commit = commit;
        // The reveal mark belongs to the ref the sidebar showed, not to
        // whatever the user selected next.
        if self.revealed_commit != commit {
            self.revealed_commit = None;
        }
        self.selected_file = None;
        self.files = None;
        self.files_error = None;
        self.diff_scroll = UniformListScrollHandle::default();
        // The tree shows one commit's contents, so another selection leaves it.
        self.tree.close();
        self.cancel_diff();

        // The smoke run scrolls as fast as it can; loading a file list and a
        // diff for every row would measure git instead of the renderer.
        if let Some(commit) = commit
            && !self.smoke
        {
            self.load_files(commit, cx);
        }
        self.load_commit_stat(commit, cx);
        cx.notify();
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
            let changes = cx
                .background_executor()
                .spawn(async move { repo.commit_changes(commit) })
                .await;
            this.update(cx, |this, cx| {
                if this.files_epoch != epoch {
                    return;
                }
                match changes {
                    Ok(files) => {
                        let empty = files.is_empty();
                        this.files = Some(files);
                        this.files_error = None;
                        if empty {
                            cx.notify();
                        } else {
                            // The first file is what the diff pane shows by
                            // default, so it is requested as soon as the list
                            // arrives.
                            this.select_file(0, cx);
                        }
                    }
                    Err(error) => {
                        this.files_error = Some(error.to_string());
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn select_file(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(path) = self
            .files
            .as_ref()
            .and_then(|files| files.get(index))
            .map(|change| change.path.clone())
        else {
            return;
        };
        let (Some(selected), Some(session)) = (self.selected, self.session.as_ref()) else {
            return;
        };
        let Some(source) = self.source() else {
            return;
        };
        let Some(commit) = source.commit_id(selected) else {
            return;
        };

        self.selected_file = Some(index);
        self.diff_error = None;
        self.diff_rows = Rc::new(Vec::new());
        let options = history_diff_options(cx);
        let ticket = session.request_diff_with(DiffRequest::Commit { commit, path }, options);
        self.diff_options = options;
        if let Some(previous) = self.diff.begin(ticket) {
            session.cancel_diff(previous);
        }
        cx.notify();
    }

    /// Re-requests the history diff when the options it was computed with
    /// changed, so a new context size or ignore-whitespace shows at once.
    fn settings_changed(&mut self, cx: &mut Context<Self>) {
        let options = history_diff_options(cx);
        if options == self.diff_options {
            return;
        }
        self.diff_options = options;
        if self.file_history.is_some() {
            self.request_history_diff(cx);
        } else if let Some(index) = self.selected_file {
            self.select_file(index, cx);
        }
    }

    fn cancel_diff(&mut self) {
        self.diff_rows = Rc::new(Vec::new());
        self.diff_error = None;
        let ticket = self.diff.cancel();
        if let (Some(session), Some(ticket)) = (self.session.as_ref(), ticket) {
            session.cancel_diff(ticket);
        }
    }

    fn toggle_frame_stats(&mut self, cx: &mut Context<Self>) {
        self.overlay = !self.overlay;
        cx.notify();
    }

    fn select_last(&mut self, cx: &mut Context<Self>) {
        let last = self.commit_count().saturating_sub(1);
        self.jump(last, cx);
    }

    pub(crate) fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.commit_count();
        if len == 0 {
            return;
        }
        let current = self.selected.unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        self.jump(next, cx);
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

    fn jump(&mut self, index: usize, cx: &mut Context<Self>) {
        self.commit_scroll
            .scroll_to_item(index, ScrollStrategy::Center);
        self.select(Some(index), cx);
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
            "Strata — synthetic (main)".to_owned()
        } else if let Some(name) = &self.name {
            let head: Option<HeadInfo> = self.source().and_then(|source| source.head().cloned());
            data::window_title(name, head.as_ref())
        } else {
            "Strata".to_owned()
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
                let handle = window.window_handle();
                window.defer(cx, move |_, cx| {
                    let _ = handle.update(cx, |_, window, cx| match dialog {
                        DebugDialog::OpenRepository => {
                            window.dispatch_action(Box::new(crate::actions::OpenRepository), cx);
                            eprintln!("debug-launch: applied --dialog open-repository");
                        }
                        _ => debug_launch::dispatch_window_dialog(dialog, window, cx),
                    });
                });
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

        let select_ready = if let Some(rev) = runner.launch.select.clone() {
            runner.resolve_rev(
                &rev,
                RevSlot::Select,
                &snapshot,
                session.repo().clone(),
                window,
                cx,
            )
        } else {
            true
        };
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

        if !select_ready || !tree_ready {
            self.debug_launch = Some(runner);
            return;
        }

        let launch = runner.launch;
        if let Some(rev) = &launch.select {
            match runner.select_oid {
                Some(Ok(oid)) => match snapshot.commit_index.get(&oid).copied() {
                    Some(index) => {
                        self.jump(index, cx);
                        eprintln!(
                            "debug-launch: applied --select {rev} -> {} (row {index})",
                            data::short_oid(oid)
                        );
                    }
                    None => eprintln!(
                        "debug-launch: error: --select '{rev}' resolved to {}, which is not in the loaded history",
                        data::short_oid(oid)
                    ),
                },
                Some(Err(err)) => {
                    eprintln!("debug-launch: error: --select '{rev}' failed: {err}");
                }
                None => {}
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
                        self.open_tree(window, cx);
                        match target {
                            TreeTarget::Directory { dir: None, .. } => {
                                eprintln!(
                                    "debug-launch: applied --tree {rev} -> {}",
                                    data::short_oid(oid)
                                );
                            }
                            TreeTarget::Directory { dir: Some(dir), .. } => {
                                eprintln!(
                                    "debug-launch: applied --tree {rev} -> {} (opened root; subdirectory '{dir}' is not yet exposed by tree_view)",
                                    data::short_oid(oid)
                                );
                            }
                            TreeTarget::File { path, .. } => {
                                eprintln!(
                                    "debug-launch: applied --tree-file {rev} -> {} (opened root; opening '{path}' directly is not yet exposed by tree_view)",
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
            RevSlot::Select => {
                runner.select_resolving = false;
                runner.select_oid = Some(resolved);
            }
            RevSlot::Tree => {
                runner.tree_resolving = false;
                runner.tree_oid = Some(resolved);
            }
        }
        self.step_debug_launch(window, cx);
    }
}

#[derive(Clone, Copy)]
enum RevSlot {
    Select,
    Tree,
}

struct DebugLaunchRunner {
    launch: DebugLaunch,
    select_oid: Option<Result<Oid, String>>,
    select_resolving: bool,
    tree_oid: Option<Result<Oid, String>>,
    tree_resolving: bool,
}

impl DebugLaunchRunner {
    fn new(launch: DebugLaunch) -> Self {
        Self {
            launch,
            select_oid: None,
            select_resolving: false,
            tree_oid: None,
            tree_resolving: false,
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
            RevSlot::Select => (&mut self.select_oid, &mut self.select_resolving),
            RevSlot::Tree => (&mut self.tree_oid, &mut self.tree_resolving),
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
    fn paths_render_relative_to_the_current_directory() {
        let cwd = std::env::current_dir().expect("current dir");
        let nested = cwd.join("crates/strata-app");
        assert_eq!(display_path(&nested), "crates/strata-app");

        let elsewhere = PathBuf::from("/definitely/not/here");
        assert_eq!(display_path(&elsewhere), "/definitely/not/here");
    }
}
