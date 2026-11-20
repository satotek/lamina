//! The diff of a multi-selection in History.
//!
//! Several selected commits compare the ends of their range: the oldest
//! selection's first parent (or the empty tree for a root commit) against the
//! newest commit. `Repo::selection_range` plans that pair; the details pane
//! then shows the summary block, the changed-file list and the selected file's
//! diff in the same split a single commit uses, so whole-file, context
//! expansion, image diffs and the encoding label all work unchanged.
//!
//! The plan is read off the UI thread, and the comparison and each file's diff
//! go through the session's compare query and diff workers. A plan belongs to
//! an epoch and a comparison to a ticket, so a late answer for a selection the
//! user has already left cannot replace the view. Nothing here reads a
//! repository on the UI thread, so a range spanning a long history stays
//! responsive.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::Context;
use lamina_core::session::{
    Comparison, QueryTicket, SelectionRange, SelectionRangeKind, SelectionRangeMode,
};
use lamina_core::{FileChange, Oid};

use super::RepoView;

/// Where the range of the current multi-selection stands.
#[derive(Default)]
pub(super) struct SelectionDiff {
    /// The selection this state describes, newest first. An equal selection
    /// means the plan is already the right one, so a repaint does not plan and
    /// request it again.
    selection: Vec<Oid>,
    /// Counts plans; a plan that finishes after the selection moved on names an
    /// older epoch and is dropped.
    epoch: u64,
    plan: Option<SelectionRange>,
    /// Which pair the two-commit case is showing. Everything else has one.
    mode: SelectionRangeMode,
    /// The comparison in flight. Only the answer for this ticket is accepted.
    ticket: Option<QueryTicket>,
    comparison: Option<Arc<Comparison>>,
    error: Option<String>,
}

impl SelectionDiff {
    /// Drops everything and returns the query the session should cancel.
    fn clear(&mut self) -> Option<QueryTicket> {
        self.selection.clear();
        self.plan = None;
        self.mode = SelectionRangeMode::default();
        self.comparison = None;
        self.error = None;
        self.ticket.take()
    }

    /// Accepts the comparison only when it answers the newest request.
    fn accept(&mut self, ticket: QueryTicket, comparison: &Arc<Comparison>) -> bool {
        if self.ticket != Some(ticket) {
            return false;
        }
        self.ticket = None;
        self.comparison = Some(Arc::clone(comparison));
        self.error = None;
        true
    }

    /// The two-commit case may switch between the range and the endpoints.
    pub(super) fn offers_mode_toggle(&self) -> bool {
        self.plan
            .is_some_and(|plan| plan.kind == SelectionRangeKind::TwoEndpoints)
    }

    /// The pair the diff is taken between, with the left side `None` for the
    /// empty tree.
    pub(super) fn range(&self) -> Option<(Option<Oid>, Oid)> {
        self.plan.map(|plan| (plan.base(self.mode), plan.head))
    }

    pub(super) fn plan(&self) -> Option<SelectionRange> {
        self.plan
    }

    pub(super) fn mode(&self) -> SelectionRangeMode {
        self.mode
    }

    pub(super) fn comparison(&self) -> Option<&Arc<Comparison>> {
        self.comparison.as_ref()
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The count a "includes N commits you did not select" note names.
    pub(super) fn skipped(&self) -> Option<usize> {
        match self.plan?.kind {
            SelectionRangeKind::Gapped { skipped } if skipped > 0 => Some(skipped),
            _ => None,
        }
    }

    /// True when the ends are not on one line of history.
    pub(super) fn diverged(&self) -> bool {
        self.plan
            .is_some_and(|plan| plan.kind == SelectionRangeKind::Diverged)
    }
}

impl RepoView {
    /// True while the details pane shows a multi-selection's range.
    pub(super) fn range_diff_active(&self) -> bool {
        self.selection.is_multi() && self.selection_diff.plan.is_some()
    }

    /// Plans the current multi-selection, unless the range already on screen is
    /// for it. Cheap to call from every selection change and repaint.
    pub(super) fn sync_selection_diff(&mut self, cx: &mut Context<Self>) {
        if !self.selection.is_multi() || self.smoke || self.session.is_none() {
            self.drop_selection_diff();
            return;
        }
        let Some(source) = self.source() else {
            self.drop_selection_diff();
            return;
        };
        let selected: Vec<Oid> = self
            .selection
            .rows()
            .filter_map(|index| source.commit_id(index))
            .collect();
        if selected.len() < 2 {
            self.drop_selection_diff();
            return;
        }
        if self.selection_diff.selection == selected {
            return;
        }
        self.start_selection_diff(selected, cx);
    }

    /// Forgets the range, cancelling its comparison and any plan still
    /// running.
    pub(super) fn drop_selection_diff(&mut self) {
        let ticket = self.selection_diff.clear();
        if let (Some(session), Some(ticket)) = (self.session.as_ref(), ticket) {
            session.cancel_query(ticket);
        }
    }

    fn start_selection_diff(&mut self, selected: Vec<Oid>, cx: &mut Context<Self>) {
        self.drop_selection_diff();
        self.selection_diff.selection = selected.clone();
        self.selection_diff.epoch = self.selection_diff.epoch.wrapping_add(1);
        let epoch = self.selection_diff.epoch;
        // The focused single commit's files and totals must not stand in while
        // the range is planned.
        self.reset_commit_pane();
        let Some(repo) = self.session.as_ref().map(|session| session.reader()) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let planned = cx
                .background_executor()
                .spawn(async move {
                    repo.selection_range(&selected)
                        .map_err(|error| error.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                if this.selection_diff.epoch != epoch || !this.selection.is_multi() {
                    return;
                }
                match planned {
                    Ok(Some(plan)) => {
                        this.selection_diff.plan = Some(plan);
                        this.request_selection_comparison(cx);
                    }
                    Ok(None) => {
                        this.drop_selection_diff();
                        this.notify_if_active(cx);
                    }
                    Err(error) => {
                        this.selection_diff.error = Some(error);
                        this.notify_if_active(cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Asks for the comparison of the planned pair, with the mode the summary
    /// block is showing.
    fn request_selection_comparison(&mut self, cx: &mut Context<Self>) {
        let Some(plan) = self.selection_diff.plan else {
            return;
        };
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let base = plan.base(self.selection_diff.mode);
        let ticket = session.request_compare_from(base, plan.head);
        if let Some(previous) = self.selection_diff.ticket.replace(ticket) {
            session.cancel_query(previous);
        }
        self.selection_diff.comparison = None;
        self.selection_diff.error = None;
        self.reset_commit_pane();
        self.notify_if_active(cx);
    }

    /// Drops the files, totals and diff a single commit left in the pane.
    ///
    /// `files_epoch` moves too: a file list already being read for the focused
    /// commit must not land on the range.
    fn reset_commit_pane(&mut self) {
        self.files_epoch = self.files_epoch.wrapping_add(1);
        self.files = None;
        self.files_lfs = Arc::new(HashMap::new());
        self.files_error = None;
        self.selected_file = None;
        self.cancel_diff();
    }

    /// The comparison answered, for `ticket` only.
    pub(super) fn selection_comparison_ready(
        &mut self,
        ticket: QueryTicket,
        comparison: &Arc<Comparison>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.selection_diff.accept(ticket, comparison) {
            return false;
        }
        let previous_path = self
            .files
            .as_ref()
            .and_then(|files| self.selected_file.and_then(|index| files.get(index)))
            .map(|file| file.path.clone());
        let files: Vec<FileChange> = comparison
            .files
            .iter()
            .map(|file| FileChange {
                path: file.stat.path.clone(),
                old_path: file.stat.old_path.clone(),
                kind: file.kind,
            })
            .collect();
        self.files = Some(files);
        self.files_lfs = Arc::new(HashMap::new());
        self.files_error = None;
        if comparison.files.is_empty() {
            self.selected_file = None;
            self.cancel_diff();
            self.notify_if_active(cx);
            return true;
        }
        // Keep looking at the same file when the mode changed the comparison.
        let preferred = previous_path
            .and_then(|path| {
                comparison
                    .files
                    .iter()
                    .position(|file| file.stat.path == path)
            })
            .unwrap_or(0);
        self.select_file(preferred, cx);
        true
    }

    pub(super) fn selection_comparison_failed(
        &mut self,
        ticket: QueryTicket,
        error: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.selection_diff.ticket != Some(ticket) {
            return false;
        }
        self.selection_diff.ticket = None;
        self.selection_diff.comparison = None;
        self.selection_diff.error = Some(error.to_owned());
        self.reset_commit_pane();
        self.notify_if_active(cx);
        true
    }

    /// The two-commit toggle between "Range changes" and "Between these two".
    pub(super) fn set_selection_range_mode(
        &mut self,
        mode: SelectionRangeMode,
        cx: &mut Context<Self>,
    ) {
        if self.selection_diff.mode == mode || !self.selection_diff.offers_mode_toggle() {
            return;
        }
        self.selection_diff.mode = mode;
        self.request_selection_comparison(cx);
    }

    /// "Open in Compare": the full compare view for the same endpoints.
    pub(super) fn open_selection_range_compare(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let Some(plan) = self.selection_diff.plan else {
            return;
        };
        let base = plan.base(self.selection_diff.mode);
        self.open_compare_range(base, plan.head, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamina_core::Oid;
    use lamina_core::session::QueryTicket;

    fn oid(byte: u8) -> Oid {
        let mut bytes = [0u8; 20];
        bytes[19] = byte;
        Oid::from(bytes)
    }

    fn comparison(head: Oid) -> Arc<Comparison> {
        Arc::new(Comparison {
            base: None,
            head,
            merge_base: None,
            ahead: 0,
            behind: 0,
            files: Vec::new(),
            added: 0,
            removed: 0,
        })
    }

    #[test]
    fn a_late_comparison_for_an_old_ticket_is_ignored() {
        let mut diff = SelectionDiff {
            selection: vec![oid(2), oid(1)],
            ticket: Some(QueryTicket(7)),
            ..Default::default()
        };

        let stale = comparison(oid(9));
        assert!(
            !diff.accept(QueryTicket(6), &stale),
            "an answer for a superseded request must not replace the view"
        );
        assert!(diff.comparison.is_none());

        let fresh = comparison(oid(2));
        assert!(diff.accept(QueryTicket(7), &fresh));
        assert_eq!(diff.comparison.as_ref().map(|c| c.head), Some(oid(2)));
        assert_eq!(diff.ticket, None, "an answered ticket is no longer current");
        assert!(
            !diff.accept(QueryTicket(7), &fresh),
            "it cannot answer twice"
        );
    }

    #[test]
    fn a_new_selection_drops_the_previous_ticket_and_comparison() {
        let mut diff = SelectionDiff {
            selection: vec![oid(2), oid(1)],
            ticket: Some(QueryTicket(3)),
            ..Default::default()
        };
        diff.accept(QueryTicket(3), &comparison(oid(2)));

        // Moving on stores a new ticket; the old answer can no longer land.
        diff.ticket = Some(QueryTicket(4));
        assert!(!diff.accept(QueryTicket(3), &comparison(oid(2))));

        // Clearing releases the in-flight request for the session to cancel.
        assert_eq!(diff.clear(), Some(QueryTicket(4)));
        assert!(diff.selection.is_empty());
        assert!(diff.comparison.is_none());
    }
}
