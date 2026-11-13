//! The working-tree view: stage, unstage, discard and commit.
//!
//! Everything here talks to [`Session`]: mutations are [`Operation`]s on its
//! worker and diffs are [`DiffRequest`]s. The panel renders from the latest
//! [`RepoSnapshot`] and never calls git itself.

use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, FocusHandle, Hsla, InteractiveElement,
    IntoElement, KeyBinding, ParentElement, SharedString, StatefulInteractiveElement, Styled,
    Subscription, UniformListScrollHandle, Window, actions, div, px, uniform_list,
};
use gpui_component::input::{InputEvent, Textarea, TextareaState};
use gpui_component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Theme};
use strata_core::diff::{FileDiff, LineKind};
use strata_core::highlight::LineHighlights;
use strata_core::patch::LineSelection;
use strata_core::session::{
    DiffRequest, DiffTicket, DiffView, Operation, OperationId, RepoSnapshot, Session,
};
use strata_core::{ChangeKind, FileChange, HeadInfo, RepoState, WorktreeStatus};

use crate::diff_view::{self, DiffColors, Row as DiffRow, RowOptions};
use crate::repo_view::RepoView;

/// Key context of the file lists and the diff picker.
///
/// The commit box is deliberately outside it: the input owns its own key
/// context, and a panel-wide `s`/`u` binding must not fire while the user writes
/// a message.
pub const WORKTREE_PANEL: &str = "WorktreePanel";

actions!(
    strata_worktree,
    [
        StageSelected,
        UnstageSelected,
        ToggleSelected,
        CommitMessage,
        ShowHistory,
    ]
);

// `KeyBinding::new` parses without an OS, so the platform modifier is chosen at
// compile time instead of using `secondary-`.
#[cfg(target_os = "macos")]
const COMMIT_KEY: &str = "cmd-enter";
#[cfg(not(target_os = "macos"))]
const COMMIT_KEY: &str = "ctrl-enter";

pub fn bind(cx: &mut App) {
    let scope = Some(WORKTREE_PANEL);
    cx.bind_keys([
        KeyBinding::new("s", StageSelected, scope),
        KeyBinding::new("u", UnstageSelected, scope),
        KeyBinding::new("enter", ToggleSelected, scope),
        KeyBinding::new(COMMIT_KEY, CommitMessage, scope),
        KeyBinding::new("escape", ShowHistory, scope),
    ]);
}

/// Which side of the index a row belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Unstaged,
    Staged,
}

impl Side {
    fn other(self) -> Self {
        match self {
            Side::Unstaged => Side::Staged,
            Side::Staged => Side::Unstaged,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Side::Unstaged => "unstaged",
            Side::Staged => "staged",
        }
    }
}

/// The selected row, identified by path so a status refresh that reorders the
/// list keeps the selection.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Selected {
    side: Side,
    path: String,
}

/// What the main area shows below the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MainMode {
    #[default]
    History,
    Worktree,
}

/// One row of the flattened diff, plus where it points in the [`FileDiff`].
struct PanelRow {
    row: DiffRow,
    position: RowPosition,
}

enum RowPosition {
    Hunk(usize),
    /// `None` for a context line, which cannot be staged on its own.
    Line(Option<(usize, usize)>),
}

impl PanelRow {
    fn pickable(&self) -> Option<(usize, usize)> {
        match self.position {
            RowPosition::Line(key) => key,
            RowPosition::Hunk(_) => None,
        }
    }
}

/// The same "only the latest ticket wins" slot as the history diff, so a stale
/// worker cannot replace the diff after the user moved on.
#[derive(Default)]
struct DiffSlot {
    ticket: Option<DiffTicket>,
    view: Option<Arc<DiffView>>,
}

impl DiffSlot {
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

    fn fail(&mut self, ticket: DiffTicket) -> bool {
        self.is_current(ticket)
    }

    fn cancel(&mut self) -> Option<DiffTicket> {
        self.view = None;
        self.ticket.take()
    }

    fn view(&self) -> Option<&Arc<DiffView>> {
        self.view.as_ref()
    }

    /// A request has been made and no view or failure has arrived yet.
    fn is_pending(&self) -> bool {
        self.ticket.is_some() && self.view.is_none()
    }
}

/// Why an operation was submitted, for the notice after it finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    Stage,
    Unstage,
    Discard,
    Delete,
    Commit,
}

impl PendingKind {
    fn notice(self) -> &'static str {
        match self {
            PendingKind::Stage => "Staged",
            PendingKind::Unstage => "Unstaged",
            PendingKind::Discard => "Changes discarded",
            PendingKind::Delete => "Untracked file deleted",
            PendingKind::Commit => "Committed",
        }
    }
}

/// A destructive action waiting for confirmation.
struct Confirm {
    title: String,
    detail: String,
    operation: Operation,
    kind: PendingKind,
}

/// State of the working-tree view. Rendering reads it, [`RepoView`] updates it
/// from session events and user input.
#[derive(Default)]
pub struct WorktreePanel {
    selected: Option<Selected>,
    diff: DiffSlot,
    rows: Rc<Vec<PanelRow>>,
    diff_error: Option<String>,
    /// Changed lines picked for a line-level operation.
    picked: BTreeSet<(usize, usize)>,
    /// Flattened row the last click landed on, for shift-click ranges.
    anchor: Option<usize>,

    message_input: Option<Entity<TextareaState>>,
    _message_events: Option<Subscription>,
    amend: bool,
    /// Message to restore when amend is switched off again.
    draft: String,
    clear_message: bool,
    /// Text to put into the box on the next render if it is still empty.
    prefill: Option<String>,
    /// Whether the current merge's prepared message was already offered, so
    /// clearing the box does not bring it back.
    merge_prefilled: bool,

    pending: Option<(OperationId, PendingKind)>,
    /// Set while a submitted operation has not refreshed the status yet.
    dirty: bool,
    confirm: Option<Confirm>,
    error: Option<String>,
    notice: Option<String>,

    focus: Option<FocusHandle>,
    unstaged_scroll: UniformListScrollHandle,
    staged_scroll: UniformListScrollHandle,
    diff_scroll: UniformListScrollHandle,
}

impl WorktreePanel {
    fn ensure_focus(&mut self, cx: &mut Context<RepoView>) -> FocusHandle {
        self.focus.get_or_insert_with(|| cx.focus_handle()).clone()
    }

    fn ensure_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<RepoView>,
    ) -> Entity<TextareaState> {
        if let Some(input) = &self.message_input {
            return input.clone();
        }
        let input = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx);
            state.set_placeholder("Summary\n\nOptional description", window, cx);
            state
        });
        self._message_events = Some(cx.subscribe(
            &input,
            |this, _input, event: &InputEvent, cx| match event {
                InputEvent::PressEnter {
                    secondary: true, ..
                } => this.commit_worktree(cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            },
        ));
        self.message_input = Some(input.clone());
        input
    }

    fn message(&self, cx: &App) -> String {
        self.message_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default()
    }

    /// Clears the box after a successful commit and keeps the input entity
    /// alive across renders.
    fn flush_input(&mut self, window: &mut Window, cx: &mut Context<RepoView>) {
        let input = self.ensure_input(window, cx);
        if std::mem::take(&mut self.clear_message) {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
        if let Some(prefill) = self.prefill.take()
            && input.read(cx).value().trim().is_empty()
        {
            input.update(cx, |state, cx| state.set_value(prefill, window, cx));
        }
    }

    /// Offers git's prepared message once per merge, the way `git commit`
    /// opens the editor with it.
    fn sync_merge_message(&mut self, session: &Session, state: RepoState) {
        if state != RepoState::Merging {
            self.merge_prefilled = false;
            return;
        }
        if self.merge_prefilled {
            return;
        }
        self.merge_prefilled = true;
        // One small file read; cheaper than a round trip through a worker.
        if let Ok(Some(message)) = session.repo().merge_message() {
            self.prefill = Some(message);
        }
    }

    fn select(&mut self, side: Side, path: String) {
        if self
            .selected
            .as_ref()
            .is_some_and(|selected| selected.side == side && selected.path == path)
        {
            return;
        }
        self.selected = Some(Selected { side, path });
        self.rows = Rc::new(Vec::new());
        self.diff_error = None;
        self.picked.clear();
        self.anchor = None;
    }

    /// Keeps the selection valid against a new status. Returns true when the
    /// user-visible file changed, which needs a new diff.
    fn reconcile(&mut self, status: &WorktreeStatus) -> bool {
        if let Some(selected) = self.selected.clone() {
            if contains(status, selected.side, &selected.path) {
                return false;
            }
            // A staged file leaves the unstaged list (and vice versa); follow it
            // to the other side instead of jumping to an unrelated file.
            let other = selected.side.other();
            if contains(status, other, &selected.path) {
                self.select(other, selected.path);
                return true;
            }
        }

        let fallback = status
            .unstaged
            .first()
            .map(|change| (Side::Unstaged, change.path.clone()))
            .or_else(|| {
                status
                    .staged
                    .first()
                    .map(|change| (Side::Staged, change.path.clone()))
            });
        match fallback {
            Some((side, path)) => {
                self.select(side, path);
                true
            }
            None => {
                let had_selection = self.selected.take().is_some();
                if had_selection {
                    self.rows = Rc::new(Vec::new());
                    self.picked.clear();
                    self.anchor = None;
                }
                had_selection
            }
        }
    }

    pub(crate) fn on_diff_ready(&mut self, ticket: DiffTicket, view: &Arc<DiffView>) -> bool {
        if !self.diff.accept(ticket, Arc::clone(view)) {
            return false;
        }
        self.rows = Rc::new(panel_rows(&view.diff));
        self.diff_error = None;
        self.picked.clear();
        self.anchor = None;
        self.dirty = false;
        true
    }

    pub(crate) fn on_diff_failed(&mut self, ticket: DiffTicket, error: &str) -> bool {
        if !self.diff.fail(ticket) {
            return false;
        }
        self.diff.ticket = None;
        self.rows = Rc::new(Vec::new());
        self.diff_error = Some(error.to_owned());
        true
    }

    pub(crate) fn on_operation_finished(&mut self, id: OperationId, error: Option<&str>) -> bool {
        let Some((pending_id, kind)) = self.pending else {
            return false;
        };
        if pending_id != id {
            return false;
        }
        self.pending = None;
        match error {
            Some(error) => self.error = Some(error.to_owned()),
            None => {
                self.error = None;
                self.notice = Some(kind.notice().to_owned());
                if kind == PendingKind::Commit {
                    self.clear_message = true;
                    self.amend = false;
                }
            }
        }
        true
    }

    fn run(&mut self, session: &Session, kind: PendingKind, operation: Operation) {
        let id = session.run(operation);
        self.pending = Some((id, kind));
        self.dirty = true;
        self.error = None;
        self.notice = None;
    }

    fn begin_confirm(
        &mut self,
        title: impl Into<String>,
        detail: impl Into<String>,
        kind: PendingKind,
        operation: Operation,
    ) {
        self.confirm = Some(Confirm {
            title: title.into(),
            detail: detail.into(),
            operation,
            kind,
        });
    }

    fn set_amend(
        &mut self,
        amend: bool,
        previous: Option<String>,
        window: &mut Window,
        cx: &mut Context<RepoView>,
    ) {
        if amend == self.amend {
            return;
        }
        let input = self.ensure_input(window, cx);
        if amend {
            self.draft = input.read(cx).value().to_string();
            if let Some(previous) = previous {
                input.update(cx, |state, cx| state.set_value(previous, window, cx));
            }
        } else {
            let draft = std::mem::take(&mut self.draft);
            input.update(cx, |state, cx| state.set_value(draft, window, cx));
        }
        self.amend = amend;
        self.notice = None;
        cx.notify();
    }
}

// ---------------------------------------------------------------------------
// RepoView glue: everything that needs the session lives here, so `repo_view`
// only adds the fields and forwards events.
// ---------------------------------------------------------------------------

impl RepoView {
    pub(crate) fn open_worktree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = MainMode::Worktree;
        let focus = self.worktree.ensure_focus(cx);
        window.focus(&focus, cx);

        if let Some(session) = self.session.as_ref() {
            let snapshot = session.snapshot();
            self.worktree.reconcile(&snapshot.status);
            self.worktree.sync_merge_message(session, snapshot.state);
        }
        self.worktree.ensure_input(window, cx);
        self.request_worktree_diff(cx);
        cx.notify();
    }

    pub(crate) fn close_worktree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = MainMode::History;
        if let Some(session) = self.session.as_ref()
            && let Some(ticket) = self.worktree.diff.cancel()
        {
            session.cancel_diff(ticket);
        }
        self.worktree.rows = Rc::new(Vec::new());
        self.worktree.picked.clear();
        let focus = self.view_focus();
        window.focus(&focus, cx);
        cx.notify();
    }

    pub(crate) fn sync_worktree(&mut self, cx: &mut Context<Self>) {
        if self.mode != MainMode::Worktree {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let snapshot = session.snapshot();
        // The status refresh of a running operation is published before the
        // operation clears `busy`; waiting for the final snapshot avoids a diff
        // of the pre-operation state.
        if snapshot.busy.is_some() {
            return;
        }
        self.worktree.sync_merge_message(session, snapshot.state);
        let moved = self.worktree.reconcile(&snapshot.status);
        let needs_view = self.worktree.selected.is_some()
            && self.worktree.diff.view().is_none()
            && !self.worktree.diff.is_pending()
            && self.worktree.diff_error.is_none();
        if moved || self.worktree.dirty || needs_view {
            self.request_worktree_diff(cx);
        }
    }

    pub(crate) fn request_worktree_diff(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let Some(selected) = self.worktree.selected.clone() else {
            // Nothing is selected, so there is no diff to show; an older
            // ticket must not land and draw a file the lists no longer have.
            self.worktree.rows = Rc::new(Vec::new());
            self.worktree.picked.clear();
            if let Some(ticket) = self.worktree.diff.cancel() {
                session.cancel_diff(ticket);
            }
            self.worktree.dirty = false;
            cx.notify();
            return;
        };
        let request = match selected.side {
            Side::Staged => DiffRequest::Staged {
                path: selected.path,
            },
            Side::Unstaged => DiffRequest::Unstaged {
                path: selected.path,
            },
        };
        let ticket = session.request_diff(request);
        if let Some(previous) = self.worktree.diff.begin(ticket) {
            session.cancel_diff(previous);
        }
        self.worktree.rows = Rc::new(Vec::new());
        self.worktree.diff_error = None;
        self.worktree.picked.clear();
        self.worktree.anchor = None;
        self.worktree.dirty = false;
        cx.notify();
    }

    pub(crate) fn select_worktree_file(
        &mut self,
        side: Side,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = self.worktree.ensure_focus(cx);
        window.focus(&focus, cx);
        if self
            .worktree
            .selected
            .as_ref()
            .is_some_and(|selected| selected.side == side && selected.path == path)
        {
            cx.notify();
            return;
        }
        self.worktree.select(side, path);
        self.request_worktree_diff(cx);
    }

    pub(crate) fn toggle_worktree_selection(&mut self, cx: &mut Context<Self>) {
        let Some(selected) = self.worktree.selected.clone() else {
            return;
        };
        let Some(session) = self.session.as_ref() else {
            return;
        };
        match selected.side {
            Side::Unstaged => self.worktree.run(
                session,
                PendingKind::Stage,
                Operation::StagePaths(vec![selected.path]),
            ),
            Side::Staged => self.worktree.run(
                session,
                PendingKind::Unstage,
                Operation::UnstagePaths(vec![selected.path]),
            ),
        }
        cx.notify();
    }

    pub(crate) fn stage_selected(&mut self, cx: &mut Context<Self>) {
        let Some(selected) = self.worktree.selected.clone() else {
            return;
        };
        if selected.side != Side::Unstaged {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        self.worktree.run(
            session,
            PendingKind::Stage,
            Operation::StagePaths(vec![selected.path]),
        );
        cx.notify();
    }

    pub(crate) fn unstage_selected(&mut self, cx: &mut Context<Self>) {
        let Some(selected) = self.worktree.selected.clone() else {
            return;
        };
        if selected.side != Side::Staged {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        self.worktree.run(
            session,
            PendingKind::Unstage,
            Operation::UnstagePaths(vec![selected.path]),
        );
        cx.notify();
    }

    pub(crate) fn stage_worktree_path(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        self.worktree.run(
            session,
            PendingKind::Stage,
            Operation::StagePaths(vec![path]),
        );
        cx.notify();
    }

    pub(crate) fn unstage_worktree_path(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        self.worktree.run(
            session,
            PendingKind::Unstage,
            Operation::UnstagePaths(vec![path]),
        );
        cx.notify();
    }

    pub(crate) fn discard_worktree_path(
        &mut self,
        path: String,
        untracked: bool,
        cx: &mut Context<Self>,
    ) {
        if untracked {
            self.worktree.begin_confirm(
                format!("Delete {path}?"),
                "The untracked file is removed from disk.",
                PendingKind::Delete,
                Operation::DeleteUntracked(vec![path]),
            );
        } else {
            self.worktree.begin_confirm(
                format!("Discard changes to {path}?"),
                "The working-tree changes are lost; this cannot be undone.",
                PendingKind::Discard,
                Operation::DiscardPaths(vec![path]),
            );
        }
        cx.notify();
    }

    pub(crate) fn stage_all_worktree(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let snapshot = session.snapshot();
        let paths: Vec<String> = snapshot
            .status
            .unstaged
            .iter()
            .map(|change| change.path.clone())
            .collect();
        if paths.is_empty() {
            return;
        }
        self.worktree
            .run(session, PendingKind::Stage, Operation::StagePaths(paths));
        cx.notify();
    }

    pub(crate) fn unstage_all_worktree(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let snapshot = session.snapshot();
        let paths: Vec<String> = snapshot
            .status
            .staged
            .iter()
            .map(|change| change.path.clone())
            .collect();
        if paths.is_empty() {
            return;
        }
        self.worktree.run(
            session,
            PendingKind::Unstage,
            Operation::UnstagePaths(paths),
        );
        cx.notify();
    }

    pub(crate) fn hunk_operation(
        &mut self,
        hunk: usize,
        action: HunkAction,
        cx: &mut Context<Self>,
    ) {
        let Some(selected) = self.worktree.selected.clone() else {
            return;
        };
        let Some(diff) = self.worktree.diff.view().map(|view| view.diff.clone()) else {
            return;
        };
        let selection = LineSelection::all_of_hunk(&diff, hunk);
        if selection.is_empty() {
            return;
        }
        self.line_operation_for(selected, selection, action, cx);
    }

    pub(crate) fn line_operation(&mut self, action: LineAction, cx: &mut Context<Self>) {
        let Some(selected) = self.worktree.selected.clone() else {
            return;
        };
        let selection = picked_selection(&self.worktree.picked);
        if selection.is_empty() {
            return;
        }
        self.line_operation_for(selected, selection, action.into(), cx);
    }

    fn line_operation_for(
        &mut self,
        selected: Selected,
        selection: LineSelection,
        action: HunkAction,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let path = selected.path.clone();
        match (selected.side, action) {
            (Side::Unstaged, HunkAction::Stage) => self.worktree.run(
                session,
                PendingKind::Stage,
                Operation::StageLines { path, selection },
            ),
            (Side::Staged, HunkAction::Unstage) => self.worktree.run(
                session,
                PendingKind::Unstage,
                Operation::UnstageLines { path, selection },
            ),
            (Side::Unstaged, HunkAction::Discard) => self.worktree.begin_confirm(
                "Discard selected lines?",
                "The selected changes are removed from the working tree.",
                PendingKind::Discard,
                Operation::DiscardLines { path, selection },
            ),
            _ => return,
        }
        cx.notify();
    }

    pub(crate) fn click_worktree_line(&mut self, row: usize, shift: bool, cx: &mut Context<Self>) {
        let Some(key) = self.worktree.rows.get(row).and_then(PanelRow::pickable) else {
            return;
        };
        if shift && let Some(anchor) = self.worktree.anchor {
            let range = picked_in_range(&self.worktree.rows, anchor, row);
            self.worktree.picked.extend(range);
        } else {
            if !self.worktree.picked.insert(key) {
                self.worktree.picked.remove(&key);
            }
            self.worktree.anchor = Some(row);
        }
        cx.notify();
    }

    pub(crate) fn toggle_amend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let previous = self.session.as_ref().and_then(previous_message);
        let amend = !self.worktree.amend;
        self.worktree.set_amend(amend, previous, window, cx);
    }

    pub(crate) fn commit_worktree(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let snapshot = session.snapshot();
        let message = trim_message(&self.worktree.message(cx));
        let staged = snapshot.status.staged.len();
        if !commit_enabled(
            staged,
            self.worktree.amend,
            &message,
            snapshot.busy.is_some(),
        ) {
            return;
        }
        self.worktree.run(
            session,
            PendingKind::Commit,
            Operation::Commit {
                message,
                amend: self.worktree.amend,
            },
        );
        cx.notify();
    }

    pub(crate) fn confirm_worktree(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.worktree.confirm.take() else {
            return;
        };
        let Some(session) = self.session.as_ref() else {
            return;
        };
        self.worktree.run(session, confirm.kind, confirm.operation);
        cx.notify();
    }

    pub(crate) fn cancel_worktree_confirm(&mut self, cx: &mut Context<Self>) {
        self.worktree.confirm = None;
        cx.notify();
    }
}

/// What a hunk header offers on one side of the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HunkAction {
    Stage,
    Unstage,
    Discard,
}

/// Line-level action from the diff toolbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineAction {
    Stage,
    Unstage,
    Discard,
}

impl From<LineAction> for HunkAction {
    fn from(action: LineAction) -> Self {
        match action {
            LineAction::Stage => HunkAction::Stage,
            LineAction::Unstage => HunkAction::Unstage,
            LineAction::Discard => HunkAction::Discard,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested below).
// ---------------------------------------------------------------------------

fn contains(status: &WorktreeStatus, side: Side, path: &str) -> bool {
    entries(status, side)
        .iter()
        .any(|change| change.path == path)
}

fn entries(status: &WorktreeStatus, side: Side) -> &[FileChange] {
    match side {
        Side::Unstaged => &status.unstaged,
        Side::Staged => &status.staged,
    }
}

/// Flattens a diff, keeping the hunk/line coordinates the patch builder needs.
fn panel_rows(diff: &FileDiff) -> Vec<PanelRow> {
    let mut rows = Vec::with_capacity(diff.hunks.iter().map(|hunk| hunk.lines.len() + 1).sum());
    for (hunk_index, hunk) in diff.hunks.iter().enumerate() {
        rows.push(PanelRow {
            row: DiffRow::HunkHeader {
                old_start: hunk.old_start,
                old_len: hunk.old_len,
                new_start: hunk.new_start,
                new_len: hunk.new_len,
            },
            position: RowPosition::Hunk(hunk_index),
        });
        for (line_index, line) in hunk.lines.iter().enumerate() {
            rows.push(PanelRow {
                row: DiffRow::Line(line.clone()),
                position: RowPosition::Line(
                    (line.kind != LineKind::Context).then_some((hunk_index, line_index)),
                ),
            });
        }
    }
    rows
}

fn picked_selection(picked: &BTreeSet<(usize, usize)>) -> LineSelection {
    LineSelection::new(picked.iter().copied())
}

/// Changed lines between two flattened rows, inclusive. Context lines and hunk
/// headers inside the range are skipped.
fn picked_in_range(rows: &[PanelRow], first: usize, last: usize) -> BTreeSet<(usize, usize)> {
    let (low, high) = if first <= last {
        (first, last)
    } else {
        (last, first)
    };
    rows.iter()
        .enumerate()
        .filter(|(index, _)| (low..=high).contains(index))
        .filter_map(|(_, row)| row.pickable())
        .collect()
}

/// Commit button rule: a message and something to commit, unless amending.
fn commit_enabled(staged: usize, amend: bool, message: &str, busy: bool) -> bool {
    !busy && !message.trim().is_empty() && (amend || staged > 0)
}

fn trim_message(message: &str) -> String {
    message.trim().to_owned()
}

/// First line length in characters, for the 72-column hint.
fn summary_len(message: &str) -> usize {
    message.lines().next().unwrap_or_default().chars().count()
}

fn summary_hint(message: &str) -> Option<String> {
    let len = summary_len(message);
    (len > 72).then(|| format!("summary is {len} characters (guideline: 72)"))
}

/// HEAD's full message, used to prefill the amend box.
///
/// A single object read through gix, so it stays on the UI thread instead of
/// adding a session round trip for every amend toggle.
fn previous_message(session: &Session) -> Option<String> {
    let head = match session.snapshot().head.as_ref()? {
        HeadInfo::Branch {
            target: Some(target),
            ..
        } => *target,
        HeadInfo::Detached { target } => *target,
        HeadInfo::Branch { target: None, .. } => return None,
    };
    session.repo().commit_message(head).ok()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Colors {
    background: Hsla,
    foreground: Hsla,
    muted: Hsla,
    muted_foreground: Hsla,
    border: Hsla,
    selection: Hsla,
    title_bar: Hsla,
    accent: Hsla,
    accent_foreground: Hsla,
    danger: Hsla,
    warning: Hsla,
    success: Hsla,
    info: Hsla,
    mono: SharedString,
}

impl Colors {
    fn from_theme(theme: &Theme) -> Self {
        Self {
            background: theme.background,
            foreground: theme.foreground,
            muted: theme.muted,
            muted_foreground: theme.muted_foreground,
            border: theme.border,
            selection: theme.list_active,
            title_bar: theme.title_bar,
            accent: theme.primary,
            accent_foreground: theme.primary_foreground,
            danger: theme.danger,
            warning: theme.warning,
            success: theme.success,
            info: theme.info,
            mono: theme.mono_font_family.clone(),
        }
    }
}

const LIST_WIDTH: f32 = 300.0;
/// Height of one file row, matching the badge/list padding.
const ROW_HEIGHT: f32 = 20.0;
/// A list stops growing here and scrolls instead.
const MAX_LIST_ROWS: usize = 8;

fn file_row(
    entity: &Entity<RepoView>,
    side: Side,
    index: usize,
    path: String,
    kind: ChangeKind,
    selected: Option<&Selected>,
    colors: &Colors,
) -> AnyElement {
    let is_selected =
        selected.is_some_and(|selected| selected.side == side && selected.path == path);

    let mut row = div()
        .flex()
        .items_center()
        .gap_2()
        .h(px(20.0))
        .w_full()
        .px_2()
        .text_size(px(11.0))
        .child(change_badge(kind, colors))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(if is_selected {
                    colors.accent
                } else {
                    colors.foreground
                })
                .child(path.clone()),
        );
    if is_selected {
        row = row.bg(colors.selection);
    }

    let click_entity = entity.clone();
    let click_path = path.clone();
    let untracked = kind == ChangeKind::Untracked;
    let menu_entity = entity.clone();
    let menu_path = path;

    row.id(gpui::ElementId::named_usize(
        format!("worktree-file-{}", side as u8),
        index,
    ))
    .cursor_pointer()
    .on_click(move |event, window, cx| {
        let path = click_path.clone();
        click_entity.update(cx, |this, cx| {
            this.select_worktree_file(side, path.clone(), window, cx);
            if event.click_count() >= 2 {
                this.toggle_worktree_selection(cx);
            }
        });
    })
    .context_menu(move |menu, _window, _cx| {
        let mut menu = menu;
        match side {
            Side::Unstaged => {
                let stage_entity = menu_entity.clone();
                let stage_path = menu_path.clone();
                menu = menu.item(PopupMenuItem::new("Stage").on_click(move |_, _window, cx| {
                    let path = stage_path.clone();
                    stage_entity.update(cx, |this, cx| this.stage_worktree_path(path.clone(), cx));
                }));

                let discard_entity = menu_entity.clone();
                let discard_path = menu_path.clone();
                let label = if untracked {
                    "Delete file…"
                } else {
                    "Discard changes…"
                };
                menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _window, cx| {
                    let path = discard_path.clone();
                    discard_entity.update(cx, |this, cx| {
                        this.discard_worktree_path(path.clone(), untracked, cx)
                    });
                }));
            }
            Side::Staged => {
                let unstage_entity = menu_entity.clone();
                let unstage_path = menu_path.clone();
                menu = menu.item(
                    PopupMenuItem::new("Unstage").on_click(move |_, _window, cx| {
                        let path = unstage_path.clone();
                        unstage_entity
                            .update(cx, |this, cx| this.unstage_worktree_path(path.clone(), cx));
                    }),
                );
            }
        }
        menu
    })
    .into_any_element()
}

fn hunk_header_row(
    entity: &Entity<RepoView>,
    row: usize,
    position: &RowPosition,
    side: Option<Side>,
    (old_start, old_len, new_start, new_len): (u32, u32, u32, u32),
    diff_colors: &DiffColors,
    colors: &Colors,
) -> AnyElement {
    let RowPosition::Hunk(hunk) = *position else {
        return div().into_any_element();
    };

    let buttons = match side {
        Some(Side::Unstaged) => {
            let stage_entity = entity.clone();
            let stage_colors = colors.clone();
            let discard_entity = entity.clone();
            let discard_colors = colors.clone();
            let discard_hunk = hunk;
            div()
                .flex()
                .items_center()
                .gap_1()
                .pr_1()
                .child(chip_button(
                    ("hunk-stage", row),
                    "Stage hunk",
                    true,
                    &stage_colors,
                    move |_, _, cx| {
                        stage_entity.update(cx, |this, cx| {
                            this.hunk_operation(hunk, HunkAction::Stage, cx)
                        });
                    },
                ))
                .child(chip_button(
                    ("hunk-discard", row),
                    "Discard hunk",
                    true,
                    &discard_colors,
                    move |_, _, cx| {
                        discard_entity.update(cx, |this, cx| {
                            this.hunk_operation(discard_hunk, HunkAction::Discard, cx)
                        });
                    },
                ))
        }
        Some(Side::Staged) => {
            let unstage_entity = entity.clone();
            let unstage_colors = colors.clone();
            div()
                .flex()
                .items_center()
                .gap_1()
                .pr_1()
                .child(chip_button(
                    ("hunk-unstage", row),
                    "Unstage hunk",
                    true,
                    &unstage_colors,
                    move |_, _, cx| {
                        unstage_entity.update(cx, |this, cx| {
                            this.hunk_operation(hunk, HunkAction::Unstage, cx)
                        });
                    },
                ))
        }
        None => div(),
    };

    div()
        .flex()
        .items_center()
        .h(px(diff_view::ROW_HEIGHT))
        .w_full()
        .bg(diff_colors.header)
        .border_t_1()
        .border_color(diff_colors.border)
        .text_size(px(11.0))
        .text_color(diff_colors.muted)
        .child(div().flex_1().min_w_0().pl_2().truncate().child(format!(
            "@@ -{old_start},{old_len} +{new_start},{new_len} @@"
        )))
        .child(buttons)
        .into_any_element()
}

fn chip_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    enabled: bool,
    colors: &Colors,
    handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let mut button = div()
        .id(id)
        .flex_none()
        .px_1()
        .py_0()
        .rounded_sm()
        .border_1()
        .border_color(colors.border)
        .text_size(px(9.5))
        .text_color(if enabled {
            colors.foreground
        } else {
            colors.muted_foreground
        })
        .bg(colors.muted.opacity(0.5));
    if enabled {
        button = button.cursor_pointer().on_click(handler);
    }
    button.child(label.into()).into_any_element()
}

fn primary_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    enabled: bool,
    colors: &Colors,
    handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let mut button = div()
        .id(id)
        .flex_none()
        .px_3()
        .py_1()
        .rounded_sm()
        .text_size(px(11.0))
        .text_color(if enabled {
            colors.accent_foreground
        } else {
            colors.muted_foreground
        })
        .bg(if enabled { colors.accent } else { colors.muted });
    if enabled {
        button = button.cursor_pointer().on_click(handler);
    }
    button.child(label.into()).into_any_element()
}

fn confirm_overlay(entity: Entity<RepoView>, confirm: &Confirm, colors: &Colors) -> AnyElement {
    let cancel_entity = entity.clone();
    let cancel_colors = colors.clone();
    let accept_entity = entity;
    let accept_colors = colors.clone();
    let label = match confirm.kind {
        PendingKind::Delete => "Delete",
        _ => "Discard",
    };
    let label: SharedString = label.into();

    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(colors.background.opacity(0.55))
        .occlude()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .w(px(420.0))
                .p_4()
                .rounded_md()
                .bg(colors.title_bar)
                .border_1()
                .border_color(colors.border)
                .text_size(px(12.0))
                .text_color(colors.foreground)
                .child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(confirm.title.clone()),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(colors.muted_foreground)
                        .child(confirm.detail.clone()),
                )
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(chip_button(
                            "confirm-cancel",
                            "Cancel",
                            true,
                            &cancel_colors,
                            move |_, _, cx| {
                                cancel_entity
                                    .update(cx, |this, cx| this.cancel_worktree_confirm(cx));
                            },
                        ))
                        .child(primary_button(
                            "confirm-accept",
                            label,
                            true,
                            &accept_colors,
                            move |_, _, cx| {
                                accept_entity.update(cx, |this, cx| this.confirm_worktree(cx));
                            },
                        )),
                ),
        )
        .into_any_element()
}

fn change_badge(kind: ChangeKind, colors: &Colors) -> AnyElement {
    let (marker, color) = match kind {
        ChangeKind::Added | ChangeKind::Untracked => ("A", colors.success),
        ChangeKind::Modified => ("M", colors.warning),
        ChangeKind::Deleted => ("D", colors.danger),
        ChangeKind::Renamed => ("R", colors.accent),
        ChangeKind::Copied => ("C", colors.info),
        ChangeKind::TypeChanged => ("T", colors.warning),
        ChangeKind::Conflicted => ("!", colors.danger),
    };
    div()
        .w(px(14.0))
        .flex_none()
        .text_size(px(10.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(color)
        .child(marker)
        .into_any_element()
}

/// `--fake` has no session, but the view still opens; it explains why it is
/// empty and keeps a way back to the synthetic history.
fn synthetic_state(
    entity: Entity<RepoView>,
    panel: &WorktreePanel,
    cx: &mut Context<RepoView>,
    colors: &Colors,
) -> AnyElement {
    let back = {
        let entity = entity.clone();
        let colors = colors.clone();
        chip_button(
            "show-history-synthetic",
            "← History",
            true,
            &colors,
            move |_, window, cx| {
                entity.update(cx, |this, cx| this.close_worktree(window, cx));
            },
        )
    };
    let mut root = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .key_context(WORKTREE_PANEL)
        .on_action(
            cx.listener(|this, _: &ShowHistory, window, cx| this.close_worktree(window, cx)),
        );
    if let Some(focus) = &panel.focus {
        root = root.track_focus(focus);
    }
    root.child(
        div()
            .flex()
            .items_center()
            .h(px(30.0))
            .flex_none()
            .px_3()
            .bg(colors.title_bar)
            .border_b_1()
            .border_color(colors.border)
            .child(back),
    )
    .child(empty_state("No working tree in synthetic mode", colors))
    .into_any_element()
}

fn empty_state(text: &str, colors: &Colors) -> AnyElement {
    div()
        .flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .text_size(px(12.0))
        .text_color(colors.muted_foreground)
        .bg(colors.background)
        .child(text.to_owned())
        .into_any_element()
}

fn message_pane(text: &str, color: Hsla, colors: &Colors) -> AnyElement {
    div()
        .flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .p_4()
        .text_size(px(12.0))
        .text_color(color)
        .bg(colors.background)
        .child(text.to_owned())
        .into_any_element()
}

/// The highlights for the side of the line that is drawn, as in the history
/// diff: context lines exist on both sides, so the new side is preferred.
fn line_highlights<'a>(row: &DiffRow, view: &'a DiffView) -> Option<&'a LineHighlights> {
    let DiffRow::Line(line) = row else {
        return None;
    };
    let (highlights, number) = match line.kind {
        LineKind::Removed => (&view.old_highlights, line.old_lineno),
        LineKind::Added => (&view.new_highlights, line.new_lineno),
        LineKind::Context => (&view.new_highlights, line.new_lineno.or(line.old_lineno)),
    };
    highlights.get(number?.checked_sub(1)? as usize)
}

impl RepoView {
    pub(crate) fn worktree_view(
        &mut self,
        entity: Entity<Self>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.worktree.flush_input(window, cx);
        self.worktree.ensure_focus(cx);

        let colors = Colors::from_theme(cx.theme());
        let Some(session) = self.session.as_ref() else {
            return synthetic_state(entity, &self.worktree, cx, &colors);
        };
        let snapshot = session.snapshot();
        self.worktree.render(entity, &snapshot, cx)
    }
}

impl WorktreePanel {
    fn render(
        &self,
        entity: Entity<RepoView>,
        snapshot: &RepoSnapshot,
        cx: &mut Context<RepoView>,
    ) -> AnyElement {
        let colors = Colors::from_theme(cx.theme());
        let mut body = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .key_context(WORKTREE_PANEL)
            .on_action(cx.listener(|this, _: &StageSelected, _, cx| this.stage_selected(cx)))
            .on_action(cx.listener(|this, _: &UnstageSelected, _, cx| this.unstage_selected(cx)))
            .on_action(
                cx.listener(|this, _: &ToggleSelected, _, cx| this.toggle_worktree_selection(cx)),
            )
            .on_action(cx.listener(|this, _: &CommitMessage, _, cx| this.commit_worktree(cx)))
            .on_action(
                cx.listener(|this, _: &ShowHistory, window, cx| this.close_worktree(window, cx)),
            )
            .child(self.lists(entity.clone(), snapshot, &colors))
            .child(self.diff_pane(entity.clone(), cx.theme(), &colors));
        if let Some(focus) = &self.focus {
            body = body.track_focus(focus);
        }

        let mut root = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .bg(colors.background);

        if snapshot.state == RepoState::Merging {
            root = root.child(
                div()
                    .flex()
                    .items_center()
                    .flex_none()
                    .px_3()
                    .py_1()
                    .bg(colors.warning.opacity(0.18))
                    .text_size(px(11.0))
                    .text_color(colors.warning)
                    .child("Merge in progress — resolve conflicts, then commit."),
            );
        }

        root = root
            .child(body)
            .child(self.commit_box(entity.clone(), snapshot, cx, &colors));

        if let Some(confirm) = &self.confirm {
            root = root.child(confirm_overlay(entity, confirm, &colors));
        }

        root.into_any_element()
    }

    fn lists(
        &self,
        entity: Entity<RepoView>,
        snapshot: &RepoSnapshot,
        colors: &Colors,
    ) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .w(px(LIST_WIDTH))
            .flex_none()
            .min_h_0()
            .border_r_1()
            .border_color(colors.border)
            .child(self.file_section(
                entity.clone(),
                Side::Unstaged,
                &snapshot.status.unstaged,
                colors,
            ))
            .child(self.file_section(entity, Side::Staged, &snapshot.status.staged, colors))
            .into_any_element()
    }

    fn file_section(
        &self,
        entity: Entity<RepoView>,
        side: Side,
        files: &[FileChange],
        colors: &Colors,
    ) -> AnyElement {
        let title = match side {
            Side::Unstaged => "UNSTAGED",
            Side::Staged => "STAGED",
        };
        let button = match side {
            Side::Unstaged => {
                let entity = entity.clone();
                let colors = colors.clone();
                chip_button(
                    gpui::ElementId::named_usize("stage-all", side as usize),
                    "Stage all",
                    !files.is_empty(),
                    &colors,
                    move |_, _, cx| {
                        entity.update(cx, |this, cx| this.stage_all_worktree(cx));
                    },
                )
            }
            Side::Staged => {
                let entity = entity.clone();
                let colors = colors.clone();
                chip_button(
                    gpui::ElementId::named_usize("unstage-all", side as usize),
                    "Unstage all",
                    !files.is_empty(),
                    &colors,
                    move |_, _, cx| {
                        entity.update(cx, |this, cx| this.unstage_all_worktree(cx));
                    },
                )
            }
        };

        let mut section = div().flex().flex_col().flex_none().min_h_0().child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .px_3()
                .pt_2()
                .pb_1()
                .child(
                    div()
                        .text_size(px(10.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(colors.muted_foreground)
                        .child(format!("{title} ({})", files.len())),
                )
                .child(button),
        );

        if files.is_empty() {
            return section
                .child(
                    div()
                        .px_3()
                        .h(px(22.0))
                        .text_size(px(11.0))
                        .text_color(colors.muted_foreground)
                        .child("none"),
                )
                .into_any_element();
        }

        let entries: Rc<Vec<(String, ChangeKind)>> = Rc::new(
            files
                .iter()
                .map(|change| (change.path.clone(), change.kind))
                .collect(),
        );
        let selected = self.selected.clone();
        let list_colors = colors.clone();
        let list_id = match side {
            Side::Unstaged => "unstaged-files",
            Side::Staged => "staged-files",
        };
        let scroll = match side {
            Side::Unstaged => self.unstaged_scroll.clone(),
            Side::Staged => self.staged_scroll.clone(),
        };

        // Lists stack from the top and only scroll once they get long; giving
        // each an equal share of the column left the staged header floating in
        // the middle of an empty pane.
        let visible_rows = entries.len().min(MAX_LIST_ROWS);
        let list = uniform_list(list_id, entries.len(), move |range, _window, _cx| {
            range
                .map(|index| {
                    let (path, kind) = entries[index].clone();
                    file_row(
                        &entity,
                        side,
                        index,
                        path,
                        kind,
                        selected.as_ref(),
                        &list_colors,
                    )
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&scroll)
        .h(px(ROW_HEIGHT * visible_rows as f32))
        .flex_none();

        section = section.child(list);
        section.into_any_element()
    }

    fn diff_pane(&self, entity: Entity<RepoView>, theme: &Theme, colors: &Colors) -> AnyElement {
        let selected = self.selected.clone();
        let (path, side) = selected
            .as_ref()
            .map(|selected| (selected.path.clone(), Some(selected.side)))
            .unwrap_or_else(|| ("no file selected".to_owned(), None));

        let mut header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .h(px(30.0))
            .flex_none()
            .px_3()
            .bg(colors.title_bar)
            .border_b_1()
            .border_color(colors.border)
            .text_size(px(12.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .child(div().truncate().child(path))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(colors.muted_foreground)
                            .child(side.map(Side::label).unwrap_or("")),
                    ),
            );

        if let (Some(selected), Some(_)) = (&selected, self.diff.view()) {
            header = header.child(self.line_actions(entity.clone(), selected, colors));
        }

        let body = match (&self.diff_error, self.diff.view()) {
            (Some(error), _) => message_pane(error, colors.danger, colors),
            (None, None) => message_pane("computing diff…", colors.muted_foreground, colors),
            (None, Some(view)) if view.diff.binary => {
                message_pane("Binary file", colors.muted_foreground, colors)
            }
            (None, Some(_)) if self.rows.is_empty() => {
                message_pane("No textual changes", colors.muted_foreground, colors)
            }
            (None, Some(view)) => self.diff_list(entity, view, theme, colors),
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn line_actions(
        &self,
        entity: Entity<RepoView>,
        selected: &Selected,
        colors: &Colors,
    ) -> AnyElement {
        let count = self.picked.len();
        if count == 0 {
            return div()
                .flex_none()
                .text_size(px(10.0))
                .text_color(colors.muted_foreground)
                .child("click lines to pick")
                .into_any_element();
        }

        let mut actions = div().flex().items_center().gap_1();
        let (label, action) = match selected.side {
            Side::Unstaged => (format!("Stage {count} line(s)"), LineAction::Stage),
            Side::Staged => (format!("Unstage {count} line(s)"), LineAction::Unstage),
        };
        let colors_for_button = colors.clone();
        let main_entity = entity.clone();
        actions = actions.child(chip_button(
            "line-action-main",
            label,
            true,
            &colors_for_button,
            move |_, _, cx| {
                main_entity.update(cx, |this, cx| this.line_operation(action, cx));
            },
        ));
        if selected.side == Side::Unstaged {
            let entity = entity.clone();
            let colors_for_button = colors.clone();
            actions = actions.child(chip_button(
                "line-action-discard",
                "Discard",
                true,
                &colors_for_button,
                move |_, _, cx| {
                    entity.update(cx, |this, cx| this.line_operation(LineAction::Discard, cx));
                },
            ));
        }
        actions.into_any_element()
    }

    fn diff_list(
        &self,
        entity: Entity<RepoView>,
        view: &Arc<DiffView>,
        theme: &Theme,
        colors: &Colors,
    ) -> AnyElement {
        let rows = Rc::clone(&self.rows);
        let picked = self.picked.clone();
        let diff_colors = DiffColors::from_theme(theme);
        let mono = colors.mono.clone();
        let view = Arc::clone(view);
        let side = self.selected.as_ref().map(|selected| selected.side);
        let list_colors = colors.clone();

        uniform_list("worktree-diff", rows.len(), move |range, _window, _cx| {
            range
                .map(|index| {
                    let row = &rows[index];
                    let highlights = line_highlights(&row.row, &view);
                    match &row.row {
                        DiffRow::HunkHeader {
                            old_start,
                            old_len,
                            new_start,
                            new_len,
                        } => hunk_header_row(
                            &entity,
                            index,
                            &row.position,
                            side,
                            (*old_start, *old_len, *new_start, *new_len),
                            &diff_colors,
                            &list_colors,
                        ),
                        DiffRow::Line(_) => {
                            let pick = row.pickable().map(|key| picked.contains(&key));
                            let entity = entity.clone();
                            div()
                                .id(("worktree-line", index))
                                .w_full()
                                .cursor_pointer()
                                .on_click(move |event, _window, cx| {
                                    entity.update(cx, |this, cx| {
                                        this.click_worktree_line(index, event.modifiers().shift, cx)
                                    });
                                })
                                .child(diff_view::render_row_with(
                                    &row.row,
                                    &diff_colors,
                                    &mono,
                                    highlights,
                                    RowOptions { pick },
                                ))
                                .into_any_element()
                        }
                    }
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.diff_scroll)
        .flex_1()
        .min_h_0()
        .into_any_element()
    }

    fn commit_box(
        &self,
        entity: Entity<RepoView>,
        snapshot: &RepoSnapshot,
        cx: &mut Context<RepoView>,
        colors: &Colors,
    ) -> AnyElement {
        let Some(input) = self.message_input.clone() else {
            return div().into_any_element();
        };
        let message = self.message(cx);
        let staged = snapshot.status.staged.len();
        let enabled = commit_enabled(
            staged,
            self.amend,
            &message,
            snapshot.busy.is_some() || self.pending.is_some(),
        );
        let hint = summary_hint(&message);

        let amend_box = {
            let entity = entity.clone();
            let colors = colors.clone();
            let checked = self.amend;
            div()
                .id("amend-checkbox")
                .flex()
                .items_center()
                .gap_1()
                .flex_none()
                .cursor_pointer()
                .text_size(px(11.0))
                .text_color(colors.foreground)
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.toggle_amend(window, cx));
                })
                .child(
                    div()
                        .w(px(12.0))
                        .h(px(12.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_sm()
                        .border_1()
                        .border_color(colors.border)
                        .text_size(px(9.0))
                        .text_color(colors.accent)
                        .child(if checked { "✓" } else { "" }),
                )
                .child("Amend")
        };

        let commit_button = {
            let entity = entity.clone();
            let colors = colors.clone();
            primary_button(
                "commit-button",
                "Commit",
                enabled,
                &colors,
                move |_, _, cx| {
                    entity.update(cx, |this, cx| this.commit_worktree(cx));
                },
            )
        };
        let history_button = {
            let entity = entity.clone();
            let colors = colors.clone();
            chip_button(
                "show-history",
                "← History",
                true,
                &colors,
                move |_, window, cx| {
                    entity.update(cx, |this, cx| this.close_worktree(window, cx));
                },
            )
        };

        let status = match (&self.error, &self.notice) {
            (Some(error), _) => div()
                .text_size(px(10.0))
                .text_color(colors.danger)
                .child(error.clone())
                .into_any_element(),
            (None, Some(notice)) => div()
                .text_size(px(10.0))
                .text_color(colors.success)
                .child(notice.clone())
                .into_any_element(),
            (None, None) => div()
                .text_size(px(10.0))
                .text_color(colors.muted_foreground)
                .child(if snapshot.busy.is_some() {
                    "working…".to_owned()
                } else {
                    String::new()
                })
                .into_any_element(),
        };

        div()
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .gap_1()
            .p_2()
            .bg(colors.title_bar)
            .border_t_1()
            .border_color(colors.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(history_button)
                    .child(
                        div()
                            .text_size(px(10.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(colors.muted_foreground)
                            .child("COMMIT MESSAGE"),
                    )
                    .child(amend_box)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(10.0))
                            .text_color(colors.warning)
                            .child(hint.unwrap_or_default()),
                    )
                    .child(commit_button),
            )
            .child(
                Textarea::new(&input)
                    .h(px(78.0))
                    .w_full()
                    .text_size(px(12.0)),
            )
            .child(status)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_core::diff::{DiffLine, Hunk};

    fn line(kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str) -> DiffLine {
        DiffLine {
            kind,
            old_lineno: old,
            new_lineno: new,
            content: text.to_owned(),
            no_newline_at_eof: false,
            emphasis: Vec::new(),
        }
    }

    fn sample_diff() -> FileDiff {
        FileDiff {
            hunks: vec![
                Hunk {
                    old_start: 1,
                    old_len: 2,
                    new_start: 1,
                    new_len: 2,
                    lines: vec![
                        line(LineKind::Context, Some(1), Some(1), "unchanged"),
                        line(LineKind::Removed, Some(2), None, "old"),
                        line(LineKind::Added, None, Some(2), "new"),
                    ],
                },
                Hunk {
                    old_start: 10,
                    old_len: 1,
                    new_start: 10,
                    new_len: 2,
                    lines: vec![
                        line(LineKind::Added, None, Some(10), "added"),
                        line(LineKind::Context, Some(10), Some(11), "tail"),
                    ],
                },
            ],
            binary: false,
        }
    }

    #[test]
    fn rows_map_back_to_hunk_and_line_indices() {
        let rows = panel_rows(&sample_diff());
        assert_eq!(rows.len(), 7, "two headers plus five lines");
        assert!(matches!(rows[0].position, RowPosition::Hunk(0)));
        assert_eq!(rows[1].pickable(), None, "context lines cannot be picked");
        assert_eq!(rows[2].pickable(), Some((0, 1)));
        assert_eq!(rows[3].pickable(), Some((0, 2)));
        assert!(matches!(rows[4].position, RowPosition::Hunk(1)));
        assert_eq!(rows[5].pickable(), Some((1, 0)));
        assert_eq!(rows[6].pickable(), None);
    }

    #[test]
    fn a_range_picks_only_changed_lines_and_handles_both_directions() {
        let rows = panel_rows(&sample_diff());

        let forward = picked_in_range(&rows, 0, 6);
        assert_eq!(forward, BTreeSet::from([(0, 1), (0, 2), (1, 0)]));

        let backward = picked_in_range(&rows, 6, 0);
        assert_eq!(backward, forward, "dragging up picks the same lines");

        let inside_one_hunk = picked_in_range(&rows, 4, 6);
        assert_eq!(inside_one_hunk, BTreeSet::from([(1, 0)]));
        assert!(
            picked_in_range(&rows, 0, 1).is_empty(),
            "header and context only"
        );
    }

    #[test]
    fn picked_lines_become_the_patch_selection() {
        let selection = picked_selection(&BTreeSet::from([(1, 2), (0, 4)]));
        let mut lines: Vec<(usize, usize)> = selection.iter().collect();
        lines.sort_unstable();
        assert_eq!(lines, [(0, 4), (1, 2)]);
        assert!(!selection.is_empty());
    }

    #[test]
    fn commit_needs_a_message_and_staged_work_unless_amending() {
        assert!(commit_enabled(1, false, "subject", false));
        assert!(!commit_enabled(1, false, "subject", true), "busy session");
        assert!(!commit_enabled(1, false, "  \n\t", false), "blank message");
        assert!(
            !commit_enabled(0, false, "subject", false),
            "nothing staged"
        );
        assert!(
            commit_enabled(0, true, "subject", false),
            "amend can rewrite a message without staged content"
        );
    }

    #[test]
    fn messages_are_trimmed_at_both_ends() {
        assert_eq!(trim_message("  subject\n\nbody  \n\n"), "subject\n\nbody");
        assert_eq!(trim_message("\n\n"), "");
    }

    #[test]
    fn long_summaries_get_a_hint() {
        assert_eq!(summary_hint("short summary"), None);
        let summary = "s".repeat(73);
        assert_eq!(
            summary_hint(&summary).as_deref(),
            Some("summary is 73 characters (guideline: 72)")
        );
        let message = format!("{summary}\n\nbody that is long but irrelevant");
        assert!(
            summary_hint(&message).is_some(),
            "only the first line counts"
        );
        assert_eq!(
            summary_len(&format!("{}\nbody", "é".repeat(72))),
            72,
            "counts characters, not bytes"
        );
    }
}
