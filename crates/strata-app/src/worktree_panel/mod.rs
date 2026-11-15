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
mod ignore;

use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Hsla, InteractiveElement, ParentElement,
    SharedString, Styled, Subscription, UniformListScrollHandle, Window, actions, div,
};
use gpui_component::input::{InputEvent, TextareaState};
use gpui_component::{ActiveTheme as _, Theme};
use strata_core::diff::{Expansion, GapAction};
use strata_core::lfs::{LfsInfo, LfsLock};
use strata_core::patch::LineSelection;
use strata_core::repo::InProgressDetails;
use strata_core::session::{
    ConflictSide, DiffRequest, DiffTicket, DiffView, Operation, OperationId, RepoSnapshot, Session,
};
use strata_core::{ChangeKind, FileChange, RepoState, WorktreeStatus};

use commit_box::{commit_enabled, previous_message, trim_message};
use hunks::{panel_rows_expanded, picked_in_range, picked_selection};

use crate::conflicts::{
    Conflicts, MarkFileResolved, NextConflict, PreviousConflict, ResetConflict, TakeBase,
    TakeBothOursFirst, TakeBothTheirsFirst, TakeOurs, TakeTheirs, UseOurs, UseTheirs,
};
use crate::diff_view::{DiffContext, Row as DiffRow};
use crate::merge_editor::Choice as MergeChoice;
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
        DiscardSelected,
        ToggleSelected,
        SelectAllFiles,
        SelectPreviousFile,
        SelectNextFile,
        ExtendPreviousFile,
        ExtendNextFile,
        CommitMessage,
        ShowHistory,
    ]
);

pub fn bind(cx: &mut App) {
    crate::keymap::bind_group(cx, crate::keymap::Group::Worktree);
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

    fn label(self) -> String {
        match self {
            Side::Unstaged => t!("worktree.side.unstaged"),
            Side::Staged => t!("worktree.side.staged"),
            Side::Conflict => t!("worktree.side.conflict"),
        }
        .to_string()
    }
}

/// The focused row, identified by path so a status refresh that reorders the
/// list keeps the selection.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Selected {
    side: Side,
    path: String,
}

/// The rows the user selected in one of the file lists.
///
/// A selection never spans the unstaged and the staged section: acting on a
/// row in the other section starts a new selection there. Conflict rows are
/// single-select and only feed the conflict view, so they never join a set.
///
/// The focused row stays [`WorktreePanel::selected`]: the diff pane, the
/// conflict view and the line-level operations already read it there, and a
/// selection change only has to keep the two in step. Every method takes the
/// section's row order as a slice instead of reading the snapshot, which keeps
/// the model pure and testable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Selection {
    side: Option<Side>,
    /// Sorted, so one operation always receives its paths in the same order.
    paths: BTreeSet<String>,
    /// The row a shift-click or shift-arrow extends from.
    anchor: Option<String>,
}

impl Selection {
    fn side(&self) -> Option<Side> {
        self.side
    }

    fn len(&self) -> usize {
        self.paths.len()
    }

    fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    fn contains(&self, side: Side, path: &str) -> bool {
        self.side == Some(side) && self.paths.contains(path)
    }

    /// The selected paths in the order operations should use.
    fn paths(&self) -> Vec<String> {
        self.paths.iter().cloned().collect()
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    /// Replaces the selection with one row, which becomes the focused one.
    fn select_one(&mut self, side: Side, path: String) -> Selected {
        self.side = Some(side);
        self.paths = BTreeSet::from([path.clone()]);
        self.anchor = Some(path.clone());
        Selected { side, path }
    }

    /// A click without a modifier collapses the selection onto one row; shift
    /// and the platform's secondary modifier (Cmd on macOS, Ctrl elsewhere)
    /// extend and toggle it.
    fn click(
        &mut self,
        focused: Option<Selected>,
        side: Side,
        path: String,
        shift: bool,
        secondary: bool,
        rows: &[String],
    ) -> Option<Selected> {
        if shift {
            Some(self.extend_to(side, path, rows))
        } else if secondary {
            self.toggle(focused, side, path)
        } else {
            Some(self.select_one(side, path))
        }
    }

    /// Cmd/Ctrl-click: adds or removes one row without collapsing the rest.
    /// `focused` is consumed because the row it points at may be the one that
    /// leaves the selection, in which case another selected row takes over.
    fn toggle(&mut self, focused: Option<Selected>, side: Side, path: String) -> Option<Selected> {
        if self.side != Some(side) {
            return Some(self.select_one(side, path));
        }
        if !self.paths.remove(&path) {
            self.paths.insert(path.clone());
            self.anchor = Some(path.clone());
            return Some(Selected { side, path });
        }
        if self.paths.is_empty() {
            self.clear();
            return None;
        }
        if self.anchor.as_deref() == Some(path.as_str()) {
            self.anchor = focused
                .as_ref()
                .map(|focused| focused.path.clone())
                .filter(|path| self.paths.contains(path));
        }
        self.anchor = self
            .anchor
            .take()
            .filter(|anchor| self.paths.contains(anchor));
        // Keep the diff on a row that is still selected.
        let next = focused
            .map(|focused| focused.path)
            .filter(|path| self.paths.contains(path))
            .or_else(|| self.anchor.clone())
            .or_else(|| self.paths.iter().next_back().cloned())?;
        Some(Selected { side, path: next })
    }

    /// Shift-click: selects every row between the anchor and `path`.
    fn extend_to(&mut self, side: Side, path: String, rows: &[String]) -> Selected {
        if self.side == Some(side)
            && let Some(anchor) = self.anchor.clone()
            && let (Some(from), Some(to)) = (
                rows.iter().position(|row| *row == anchor),
                rows.iter().position(|row| *row == path),
            )
        {
            let (low, high) = if from <= to { (from, to) } else { (to, from) };
            self.paths = rows[low..=high].iter().cloned().collect();
            return Selected { side, path };
        }
        self.select_one(side, path)
    }

    /// Cmd/Ctrl-A: everything the focused section lists. A focused row that is
    /// still listed stays focused so the diff does not jump.
    fn select_all(
        &mut self,
        side: Side,
        rows: &[String],
        focused: Option<&Selected>,
    ) -> Option<Selected> {
        self.side = Some(side);
        self.paths = rows.iter().cloned().collect();
        let path = focused
            .filter(|focused| focused.side == side && self.paths.contains(&focused.path))
            .map(|focused| focused.path.clone())
            .or_else(|| rows.first().cloned())?;
        self.anchor = Some(path.clone());
        Some(Selected { side, path })
    }

    /// Moves the focused row by `delta` rows, or extends the selection to it
    /// when `extend` is set.
    fn move_by(
        &mut self,
        rows: &[String],
        focused: Option<&Selected>,
        delta: isize,
        extend: bool,
    ) -> Option<Selected> {
        let side = self.side?;
        if rows.is_empty() {
            return None;
        }
        let current = focused
            .filter(|focused| focused.side == side)
            .and_then(|focused| rows.iter().position(|row| *row == focused.path));
        let index = match current {
            Some(index) => (index as isize + delta).clamp(0, rows.len() as isize - 1) as usize,
            None if delta < 0 => rows.len() - 1,
            None => 0,
        };
        let path = rows[index].clone();
        Some(if extend {
            self.extend_to(side, path, rows)
        } else {
            self.select_one(side, path)
        })
    }

    /// The row to focus when the previous focus is gone: the anchor while it
    /// is still selected, otherwise the last remaining path.
    fn focus_fallback(&self, side: Side) -> Option<Selected> {
        if self.side != Some(side) {
            return None;
        }
        let path = self
            .anchor
            .clone()
            .filter(|anchor| self.paths.contains(anchor))
            .or_else(|| self.paths.iter().next_back().cloned())?;
        Some(Selected { side, path })
    }

    /// Drops selected paths a refreshed list no longer shows. The caller keeps
    /// the focused row when it is still selected.
    fn retain(&mut self, listed: &[String]) {
        self.paths.retain(|path| listed.contains(path));
        self.anchor = self
            .anchor
            .take()
            .filter(|anchor| self.paths.contains(anchor));
        if self.paths.is_empty() {
            self.clear();
        }
    }
}

/// What the main area shows below the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MainMode {
    #[default]
    History,
    Worktree,
    Reflog,
}

/// One row of the flattened diff, plus where it points in the [`FileDiff`].
///
/// [`FileDiff`]: strata_core::diff::FileDiff
pub(crate) struct PanelRow {
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
    fn notice(self) -> String {
        match self {
            PendingKind::Stage => t!("worktree.pending.staged"),
            PendingKind::Unstage => t!("worktree.pending.unstaged"),
            PendingKind::Discard => t!("worktree.pending.discarded"),
            PendingKind::Delete => t!("worktree.pending.deleted"),
            PendingKind::Commit => t!("worktree.pending.committed"),
        }
        .to_string()
    }
}

/// State of the working-tree view. Rendering reads it, [`RepoView`] updates it
/// from session events and user input.
#[derive(Default)]
pub struct WorktreePanel {
    /// The focused row: the one the diff pane shows.
    selected: Option<Selected>,
    /// Every selected row on `selected`'s section.
    selection: Selection,
    /// Conflicts list, per-file conflict view and merge banner state.
    pub(crate) conflicts: Conflicts,
    diff: DiffSlot,
    rows: Rc<Vec<PanelRow>>,
    /// The mode, zoom and swipe position of the worktree's image diff.
    image_diff: crate::image_diff::State,
    /// Which hidden lines of the selected file's diff are revealed. Display
    /// only: the staging selection stays on the real hunks.
    context: DiffContext,
    diff_error: Option<String>,
    /// Changed lines picked for a line-level operation.
    picked: BTreeSet<(usize, usize)>,
    /// Flattened diff row the last click landed on, for shift-click ranges.
    line_anchor: Option<usize>,

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

    /// The last submitted operation and why, for the notice after it finishes.
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
            state.set_placeholder(t!("worktree.summary_placeholder").to_string(), window, cx);
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

    /// Makes `path` the only selected row and resets the diff, so the next
    /// frame reads the new file.
    fn select(&mut self, side: Side, path: String) {
        self.selected = Some(self.selection.select_one(side, path));
        self.reset_diff();
    }

    /// Takes the focused row the selection model computed and reports whether
    /// the diff now shows a different file.
    fn apply_focus(&mut self, focused: Option<Selected>) -> bool {
        if self.selected == focused {
            return false;
        }
        self.selected = focused;
        self.reset_diff();
        true
    }

    /// A click on a file row; returns whether the diff's file changed.
    fn click_file(
        &mut self,
        side: Side,
        path: String,
        shift: bool,
        secondary: bool,
        rows: &[String],
    ) -> bool {
        let focused =
            self.selection
                .click(self.selected.clone(), side, path, shift, secondary, rows);
        self.apply_focus(focused)
    }

    /// Cmd/Ctrl-A; returns whether the diff's file changed.
    fn select_all(&mut self, side: Side, rows: &[String]) -> bool {
        let focused = self
            .selection
            .select_all(side, rows, self.selected.as_ref());
        self.apply_focus(focused)
    }

    /// Up/Down and their shift variants; returns whether the diff's file
    /// changed.
    fn move_selection(&mut self, rows: &[String], delta: isize, extend: bool) -> bool {
        let focused = self
            .selection
            .move_by(rows, self.selected.as_ref(), delta, extend);
        match focused {
            Some(focused) => self.apply_focus(Some(focused)),
            None => false,
        }
    }

    /// Drops the diff rows and line picks; the next request fills them again.
    fn reset_diff(&mut self) {
        self.rows = Rc::new(Vec::new());
        self.context.reset();
        self.diff_error = None;
        self.picked.clear();
        self.line_anchor = None;
    }

    /// Keeps the selection valid against a new snapshot. Returns true when the
    /// user-visible file changed, which needs a new diff or conflict read.
    ///
    /// Selected paths the refreshed lists no longer show are dropped. The
    /// focused row follows its path to the other section when staging or
    /// unstaging moved it there. Conflicted paths are listed in
    /// `snapshot.conflicts` and not in the unstaged list, so they get their own
    /// section and are preferred when the selection has to start somewhere: a
    /// stopped merge should open on the first conflict instead of an unrelated
    /// edited file.
    fn reconcile(&mut self, snapshot: &RepoSnapshot) -> bool {
        let had_selection = self.selection.side().is_some();
        if let Some(side) = self.selection.side() {
            let before = self.selected.clone();
            self.selection.retain(&listed_paths(snapshot, side));
            if !self.selection.is_empty() {
                // Stay on the focused row while it is still listed; anything
                // else is a stale path that moved to another file.
                let focused = match before.clone() {
                    Some(focused)
                        if focused.side == side && self.selection.contains(side, &focused.path) =>
                    {
                        focused
                    }
                    _ => self
                        .selection
                        .focus_fallback(side)
                        .expect("a non-empty selection has a path"),
                };
                return self.apply_focus(Some(focused));
            }
            // The whole selection is gone. A staged file leaves the unstaged
            // list (and vice versa), so follow the focused path across instead
            // of jumping to an unrelated file.
            self.selected = None;
            if let Some(previous) = before
                && previous.side != Side::Conflict
                && contains(snapshot, previous.side.other(), &previous.path)
            {
                self.select(previous.side.other(), previous.path);
                return true;
            }
        }

        let fallback = snapshot
            .conflicts
            .first()
            .map(|entry| (Side::Conflict, entry.path.clone()))
            .or_else(|| {
                snapshot
                    .status
                    .unstaged
                    .iter()
                    .find(|change| change.kind != ChangeKind::Conflicted)
                    .map(|change| (Side::Unstaged, change.path.clone()))
            })
            .or_else(|| {
                snapshot
                    .status
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
                self.selection.clear();
                self.selected = None;
                if had_selection {
                    self.reset_diff();
                }
                had_selection
            }
        }
    }

    pub(crate) fn on_diff_ready(&mut self, ticket: DiffTicket, view: &Arc<DiffView>) -> bool {
        if !self.diff.accept(ticket, Arc::clone(view)) {
            return false;
        }
        self.context.reset();
        self.rows = Rc::new(panel_rows_expanded(
            &view.diff,
            &view.old_text,
            &view.new_text,
            &view.gaps,
            self.context.expansion(),
        ));
        self.diff_error = None;
        self.picked.clear();
        self.line_anchor = None;
        self.dirty = false;
        true
    }

    /// Applies one press on a hidden-lines bar. Returns the view to rebuild
    /// from when the press changed what is shown.
    pub(crate) fn expand_gap(&mut self, gap: usize, action: GapAction) -> Option<Arc<DiffView>> {
        let view = self.diff.view().cloned()?;
        self.context.expand(gap, action, &view).then_some(view)
    }

    /// The staging pane's "Whole file" toggle.
    pub(crate) fn toggle_whole_file(&mut self) -> Option<Arc<DiffView>> {
        let view = self.diff.view().cloned()?;
        self.context.toggle_whole_file(&view).then_some(view)
    }

    pub(crate) fn set_rows(&mut self, rows: Vec<PanelRow>) {
        self.rows = Rc::new(rows);
    }

    /// The staging rows a view has under `expansion`, without touching the
    /// panel; the caller builds them off the UI thread.
    pub(crate) fn rows_for(view: &DiffView, expansion: &Expansion) -> Vec<PanelRow> {
        panel_rows_expanded(
            &view.diff,
            &view.old_text,
            &view.new_text,
            &view.gaps,
            expansion,
        )
    }

    pub(crate) fn on_diff_failed(&mut self, ticket: DiffTicket, error: &str) -> bool {
        if !self.diff.fail(ticket) {
            return false;
        }
        self.diff.ticket = None;
        self.rows = Rc::new(Vec::new());
        self.context.reset();
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

    pub(crate) fn cancel_diff(&mut self) -> Option<DiffTicket> {
        self.diff.cancel()
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
        self.leave_reflog(cx);
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
        // Leaving the reflog for the history view has to put the history
        // selection back in the details pane.
        self.leave_reflog(cx);
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
            self.worktree.line_anchor = None;
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
            self.worktree.reset_diff();
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
        self.worktree.reset_diff();
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

    /// Enter or a double-click: moves the whole selection across the index.
    pub(crate) fn toggle_worktree_selection(&mut self, cx: &mut Context<Self>) {
        match self.worktree.selection.side() {
            Some(Side::Unstaged) => self.stage_selected(cx),
            Some(Side::Staged) => self.unstage_selected(cx),
            // A conflict is resolved from its own view, not by staging it.
            Some(Side::Conflict) | None => {}
        }
    }

    /// `s`: stages every selected unstaged path in one operation.
    pub(crate) fn stage_selected(&mut self, cx: &mut Context<Self>) {
        if self.worktree.selection.side() != Some(Side::Unstaged) {
            return;
        }
        self.stage_paths(self.worktree.selection.paths(), cx);
    }

    pub(crate) fn stage_path(&mut self, path: String, cx: &mut Context<Self>) {
        self.stage_paths(vec![path], cx);
    }

    fn stage_paths(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        self.worktree
            .run(session, PendingKind::Stage, Operation::StagePaths(paths));
        cx.notify();
    }

    /// `u`: unstages every selected staged path in one operation.
    pub(crate) fn unstage_selected(&mut self, cx: &mut Context<Self>) {
        if self.worktree.selection.side() != Some(Side::Staged) {
            return;
        }
        self.unstage_paths(self.worktree.selection.paths(), cx);
    }

    pub(crate) fn unstage_path(&mut self, path: String, cx: &mut Context<Self>) {
        self.unstage_paths(vec![path], cx);
    }

    fn unstage_paths(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        self.worktree.run(
            session,
            PendingKind::Unstage,
            Operation::UnstagePaths(paths),
        );
        cx.notify();
    }

    /// A click on a file row. Shift extends the range and the platform's
    /// secondary modifier toggles one row; either way the clicked row becomes
    /// the focused one, so the diff follows the pointer.
    #[allow(
        clippy::too_many_arguments,
        reason = "a click carries the row's section, order and modifiers"
    )]
    pub(crate) fn click_worktree_file(
        &mut self,
        side: Side,
        path: String,
        shift: bool,
        secondary: bool,
        rows: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = self.worktree.ensure_focus(cx);
        window.focus(&focus, cx);
        if self.worktree.click_file(side, path, shift, secondary, rows) {
            self.request_worktree_diff(cx);
        } else {
            cx.notify();
        }
    }

    /// Makes the right-clicked row part of the selection. A menu opened on a
    /// row outside it acts on that row alone, never on the unrelated
    /// selection.
    pub(crate) fn ensure_worktree_selection(
        &mut self,
        side: Side,
        path: String,
        cx: &mut Context<Self>,
    ) {
        if self.worktree.selection.contains(side, &path) {
            return;
        }
        let selected = self.worktree.selection.select_one(side, path);
        if self.worktree.apply_focus(Some(selected)) {
            self.request_worktree_diff(cx);
        } else {
            cx.notify();
        }
    }

    /// Cmd/Ctrl-A: every row of the section the selection is in.
    pub(crate) fn select_all_files(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let snapshot = session.snapshot();
        let side = match self.worktree.selection.side() {
            // A conflict is a single-file view, so select-all starts in the
            // first file section that has rows.
            Some(Side::Conflict) | None => {
                if !visible_unstaged(&snapshot.status).is_empty() {
                    Side::Unstaged
                } else if !snapshot.status.staged.is_empty() {
                    Side::Staged
                } else {
                    return;
                }
            }
            Some(side) => side,
        };
        let rows = listed_paths(&snapshot, side);
        if rows.is_empty() {
            return;
        }
        if self.worktree.select_all(side, &rows) {
            self.request_worktree_diff(cx);
        } else {
            cx.notify();
        }
    }

    /// Up/Down move the focused row; Shift extends the selection to it.
    pub(crate) fn move_file_selection(
        &mut self,
        delta: isize,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(side) = self.worktree.selection.side() else {
            return;
        };
        if side == Side::Conflict {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let rows = listed_paths(&session.snapshot(), side);
        if self.worktree.move_selection(&rows, delta, extend) {
            self.request_worktree_diff(cx);
        } else {
            cx.notify();
        }
    }

    /// `d` and the context menus: discards the whole selection on whichever
    /// side it is in.
    pub(crate) fn discard_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.worktree.selection.side() {
            Some(Side::Unstaged) => self.discard_unstaged_selection(window, cx),
            Some(Side::Staged) => self.discard_staged_selection(window, cx),
            Some(Side::Conflict) | None => {}
        }
    }

    /// Asks before throwing away the working-tree changes of the selection.
    ///
    /// The confirmation is a real dialog, not an overlay: it holds focus, so
    /// `Enter` and `Esc` belong to it and nothing behind it can act while it is
    /// open. Nothing is remembered here either, so a cancelled dialog cannot
    /// come back on a later frame.
    pub(crate) fn discard_unstaged_selection(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.worktree.selection.side() != Some(Side::Unstaged) {
            return;
        }
        self.discard_unstaged_paths(self.worktree.selection.paths(), window, cx);
    }

    pub(crate) fn discard_unstaged_path(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.discard_unstaged_paths(vec![path], window, cx);
    }

    fn discard_unstaged_paths(
        &mut self,
        paths: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }

        // `git restore` cannot drop an untracked path, so those go through
        // `git clean` instead. Both groups go out as one operation each,
        // carrying all their paths.
        let (tracked, untracked) = {
            let Some(session) = self.session.as_ref() else {
                return;
            };
            let snapshot = session.snapshot();
            let kind = |path: &str| {
                snapshot
                    .status
                    .unstaged
                    .iter()
                    .find(|change| change.path == path)
                    .map(|change| change.kind)
            };
            let mut tracked = Vec::new();
            let mut untracked = Vec::new();
            for path in &paths {
                if kind(path) == Some(ChangeKind::Untracked) {
                    untracked.push(path.clone());
                } else {
                    tracked.push(path.clone());
                }
            }
            (tracked, untracked)
        };

        let discards = !tracked.is_empty();
        let deletes = !untracked.is_empty();
        let title = match paths.as_slice() {
            [only] if !deletes => t!("worktree.discard_path", path = only).to_string(),
            [only] => t!("worktree.delete_path", path = only).to_string(),
            _ if discards => t!("worktree.discard_files", count = paths.len()).to_string(),
            _ => t!("worktree.delete_files", count = paths.len()).to_string(),
        };
        let mut operations = Vec::new();
        if discards {
            operations.push(Operation::DiscardPaths(tracked));
        }
        if deletes {
            operations.push(Operation::DeleteUntracked(untracked));
        }
        let kind = if discards {
            PendingKind::Discard
        } else {
            PendingKind::Delete
        };
        self.ask_destructive(
            window,
            cx,
            crate::ui::Confirm::new(title, discard_detail(&paths, discards, deletes)).ok_label(
                if discards {
                    t!("common.discard").to_string()
                } else {
                    t!("common.delete").to_string()
                },
            ),
            kind,
            operations,
        );
    }

    /// Asks before dropping the staged and the working-tree state of the
    /// selection. This is always confirmed: it loses edits on both sides and
    /// deletes files that only exist in the index.
    pub(crate) fn discard_staged_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.worktree.selection.side() != Some(Side::Staged) {
            return;
        }
        self.discard_staged_paths(self.worktree.selection.paths(), window, cx);
    }

    pub(crate) fn discard_staged_path(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.discard_staged_paths(vec![path], window, cx);
    }

    fn discard_staged_paths(
        &mut self,
        paths: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        let title = match paths.as_slice() {
            [only] => t!("worktree.discard_staged_path", path = only).to_string(),
            _ => t!("worktree.discard_staged_files", count = paths.len()).to_string(),
        };
        self.ask_destructive(
            window,
            cx,
            crate::ui::Confirm::new(title, discard_staged_detail(&paths))
                .ok_label(t!("common.discard").to_string())
                .always_ask(),
            PendingKind::Discard,
            vec![Operation::DiscardStagedPaths { paths }],
        );
    }

    /// Opens the confirmation dialog for `operations`, which run only when the
    /// user accepts it (or immediately, when the settings turn confirmations
    /// off).
    ///
    /// A selection that mixes tracked and untracked files needs two queued
    /// operations because core has a separate command for each; both carry all
    /// their paths, and core batches long lists itself.
    fn ask_destructive(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        confirm: crate::ui::Confirm,
        kind: PendingKind,
        operations: Vec<Operation>,
    ) {
        // A dialog hands focus back to whatever had it when it opened. The
        // context menu that usually offers these is already closing and its
        // focus handle dies with it, so return focus to whichever view owns
        // the surface that opened it.
        let focus = if self.mode == MainMode::Worktree {
            self.worktree.ensure_focus(cx)
        } else {
            self.view_focus()
        };
        window.focus(&focus, cx);
        let entity = cx.weak_entity();
        crate::ui::confirm(window, cx, confirm, move |_, cx| {
            let _ = entity.update(cx, |view, cx| {
                view.run_confirmed(kind, operations.clone(), cx);
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
                        t!("worktree.discard_lines").to_string(),
                        t!("worktree.discard_lines_detail").to_string(),
                    )
                    .ok_label(t!("common.discard").to_string()),
                    PendingKind::Discard,
                    vec![Operation::DiscardLines { path, selection }],
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
        if shift && let Some(anchor) = self.worktree.line_anchor {
            let range = picked_in_range(&self.worktree.rows, anchor, row);
            self.worktree.picked.extend(range);
        } else {
            if !self.worktree.picked.insert(key) {
                self.worktree.picked.remove(&key);
            }
            self.worktree.line_anchor = Some(row);
        }
        cx.notify();
    }

    /// Applies one press on a hidden-lines bar of the staging diff.
    ///
    /// The bar only changes what is drawn: the pick set stays keyed by hunk
    /// and line indices, so the next stage/unstage builds the same patch.
    pub(crate) fn expand_worktree_gap(
        &mut self,
        gap: usize,
        action: GapAction,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.worktree.expand_gap(gap, action) else {
            return;
        };
        self.rebuild_worktree_rows(view, cx);
    }

    /// The staging pane's "Whole file" toggle.
    pub(crate) fn toggle_worktree_whole_file(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.worktree.toggle_whole_file() else {
            // A refused toggle still has a notice to show.
            self.notify_if_active(cx);
            return;
        };
        self.rebuild_worktree_rows(view, cx);
    }

    /// Rebuilds the staging rows off the UI thread; revealing context can add
    /// one row per line of the file.
    fn rebuild_worktree_rows(&mut self, view: Arc<DiffView>, cx: &mut Context<Self>) {
        let context = self.worktree.context.clone();
        let revision = context.revision();
        cx.spawn(async move |this, cx| {
            let rows = cx
                .background_executor()
                .spawn(async move { WorktreePanel::rows_for(&view, context.expansion()) })
                .await;
            this.update(cx, |this, cx| {
                if this.worktree.context.revision() == revision {
                    this.worktree.set_rows(rows);
                    this.notify_if_active(cx);
                }
            })
            .ok();
        })
        .detach();
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

    /// Runs the operations the user confirmed in [`crate::ui::confirm`].
    fn run_confirmed(
        &mut self,
        kind: PendingKind,
        operations: Vec<Operation>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        for operation in operations {
            self.worktree.run(session, kind, operation);
        }
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
pub(crate) fn visible_unstaged(status: &WorktreeStatus) -> Vec<&FileChange> {
    status
        .unstaged
        .iter()
        .filter(|change| change.kind != ChangeKind::Conflicted)
        .collect()
}

/// The paths a section lists, in the order its rows appear. Keyboard selection
/// ranges need the same order the rendered list uses.
fn listed_paths(snapshot: &RepoSnapshot, side: Side) -> Vec<String> {
    match side {
        Side::Unstaged => visible_unstaged(&snapshot.status)
            .into_iter()
            .map(|change| change.path.clone())
            .collect(),
        Side::Staged => snapshot
            .status
            .staged
            .iter()
            .map(|change| change.path.clone())
            .collect(),
        Side::Conflict => snapshot
            .conflicts
            .iter()
            .map(|entry| entry.path.clone())
            .collect(),
    }
}

/// How many paths a discard confirmation names before summarising the rest.
const DISCARD_NAMES: usize = 5;

/// The names a confirmation lists, at most [`DISCARD_NAMES`] of them.
fn listed_names(paths: &[String]) -> String {
    let shown: Vec<&str> = paths
        .iter()
        .take(DISCARD_NAMES)
        .map(String::as_str)
        .collect();
    let mut text = shown.join(", ");
    if let Some(rest) = paths
        .len()
        .checked_sub(DISCARD_NAMES)
        .filter(|rest| *rest > 0)
    {
        text.push_str(&t!("worktree.and_more", count = rest));
    }
    text
}

/// The body of the unstaged discard confirmation: what is lost, then the
/// files it happens to.
fn discard_detail(paths: &[String], discards: bool, deletes: bool) -> String {
    let mut detail = match (discards, deletes) {
        (true, true) => t!("worktree.discard_detail.both"),
        (false, true) => t!("worktree.discard_detail.untracked"),
        _ => t!("worktree.discard_detail.changes"),
    }
    .to_string();
    if !paths.is_empty() {
        detail.push_str(&t!("worktree.files_prefix"));
        detail.push_str(&listed_names(paths));
    }
    detail
}

/// The body of the staged discard confirmation. Unlike the unstaged one this
/// drops both sides at once, which needs saying.
fn discard_staged_detail(paths: &[String]) -> String {
    let mut detail = t!("worktree.discard_staged_detail").to_string();
    if !paths.is_empty() {
        detail.push_str(&t!("worktree.files_prefix"));
        detail.push_str(&listed_names(paths));
    }
    detail
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
        crate::ui::button(
            "show-history-synthetic",
            t!("worktree.history_button").to_string(),
        )
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
            .h(colors.design.px(30.0))
            .flex_none()
            .px_3()
            .bg(colors.title_bar)
            .border_b_1()
            .border_color(colors.border)
            .child(back),
    )
    .child(crate::ui::empty_state(
        t!("worktree.synthetic_empty").to_string(),
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
        // The conflict editor names the commits a stopped rebase, cherry-pick
        // or revert works with; the banner already asked for the same details.
        let details = self.stopped_operation_details().cloned();
        self.worktree.render(
            entity,
            &snapshot,
            details.as_ref(),
            Arc::clone(&self.worktree_lfs),
            Rc::clone(&self.lfs_locks),
            self.lfs_locking_available,
            cx,
        )
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

    /// The focused file in any of the lists, for actions that carry no path of
    /// their own, such as a palette command.
    pub(crate) fn selected_path(&self) -> Option<&str> {
        self.selected
            .as_ref()
            .map(|selected| selected.path.as_str())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the panel draws the rows, the conflict editor and the commit box together"
    )]
    fn render(
        &self,
        entity: Entity<RepoView>,
        snapshot: &RepoSnapshot,
        details: Option<&InProgressDetails>,
        lfs: Arc<HashMap<String, LfsInfo>>,
        locks: Rc<HashMap<String, LfsLock>>,
        locking_available: bool,
        cx: &mut Context<RepoView>,
    ) -> AnyElement {
        let colors = Colors::from_theme(cx.theme(), Settings::get(cx));
        let selected_conflict = self.selected_conflict().map(str::to_owned);
        let full_window = self.conflicts.is_full_window();
        let mut body = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .key_context(WORKTREE_PANEL)
            .on_action(cx.listener(|this, _: &StageSelected, _, cx| this.stage_selected(cx)))
            .on_action(cx.listener(|this, _: &UnstageSelected, _, cx| this.unstage_selected(cx)))
            .on_action(cx.listener(|this, _: &DiscardSelected, window, cx| {
                this.discard_selected(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ToggleSelected, _, cx| this.toggle_worktree_selection(cx)),
            )
            .on_action(cx.listener(|this, _: &SelectAllFiles, _, cx| this.select_all_files(cx)))
            .on_action(cx.listener(|this, _: &SelectPreviousFile, _, cx| {
                this.move_file_selection(-1, false, cx)
            }))
            .on_action(
                cx.listener(|this, _: &SelectNextFile, _, cx| {
                    this.move_file_selection(1, false, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &ExtendPreviousFile, _, cx| {
                this.move_file_selection(-1, true, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ExtendNextFile, _, cx| {
                    this.move_file_selection(1, true, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &CommitMessage, _, cx| this.commit_worktree(cx)))
            // Escape leaves the whole-view merge editor first; from the
            // normal layout it still goes back to the history.
            .on_action(cx.listener(|this, _: &ShowHistory, window, cx| {
                if this.worktree.conflicts.exit_full_window() {
                    cx.notify();
                } else {
                    this.close_worktree(window, cx);
                }
            }))
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
            .on_action(cx.listener(|this, _: &NextConflict, _, cx| this.move_conflict(true, cx)))
            .on_action(
                cx.listener(|this, _: &PreviousConflict, _, cx| this.move_conflict(false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ResetConflict, window, cx| this.reset_conflict(window, cx)),
            )
            .on_action(cx.listener(|this, _: &TakeOurs, window, cx| {
                this.apply_conflict_choice(MergeChoice::Ours, window, cx)
            }))
            .on_action(cx.listener(|this, _: &TakeTheirs, window, cx| {
                this.apply_conflict_choice(MergeChoice::Theirs, window, cx)
            }))
            .on_action(cx.listener(|this, _: &TakeBothOursFirst, window, cx| {
                this.apply_conflict_choice(MergeChoice::OursThenTheirs, window, cx)
            }))
            .on_action(cx.listener(|this, _: &TakeBothTheirsFirst, window, cx| {
                this.apply_conflict_choice(MergeChoice::TheirsThenOurs, window, cx)
            }))
            .on_action(cx.listener(|this, _: &TakeBase, window, cx| {
                this.apply_conflict_choice(MergeChoice::Base, window, cx)
            }));
        if !full_window {
            body = body.child(self.lists(
                entity.clone(),
                snapshot,
                &colors,
                &lfs,
                &locks,
                locking_available,
                cx,
            ));
        }
        body = body.child(match selected_conflict {
            Some(path) => self.conflicts.pane(
                &path,
                entity.clone(),
                snapshot,
                details,
                &self.image_diff,
                cx,
            ),
            None => self.diff_pane(entity.clone(), snapshot, &colors, cx),
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

        root = root.child(body);
        if !full_window {
            root = root.child(self.commit_box(entity.clone(), snapshot, cx, &colors));
        }

        root.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    fn selected(side: Side, path: &str) -> Selected {
        Selected {
            side,
            path: path.to_owned(),
        }
    }

    #[test]
    fn a_plain_click_selects_one_row_and_a_secondary_click_toggles() {
        let rows = names(&["a", "b", "c"]);
        let mut selection = Selection::default();

        let focused = selection.click(None, Side::Unstaged, "a".to_owned(), false, false, &rows);
        assert_eq!(focused, Some(selected(Side::Unstaged, "a")));
        assert_eq!(selection.paths(), names(&["a"]));

        // Cmd-click adds a row and moves the focus to it.
        let focused = selection.click(focused, Side::Unstaged, "b".to_owned(), false, true, &rows);
        assert_eq!(focused, Some(selected(Side::Unstaged, "b")));
        assert_eq!(selection.paths(), names(&["a", "b"]));

        // Removing the focused row hands the focus back to what is left.
        let focused = selection.click(focused, Side::Unstaged, "b".to_owned(), false, true, &rows);
        assert_eq!(focused, Some(selected(Side::Unstaged, "a")));
        assert_eq!(selection.paths(), names(&["a"]));

        // Removing the last row leaves nothing to focus.
        let focused = selection.click(focused, Side::Unstaged, "a".to_owned(), false, true, &rows);
        assert_eq!(focused, None);
        assert!(selection.is_empty());
        assert_eq!(selection.side(), None);
    }

    #[test]
    fn a_secondary_click_on_another_section_replaces_the_selection() {
        let unstaged = names(&["a", "b"]);
        let staged = names(&["x", "y"]);
        let mut selection = Selection::default();
        let focused = selection.click(
            None,
            Side::Unstaged,
            "a".to_owned(),
            false,
            false,
            &unstaged,
        );
        let focused = selection.click(
            focused,
            Side::Unstaged,
            "b".to_owned(),
            false,
            true,
            &unstaged,
        );
        assert_eq!(selection.paths(), names(&["a", "b"]));

        let focused = selection.click(focused, Side::Staged, "y".to_owned(), false, true, &staged);
        assert_eq!(focused, Some(selected(Side::Staged, "y")));
        assert_eq!(selection.paths(), names(&["y"]));
        assert!(!selection.contains(Side::Unstaged, "a"));
        assert_eq!(selection.side(), Some(Side::Staged));
    }

    #[test]
    fn shift_click_selects_a_range_from_the_anchor_in_both_directions() {
        let rows = names(&["a", "b", "c", "d"]);
        let mut selection = Selection::default();
        let focused = selection.click(None, Side::Unstaged, "b".to_owned(), false, false, &rows);

        let focused = selection.click(focused, Side::Unstaged, "d".to_owned(), true, false, &rows);
        assert_eq!(focused, Some(selected(Side::Unstaged, "d")));
        assert_eq!(selection.paths(), names(&["b", "c", "d"]));

        // The anchor does not move, so extending back up flips the range.
        let focused = selection.click(focused, Side::Unstaged, "a".to_owned(), true, false, &rows);
        assert_eq!(focused, Some(selected(Side::Unstaged, "a")));
        assert_eq!(selection.paths(), names(&["a", "b"]));
    }

    #[test]
    fn a_shift_click_in_another_section_starts_a_new_selection_there() {
        let unstaged = names(&["a", "b"]);
        let staged = names(&["x", "y"]);
        let mut selection = Selection::default();
        let focused = selection.click(
            None,
            Side::Unstaged,
            "a".to_owned(),
            false,
            false,
            &unstaged,
        );
        let _ = selection.click(
            focused,
            Side::Unstaged,
            "b".to_owned(),
            true,
            false,
            &unstaged,
        );

        let focused = selection.click(None, Side::Staged, "y".to_owned(), true, false, &staged);
        assert_eq!(focused, Some(selected(Side::Staged, "y")));
        assert_eq!(selection.paths(), names(&["y"]));
        assert_eq!(selection.side(), Some(Side::Staged));
    }

    #[test]
    fn arrow_keys_move_and_shift_arrows_extend() {
        let rows = names(&["a", "b", "c"]);
        let mut selection = Selection::default();
        let focused = selection.click(None, Side::Unstaged, "b".to_owned(), false, false, &rows);

        let focused = selection.move_by(&rows, focused.as_ref(), 1, false);
        assert_eq!(focused, Some(selected(Side::Unstaged, "c")));
        assert_eq!(
            selection.paths(),
            names(&["c"]),
            "a plain move selects only the new row"
        );

        let focused = selection.move_by(&rows, focused.as_ref(), 1, false);
        assert_eq!(
            focused,
            Some(selected(Side::Unstaged, "c")),
            "the end of the list holds"
        );

        let mut selection = Selection::default();
        let focused = selection.click(None, Side::Unstaged, "a".to_owned(), false, false, &rows);
        let focused = selection.move_by(&rows, focused.as_ref(), 1, true);
        let focused = selection.move_by(&rows, focused.as_ref(), 1, true);
        assert_eq!(focused, Some(selected(Side::Unstaged, "c")));
        assert_eq!(selection.paths(), names(&["a", "b", "c"]));
    }

    #[test]
    fn select_all_takes_the_section_and_keeps_a_listed_focus() {
        let rows = names(&["a", "b"]);
        let mut selection = Selection::default();
        let focused = selection.click(None, Side::Unstaged, "b".to_owned(), false, false, &rows);

        let focused = selection.select_all(Side::Unstaged, &rows, focused.as_ref());
        assert_eq!(focused, Some(selected(Side::Unstaged, "b")));
        assert_eq!(selection.paths(), names(&["a", "b"]));

        let refreshed = names(&["x", "y", "z"]);
        let focused = selection.select_all(Side::Unstaged, &refreshed, focused.as_ref());
        assert_eq!(
            focused,
            Some(selected(Side::Unstaged, "x")),
            "a focus that vanished falls back to the first row"
        );
        assert_eq!(selection.paths(), names(&["x", "y", "z"]));
    }

    #[test]
    fn a_refresh_keeps_only_the_paths_the_section_still_lists() {
        let rows = names(&["a", "b", "c"]);
        let mut selection = Selection::default();
        let focused = selection.click(None, Side::Unstaged, "a".to_owned(), false, false, &rows);
        let focused = selection.click(focused, Side::Unstaged, "b".to_owned(), false, true, &rows);
        let _ = selection.click(focused, Side::Unstaged, "c".to_owned(), false, true, &rows);
        assert_eq!(selection.paths(), names(&["a", "b", "c"]));

        selection.retain(&names(&["a", "c", "d"]));
        assert_eq!(
            selection.paths(),
            names(&["a", "c"]),
            "b vanished, the listed paths stay"
        );
        assert!(selection.contains(Side::Unstaged, "c"));

        selection.retain(&names(&["x"]));
        assert!(selection.is_empty());
        assert_eq!(selection.side(), None);
    }

    #[test]
    fn a_refresh_that_drops_the_focused_row_falls_back_to_the_anchor() {
        let rows = names(&["a", "b", "c"]);
        let mut selection = Selection::default();
        let focused = selection.click(None, Side::Unstaged, "a".to_owned(), false, false, &rows);
        let focused = selection.click(focused, Side::Unstaged, "c".to_owned(), true, false, &rows);
        assert_eq!(focused, Some(selected(Side::Unstaged, "c")));

        selection.retain(&names(&["a", "b"]));
        assert_eq!(selection.paths(), names(&["a", "b"]));
        assert_eq!(
            selection.focus_fallback(Side::Unstaged),
            Some(selected(Side::Unstaged, "a")),
            "the range anchor is a better landing spot than the sort order"
        );
        assert_eq!(selection.focus_fallback(Side::Staged), None);
    }

    #[test]
    fn discard_confirmations_count_and_name_at_most_five_files() {
        let many: Vec<String> = (1..=7).map(|n| format!("f{n}.txt")).collect();
        assert_eq!(
            listed_names(&many),
            "f1.txt, f2.txt, f3.txt, f4.txt, f5.txt and 2 more"
        );
        assert_eq!(listed_names(&many[..1]), "f1.txt");
        assert_eq!(listed_names(&[]), "", "no names, no dangling text");

        let one = discard_detail(&many[..1], true, false);
        assert!(one.contains("working-tree changes are lost"));
        assert!(one.ends_with("f1.txt"));

        let mixed = discard_detail(&many, true, true);
        assert!(mixed.contains("untracked files are removed from disk"));
        assert!(discard_detail(&many, false, true).contains("untracked files are removed"));

        let staged = discard_staged_detail(&many);
        assert!(staged.contains("staged changes"));
        assert!(staged.contains("working tree are lost"));
        assert!(staged.contains("only in the index are deleted"));
    }
}
