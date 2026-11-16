//! The Reflog view and undo.
//!
//! The reflog is git's own record of what moved a ref, so this pane lists it
//! newest first and lets the user inspect any entry's commit with the same
//! details and diff parts the history uses. The same list is where undo reads
//! the last action from, which keeps the enablement rules and the action the
//! palette runs looking at the same entries.
//!
//! Undo itself runs through the session (`Operation::UndoReflog`) so it is
//! serialized with every other git write; this module only decides whether the
//! action is available and what to tell the user afterwards.

use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Entity, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, Window, div, px, uniform_list,
};
use gpui_component::menu::PopupMenuItem;
use strata_core::session::{Operation, OperationId, QueryTicket};
use strata_core::{HeadInfo, Oid, ReflogEntry, ReflogKind, RepoState};

use super::RepoView;
use super::history_ops::head_target;
use crate::data;
use crate::style::{self, Design};
use crate::ui;
use crate::worktree_panel::MainMode;

/// Entries read per reflog. Enough to cover a session's work; the rest of a
/// huge reflog stays on disk instead of in the pane.
const PAGE: usize = 200;

/// Height of one reflog row: two lines of text.
const ROW_HEIGHT: f32 = 46.0;

/// The undo answers this view is waiting for, and the rows it shows.
#[derive(Default)]
pub(crate) struct ReflogState {
    entries: Arc<Vec<ReflogEntry>>,
    pending: Option<QueryTicket>,
    selected: Option<usize>,
    error: Option<String>,
    /// The HEAD target the rows were read for, so a snapshot that did not move
    /// HEAD does not re-read every branch reflog.
    head: Option<Oid>,
    /// HEAD moved since the rows were read: "Undo Last Action" re-reads them
    /// first rather than acting on an entry that is no longer the last action.
    stale: bool,
    /// The undo this view submitted, with the entry it moved away from so a
    /// successful finish can offer redo.
    pending_undo: Option<(OperationId, ReflogEntry)>,
    /// An undo waiting for the fresh rows it asked for.
    undo_when_ready: bool,
    /// The redo this view submitted, waiting for its result.
    pending_redo: Option<OperationId>,
    /// The undos this view can re-apply, newest undo last.
    redo: RedoStack,
}

impl ReflogState {
    fn begin(&mut self, ticket: QueryTicket) {
        self.pending = Some(ticket);
    }

    /// Forgets the request in flight and returns its ticket for cancellation.
    pub(crate) fn cancel(&mut self) -> Option<QueryTicket> {
        self.pending.take()
    }

    fn accept(&mut self, ticket: QueryTicket, entries: Arc<Vec<ReflogEntry>>) -> bool {
        if self.pending != Some(ticket) {
            return false;
        }
        self.pending = None;
        self.error = None;
        self.stale = false;
        if self.selected.is_some_and(|index| index >= entries.len()) {
            self.selected = None;
        }
        self.entries = entries;
        true
    }

    fn fail(&mut self, ticket: QueryTicket, error: &str) -> bool {
        if self.pending != Some(ticket) {
            return false;
        }
        self.pending = None;
        self.error = Some(error.to_owned());
        true
    }

    fn entry(&self, index: usize) -> Option<&ReflogEntry> {
        self.entries.get(index)
    }

    fn selected_entry(&self) -> Option<&ReflogEntry> {
        self.selected.and_then(|index| self.entries.get(index))
    }

    /// The newest HEAD entry strata can restore, which is what "Undo last
    /// action" targets.
    fn newest_restorable(&self) -> Option<&ReflogEntry> {
        self.entries
            .iter()
            .find(|entry| entry.reference == "HEAD" && entry.kind.can_restore())
    }

    fn note_finished(&mut self, id: OperationId, failed: bool) -> Option<String> {
        if self
            .pending_undo
            .as_ref()
            .is_some_and(|(pending, _)| *pending == id)
        {
            let (_, entry) = self.pending_undo.take()?;
            if failed {
                return None;
            }
            let note = undo_note(&entry);
            self.redo.push(entry);
            return Some(note);
        }
        let pending = self.pending_redo?;
        if pending != id {
            return None;
        }
        self.pending_redo = None;
        if failed {
            return None;
        }
        let entry = self.redo.pop()?;
        Some(redo_note(&entry))
    }
}

/// The undos this view can re-apply, newest undo last.
///
/// An undo and its redo are two moves between the same two states, so the
/// entry an undo came from is everything a redo needs. A new operation clears
/// the stack: the branch is no longer where the undo left it, and a redo that
/// guessed its way around that could discard work.
#[derive(Debug, Default)]
pub(crate) struct RedoStack(Vec<ReflogEntry>);

impl RedoStack {
    fn push(&mut self, entry: ReflogEntry) {
        self.0.push(entry);
    }

    fn pop(&mut self) -> Option<ReflogEntry> {
        self.0.pop()
    }

    fn top(&self) -> Option<&ReflogEntry> {
        self.0.last()
    }

    /// Forgets every recorded redo; called when another operation starts.
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }
}

/// What the enablement rules need to know, as plain flags so every case is
/// reachable from a test.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct UndoGate {
    /// The session or this view is running an operation.
    pub busy: bool,
    /// A merge, rebase, cherry-pick, revert, bisect or `am` is in progress.
    pub pending: bool,
    /// Tracked changes in the index or the working tree.
    pub dirty: bool,
    /// A remote-tracking branch already contains the current tip.
    pub pushed: bool,
    /// HEAD is not on a branch.
    pub detached: bool,
}

/// Why the action is unavailable, in the order the checks run: the repository
/// state decides first, then the entry itself, then the worktree.
pub(crate) fn blocked_reason(
    entry: Option<&ReflogEntry>,
    gate: UndoGate,
    head_branch: Option<&str>,
) -> Option<String> {
    if gate.busy {
        return Some(t!("branch.reason.busy").to_string());
    }
    if gate.pending {
        return Some(t!("reflog.finish_first").to_string());
    }
    let Some(entry) = entry else {
        return Some(t!("reflog.nothing_to_undo").to_string());
    };
    if !entry.kind.can_restore() {
        return Some(t!("reflog.cannot_undo_entry").to_string());
    }
    // A branch's own reflog can only be restored while that branch is checked
    // out; resetting another branch needs a checkout first.
    if entry.reference != "HEAD" && Some(entry.reference.as_str()) != head_branch {
        return Some(t!("reflog.switch_branch_first").to_string());
    }
    if entry.kind.moves_branch() && gate.detached {
        return Some(t!("reflog.head_detached").to_string());
    }
    if gate.dirty {
        return Some(t!("reflog.commit_or_stash").to_string());
    }
    if entry.kind.moves_branch() && gate.pushed {
        return Some(t!("reflog.already_pushed").to_string());
    }
    None
}

/// Why redo is unavailable, in the same order the undo rules run.
///
/// A recorded undo stays a valid redo only while HEAD sits where that undo
/// left it; anything newer is a state the undo never saw.
pub(crate) fn redo_blocked_reason(
    entry: Option<&ReflogEntry>,
    gate: UndoGate,
    head: Option<Oid>,
) -> Option<String> {
    if gate.busy {
        return Some(t!("branch.reason.busy").to_string());
    }
    if gate.pending {
        return Some(t!("reflog.finish_first").to_string());
    }
    let Some(entry) = entry else {
        return Some(t!("reflog.nothing_to_redo").to_string());
    };
    if head != Some(entry.old_id) {
        return Some(t!("reflog.moved_since_undo").to_string());
    }
    None
}

impl RepoView {
    /// Opens the reflog list and asks for its rows.
    pub(crate) fn open_reflog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.refuse_if_ssh(cx) {
            return;
        }
        if self.mode == MainMode::Reflog {
            self.refresh_reflog(cx);
            cx.notify();
            return;
        }
        if self.mode == MainMode::Worktree {
            self.close_worktree(window, cx);
        }
        self.close_file_history(window, cx);
        self.mode = MainMode::Reflog;
        // The pane shows a reflog commit now; the history selection's files
        // come back through `leave_reflog`.
        self.files = None;
        self.files_error = None;
        self.selected_file = None;
        self.files_epoch = self.files_epoch.wrapping_add(1);
        self.cancel_diff();
        self.tree.close();
        self.discard_blame();
        self.reflog.selected = None;
        self.refresh_reflog(cx);
        cx.notify();
    }

    /// Leaves the reflog pane for the history view, putting the history
    /// selection back in the details area.
    pub(crate) fn leave_reflog(&mut self, cx: &mut Context<Self>) {
        if self.mode != MainMode::Reflog {
            return;
        }
        if let Some(session) = self.session.as_ref()
            && let Some(ticket) = self.reflog.cancel()
        {
            session.cancel_query(ticket);
        }
        let index = self.selected;
        let commit =
            index.and_then(|index| self.source().and_then(|source| source.commit_id(index)));
        self.focus_commit(index, commit, cx);
    }

    pub(super) fn refresh_reflog(&mut self, cx: &mut Context<Self>) {
        if self.is_ssh() {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        if let Some(ticket) = self.reflog.cancel() {
            session.cancel_query(ticket);
        }
        self.reflog.head = head_target(session.snapshot().head.as_ref());
        let ticket = session.request_reflog(PAGE);
        self.reflog.begin(ticket);
        cx.notify();
    }

    /// A snapshot published while the reflog is open: re-read the rows when
    /// HEAD moved, since an edit elsewhere cannot add a HEAD entry.
    pub(crate) fn reflog_on_snapshot(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let head = head_target(session.snapshot().head.as_ref());
        if head == self.reflog.head {
            return;
        }
        self.reflog.stale = true;
        // The title bar's undo tooltip reads these rows too, so they are kept
        // current even while the reflog pane is not the one on screen.
        if self.reflog.pending.is_none() {
            self.refresh_reflog(cx);
        }
    }

    pub(crate) fn reflog_ready(
        &mut self,
        ticket: QueryTicket,
        entries: &Arc<Vec<ReflogEntry>>,
        cx: &mut Context<Self>,
    ) {
        if !self.reflog.accept(ticket, Arc::clone(entries)) {
            return;
        }
        // Keep the row the user was on when it still exists; a fresh list
        // starts at the newest entry. The selection only drives the details
        // while the reflog pane is what is on screen.
        if self.mode == MainMode::Reflog {
            let index = self
                .reflog
                .selected
                .filter(|index| *index < entries.len())
                .or(if entries.is_empty() { None } else { Some(0) });
            self.select_reflog(index, cx);
        }
        if std::mem::take(&mut self.reflog.undo_when_ready) {
            // The rows the undo waited for are in; run it outside the entity
            // update the event arrived in.
            let entity = self.entity();
            let handle = cx
                .active_window()
                .or_else(|| cx.windows().into_iter().next());
            if let Some(handle) = handle {
                cx.defer(move |cx| {
                    let _ = handle.update(cx, |_, window, cx| {
                        entity.update(cx, |view, cx| view.run_undo_last(window, cx));
                    });
                });
            }
        }
    }

    pub(crate) fn reflog_failed(
        &mut self,
        ticket: QueryTicket,
        error: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.reflog.fail(ticket, error) {
            return false;
        }
        self.notify_if_active(cx);
        true
    }

    /// Points the details pane at one reflog row's commit.
    pub(crate) fn select_reflog(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        let index = index.filter(|index| *index < self.reflog.entries.len());
        let commit = index
            .and_then(|index| self.reflog.entry(index))
            .map(|entry| entry.new_id);
        let commit = commit.filter(|id| !id.is_null());
        if index == self.reflog.selected && commit == self.selection_commit {
            return;
        }
        self.reflog.selected = index;
        self.files = None;
        self.files_error = None;
        self.selected_file = None;
        self.files_epoch = self.files_epoch.wrapping_add(1);
        self.tree.close();
        self.cancel_diff();
        self.selection_commit = commit;
        if let Some(commit) = commit {
            self.load_files(commit, cx);
        }
        self.load_commit_stat(commit, cx);
        cx.notify();
    }

    /// "Undo Last Action": restore the state before the newest restorable HEAD
    /// entry, or say why that is not possible.
    ///
    /// The rows are read again first: an undo must target the action that is
    /// last now, not the one that was last when the pane opened.
    pub(crate) fn undo_last_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_none() {
            return;
        }
        let gate = self.undo_gate();
        if let Some(reason) = blocked_reason(None, gate, self.head_branch().as_deref())
            .filter(|_| gate.busy || gate.pending)
        {
            self.status_note = Some(t!("reflog.cannot_undo", reason = reason).to_string());
            cx.notify();
            return;
        }
        if self.reflog.entries.is_empty() || self.reflog.stale || self.reflog.pending.is_some() {
            self.reflog.undo_when_ready = true;
            self.refresh_reflog(cx);
            return;
        }
        self.run_undo_last(window, cx);
    }

    /// The part of [`Self::undo_last_action`] that runs once the rows are
    /// fresh.
    fn run_undo_last(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entry = self.reflog.newest_restorable().cloned();
        let gate = self.undo_gate();
        if let Some(reason) = blocked_reason(entry.as_ref(), gate, self.head_branch().as_deref()) {
            self.status_note = Some(t!("reflog.cannot_undo", reason = reason).to_string());
            cx.notify();
            return;
        }
        let Some(entry) = entry else {
            return;
        };
        let label = t!("reflog.undo_op", action = kind_label(entry.kind)).to_string();
        if let Some(id) = self.run_operation(
            Operation::UndoReflog {
                entry: Some(entry.clone()),
            },
            label,
            window,
            cx,
        ) {
            self.reflog.pending_undo = Some((id, entry));
        }
    }

    /// "Redo Last Action": re-applies the state the last undo moved away from.
    pub(crate) fn redo_last_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_none() {
            return;
        }
        let gate = self.undo_gate();
        let head = self.source().and_then(|source| head_target(source.head()));
        if let Some(reason) = redo_blocked_reason(self.reflog.redo.top(), gate, head) {
            self.status_note = Some(t!("reflog.cannot_redo", reason = reason).to_string());
            cx.notify();
            return;
        }
        let Some(entry) = self.reflog.redo.top().cloned() else {
            return;
        };
        let label = t!("reflog.redo_op", action = kind_label(entry.kind)).to_string();
        if let Some(id) = self.run_operation(
            Operation::RedoReflog {
                entry: entry.clone(),
            },
            label,
            window,
            cx,
        ) {
            self.reflog.pending_redo = Some(id);
        }
    }

    /// "Restore to this point…": asks where the branch moves, then restores
    /// the state before the clicked row.
    pub(crate) fn confirm_restore_reflog(
        &mut self,
        entry: ReflogEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session.is_none() {
            return;
        }
        let gate = self.undo_gate();
        if let Some(reason) = blocked_reason(Some(&entry), gate, self.head_branch().as_deref()) {
            self.status_note = Some(t!("reflog.cannot_restore", reason = reason).to_string());
            cx.notify();
            return;
        }
        self.focus_root(window, cx);
        let detail = restore_detail(self.session.as_ref().map(|s| s.snapshot()), &entry);
        let entity = cx.weak_entity();
        ui::confirm(
            window,
            cx,
            ui::Confirm::new(t!("reflog.restore_title").to_string(), detail)
                .ok_label(t!("reflog.restore_button").to_string())
                // Restoring is a history rewrite, so the question is never
                // skipped by the "do not ask" setting.
                .always_ask(),
            move |window, cx| {
                let entry = entry.clone();
                let _ = entity.update(cx, |view, cx| {
                    let label = t!("reflog.restore_op", id = kind_label(entry.kind)).to_string();
                    let redo = entry.clone();
                    if let Some(id) = view.run_operation(
                        Operation::UndoReflog { entry: Some(entry) },
                        label,
                        window,
                        cx,
                    ) {
                        view.reflog.pending_undo = Some((id, redo));
                    }
                });
            },
        );
    }

    /// The status line for a finished undo, when it succeeded.
    pub(crate) fn reflog_operation_finished(&mut self, id: OperationId, failed: bool) {
        if let Some(note) = self.reflog.note_finished(id, failed) {
            self.status_note = Some(note);
        }
    }

    /// Forgets the recorded redo. A new operation calls this, because the
    /// branch no longer sits where the undo left it.
    pub(super) fn clear_redo(&mut self) {
        self.reflog.redo.clear();
    }

    /// Whether the title bar's Undo button is enabled, and the tooltip it shows.
    ///
    /// The tooltip names the entry undo would target, so the button says what
    /// it is about to do instead of repeating its own label.
    pub(crate) fn undo_button_state(&self, window: &Window) -> (bool, String) {
        if self.is_ssh() {
            return (false, crate::remote_open::unavailable_reason());
        }
        let entry = self.reflog.newest_restorable();
        match blocked_reason(entry, self.button_gate(), self.head_branch().as_deref()) {
            Some(reason) => (
                false,
                t!("reflog.undo_unavailable", reason = reason).to_string(),
            ),
            None => {
                let Some(entry) = entry else {
                    return (false, t!("reflog.nothing_to_undo").to_string());
                };
                let shortcut = self.action_shortcut(&crate::actions::UndoLastAction, window);
                (
                    true,
                    match shortcut {
                        Some(shortcut) => t!(
                            "reflog.undo_tooltip",
                            action = action_summary(entry),
                            shortcut = shortcut
                        ),
                        None => t!("reflog.undo_tooltip_plain", action = action_summary(entry)),
                    }
                    .to_string(),
                )
            }
        }
    }

    /// Whether the title bar's Redo button is enabled, and the tooltip it shows.
    pub(crate) fn redo_button_state(&self, window: &Window) -> (bool, String) {
        if self.is_ssh() {
            return (false, crate::remote_open::unavailable_reason());
        }
        let entry = self.reflog.redo.top();
        let head = self.source().and_then(|source| head_target(source.head()));
        match redo_blocked_reason(entry, self.button_gate(), head) {
            Some(reason) => (
                false,
                t!("reflog.redo_unavailable", reason = reason).to_string(),
            ),
            None => {
                let Some(entry) = entry else {
                    return (false, t!("reflog.nothing_to_redo").to_string());
                };
                let shortcut = self.action_shortcut(&crate::actions::RedoLastAction, window);
                (
                    true,
                    match shortcut {
                        Some(shortcut) => t!(
                            "reflog.redo_tooltip",
                            action = action_summary(entry),
                            shortcut = shortcut
                        ),
                        None => t!("reflog.redo_tooltip_plain", action = action_summary(entry)),
                    }
                    .to_string(),
                )
            }
        }
    }

    /// A shortcut as the platform spells it, read from the live keymap.
    pub(crate) fn action_shortcut(
        &self,
        action: &dyn gpui::Action,
        window: &Window,
    ) -> Option<String> {
        let os = if cfg!(target_os = "macos") {
            gpui::OperatingSystem::Mac
        } else if cfg!(target_os = "windows") {
            gpui::OperatingSystem::Windows
        } else {
            gpui::OperatingSystem::Linux
        };
        window
            .bindings_for_action_in(action, &self.focus)
            .last()
            .map(|binding| crate::palette::shortcut(binding, os))
    }

    /// The branch HEAD is on, for the reflog-row rules.
    fn head_branch(&self) -> Option<String> {
        let source = self.source()?;
        match source.head()? {
            HeadInfo::Branch { short_name, .. } => Some(short_name.clone()),
            HeadInfo::Detached { .. } => None,
        }
    }

    pub(super) fn undo_gate(&self) -> UndoGate {
        let Some(session) = self.session.as_ref() else {
            return UndoGate {
                busy: true,
                ..UndoGate::default()
            };
        };
        let snapshot = session.snapshot();
        UndoGate {
            busy: self.is_busy() || self.remote.is_running() || self.in_progress.is_running(),
            pending: snapshot.state != RepoState::Normal,
            dirty: tracked_changes(&snapshot.status),
            pushed: head_target(snapshot.head.as_ref())
                .is_some_and(|head| !self.pushed_remotes_for(&[head]).is_empty()),
            detached: matches!(snapshot.head, Some(HeadInfo::Detached { .. })),
        }
    }

    /// The gate for the title bar's buttons, which is read every frame.
    ///
    /// It skips the ancestry walk behind `pushed` and the worktree comparison:
    /// the click runs the full rules and reports what they find in the status
    /// bar, while the button only needs to know whether there is anything to
    /// offer at all.
    fn button_gate(&self) -> UndoGate {
        let Some(session) = self.session.as_ref() else {
            return UndoGate {
                busy: true,
                ..UndoGate::default()
            };
        };
        let snapshot = session.snapshot();
        UndoGate {
            busy: self.is_busy() || self.remote.is_running() || self.in_progress.is_running(),
            pending: snapshot.state != RepoState::Normal,
            dirty: false,
            pushed: false,
            detached: matches!(snapshot.head, Some(HeadInfo::Detached { .. })),
        }
    }

    /// The list above the details pane.
    pub(super) fn reflog_view(
        &self,
        entity: Entity<Self>,
        design: &Design,
        cx: &mut App,
    ) -> AnyElement {
        let list_pane = div()
            .flex()
            .flex_col()
            .w_full()
            .min_w_0()
            .flex_grow(0.0)
            .flex_shrink(0.0)
            .flex_basis(gpui::relative(0.45))
            .border_b_1()
            .border_color(design.border)
            .child(
                ui::section_header(
                    "reflog-header",
                    t!("reflog.recent_actions").to_string(),
                    design,
                )
                .count(self.reflog.entries.len()),
            )
            .child(self.reflog_list(entity, design));
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(list_pane)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(self.diff_pane(design, cx)),
            )
            .into_any_element()
    }

    fn reflog_list(&self, entity: Entity<Self>, design: &Design) -> AnyElement {
        if let Some(error) = &self.reflog.error {
            return ui::message(error.clone(), design.remove_text, design).into_any_element();
        }
        if self.reflog.entries.is_empty() {
            return ui::empty_state(t!("reflog.empty").to_string(), design).into_any_element();
        }

        let rows = Arc::clone(&self.reflog.entries);
        let selected = self.reflog.selected;
        let now = style::now_epoch();
        let row_design = design.clone();
        let list_entity = entity;
        uniform_list("reflog-entries", rows.len(), move |range, _window, _cx| {
            range
                .map(|index| {
                    let entity = list_entity.clone();
                    let entry = rows[index].clone();
                    let design = row_design.clone();
                    let element = reflog_row(index, &entry, selected == Some(index), now, &design)
                        .on_click(move |_, _, cx| {
                            let entity = entity.clone();
                            entity.update(cx, |view, cx| view.select_reflog(Some(index), cx));
                        });
                    let menu_entry = entry.clone();
                    let menu_entity = list_entity.clone();
                    ui::context_menu(
                        ("reflog-row-menu", index),
                        &design,
                        element,
                        move |menu, _window, cx| {
                            let reason = menu_entity.read(cx).restore_blocked(&menu_entry);
                            let entity = menu_entity.clone();
                            let entry = menu_entry.clone();
                            let handler =
                                move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                                    let entry = entry.clone();
                                    entity.update(cx, |view, cx| {
                                        view.confirm_restore_reflog(entry, window, cx);
                                    });
                                };
                            menu.item(
                                PopupMenuItem::new(ui::menu_label(
                                    t!("reflog.restore_menu").to_string(),
                                    reason.as_ref(),
                                ))
                                .disabled(reason.is_some())
                                .on_click(handler),
                            )
                        },
                    )
                    .w_full()
                    .into_any_element()
                })
                .collect::<Vec<_>>()
        })
        .flex_1()
        .min_h_0()
        .into_any_element()
    }

    fn restore_blocked(&self, entry: &ReflogEntry) -> Option<String> {
        blocked_reason(Some(entry), self.undo_gate(), self.head_branch().as_deref())
    }

    /// The details column for the selected reflog row, replacing the commit
    /// block while the reflog is open.
    pub(super) fn reflog_details(&self, design: &Design) -> AnyElement {
        let Some(entry) = self.reflog.selected_entry() else {
            return div().into_any_element();
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_b_1()
            .border_color(design.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(ui::status_badge(
                        kind_label(entry.kind),
                        kind_tone(entry.kind),
                        design,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(design.text)
                            .child(entry.message.clone()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(design.px(6.0))
                    .font_family(design.mono.clone())
                    .text_size(px(design.code))
                    .text_color(design.muted)
                    .child(id_span(entry.old_id))
                    .child("→")
                    .child(id_span(entry.new_id)),
            )
            .child(
                div()
                    .text_size(design.px(11.0))
                    .text_color(design.muted)
                    .child(
                        t!(
                            "reflog.entry_detail",
                            reference = entry.reference,
                            author = entry.committer_name,
                            time = style::relative_time(entry.time, style::now_epoch())
                        )
                        .to_string(),
                    ),
            )
            .into_any_element()
    }
}

/// The pure part of the details column's "where it moves" sentence.
fn restore_detail(
    snapshot: Option<Arc<strata_core::session::RepoSnapshot>>,
    entry: &ReflogEntry,
) -> String {
    if entry.kind == ReflogKind::Checkout
        && let Some(source) = checkout_source(&entry.message)
    {
        return t!(
            "reflog.restore_switch",
            source = source,
            message = entry.message
        )
        .to_string();
    }
    let branch = snapshot
        .as_deref()
        .and_then(|snapshot| match snapshot.head.as_ref() {
            Some(HeadInfo::Branch { short_name, .. }) => Some(short_name.clone()),
            _ => None,
        })
        .unwrap_or_else(|| t!("reflog.current_branch").to_string());
    let current = snapshot
        .as_deref()
        .and_then(|snapshot| head_target(snapshot.head.as_ref()))
        .map(data::short_oid)
        .unwrap_or_else(|| "HEAD".to_owned());
    t!(
        "reflog.move_detail",
        branch = branch,
        current = current,
        target = data::short_oid(entry.old_id),
        message = entry.message
    )
    .to_string()
}

fn checkout_source(message: &str) -> Option<&str> {
    for prefix in ["checkout: moving from ", "switch: moving from "] {
        if let Some(rest) = message.strip_prefix(prefix) {
            return rest.split_once(" to ").map(|(source, _)| source);
        }
    }
    None
}

/// The ref a checkout moved to, which is where redoing that checkout goes.
fn checkout_destination(message: &str) -> Option<&str> {
    for prefix in ["checkout: moving from ", "switch: moving from "] {
        if let Some(rest) = message.strip_prefix(prefix) {
            return rest.split_once(" to ").map(|(_, target)| target);
        }
    }
    None
}

/// One reflog entry as a short phrase for the undo and redo tooltips: the kind
/// and git's own message without its `prefix: `.
fn action_summary(entry: &ReflogEntry) -> String {
    let detail = entry
        .message
        .split_once(": ")
        .map_or(entry.message.as_str(), |(_, detail)| detail);
    t!(
        "reflog.action_summary",
        action = kind_label(entry.kind),
        detail = detail
    )
    .to_string()
}

/// The status line after a successful undo, naming the action and where the
/// branch landed.
fn undo_note(entry: &ReflogEntry) -> String {
    if entry.kind == ReflogKind::Checkout
        && let Some(source) = checkout_source(&entry.message)
    {
        return t!("reflog.undid_checkout", source = source).to_string();
    }
    t!(
        "reflog.undid",
        action = kind_label(entry.kind).to_lowercase(),
        reference = entry.reference,
        id = data::short_oid(entry.old_id)
    )
    .to_string()
}

/// The status line after a successful redo, the mirror of [`undo_note`].
fn redo_note(entry: &ReflogEntry) -> String {
    if entry.kind == ReflogKind::Checkout
        && let Some(destination) = checkout_destination(&entry.message)
    {
        return t!("reflog.redid_checkout", destination = destination).to_string();
    }
    t!(
        "reflog.redid",
        action = kind_label(entry.kind).to_lowercase(),
        reference = entry.reference,
        id = data::short_oid(entry.new_id)
    )
    .to_string()
}

/// Whether the worktree status has changes to tracked files. Untracked files
/// alone do not make an undo unsafe.
fn tracked_changes(status: &strata_core::WorktreeStatus) -> bool {
    status
        .staged
        .iter()
        .chain(status.unstaged.iter())
        .any(|change| change.kind != strata_core::ChangeKind::Untracked)
}

fn reflog_row(
    index: usize,
    entry: &ReflogEntry,
    selected: bool,
    now: i64,
    design: &Design,
) -> gpui::Stateful<gpui::Div> {
    let mut second = format!(
        "{} → {}  ·  {}",
        short_or_dash(entry.old_id),
        data::short_oid(entry.new_id),
        style::relative_time(entry.time, now)
    );
    if entry.reference != "HEAD" {
        second.push_str("  ·  ");
        second.push_str(&entry.reference);
    }
    ui::list_row(
        ("reflog-row", index),
        ui::RowState::NORMAL.selected(selected),
        design,
    )
    .h(design.px(ROW_HEIGHT))
    .cursor_pointer()
    .child(ui::status_badge(
        kind_label(entry.kind),
        kind_tone(entry.kind),
        design,
    ))
    .child(
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .gap(design.px(1.0))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(design.text)
                    .child(entry.message.clone()),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(design.mono.clone())
                    .text_size(design.px(10.5))
                    .text_color(design.muted)
                    .child(second),
            ),
    )
}

/// A short id, with a dash for the null id a reflog records when a ref was
/// created.
fn short_or_dash(id: Oid) -> String {
    if id.is_null() {
        "—".to_owned()
    } else {
        data::short_oid(id)
    }
}

fn id_span(id: Oid) -> AnyElement {
    let text = if id.is_null() {
        "—".to_owned()
    } else {
        data::short_oid(id)
    };
    div().child(text).into_any_element()
}

/// The badge word for one kind.
pub(crate) fn kind_label(kind: ReflogKind) -> String {
    match kind {
        ReflogKind::Commit => t!("reflog.kind.commit"),
        ReflogKind::Amend => t!("reflog.kind.amend"),
        ReflogKind::Checkout => t!("reflog.kind.checkout"),
        ReflogKind::Reset => t!("reflog.kind.reset"),
        ReflogKind::RebaseStart => t!("reflog.kind.rebase"),
        ReflogKind::RebaseFinish => t!("reflog.kind.rebase_done"),
        ReflogKind::Merge => t!("reflog.kind.merge"),
        ReflogKind::CherryPick => t!("reflog.kind.cherry_pick"),
        ReflogKind::Pull => t!("reflog.kind.pull"),
        ReflogKind::BranchCreate => t!("reflog.kind.branch"),
        ReflogKind::BranchRename => t!("reflog.kind.rename"),
        ReflogKind::Other => t!("reflog.kind.other"),
    }
    .to_string()
}

fn kind_tone(kind: ReflogKind) -> ui::BadgeTone {
    match kind {
        ReflogKind::Commit | ReflogKind::Amend | ReflogKind::CherryPick | ReflogKind::Pull => {
            ui::BadgeTone::Accent
        }
        ReflogKind::Reset
        | ReflogKind::RebaseStart
        | ReflogKind::RebaseFinish
        | ReflogKind::Merge => ui::BadgeTone::Warning,
        ReflogKind::Checkout
        | ReflogKind::BranchCreate
        | ReflogKind::BranchRename
        | ReflogKind::Other => ui::BadgeTone::Neutral,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: ReflogKind) -> ReflogEntry {
        ReflogEntry {
            reference: "HEAD".into(),
            old_id: "1111111111111111111111111111111111111111"
                .parse()
                .expect("oid"),
            new_id: "2222222222222222222222222222222222222222"
                .parse()
                .expect("oid"),
            committer_name: "Strata Test".into(),
            committer_email: "strata@example.invalid".into(),
            time: 1_700_000_000,
            message: "commit: two".into(),
            kind,
        }
    }

    #[test]
    fn a_clean_repository_can_undo_a_commit() {
        assert_eq!(
            blocked_reason(
                Some(&entry(ReflogKind::Commit)),
                UndoGate::default(),
                Some("main")
            ),
            None
        );
    }

    #[test]
    fn a_running_operation_wins_over_every_other_reason() {
        let gate = UndoGate {
            busy: true,
            pending: true,
            dirty: true,
            pushed: true,
            detached: true,
        };
        assert_eq!(
            blocked_reason(Some(&entry(ReflogKind::Commit)), gate, Some("main")),
            Some(t!("branch.reason.busy").to_string())
        );
    }

    #[test]
    fn an_operation_in_progress_blocks_undo() {
        let gate = UndoGate {
            pending: true,
            ..UndoGate::default()
        };
        assert_eq!(
            blocked_reason(Some(&entry(ReflogKind::Commit)), gate, Some("main")),
            Some(t!("reflog.finish_first").to_string())
        );
        assert_eq!(
            blocked_reason(Some(&entry(ReflogKind::Checkout)), gate, Some("main")),
            Some(t!("reflog.finish_first").to_string())
        );
    }

    #[test]
    fn nothing_restorable_says_nothing_to_undo() {
        assert_eq!(
            blocked_reason(None, UndoGate::default(), Some("main")),
            Some(t!("reflog.nothing_to_undo").to_string())
        );
        assert_eq!(
            blocked_reason(
                Some(&entry(ReflogKind::BranchCreate)),
                UndoGate::default(),
                Some("main")
            ),
            Some(t!("reflog.cannot_undo_entry").to_string())
        );
        assert_eq!(
            blocked_reason(
                Some(&entry(ReflogKind::Other)),
                UndoGate::default(),
                Some("main")
            ),
            Some(t!("reflog.cannot_undo_entry").to_string())
        );
    }

    #[test]
    fn a_branch_move_needs_a_branch_and_a_clean_tree() {
        let detached = UndoGate {
            detached: true,
            ..UndoGate::default()
        };
        assert_eq!(
            blocked_reason(Some(&entry(ReflogKind::Commit)), detached, Some("main")),
            Some(t!("reflog.head_detached").to_string())
        );
        // A checkout can still be undone from a detached HEAD: undoing it
        // switches back to the branch it left.
        assert_eq!(
            blocked_reason(Some(&entry(ReflogKind::Checkout)), detached, Some("main")),
            None
        );
        let dirty = UndoGate {
            dirty: true,
            ..UndoGate::default()
        };
        assert_eq!(
            blocked_reason(Some(&entry(ReflogKind::Commit)), dirty, Some("main")),
            Some(t!("reflog.commit_or_stash").to_string())
        );
    }

    #[test]
    fn published_history_is_not_rewritten_but_a_checkout_is_still_allowed() {
        let pushed = UndoGate {
            pushed: true,
            ..UndoGate::default()
        };
        assert_eq!(
            blocked_reason(Some(&entry(ReflogKind::Commit)), pushed, Some("main")),
            Some(t!("reflog.already_pushed").to_string())
        );
        assert_eq!(
            blocked_reason(Some(&entry(ReflogKind::Checkout)), pushed, Some("main")),
            None,
            "switching branches does not rewrite the pushed commit"
        );
    }

    #[test]
    fn another_branchs_entry_needs_that_branch_checked_out() {
        let mut branch = entry(ReflogKind::Commit);
        branch.reference = "topic".into();
        assert_eq!(
            blocked_reason(Some(&branch), UndoGate::default(), Some("main")),
            Some(t!("reflog.switch_branch_first").to_string())
        );
        assert_eq!(
            blocked_reason(Some(&branch), UndoGate::default(), Some("topic")),
            None,
            "the branch's own entry is fine while it is checked out"
        );
    }

    #[test]
    fn the_status_line_names_the_undone_action() {
        let mut commit = entry(ReflogKind::Commit);
        commit.message = "commit: two".into();
        assert_eq!(undo_note(&commit), "Undid commit: HEAD now at 1111111");

        let mut checkout = entry(ReflogKind::Checkout);
        checkout.message = "checkout: moving from main to topic".into();
        assert_eq!(undo_note(&checkout), "Undid checkout: back on main");
    }

    #[test]
    fn a_redo_names_the_restored_action() {
        let mut commit = entry(ReflogKind::Commit);
        commit.message = "commit: two".into();
        assert_eq!(redo_note(&commit), "Redid commit: HEAD now at 2222222");

        let mut checkout = entry(ReflogKind::Checkout);
        checkout.message = "checkout: moving from main to topic".into();
        assert_eq!(redo_note(&checkout), "Redid checkout: back on topic");
    }

    #[test]
    fn the_redo_stack_keeps_the_newest_undo_on_top() {
        let mut stack = RedoStack::default();
        let mut first = entry(ReflogKind::Commit);
        first.old_id = "3333333333333333333333333333333333333333"
            .parse()
            .expect("oid");
        let second = entry(ReflogKind::Reset);

        stack.push(first.clone());
        stack.push(second.clone());
        assert_eq!(stack.len(), 2);
        assert_eq!(stack.top(), Some(&second));
        assert_eq!(stack.pop(), Some(second));
        assert_eq!(stack.pop(), Some(first));
        assert_eq!(stack.pop(), None, "an empty stack has nothing to redo");
    }

    #[test]
    fn a_new_operation_clears_the_redo_stack() {
        let mut stack = RedoStack::default();
        stack.push(entry(ReflogKind::Commit));
        stack.clear();
        assert_eq!(stack.len(), 0);
        assert_eq!(stack.top(), None);
    }

    #[test]
    fn redo_is_unavailable_without_a_record_or_after_the_branch_moved() {
        let record = entry(ReflogKind::Commit);
        let head = Some(record.old_id);
        assert_eq!(
            redo_blocked_reason(None, UndoGate::default(), head),
            Some(t!("reflog.nothing_to_redo").to_string())
        );
        assert_eq!(
            redo_blocked_reason(Some(&record), UndoGate::default(), head),
            None,
            "HEAD still sits where the undo left it"
        );
        assert_eq!(
            redo_blocked_reason(Some(&record), UndoGate::default(), Some(record.new_id)),
            Some(t!("reflog.moved_since_undo").to_string())
        );
        let busy = UndoGate {
            busy: true,
            ..UndoGate::default()
        };
        assert_eq!(
            redo_blocked_reason(Some(&record), busy, head),
            Some(t!("branch.reason.busy").to_string())
        );
        let pending = UndoGate {
            pending: true,
            ..UndoGate::default()
        };
        assert_eq!(
            redo_blocked_reason(Some(&record), pending, head),
            Some(t!("reflog.finish_first").to_string())
        );
    }

    #[test]
    fn a_tooltip_phrase_names_the_kind_and_gits_message() {
        let mut commit = entry(ReflogKind::Commit);
        commit.message = "commit: two".into();
        assert_eq!(action_summary(&commit), "Commit “two”");

        let mut checkout = entry(ReflogKind::Checkout);
        checkout.message = "checkout: moving from main to topic".into();
        assert_eq!(
            action_summary(&checkout),
            "Checkout “moving from main to topic”"
        );
    }

    #[test]
    fn a_restore_says_where_the_branch_moves() {
        let commit = entry(ReflogKind::Commit);
        let detail = restore_detail(None, &commit);
        assert!(detail.contains("1111111"), "{detail}");
        assert!(detail.contains("commit: two"), "{detail}");
    }

    #[test]
    fn only_untracked_files_are_not_a_dirty_tree() {
        use strata_core::{ChangeKind, FileChange, WorktreeStatus};
        let untracked = WorktreeStatus {
            staged: Vec::new(),
            unstaged: vec![FileChange {
                path: "new.txt".into(),
                old_path: None,
                kind: ChangeKind::Untracked,
            }],
        };
        assert!(!tracked_changes(&untracked));
        let modified = WorktreeStatus {
            staged: Vec::new(),
            unstaged: vec![FileChange {
                path: "a.txt".into(),
                old_path: None,
                kind: ChangeKind::Modified,
            }],
        };
        assert!(tracked_changes(&modified));
    }
}
