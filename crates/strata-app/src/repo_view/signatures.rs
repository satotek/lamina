//! Commit signature state and debounced visible-row verification on `RepoView`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use gpui::{Context, Task};
use strata_core::Oid;
use strata_core::repo::{CommitSignature, SignatureFormat};
use strata_core::session::QueryTicket;

use super::RepoView;
use crate::settings::Settings;
use crate::ui::CommitSignatureState;

const VISIBLE_DEBOUNCE: Duration = Duration::from_millis(80);

#[derive(Clone, Debug)]
enum Entry {
    Unsigned,
    Detected(SignatureFormat),
    Verified(Arc<CommitSignature>),
}

#[derive(Default)]
pub(super) struct SignatureTracker {
    entries: HashMap<Oid, Entry>,
    pending_visible: Vec<Oid>,
    visible_ticket: Option<(QueryTicket, Vec<Oid>)>,
    selected_ticket: Option<(QueryTicket, Vec<Oid>)>,
    debounce: Option<Task<()>>,
    pub(super) details_expanded: bool,
}

impl SignatureTracker {
    pub(super) fn cancel_all(&mut self) -> impl Iterator<Item = QueryTicket> {
        self.debounce = None;
        self.pending_visible.clear();
        [
            self.visible_ticket.take().map(|(ticket, _)| ticket),
            self.selected_ticket.take().map(|(ticket, _)| ticket),
        ]
        .into_iter()
        .flatten()
    }

    pub(super) fn reset(&mut self) -> impl Iterator<Item = QueryTicket> {
        self.entries.clear();
        self.details_expanded = false;
        self.cancel_all()
    }

    pub(super) fn state(&self, id: Oid) -> Option<CommitSignatureState> {
        match self.entries.get(&id) {
            Some(Entry::Detected(format)) => Some(CommitSignatureState::Detected(*format)),
            Some(Entry::Verified(sig)) => Some(CommitSignatureState::Verified(Arc::clone(sig))),
            Some(Entry::Unsigned) | None => None,
        }
    }

    /// Snapshots which OIDs from a candidate list still need to be requested,
    /// skipping ones already resolved, queued in `pending_visible`, or in
    /// flight in `visible_ticket`.
    pub(super) fn missing_for_visible(&self, candidate: Oid) -> bool {
        if self.entries.contains_key(&candidate) {
            return false;
        }
        if self.pending_visible.contains(&candidate) {
            return false;
        }
        if let Some((_, inflight)) = &self.visible_ticket
            && inflight.contains(&candidate)
        {
            return false;
        }
        true
    }
}

impl RepoView {
    pub(super) fn commit_signature_state(&self, id: Oid) -> Option<CommitSignatureState> {
        self.signatures.state(id)
    }

    pub(super) fn should_schedule_signature(&self, id: Oid) -> bool {
        self.signatures.missing_for_visible(id)
    }

    pub(super) fn schedule_visible_signatures(
        &mut self,
        commits: Vec<Oid>,
        cx: &mut Context<Self>,
    ) {
        if self.smoke || self.session.is_none() || !Settings::get(cx).history.show_commit_signatures
        {
            return;
        }

        let mut deduped = Vec::new();
        let mut seen = HashSet::new();
        for id in commits {
            if seen.insert(id) && !self.signatures.entries.contains_key(&id) {
                deduped.push(id);
            }
        }
        if deduped.is_empty() {
            return;
        }
        if deduped == self.signatures.pending_visible && self.signatures.debounce.is_some() {
            return;
        }
        if let Some((_, inflight)) = &self.signatures.visible_ticket
            && deduped.iter().all(|id| inflight.contains(id))
            && self.signatures.debounce.is_none()
        {
            return;
        }

        self.signatures.pending_visible = deduped;
        self.signatures.debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(VISIBLE_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                this.flush_visible_signatures(cx);
            });
        }));
    }

    fn flush_visible_signatures(&mut self, cx: &mut Context<Self>) {
        self.signatures.debounce = None;
        if self.smoke || !Settings::get(cx).history.show_commit_signatures {
            self.signatures.pending_visible.clear();
            return;
        }
        let Some(session) = self.session.as_ref() else {
            self.signatures.pending_visible.clear();
            return;
        };

        let batch: Vec<Oid> = self
            .signatures
            .pending_visible
            .drain(..)
            .filter(|id| !self.signatures.entries.contains_key(id))
            .collect();
        if batch.is_empty() {
            return;
        }

        if let Some((prev, _)) = self.signatures.visible_ticket.take() {
            session.cancel_query(prev);
        }
        let ticket = session.request_commit_signatures(batch.clone());
        self.signatures.visible_ticket = Some((ticket, batch));
    }

    pub(super) fn load_selected_signature(&mut self, commit: Option<Oid>, cx: &mut Context<Self>) {
        self.signatures.details_expanded = false;
        if let (Some(session), Some((prev, _))) = (
            self.session.as_ref(),
            self.signatures.selected_ticket.take(),
        ) {
            session.cancel_query(prev);
        }
        if self.smoke || !Settings::get(cx).history.show_commit_signatures {
            return;
        }
        let (Some(commit), Some(session)) = (commit, self.session.as_ref()) else {
            return;
        };
        if matches!(
            self.signatures.entries.get(&commit),
            Some(Entry::Unsigned | Entry::Verified(_))
        ) {
            return;
        }
        let batch = vec![commit];
        let ticket = session.request_commit_signatures(batch.clone());
        self.signatures.selected_ticket = Some((ticket, batch));
    }

    pub(super) fn toggle_signature_details_expanded(&mut self, cx: &mut Context<Self>) {
        self.signatures.details_expanded = !self.signatures.details_expanded;
        self.notify_if_active(cx);
    }

    pub(super) fn signatures_detected(
        &mut self,
        ticket: QueryTicket,
        formats: &HashMap<Oid, SignatureFormat>,
        cx: &mut Context<Self>,
    ) {
        let is_current = self
            .signatures
            .visible_ticket
            .as_ref()
            .is_some_and(|(t, _)| *t == ticket)
            || self
                .signatures
                .selected_ticket
                .as_ref()
                .is_some_and(|(t, _)| *t == ticket);
        if !is_current {
            return;
        }
        let mut changed = false;
        for (&id, &format) in formats {
            if !matches!(self.signatures.entries.get(&id), Some(Entry::Verified(_))) {
                self.signatures.entries.insert(id, Entry::Detected(format));
                changed = true;
            }
        }
        if changed {
            self.notify_if_active(cx);
        }
    }

    pub(super) fn signatures_ready(
        &mut self,
        ticket: QueryTicket,
        signatures: &HashMap<Oid, CommitSignature>,
        cx: &mut Context<Self>,
    ) {
        let requested = if self
            .signatures
            .visible_ticket
            .as_ref()
            .is_some_and(|(t, _)| *t == ticket)
        {
            self.signatures.visible_ticket.take().map(|(_, oids)| oids)
        } else if self
            .signatures
            .selected_ticket
            .as_ref()
            .is_some_and(|(t, _)| *t == ticket)
        {
            self.signatures.selected_ticket.take().map(|(_, oids)| oids)
        } else {
            return;
        };

        let Some(requested) = requested else {
            return;
        };

        for id in requested {
            if let Some(sig) = signatures.get(&id) {
                self.signatures
                    .entries
                    .insert(id, Entry::Verified(Arc::new(sig.clone())));
            } else {
                self.signatures.entries.insert(id, Entry::Unsigned);
            }
        }
        for (id, sig) in signatures {
            self.signatures
                .entries
                .insert(*id, Entry::Verified(Arc::new(sig.clone())));
        }
        self.notify_if_active(cx);
    }

    pub(super) fn signatures_invalidated(&mut self, cx: &mut Context<Self>) {
        let cancelled: Vec<_> = self.signatures.reset().collect();
        if let Some(session) = self.session.as_ref() {
            for ticket in cancelled {
                session.cancel_query(ticket);
            }
        }
        self.load_selected_signature(self.selection_commit, cx);
        self.notify_if_active(cx);
    }

    pub(super) fn sync_signature_setting(&mut self, cx: &mut Context<Self>) {
        if Settings::get(cx).history.show_commit_signatures {
            self.load_selected_signature(self.selection_commit, cx);
        } else {
            let cancelled: Vec<_> = self.signatures.cancel_all().collect();
            if let Some(session) = self.session.as_ref() {
                for ticket in cancelled {
                    session.cancel_query(ticket);
                }
            }
        }
        self.notify_if_active(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> Oid {
        format!("{byte:02x}")
            .repeat(20)
            .parse()
            .expect("hex object id")
    }

    #[test]
    fn tracker_skips_resolved_pending_and_inflight_commits() {
        let mut tracker = SignatureTracker::default();
        let a = oid(1);
        let b = oid(2);
        let c = oid(3);
        let d = oid(4);

        tracker.entries.insert(a, Entry::Unsigned);
        tracker.pending_visible.push(b);
        tracker.visible_ticket = Some((QueryTicket(9), vec![c]));

        assert!(!tracker.missing_for_visible(a));
        assert!(!tracker.missing_for_visible(b));
        assert!(!tracker.missing_for_visible(c));
        assert!(tracker.missing_for_visible(d));
    }

    #[test]
    fn cancel_all_and_reset_return_active_tickets() {
        let mut tracker = SignatureTracker::default();
        tracker
            .entries
            .insert(oid(1), Entry::Detected(SignatureFormat::Ssh));
        tracker.details_expanded = true;
        tracker.visible_ticket = Some((QueryTicket(11), vec![oid(1)]));
        tracker.selected_ticket = Some((QueryTicket(12), vec![oid(2)]));

        let tickets: Vec<_> = tracker.reset().collect();
        assert_eq!(tickets, vec![QueryTicket(11), QueryTicket(12)]);
        assert!(!tracker.details_expanded);
        assert!(tracker.state(oid(1)).is_none());
        assert!(tracker.missing_for_visible(oid(1)));
    }
}
