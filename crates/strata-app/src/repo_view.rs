//! The repository view model and the bridge from session events to GPUI.
//!
//! [`Session`] calls its `on_event` callback on background threads. [`open`]
//! moves those events into an unbounded async channel, and one foreground task
//! drains it, updates this entity and calls `cx.notify()`. No git call ever
//! runs on the UI thread: file lists and diffs are requested through the
//! session, whose own workers do the work.

mod chrome;

use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use futures::StreamExt;
use futures::channel::mpsc::{self, UnboundedReceiver};
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable as _, ScrollStrategy,
    Subscription, Task, UniformListScrollHandle, Window,
};
use gpui_component::Theme;
use gpui_component::input::InputState;
use strata_core::git_cli::CommandRecord;
use strata_core::session::{DiffRequest, DiffTicket, DiffView, Operation, Session, SessionEvent};
use strata_core::{FileChange, HeadInfo, Oid};

use crate::branches::{BranchStart, Branches};
use crate::data::{self, LaneWidth, Source};
use crate::diff_view::{self, Row as DiffRow};
use crate::fake::{self, FakeRepo};
use crate::metrics::FrameMetrics;
use crate::worktree_panel::{MainMode, WorktreePanel};

/// Height of the diff pane below the commit list.
const DIFF_HEIGHT: f32 = 360.0;
/// How many rows a `PageUp`/`PageDown` moves.
const PAGE: isize = 15;
/// The smoke run scrolls this far every frame.
const SMOKE_STEP: usize = 3;
const SMOKE_FRAMES: u64 = 300;

/// What the window should show when it opens.
pub enum Launch {
    /// `--fake [N]`: synthetic commits for the render benchmark.
    Fake { repo: Rc<FakeRepo> },
    /// A real repository, already loading in the background.
    Repo {
        session: Session,
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
    pub fn open(path: &Path) -> Self {
        let (sender, events) = mpsc::unbounded();
        match Session::open(path, move |event| {
            // A closed receiver only means the window is gone; the session is
            // shutting down anyway.
            let _ = sender.unbounded_send(event);
        }) {
            Ok(session) => {
                let name = repo_name(&session);
                let head = session.repo().head().ok();
                let title = data::window_title(&name, head.as_ref());
                Self::Repo {
                    session,
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

pub struct RepoView {
    pub(crate) session: Option<Session>,
    fake: Option<Rc<FakeRepo>>,
    error: Option<(String, String)>,
    name: Option<String>,

    /// Which view fills the area right of the sidebar.
    pub(crate) mode: MainMode,
    /// Staging state and rendering, in its own module.
    pub(crate) worktree: WorktreePanel,
    /// Handle to this view. The sidebar's branch rows get no `Context`, so they
    /// update the view through this entity.
    entity: Entity<RepoView>,
    /// Branch sections, dialogs and the operations they started.
    pub(crate) branches: Branches,

    /// Running maximum of graph widths, updated as chunks arrive.
    lanes: LaneWidth,
    selected: Option<usize>,
    /// Commit whose changed files were requested; guards against reloading the
    /// same selection on every snapshot publish.
    selection_commit: Option<Oid>,
    commit_scroll: UniformListScrollHandle,
    diff_scroll: UniformListScrollHandle,
    file_scroll: UniformListScrollHandle,
    focus: FocusHandle,

    files: Option<Vec<FileChange>>,
    files_epoch: u64,
    files_error: Option<String>,
    selected_file: Option<usize>,

    diff: DiffSlot,
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

    _appearance: Subscription,
    /// Held so the event loop is cancelled with the view.
    _bridge: Option<Task<()>>,
}

impl RepoView {
    pub fn new(launch: Launch, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let appearance = window.observe_window_appearance(|window, cx| {
            Theme::sync_system_appearance(Some(window), cx)
        });
        let smoke = std::env::var_os("STRATA_SMOKE").is_some();
        let title = launch.title();
        let entity = cx.entity();

        let mut view = Self {
            session: None,
            fake: None,
            error: None,
            name: None,
            mode: MainMode::History,
            worktree: WorktreePanel::default(),
            entity,
            branches: Branches::default(),
            lanes: LaneWidth::default(),
            selected: None,
            selection_commit: None,
            commit_scroll: UniformListScrollHandle::default(),
            diff_scroll: UniformListScrollHandle::default(),
            file_scroll: UniformListScrollHandle::default(),
            focus,
            files: None,
            files_epoch: 0,
            files_error: None,
            selected_file: None,
            diff: DiffSlot::default(),
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
            _appearance: appearance,
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
                // Every session event is delivered here, on the foreground
                // executor: the session callback only pushes into the channel.
                view._bridge = Some(cx.spawn(async move |this, cx| {
                    while let Some(event) = events.next().await {
                        if this
                            .update(cx, |this, cx| this.on_event(event, cx))
                            .is_err()
                        {
                            break;
                        }
                    }
                }));
                view.session = Some(session);
            }
            Launch::Error { path, error } => view.error = Some((path, error)),
        }

        view
    }

    /// The view's own focus handle, for panels that hand focus back on close.
    pub(crate) fn view_focus(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn source(&self) -> Option<Source> {
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
                self.sync_worktree(cx);
                cx.notify();
            }
            SessionEvent::DiffReady { ticket, view } => {
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
            SessionEvent::CommandLogged(record) => {
                self.last_command = Some(command_line(&record));
                cx.notify();
            }
            SessionEvent::LoadFailed { error } => {
                self.status_error = Some(error);
                cx.notify();
            }
            SessionEvent::OperationFinished { id, error } => {
                self.worktree.on_operation_finished(id, error.as_deref());
                // The label records which operation failed, so the status bar
                // names it instead of showing a bare git message.
                let had_dialog = self.branches.dialog_open();
                let label = self.branches.finish(id, error.as_deref());
                if let Some(error) = error {
                    self.status_error = Some(match label {
                        Some(label) => format!("{label}: {}", one_line(&error)),
                        None => one_line(&error),
                    });
                }
                if !had_dialog && self.branches.dialog_open() {
                    // The force-delete offer opens here rather than in a click
                    // handler, so it has to take focus for its keys to work.
                    self.focus_dialog(cx);
                }
                cx.notify();
            }
            // Progress and query results get their UI with remote operations and search.
            SessionEvent::OperationStarted { .. }
            | SessionEvent::OperationProgress { .. }
            | SessionEvent::QueryReady { .. }
            | SessionEvent::QueryFailed { .. } => {}
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

    /// Runs a mutation through the session.
    ///
    /// The branch UI is disabled while an operation runs, and `--fake` has no
    /// session at all, so both cases return without doing anything. `label` is
    /// what a failure is reported as in the status bar.
    pub(crate) fn run_operation(
        &mut self,
        operation: Operation,
        label: impl Into<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session.is_none() || self.is_busy() {
            return;
        }
        let Some(session) = &self.session else {
            return;
        };
        let label = label.into();
        let delete = match &operation {
            Operation::DeleteBranch { name, force } => Some((name.clone(), *force)),
            _ => None,
        };
        let id = session.run(operation);
        self.branches
            .start(id, label, delete, window.viewport_size());
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

    pub(crate) fn open_delete_dialog(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session.is_none() {
            return;
        }
        self.branches.set_viewport(window.viewport_size());
        self.branches.open_delete(name);
        if let Some(focus) = self.branches.dialog_focus() {
            window.defer(cx, move |window, cx| focus.focus(window, cx));
        }
        cx.notify();
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
            return;
        }
        if let Some((name, force)) = self.branches.delete_request() {
            let label = format!("delete branch {name}");
            self.branches.close_dialog();
            self.focus_root(window, cx);
            self.run_operation(Operation::DeleteBranch { name, force }, label, window, cx);
        }
    }

    /// Returns focus from a dialog to the commit list, which owns the keys.
    pub(crate) fn focus_root(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus, cx);
    }

    /// Moves focus to the open dialog's card, if one is open.
    fn focus_dialog(&self, cx: &mut Context<Self>) {
        let Some(focus) = self.branches.dialog_focus() else {
            return;
        };
        // The event arrives off the window's own update, but the window still
        // has to be reachable through the app.
        let Some(handle) = cx.windows().into_iter().next() else {
            return;
        };
        let _ = handle.update(cx, |_, window, cx| focus.focus(window, cx));
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

    fn select(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        let Some(source) = self.source() else {
            return;
        };
        let index = index.filter(|index| *index < source.len());
        let commit = index.and_then(|index| source.commit_id(index));
        if index == self.selected && commit == self.selection_commit {
            return;
        }

        self.selected = index;
        self.selection_commit = commit;
        self.selected_file = None;
        self.files = None;
        self.files_error = None;
        self.diff_scroll = UniformListScrollHandle::default();
        self.cancel_diff();

        // The smoke run scrolls as fast as it can; loading a file list and a
        // diff for every row would measure git instead of the renderer.
        if let Some(commit) = commit
            && !self.smoke
        {
            self.load_files(commit, cx);
        }
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
        let ticket = session.request_diff(DiffRequest::Commit { commit, path });
        if let Some(previous) = self.diff.begin(ticket) {
            session.cancel_diff(previous);
        }
        cx.notify();
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

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.commit_count();
        if len == 0 {
            return;
        }
        let current = self.selected.unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        self.jump(next, cx);
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
