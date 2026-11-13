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
    AnyElement, App, ClickEvent, Context, Entity, FocusHandle, FontWeight, Hsla,
    InteractiveElement, IntoElement, KeyBinding, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Subscription, UniformListScrollHandle, Window, actions,
    div, px, uniform_list,
};
use gpui_component::input::{InputEvent, Textarea, TextareaState};
use gpui_component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Theme};
use strata_core::diff::{FileDiff, LineKind};
use strata_core::highlight::LineHighlights;
use strata_core::patch::LineSelection;
use strata_core::session::{
    ConflictSide, DiffRequest, DiffTicket, DiffView, Operation, OperationId, RepoSnapshot, Session,
};
use strata_core::{ChangeKind, FileChange, HeadInfo, RepoState, WorktreeStatus};

use crate::conflicts::{Conflicts, MarkFileResolved, UseOurs, UseTheirs};
use crate::diff_view::{self, DiffColors, Row as DiffRow, RowOptions};
use crate::repo_view::RepoView;
use crate::scrollbar::WithScrollbar as _;
use crate::settings::Settings;
use crate::style::{self, Design};

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
    /// An unmerged path; the right-hand area shows the conflict view.
    Conflict,
}

impl Side {
    fn other(self) -> Self {
        match self {
            Side::Unstaged => Side::Staged,
            Side::Staged => Side::Unstaged,
            // A conflicted path does not move between the index sides while it
            // stays conflicted; resolution removes it instead.
            Side::Conflict => Side::Conflict,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Side::Unstaged => "unstaged",
            Side::Staged => "staged",
            Side::Conflict => "conflict",
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
    /// Conflicts list, per-file conflict view and merge banner state.
    pub(crate) conflicts: Conflicts,
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
    /// The prepared merge message that was offered, so it can be withdrawn if
    /// the merge ends (aborted) while the box still holds it unchanged.
    offered_merge_message: Option<String>,
    /// Set when the merge ended; the next render clears the box if it still
    /// holds exactly the offered message.
    withdraw_merge_message: Option<String>,

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
        if let Some(offered) = self.withdraw_merge_message.take()
            && input.read(cx).value().trim() == offered.trim()
        {
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
            self.withdraw_merge_message = self.offered_merge_message.take();
            return;
        }
        if self.merge_prefilled {
            return;
        }
        self.merge_prefilled = true;
        // One small file read; cheaper than a round trip through a worker.
        if let Ok(Some(message)) = session.repo().merge_message() {
            self.offered_merge_message = Some(message.clone());
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

    /// Keeps the selection valid against a new snapshot. Returns true when the
    /// user-visible file changed, which needs a new diff or conflict read.
    ///
    /// Conflicted paths are listed in `snapshot.conflicts` and not in the
    /// unstaged list, so they get their own selection side and are preferred
    /// when the selection has to start somewhere: a stopped merge should open
    /// on the first conflict instead of an unrelated edited file.
    fn reconcile(&mut self, snapshot: &RepoSnapshot) -> bool {
        let status = &snapshot.status;
        if let Some(selected) = self.selected.clone() {
            if contains(snapshot, selected.side, &selected.path) {
                return false;
            }
            if selected.side != Side::Conflict {
                // A staged file leaves the unstaged list (and vice versa); follow
                // it to the other side instead of jumping to an unrelated file.
                let other = selected.side.other();
                if contains(snapshot, other, &selected.path) {
                    self.select(other, selected.path);
                    return true;
                }
            }
        }

        let fallback = snapshot
            .conflicts
            .first()
            .map(|entry| (Side::Conflict, entry.path.clone()))
            .or_else(|| {
                status
                    .unstaged
                    .iter()
                    .find(|change| change.kind != ChangeKind::Conflicted)
                    .map(|change| (Side::Unstaged, change.path.clone()))
            })
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
        let conflict_handled = self.conflicts.on_operation_finished(id, error);
        let Some((pending_id, kind)) = self.pending else {
            return conflict_handled;
        };
        if pending_id != id {
            return conflict_handled;
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
            self.worktree.conflicts.observe(&snapshot);
            self.worktree.reconcile(&snapshot);
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
        // The sides are re-read when the view opens again; a conflict resolved
        // in another window in the meantime must not show its old contents.
        self.worktree.conflicts.clear();
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
        self.worktree.conflicts.observe(&snapshot);
        let moved = self.worktree.reconcile(&snapshot);
        let needs_view = match self.worktree.selected.as_ref() {
            Some(selected) if selected.side == Side::Conflict => {
                !self.worktree.conflicts.is_pending()
                    && self.worktree.conflicts.needs_load(&selected.path)
            }
            Some(_) => {
                self.worktree.diff.view().is_none()
                    && !self.worktree.diff.is_pending()
                    && self.worktree.diff_error.is_none()
            }
            None => false,
        };
        if moved || self.worktree.dirty || needs_view {
            self.request_worktree_diff(cx);
        }
    }

    pub(crate) fn request_worktree_diff(&mut self, cx: &mut Context<Self>) {
        let Some(selected) = self.worktree.selected.clone() else {
            // Nothing is selected, so there is no diff to show; an older
            // ticket must not land and draw a file the lists no longer have.
            self.worktree.rows = Rc::new(Vec::new());
            self.worktree.picked.clear();
            if let (Some(session), Some(ticket)) =
                (self.session.as_ref(), self.worktree.diff.cancel())
            {
                session.cancel_diff(ticket);
            }
            self.worktree.dirty = false;
            cx.notify();
            return;
        };

        if selected.side == Side::Conflict {
            if let (Some(session), Some(ticket)) =
                (self.session.as_ref(), self.worktree.diff.cancel())
            {
                session.cancel_diff(ticket);
            }
            self.worktree.rows = Rc::new(Vec::new());
            self.worktree.picked.clear();
            self.worktree.anchor = None;
            self.worktree.diff_error = None;
            self.worktree.dirty = false;
            if self.worktree.conflicts.needs_load(&selected.path) {
                self.load_conflict_sides(selected.path, cx);
            }
            cx.notify();
            return;
        }

        let Some(session) = self.session.as_ref() else {
            return;
        };
        let request = match selected.side {
            Side::Staged => DiffRequest::Staged {
                path: selected.path,
            },
            Side::Unstaged => DiffRequest::Unstaged {
                path: selected.path,
            },
            Side::Conflict => unreachable!("handled above"),
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

    /// Selects a conflicted path; the right-hand area becomes the conflict view.
    pub(crate) fn select_conflict_file(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_worktree_file(Side::Conflict, path, window, cx);
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
            // A conflict is resolved from its own view, not by staging it.
            Side::Conflict => return,
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
        let paths: Vec<String> = visible_unstaged(&snapshot.status)
            .into_iter()
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

fn contains(snapshot: &RepoSnapshot, side: Side, path: &str) -> bool {
    match side {
        Side::Conflict => snapshot.conflicts.iter().any(|entry| entry.path == path),
        // A conflicted path is listed in the conflicts section instead of the
        // unstaged one, so it must not match here or the selection would jump.
        Side::Unstaged => snapshot
            .status
            .unstaged
            .iter()
            .any(|change| change.path == path && change.kind != ChangeKind::Conflicted),
        Side::Staged => snapshot
            .status
            .staged
            .iter()
            .any(|change| change.path == path),
    }
}

/// Unstaged rows as the list shows them: conflicted paths live in their own
/// section, so staging or discarding "all" must not sweep them up.
fn visible_unstaged(status: &WorktreeStatus) -> Vec<&FileChange> {
    status
        .unstaged
        .iter()
        .filter(|change| change.kind != ChangeKind::Conflicted)
        .collect()
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

/// First line length in characters, for the summary-length hint.
fn summary_len(message: &str) -> usize {
    message.lines().next().unwrap_or_default().chars().count()
}

fn summary_hint(message: &str, limit: u32) -> Option<String> {
    let len = summary_len(message);
    (len > limit as usize).then(|| format!("summary is {len} characters (guideline: {limit})"))
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
    /// The shared design palette, sizes and history settings.
    design: Design,
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
    mono: SharedString,
}

impl Colors {
    fn from_theme(theme: &Theme, settings: &Settings) -> Self {
        let design = Design::new(theme, settings);
        Self {
            background: design.background,
            foreground: design.text,
            muted: design.field,
            muted_foreground: design.muted,
            border: design.border,
            selection: design.row_selected,
            title_bar: design.chrome,
            accent: design.accent,
            accent_foreground: design.on_accent,
            danger: design.remove_text,
            warning: design.tag_text,
            success: design.add_text,
            mono: design.mono.clone(),
            design,
        }
    }
}

const LIST_WIDTH: f32 = 300.0;
/// Height of one file row, matching the commit list and the design.
const ROW_HEIGHT: f32 = 28.0;
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
    let (name, dir) = style::path_parts(&path);

    let mut row = div()
        .flex()
        .items_center()
        .gap_2()
        .h(px(ROW_HEIGHT))
        .mx(px(6.0))
        .px_2()
        .rounded(px(6.0))
        .text_size(px(colors.design.small() + 0.5))
        .child(style::file_badge(kind, &colors.design))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(if is_selected {
                    colors.design.accent_text
                } else {
                    colors.foreground
                })
                .child(name.to_owned()),
        )
        .when_some(dir, |row, dir| {
            row.child(
                div()
                    .max_w(px(120.0))
                    .flex_none()
                    .truncate()
                    .text_size(px(11.0))
                    .text_color(colors.muted_foreground)
                    .child(dir.to_owned()),
            )
        });
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
            // The conflict section has no context menu; its actions live in
            // the conflict view.
            Side::Conflict => {}
        }
        menu
    })
    .into_any_element()
}

#[allow(
    clippy::too_many_arguments,
    reason = "one call per hunk header; a struct would only rename the fields"
)]
fn hunk_header_row(
    entity: &Entity<RepoView>,
    row: usize,
    position: &RowPosition,
    side: Option<Side>,
    (old_start, old_len, new_start, new_len): (u32, u32, u32, u32),
    height: f32,
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
                .child(chip_button_compact(
                    ("hunk-stage", row),
                    "Stage hunk",
                    false,
                    &stage_colors,
                    move |_, _, cx| {
                        stage_entity.update(cx, |this, cx| {
                            this.hunk_operation(hunk, HunkAction::Stage, cx)
                        });
                    },
                ))
                .child(chip_button_compact(
                    ("hunk-discard", row),
                    "Discard",
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
                .child(chip_button_compact(
                    ("hunk-unstage", row),
                    "Unstage hunk",
                    false,
                    &unstage_colors,
                    move |_, _, cx| {
                        unstage_entity.update(cx, |this, cx| {
                            this.hunk_operation(hunk, HunkAction::Unstage, cx)
                        });
                    },
                ))
        }
        Some(Side::Conflict) => div(),
        None => div(),
    };

    div()
        .flex()
        .items_center()
        .h(px(height))
        .w_full()
        .bg(colors.design.panel)
        .border_t_1()
        .border_color(colors.border)
        .text_size(px(11.0))
        .text_color(diff_colors.muted)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .pl_2()
                .truncate()
                .font_family(colors.mono.clone())
                .child(format!(
                    "@@ -{old_start},{old_len} +{new_start},{new_len} @@"
                )),
        )
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
        .flex()
        .items_center()
        .justify_center()
        .h(px(24.0))
        .px(px(9.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(colors.border)
        .text_size(px(colors.design.small()))
        .text_color(if enabled {
            colors.foreground
        } else {
            colors.muted_foreground
        })
        .bg(colors.muted);
    if enabled {
        button = button.cursor_pointer().on_click(handler);
    }
    button.child(label.into()).into_any_element()
}

/// The hunk header's buttons: same look, sized for a 20 px diff row.
fn chip_button_compact(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    danger: bool,
    colors: &Colors,
    handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .h(px(16.0))
        .px(px(6.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(colors.border)
        .text_size(px(10.0))
        .text_color(if danger {
            colors.danger
        } else {
            colors.foreground
        })
        .bg(colors.muted)
        .cursor_pointer()
        .on_click(handler)
        .child(label.into())
        .into_any_element()
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
        .flex()
        .items_center()
        .justify_center()
        .h(px(28.0))
        .px(px(14.0))
        .rounded(px(7.0))
        .text_size(px(colors.design.small()))
        .font_weight(FontWeight::SEMIBOLD)
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

        let colors = Colors::from_theme(cx.theme(), Settings::get(cx));
        let Some(session) = self.session.as_ref() else {
            return synthetic_state(entity, &self.worktree, cx, &colors);
        };
        let snapshot = session.snapshot();
        self.worktree.render(entity, &snapshot, cx)
    }
}

impl WorktreePanel {
    /// The path selected in the conflicts section, if any. Conflict operations
    /// go through this rather than loaded data, so a stale read cannot act on a
    /// file the user has already moved away from.
    pub(crate) fn selected_conflict(&self) -> Option<&str> {
        self.selected
            .as_ref()
            .filter(|selected| selected.side == Side::Conflict)
            .map(|selected| selected.path.as_str())
    }

    fn render(
        &self,
        entity: Entity<RepoView>,
        snapshot: &RepoSnapshot,
        cx: &mut Context<RepoView>,
    ) -> AnyElement {
        let colors = Colors::from_theme(cx.theme(), Settings::get(cx));
        let selected_conflict = self.selected_conflict().map(str::to_owned);
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
            // The conflict keys only do something while a conflicted file is
            // selected; `use_conflict_side` re-checks that.
            .on_action(cx.listener(|this, _: &UseOurs, _, cx| {
                this.use_conflict_side(ConflictSide::Ours, cx)
            }))
            .on_action(cx.listener(|this, _: &UseTheirs, _, cx| {
                this.use_conflict_side(ConflictSide::Theirs, cx)
            }))
            .on_action(
                cx.listener(|this, _: &MarkFileResolved, _, cx| this.mark_conflict_resolved(cx)),
            )
            .child(self.lists(entity.clone(), snapshot, &colors, cx))
            .child(match selected_conflict {
                Some(path) => self.conflicts.pane(&path, entity.clone(), snapshot, cx),
                None => self.diff_pane(entity.clone(), snapshot, cx.theme(), &colors),
            });
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

        if let Some(banner) = self.merge_banner(entity.clone(), snapshot, &colors) {
            root = root.child(banner);
        }

        root = root
            .child(body)
            .child(self.commit_box(entity.clone(), snapshot, cx, &colors));

        if let Some(confirm) = &self.confirm {
            root = root.child(confirm_overlay(entity, confirm, &colors));
        }

        root.into_any_element()
    }

    /// A stopped merge or another multi-step operation, with the actions core
    /// can express: continue commits the merge with git's prepared message,
    /// abort restores the pre-merge state after a confirmation.
    fn merge_banner(
        &self,
        entity: Entity<RepoView>,
        snapshot: &RepoSnapshot,
        colors: &Colors,
    ) -> Option<AnyElement> {
        let state = self.conflicts.banner_state(snapshot)?;
        let mut buttons = div().flex().items_center().gap_2().flex_none();
        if state.can_abort {
            let button_colors = colors.clone();
            buttons = buttons.child(chip_button(
                "merge-abort",
                "Abort merge…",
                true,
                &button_colors,
                {
                    let entity = entity.clone();
                    move |_, window, cx| {
                        crate::conflicts::confirm_abort_merge(entity.clone(), window, cx);
                    }
                },
            ));
        }
        if state.can_continue {
            let button_colors = colors.clone();
            buttons = buttons.child(primary_button(
                "merge-continue",
                "Continue merge",
                true,
                &button_colors,
                {
                    let entity = entity.clone();
                    move |_, _, cx| {
                        entity.update(cx, |this, cx| this.continue_merge(cx));
                    }
                },
            ));
        } else if state.can_abort {
            // Keep the row from jumping when the button appears; a disabled
            // Continue also explains why the merge cannot finish yet.
            let button_colors = colors.clone();
            buttons = buttons.child(primary_button(
                "merge-continue",
                "Continue merge",
                false,
                &button_colors,
                |_, _, _| {},
            ));
        }

        Some(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .flex_none()
                .px_3()
                .py_1()
                .bg(colors.warning.opacity(0.18))
                .text_size(px(11.0))
                .text_color(colors.warning)
                .child(div().flex_1().min_w_0().truncate().child(state.text))
                .child(buttons)
                .into_any_element(),
        )
    }

    fn lists(
        &self,
        entity: Entity<RepoView>,
        snapshot: &RepoSnapshot,
        colors: &Colors,
        cx: &Context<RepoView>,
    ) -> AnyElement {
        let selected_conflict = self.selected_conflict();
        div()
            .flex()
            .flex_col()
            .w(px(LIST_WIDTH))
            .flex_none()
            .min_h_0()
            .border_r_1()
            .border_color(colors.border)
            .child(self.conflicts.section(
                entity.clone(),
                &snapshot.conflicts,
                selected_conflict,
                cx,
            ))
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
        // Unmerged paths are listed in the conflicts section; repeating them
        // under "unstaged" would offer stage/discard actions that cannot work.
        let files: Vec<&FileChange> = files
            .iter()
            .filter(|change| change.kind != ChangeKind::Conflicted)
            .collect();
        let title = match side {
            Side::Unstaged => "UNSTAGED",
            Side::Staged => "STAGED",
            Side::Conflict => "CONFLICTS",
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
            // The conflicts section has its own renderer.
            Side::Conflict => div().into_any_element(),
        };

        let mut section = div().flex().flex_col().flex_none().min_h_0().child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .px_3()
                .pt_3()
                .pb_1p5()
                .child(
                    div()
                        .text_size(px(colors.design.tiny()))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.muted_foreground)
                        .child(format!("{title} · {}", files.len())),
                )
                .child(button),
        );

        if files.is_empty() {
            return section
                .child(
                    div()
                        .px_3()
                        .h(px(24.0))
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
            Side::Conflict => "conflict-files",
        };
        let scroll = match side {
            Side::Unstaged => self.unstaged_scroll.clone(),
            Side::Staged => self.staged_scroll.clone(),
            Side::Conflict => self.unstaged_scroll.clone(),
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
        .with_scrollbar(&scroll)
        .h(px(ROW_HEIGHT * visible_rows as f32))
        .flex_none();

        section = section.child(list);
        section.into_any_element()
    }

    fn diff_pane(
        &self,
        entity: Entity<RepoView>,
        snapshot: &RepoSnapshot,
        theme: &Theme,
        colors: &Colors,
    ) -> AnyElement {
        let selected = self.selected.clone();
        let (path, side) = selected
            .as_ref()
            .map(|selected| (selected.path.clone(), Some(selected.side)))
            .unwrap_or_else(|| ("no file selected".to_owned(), None));
        let kind = selected.as_ref().and_then(|selected| match selected.side {
            Side::Unstaged => snapshot
                .status
                .unstaged
                .iter()
                .find(|change| change.path == selected.path)
                .map(|change| change.kind),
            Side::Staged => snapshot
                .status
                .staged
                .iter()
                .find(|change| change.path == selected.path)
                .map(|change| change.kind),
            Side::Conflict => Some(ChangeKind::Conflicted),
        });
        let (name, dir) = style::path_parts(&path);

        let mut header = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .h(px(40.0))
            .flex_none()
            .px_3p5()
            .bg(colors.background)
            .border_b_1()
            .border_color(colors.border)
            .text_size(px(12.0));
        if let Some(kind) = kind {
            header = header.child(style::file_badge(kind, &colors.design));
        }
        header = header
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .child(div().font_weight(FontWeight::MEDIUM).child(name.to_owned()))
                    .when_some(dir, |row, dir| {
                        row.child(
                            div()
                                .ml_2()
                                .min_w_0()
                                .truncate()
                                .text_size(px(11.5))
                                .text_color(colors.muted_foreground)
                                .child(dir.to_owned()),
                        )
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.0))
                    .text_color(colors.muted_foreground)
                    .child(side.map(Side::label).unwrap_or("")),
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
                .text_size(px(11.0))
                .text_color(colors.muted_foreground)
                .child("Click lines to pick · Shift-click for a range")
                .into_any_element();
        }

        let mut actions = div().flex().items_center().gap_1();
        let (label, action) = match selected.side {
            Side::Unstaged => (format!("Stage {count} line(s)"), LineAction::Stage),
            Side::Staged => (format!("Unstage {count} line(s)"), LineAction::Unstage),
            Side::Conflict => return div().into_any_element(),
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

        uniform_list("worktree-diff", rows.len(), move |range, _window, cx| {
            let style = diff_view::DiffStyle::from_settings(cx, &mono).one_line_per_row();
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
                            style.row_height(),
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
                                    &style,
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
        .with_scrollbar(&self.diff_scroll)
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
        let limit = colors.design.summary_hint;
        let length = summary_len(&message);
        let over = length > limit as usize;
        let hint = summary_hint(&message, limit);

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
                        .rounded(px(3.0))
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
            let mut button = div()
                .id("commit-button")
                .flex_none()
                .flex()
                .items_center()
                .gap_2()
                .h(px(32.0))
                .px(px(14.0))
                .rounded(px(7.0))
                .text_size(px(12.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(if enabled {
                    colors.accent_foreground
                } else {
                    colors.muted_foreground
                })
                .bg(if enabled { colors.accent } else { colors.muted })
                .child(format!(
                    "Commit {staged} file{}",
                    if staged == 1 { "" } else { "s" }
                ))
                .child(
                    div()
                        .px(px(5.0))
                        .py(px(1.0))
                        .rounded(px(4.0))
                        .bg(colors.accent_foreground.opacity(0.18))
                        .text_size(px(10.5))
                        .child(commit_key_label()),
                );
            if enabled {
                button = button.cursor_pointer().on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| this.commit_worktree(cx));
                });
            }
            button
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
            .gap_2()
            .p_3()
            .bg(colors.title_bar)
            .border_t_1()
            .border_color(colors.border)
            .child(
                div()
                    .text_size(px(11.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.muted_foreground)
                    .child("COMMIT MESSAGE"),
            )
            .child(
                Textarea::new(&input)
                    .h(px(84.0))
                    .w_full()
                    .text_size(px(colors.design.code)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(history_button)
                    .child(amend_box)
                    .when_some(hint, |row, hint| {
                        row.child(
                            div()
                                .flex_none()
                                .text_size(px(10.0))
                                .text_color(colors.warning)
                                .child(hint),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.0))
                            .text_color(if over {
                                colors.warning
                            } else {
                                colors.muted_foreground
                            })
                            .child(format!("{length} / {limit}")),
                    )
                    .child(div().flex_1())
                    .child(commit_button),
            )
            .child(status)
            .into_any_element()
    }
}

#[cfg(not(target_os = "macos"))]
fn commit_key_label() -> &'static str {
    "Ctrl ↵"
}

#[cfg(target_os = "macos")]
fn commit_key_label() -> &'static str {
    "⌘↵"
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
        assert_eq!(summary_hint("short summary", 72), None);
        let summary = "s".repeat(73);
        assert_eq!(
            summary_hint(&summary, 72).as_deref(),
            Some("summary is 73 characters (guideline: 72)")
        );
        let message = format!("{summary}\n\nbody that is long but irrelevant");
        assert!(
            summary_hint(&message, 72).is_some(),
            "only the first line counts"
        );
        assert_eq!(
            summary_hint(&summary, 120),
            None,
            "the limit comes from the settings"
        );
        assert_eq!(
            summary_len(&format!("{}\nbody", "é".repeat(72))),
            72,
            "counts characters, not bytes"
        );
    }
}
