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
    /// The undo this view submitted and the status line it earns on success.
    pending_undo: Option<(OperationId, String)>,
    /// An undo waiting for the fresh rows it asked for.
    undo_when_ready: bool,
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
        let (pending, note) = self.pending_undo.take()?;
        (pending == id && !failed).then_some(note)
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
) -> Option<&'static str> {
    if gate.busy {
        return Some("an operation is running");
    }
    if gate.pending {
        return Some("finish the operation in progress first");
    }
    let Some(entry) = entry else {
        return Some("nothing to undo");
    };
    if !entry.kind.can_restore() {
        return Some("this entry cannot be undone");
    }
    // A branch's own reflog can only be restored while that branch is checked
    // out; resetting another branch needs a checkout first.
    if entry.reference != "HEAD" && Some(entry.reference.as_str()) != head_branch {
        return Some("switch to that branch first");
    }
    if entry.kind.moves_branch() && gate.detached {
        return Some("HEAD is detached");
    }
    if gate.dirty {
        return Some("commit or stash changes first");
    }
    if entry.kind.moves_branch() && gate.pushed {
        return Some("already pushed");
    }
    None
}

impl RepoView {
    /// Opens the reflog list and asks for its rows.
    pub(crate) fn open_reflog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    fn refresh_reflog(&mut self, cx: &mut Context<Self>) {
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
        if self.mode == MainMode::Reflog && self.reflog.pending.is_none() {
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
            self.status_note = Some(format!("cannot undo: {reason}"));
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
            self.status_note = Some(format!("cannot undo: {reason}"));
            cx.notify();
            return;
        }
        let Some(entry) = entry else {
            return;
        };
        let note = undo_note(&entry);
        let label = format!("undo {}", kind_label(entry.kind).to_lowercase());
        if let Some(id) = self.run_operation(
            Operation::UndoReflog { entry: Some(entry) },
            label,
            window,
            cx,
        ) {
            self.reflog.pending_undo = Some((id, note));
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
            self.status_note = Some(format!("cannot restore: {reason}"));
            cx.notify();
            return;
        }
        self.focus_root(window, cx);
        let detail = restore_detail(self.session.as_ref().map(|s| s.snapshot()), &entry);
        let entity = cx.weak_entity();
        let note = undo_note(&entry);
        ui::confirm(
            window,
            cx,
            ui::Confirm::new("Restore to this point?", detail)
                .ok_label("Restore")
                // Restoring is a history rewrite, so the question is never
                // skipped by the "do not ask" setting.
                .always_ask(),
            move |window, cx| {
                let entry = entry.clone();
                let note = note.clone();
                let _ = entity.update(cx, |view, cx| {
                    let label = format!("restore before {}", kind_label(entry.kind).to_lowercase());
                    if let Some(id) = view.run_operation(
                        Operation::UndoReflog { entry: Some(entry) },
                        label,
                        window,
                        cx,
                    ) {
                        view.reflog.pending_undo = Some((id, note));
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

    /// The branch HEAD is on, for the reflog-row rules.
    fn head_branch(&self) -> Option<String> {
        let source = self.source()?;
        match source.head()? {
            HeadInfo::Branch { short_name, .. } => Some(short_name.clone()),
            HeadInfo::Detached { .. } => None,
        }
    }

    fn undo_gate(&self) -> UndoGate {
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
                ui::section_header("reflog-header", "Recent actions", design)
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
            return ui::empty_state("No reflog entries", design).into_any_element();
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
                                    "Restore to this point…",
                                    reason,
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

    fn restore_blocked(&self, entry: &ReflogEntry) -> Option<&'static str> {
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
                    .gap(px(6.0))
                    .font_family(design.mono.clone())
                    .text_size(px(design.code))
                    .text_color(design.muted)
                    .child(id_span(entry.old_id))
                    .child("→")
                    .child(id_span(entry.new_id)),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(design.muted)
                    .child(format!(
                        "{} · {} · {}",
                        entry.reference,
                        entry.committer_name,
                        style::relative_time(entry.time, style::now_epoch())
                    )),
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
        return format!(
            "Switch back to {source}, the branch checked out before \"{}\".",
            entry.message
        );
    }
    let branch = snapshot
        .as_deref()
        .and_then(|snapshot| match snapshot.head.as_ref() {
            Some(HeadInfo::Branch { short_name, .. }) => Some(short_name.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "the current branch".to_owned());
    let current = snapshot
        .as_deref()
        .and_then(|snapshot| head_target(snapshot.head.as_ref()))
        .map(data::short_oid)
        .unwrap_or_else(|| "HEAD".to_owned());
    format!(
        "{branch} moves from {current} to {}, the state before \"{}\".",
        data::short_oid(entry.old_id),
        entry.message
    )
}

fn checkout_source(message: &str) -> Option<&str> {
    for prefix in ["checkout: moving from ", "switch: moving from "] {
        if let Some(rest) = message.strip_prefix(prefix) {
            return rest.split_once(" to ").map(|(source, _)| source);
        }
    }
    None
}

/// The status line after a successful undo, naming the action and where the
/// branch landed.
fn undo_note(entry: &ReflogEntry) -> String {
    if entry.kind == ReflogKind::Checkout
        && let Some(source) = checkout_source(&entry.message)
    {
        return format!("Undid checkout: back on {source}");
    }
    format!(
        "Undid {}: {} now at {}",
        kind_label(entry.kind).to_lowercase(),
        entry.reference,
        data::short_oid(entry.old_id)
    )
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
    .h(px(ROW_HEIGHT))
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
            .gap(px(1.0))
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
                    .text_size(px(10.5))
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
pub(crate) fn kind_label(kind: ReflogKind) -> &'static str {
    match kind {
        ReflogKind::Commit => "Commit",
        ReflogKind::Amend => "Amend",
        ReflogKind::Checkout => "Checkout",
        ReflogKind::Reset => "Reset",
        ReflogKind::RebaseStart => "Rebase",
        ReflogKind::RebaseFinish => "Rebase done",
        ReflogKind::Merge => "Merge",
        ReflogKind::CherryPick => "Cherry-pick",
        ReflogKind::Pull => "Pull",
        ReflogKind::BranchCreate => "Branch",
        ReflogKind::BranchRename => "Rename",
        ReflogKind::Other => "Action",
    }
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
            Some("an operation is running")
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
            Some("finish the operation in progress first")
        );
        assert_eq!(
            blocked_reason(Some(&entry(ReflogKind::Checkout)), gate, Some("main")),
            Some("finish the operation in progress first")
        );
    }

    #[test]
    fn nothing_restorable_says_nothing_to_undo() {
        assert_eq!(
            blocked_reason(None, UndoGate::default(), Some("main")),
            Some("nothing to undo")
        );
        assert_eq!(
            blocked_reason(
                Some(&entry(ReflogKind::BranchCreate)),
                UndoGate::default(),
                Some("main")
            ),
            Some("this entry cannot be undone")
        );
        assert_eq!(
            blocked_reason(
                Some(&entry(ReflogKind::Other)),
                UndoGate::default(),
                Some("main")
            ),
            Some("this entry cannot be undone")
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
            Some("HEAD is detached")
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
            Some("commit or stash changes first")
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
            Some("already pushed")
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
            Some("switch to that branch first")
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
