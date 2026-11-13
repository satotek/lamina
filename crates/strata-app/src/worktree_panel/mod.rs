//! The working-tree view: stage, unstage, discard and commit.
//!
//! Everything here talks to [`Session`]: mutations are [`Operation`]s on its
//! worker and diffs are [`DiffRequest`]s. The panel renders from the latest
//! [`RepoSnapshot`] and never calls git itself.
//!
//! The file lists live in [`file_list`], the diff and hunk rendering in
//! [`hunks`], and the commit message box in [`commit_box`]; this module owns the
//! state, the actions and the entry points.

mod commit_box;
mod file_list;
mod hunks;

use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Hsla, InteractiveElement, KeyBinding,
    ParentElement, SharedString, Styled, Subscription, UniformListScrollHandle, Window, actions,
    div, px,
};
use gpui_component::input::{InputEvent, TextareaState};
use gpui_component::{ActiveTheme as _, Theme};
use strata_core::patch::LineSelection;
use strata_core::session::{
    ConflictSide, DiffRequest, DiffTicket, DiffView, Operation, OperationId, RepoSnapshot, Session,
};
use strata_core::{ChangeKind, FileChange, RepoState, WorktreeStatus};

use commit_box::{commit_enabled, previous_message, trim_message};
use hunks::{panel_rows, picked_in_range, picked_selection};

use crate::conflicts::{Conflicts, MarkFileResolved, UseOurs, UseTheirs};
use crate::diff_view::Row as DiffRow;
use crate::repo_view::RepoView;
use crate::settings::Settings;
use crate::style::Design;

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
///
/// [`FileDiff`]: strata_core::diff::FileDiff
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

    /// Opens the working-tree view on the conflicts list, selecting its first
    /// path.
    ///
    /// Used when an operation stops on conflicts. The file history and the
    /// search filter belong to the history view, and leaving either in place
    /// would hide what just happened when the user comes back.
    pub(crate) fn open_conflicts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_file_history(window, cx);
        if self.search.is_active() {
            self.clear_search(window, cx);
        }
        self.open_worktree(window, cx);
        let first = self.session.as_ref().and_then(|session| {
            session
                .snapshot()
                .conflicts
                .first()
                .map(|entry| entry.path.clone())
        });
        if let Some(path) = first {
            self.select_conflict_file(path, window, cx);
        }
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

    /// Asks before throwing away one path's working-tree changes.
    ///
    /// The confirmation is a real dialog, not an overlay: it holds focus, so
    /// `Enter` and `Esc` belong to it and nothing behind it can act while it is
    /// open. Nothing is remembered here either, so a cancelled dialog cannot
    /// come back on a later frame.
    pub(crate) fn discard_worktree_path(
        &mut self,
        path: String,
        untracked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (title, detail, ok_label, kind, operation) = if untracked {
            (
                format!("Delete {path}?"),
                "The untracked file is removed from disk.".to_owned(),
                "Delete",
                PendingKind::Delete,
                Operation::DeleteUntracked(vec![path]),
            )
        } else {
            (
                format!("Discard changes to {path}?"),
                "The working-tree changes are lost; this cannot be undone.".to_owned(),
                "Discard",
                PendingKind::Discard,
                Operation::DiscardPaths(vec![path]),
            )
        };
        self.ask_destructive(
            window,
            cx,
            crate::ui::Confirm::new(title, detail).ok_label(ok_label),
            kind,
            operation,
        );
    }

    /// Opens the confirmation dialog for `operation`, which runs only when the
    /// user accepts it (or immediately, when the settings turn confirmations
    /// off).
    fn ask_destructive(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        confirm: crate::ui::Confirm,
        kind: PendingKind,
        operation: Operation,
    ) {
        // A dialog hands focus back to whatever had it when it opened. The
        // context menu that usually offers these is already closing and its
        // focus handle dies with it, so give the dialog the panel's handle and
        // the panel keeps its keys (Enter, s, u) after the dialog closes.
        let focus = self.worktree.ensure_focus(cx);
        window.focus(&focus, cx);
        let entity = cx.entity();
        crate::ui::confirm(window, cx, confirm, move |_, cx| {
            entity.update(cx, |view, cx| {
                view.run_confirmed(kind, operation.clone(), cx);
            });
        });
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
        window: &mut Window,
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
        self.line_operation_for(selected, selection, action, window, cx);
    }

    pub(crate) fn line_operation(
        &mut self,
        action: LineAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selected) = self.worktree.selected.clone() else {
            return;
        };
        let selection = picked_selection(&self.worktree.picked);
        if selection.is_empty() {
            return;
        }
        self.line_operation_for(selected, selection, action.into(), window, cx);
    }

    fn line_operation_for(
        &mut self,
        selected: Selected,
        selection: LineSelection,
        action: HunkAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = selected.path.clone();
        let operation = match (selected.side, action) {
            (Side::Unstaged, HunkAction::Stage) => Operation::StageLines { path, selection },
            (Side::Staged, HunkAction::Unstage) => Operation::UnstageLines { path, selection },
            (Side::Unstaged, HunkAction::Discard) => {
                self.ask_destructive(
                    window,
                    cx,
                    crate::ui::Confirm::new(
                        "Discard selected lines?",
                        "The selected changes are removed from the working tree.",
                    )
                    .ok_label("Discard"),
                    PendingKind::Discard,
                    Operation::DiscardLines { path, selection },
                );
                return;
            }
            _ => return,
        };
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let kind = match action {
            HunkAction::Stage => PendingKind::Stage,
            HunkAction::Unstage => PendingKind::Unstage,
            HunkAction::Discard => unreachable!("handled above"),
        };
        self.worktree.run(session, kind, operation);
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

    /// Runs a destructive operation the user confirmed in [`crate::ui::confirm`].
    fn run_confirmed(&mut self, kind: PendingKind, operation: Operation, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        self.worktree.run(session, kind, operation);
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

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Colors {
    /// The shared design palette, sizes and history settings.
    design: Design,
    background: Hsla,
    foreground: Hsla,
    muted_foreground: Hsla,
    border: Hsla,
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
            muted_foreground: design.muted,
            border: design.border,
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
        crate::ui::button("show-history-synthetic", "← History")
            .ghost()
            .on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| this.close_worktree(window, cx));
            })
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
    .child(crate::ui::empty_state(
        "No working tree in synthetic mode",
        &colors.design,
    ))
    .into_any_element()
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
        let mut banner =
            crate::ui::banner(crate::ui::BannerKind::Warning, state.text, &colors.design);
        // Abort is the escape hatch, so it reads as an ordinary button; Continue
        // is the one the banner is asking for.
        if state.can_abort {
            let abort = entity.clone();
            banner = banner.action(crate::ui::button("merge-abort", "Abort merge…").on_click(
                move |_, window, cx| {
                    crate::conflicts::confirm_abort_merge(abort.clone(), window, cx);
                },
            ));
        }
        if state.can_continue {
            let continue_merge = entity.clone();
            banner = banner.action(
                crate::ui::button("merge-continue", "Continue merge").on_click(move |_, _, cx| {
                    continue_merge.update(cx, |this, cx| this.continue_merge(cx));
                }),
            );
        } else if state.can_abort {
            // Keep the row from jumping when the button appears; a disabled
            // Continue also explains why the merge cannot finish yet.
            banner =
                banner.action(crate::ui::button("merge-continue", "Continue merge").disabled(true));
        }
        Some(banner.into_any_element())
    }
}
