//! The banner for a multi-step operation git left unfinished.
//!
//! A stopped merge, rebase, cherry-pick, revert, `am` or bisect is announced
//! across the top of the main area, whichever view is showing, together with
//! the two actions core can run: `ContinueInProgress` and `AbortInProgress`.
//! A bisect has no continue, so it gets `bisect reset` alone.
//!
//! Core reports which operation is in progress but not the branch it is
//! rebasing, the commit being picked or the step in the sequence, so the banner
//! names the operation and how many conflicts are left instead of guessing
//! details the snapshot does not carry.
//!
//! The module also decides whether a finished operation takes the user to the
//! conflicts. That decision is a pure function so the cases that must not switch
//! (a watcher refresh, conflicts that were already there) are easy to read and
//! to test.

use gpui::{
    AnyElement, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, Window, div,
};
use gpui_component::tooltip::Tooltip;
use lamina_core::RepoState;
use lamina_core::repo::{InProgressCommit, InProgressDetails};
use lamina_core::session::{Operation, OperationId, QueryTicket};

use super::RepoView;
use crate::style::Design;

/// What a finished job was, as far as this view can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FinishedOperation {
    Merge,
    Pull,
    CherryPick,
    Revert,
    Stash,
    Rebase,
    Continue,
    /// A job another module submitted. The session reports the id only, so this
    /// view cannot name it: today that is a pull from the title bar and the
    /// conflicts module's own continue, and both can stop on a conflict.
    Unreported,
    /// A job this view submitted that cannot leave conflicts behind.
    Other,
}

impl FinishedOperation {
    /// The operation behind a job this view submitted.
    pub(crate) fn of(operation: &Operation) -> Self {
        match operation {
            Operation::Merge { .. } | Operation::MergeContinue => Self::Merge,
            Operation::Pull { .. } => Self::Pull,
            Operation::CherryPick { .. } | Operation::CherryPickMany { .. } => Self::CherryPick,
            Operation::Revert { .. } => Self::Revert,
            Operation::Rebase { .. } => Self::Rebase,
            Operation::StashApply { .. } | Operation::StashPop { .. } => Self::Stash,
            Operation::ContinueInProgress => Self::Continue,
            _ => Self::Other,
        }
    }

    /// Whether git can stop this operation on a conflict.
    ///
    /// `Unreported` counts: the only jobs submitted outside this view either
    /// run a merge-like command (a pull, a continuation) or cannot change the
    /// conflict count at all, so the count in the snapshot stays the guard.
    fn can_stop_on_conflicts(self) -> bool {
        !matches!(self, Self::Other)
    }
}

/// One operation the session reported starting that has not reported back yet.
struct Running {
    id: OperationId,
    operation: FinishedOperation,
    /// Conflicts in the index when the operation started.
    conflicts_before: usize,
    /// The user asked for this job. A watcher refresh only publishes a
    /// snapshot, so it never starts a job and never reaches here.
    by_user: bool,
}

/// What the view knows about the job in flight and the stopped operation's details.
#[derive(Default)]
pub(crate) struct InProgress {
    /// What `run_operation` submitted, waiting for its start event; the session
    /// reports the id only, so the kind has to be kept here.
    submitted: Option<(OperationId, FinishedOperation)>,
    running: Option<Running>,
    details_ticket: Option<QueryTicket>,
    details: Option<InProgressDetails>,
}

impl InProgress {
    pub(crate) fn is_running(&self) -> bool {
        self.submitted.is_some() || self.running.is_some()
    }

    /// Records what a job this view just submitted is.
    pub(crate) fn submitted(&mut self, id: OperationId, operation: FinishedOperation) {
        self.submitted = Some((id, operation));
    }

    /// The session started `id`; `conflicts_before` is the conflict count in
    /// the snapshot published just before the command ran.
    pub(crate) fn started(&mut self, id: OperationId, conflicts_before: usize, by_user: bool) {
        let operation = match self.submitted.take() {
            Some((submitted, operation)) if submitted == id => operation,
            _ => FinishedOperation::Unreported,
        };
        self.running = Some(Running {
            id,
            operation,
            conflicts_before,
            by_user,
        });
    }

    /// Records the end of `id` and says whether the view should show the
    /// conflicts. `conflicts_after` is the count in the refreshed snapshot,
    /// which the session publishes before it reports the end.
    pub(crate) fn finished(&mut self, id: OperationId, conflicts_after: usize) -> bool {
        let Some(running) = self.running.take() else {
            return false;
        };
        if running.id != id {
            self.running = Some(running);
            return false;
        }
        switch_to_conflicts(
            running.conflicts_before,
            conflicts_after,
            running.operation,
            running.by_user,
        )
    }

    /// Tracks a new details query for `state`, returning any superseded ticket
    /// to cancel. Keeps existing details when `state` still matches so a
    /// status refresh during the same operation does not flicker.
    fn begin_details(&mut self, ticket: QueryTicket, state: RepoState) -> Option<QueryTicket> {
        if self
            .details
            .as_ref()
            .is_some_and(|details| !details_match_state(details, state))
        {
            self.details = None;
        }
        self.details_ticket.replace(ticket)
    }

    /// Clears cached details when the repository returns to `Normal`.
    pub(super) fn clear_details(&mut self) -> Option<QueryTicket> {
        self.details = None;
        self.details_ticket.take()
    }

    /// Accepts `details` if `ticket` is still the latest request.
    fn accept_details(&mut self, ticket: QueryTicket, details: Option<InProgressDetails>) -> bool {
        if self.details_ticket != Some(ticket) {
            return false;
        }
        self.details_ticket = None;
        self.details = details;
        true
    }

    pub(super) fn details_for(&self, state: RepoState) -> Option<&InProgressDetails> {
        self.details
            .as_ref()
            .filter(|details| details_match_state(details, state))
    }
}

pub(super) fn details_match_state(details: &InProgressDetails, state: RepoState) -> bool {
    matches!(
        (details, state),
        (InProgressDetails::Rebase { .. }, RepoState::Rebasing)
            | (InProgressDetails::Merge { .. }, RepoState::Merging)
            | (
                InProgressDetails::CherryPick { .. },
                RepoState::CherryPicking
            )
            | (InProgressDetails::Revert { .. }, RepoState::Reverting)
            | (InProgressDetails::Bisect { .. }, RepoState::Bisecting)
            | (
                InProgressDetails::ApplyingPatches { .. },
                RepoState::ApplyingPatches
            )
    )
}

/// Whether a finished operation should take the user to the conflicts.
///
/// The switch is only news when the operation created the conflict: a merge
/// started on top of an earlier stopped one must not move the user again, and a
/// refresh that merely noticed conflicts (no operation) must not either.
fn switch_to_conflicts(
    conflicts_before: usize,
    conflicts_after: usize,
    operation: FinishedOperation,
    by_user: bool,
) -> bool {
    by_user && operation.can_stop_on_conflicts() && conflicts_before == 0 && conflicts_after > 0
}

/// What the in-progress banner says and which actions it offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BannerState {
    pub text: String,
    pub tooltip: Option<String>,
    /// `None` when core cannot continue the operation (bisect).
    pub can_continue: Option<bool>,
    pub continue_tooltip: Option<String>,
    /// A bisect is ended with `git bisect reset`, so its action is named after
    /// that instead of an abort.
    pub bisecting: bool,
    /// Conflicts still in the index.
    pub conflicts: usize,
    pub busy: bool,
}

fn banner_state(
    state: RepoState,
    details: Option<&InProgressDetails>,
    conflicts: usize,
    busy: bool,
) -> Option<BannerState> {
    if state == RepoState::Normal {
        return None;
    }
    let details = details.filter(|details| details_match_state(details, state));
    let text = format_banner_text(state, details, conflicts);
    let tooltip = details.map(|details| format_banner_tooltip(details, conflicts));
    let can_continue = (state != RepoState::Bisecting).then_some(conflicts == 0 && !busy);
    let continue_tooltip = match (can_continue, conflicts, busy) {
        (Some(false), 1, _) => Some(t!("progress.conflicts_left.one", count = 1).to_string()),
        (Some(false), count, _) if count > 1 => {
            Some(t!("progress.conflicts_left.other", count = count).to_string())
        }
        (Some(false), _, _) => Some(t!("progress.busy").to_string()),
        _ => None,
    };
    Some(BannerState {
        text,
        tooltip,
        can_continue,
        continue_tooltip,
        bisecting: state == RepoState::Bisecting,
        conflicts,
        busy,
    })
}

fn format_banner_text(
    state: RepoState,
    details: Option<&InProgressDetails>,
    conflicts: usize,
) -> String {
    let mut parts: Vec<String> = match details {
        Some(InProgressDetails::Rebase {
            branch,
            onto,
            step,
            total_steps,
            current_commit,
        }) => {
            let mut head = t!("progress.rebasing").to_string();
            if let Some(branch) = branch {
                head.push(' ');
                head.push_str(branch);
            }
            if let Some(onto) = onto {
                match &onto.ref_name {
                    Some(ref_name) => {
                        head.push_str(&t!(
                            "progress.onto_named",
                            name = ref_name,
                            short = onto.short_id
                        ));
                    }
                    None => {
                        head.push_str(&t!("progress.onto", short = onto.short_id));
                    }
                }
            }
            let mut parts = vec![head];
            match (step, total_steps) {
                (Some(step), Some(total)) => {
                    parts.push(t!("progress.step_of", step = step, total = total).to_string())
                }
                (Some(step), None) => parts.push(t!("progress.step", step = step).to_string()),
                _ => {}
            }
            if let Some(commit) = current_commit {
                parts.push(
                    t!("progress.applying", commit = format_commit_brief(commit)).to_string(),
                );
            }
            parts
        }
        Some(InProgressDetails::Merge { heads, source }) => {
            let ids: Vec<&str> = heads.iter().map(|head| head.short_id.as_str()).collect();
            let head = match (source.as_deref(), ids.is_empty()) {
                (Some(source), false) => t!(
                    "progress.merging_source",
                    source = source,
                    ids = ids.join(", ")
                )
                .to_string(),
                (Some(source), true) => {
                    t!("progress.merging_source_only", source = source).to_string()
                }
                (None, false) => t!("progress.merging_ids", ids = ids.join(", ")).to_string(),
                (None, true) => t!("progress.merging").to_string(),
            };
            vec![head]
        }
        Some(InProgressDetails::CherryPick { commit, remaining }) => {
            let head = match commit {
                Some(commit) => t!(
                    "progress.cherry_picking_commit",
                    commit = format_commit_brief(commit)
                )
                .to_string(),
                None => t!("progress.cherry_picking").to_string(),
            };
            let mut parts = vec![head];
            if let Some(remaining) = remaining {
                parts.push(t!("progress.remaining", count = remaining).to_string());
            }
            parts
        }
        Some(InProgressDetails::Revert { commit, remaining }) => {
            let head = match commit {
                Some(commit) => t!(
                    "progress.reverting_commit",
                    commit = format_commit_brief(commit)
                )
                .to_string(),
                None => t!("progress.reverting").to_string(),
            };
            let mut parts = vec![head];
            if let Some(remaining) = remaining {
                parts.push(t!("progress.remaining", count = remaining).to_string());
            }
            parts
        }
        Some(InProgressDetails::Bisect {
            good_count,
            bad_count,
            current_commit,
        }) => {
            let mut parts = vec![t!("progress.bisecting").to_string()];
            match (good_count, bad_count) {
                (Some(good), Some(bad)) => {
                    parts.push(t!("progress.good_bad", good = good, bad = bad).to_string())
                }
                (Some(good), None) => parts.push(t!("progress.good", good = good).to_string()),
                (None, Some(bad)) => parts.push(t!("progress.bad", bad = bad).to_string()),
                (None, None) => {}
            }
            if let Some(commit) = current_commit {
                parts.push(t!("progress.at", commit = format_commit_brief(commit)).to_string());
            }
            parts
        }
        Some(InProgressDetails::ApplyingPatches {
            step,
            total_steps,
            subject,
        }) => {
            let mut parts = vec![t!("progress.applying_patches").to_string()];
            match (step, total_steps) {
                (Some(step), Some(total)) => {
                    parts.push(t!("progress.patch_of", step = step, total = total).to_string())
                }
                (Some(step), None) => parts.push(t!("progress.patch", step = step).to_string()),
                _ => {}
            }
            if let Some(subject) = subject {
                parts.push(subject.clone());
            }
            parts
        }
        None => {
            let name = match state {
                RepoState::Normal => return String::new(),
                RepoState::Merging => &*t!("progress.merging"),
                RepoState::Rebasing => &*t!("progress.rebasing"),
                RepoState::CherryPicking => &*t!("progress.cherry_picking"),
                RepoState::Reverting => &*t!("progress.reverting"),
                RepoState::ApplyingPatches => &*t!("progress.applying_patches"),
                RepoState::Bisecting => &*t!("progress.bisecting"),
            };
            vec![name.to_owned()]
        }
    };

    match conflicts {
        0 => {}
        1 => parts.push(t!("progress.conflict_to_resolve.one", count = 1).to_string()),
        count => parts.push(t!("progress.conflict_to_resolve.other", count = count).to_string()),
    }
    parts.join(" · ")
}

fn format_commit_brief(commit: &InProgressCommit) -> String {
    match &commit.summary {
        Some(summary) => format!("{} {summary}", commit.short_id),
        None => commit.short_id.clone(),
    }
}

fn format_commit_full(commit: &InProgressCommit) -> String {
    let hash = commit
        .id
        .map(|id| id.to_hex().to_string())
        .unwrap_or_else(|| commit.short_id.clone());
    match &commit.summary {
        Some(summary) => format!("{hash} {summary}"),
        None => hash,
    }
}

fn format_banner_tooltip(details: &InProgressDetails, conflicts: usize) -> String {
    let mut lines = Vec::new();
    match details {
        InProgressDetails::Rebase {
            branch,
            onto,
            step,
            total_steps,
            current_commit,
        } => {
            lines.push(t!("progress.operation.rebase").to_string());
            if let Some(branch) = branch {
                lines.push(t!("progress.branch", branch = branch).to_string());
            }
            if let Some(onto) = onto {
                let hash = onto
                    .id
                    .map(|id| id.to_hex().to_string())
                    .unwrap_or_else(|| onto.short_id.clone());
                match &onto.ref_name {
                    Some(ref_name) => lines.push(
                        t!(
                            "progress.onto_line",
                            target = format!("{ref_name} ({hash})")
                        )
                        .to_string(),
                    ),
                    None => lines.push(t!("progress.onto_line", target = hash).to_string()),
                }
            }
            match (step, total_steps) {
                (Some(step), Some(total)) => {
                    lines.push(t!("progress.step_line", step = step, total = total).to_string())
                }
                (Some(step), None) => {
                    lines.push(t!("progress.step_line_single", step = step).to_string())
                }
                _ => {}
            }
            if let Some(commit) = current_commit {
                lines.push(
                    t!(
                        "progress.applying_line",
                        commit = format_commit_full(commit)
                    )
                    .to_string(),
                );
            }
        }
        InProgressDetails::Merge { heads, source } => {
            lines.push(t!("progress.operation.merge").to_string());
            if let Some(source) = source {
                lines.push(t!("progress.source", source = source).to_string());
            }
            for head in heads {
                lines.push(t!("progress.commit", commit = format_commit_full(head)).to_string());
            }
        }
        InProgressDetails::CherryPick { commit, remaining } => {
            lines.push(t!("progress.operation.cherry_pick").to_string());
            if let Some(commit) = commit {
                lines.push(t!("progress.commit", commit = format_commit_full(commit)).to_string());
            }
            if let Some(remaining) = remaining {
                lines.push(t!("progress.remaining_line", count = remaining).to_string());
            }
        }
        InProgressDetails::Revert { commit, remaining } => {
            lines.push(t!("progress.operation.revert").to_string());
            if let Some(commit) = commit {
                lines.push(t!("progress.commit", commit = format_commit_full(commit)).to_string());
            }
            if let Some(remaining) = remaining {
                lines.push(t!("progress.remaining_line", count = remaining).to_string());
            }
        }
        InProgressDetails::Bisect {
            good_count,
            bad_count,
            current_commit,
        } => {
            lines.push(t!("progress.operation.bisect").to_string());
            if let Some(good) = good_count {
                lines.push(t!("progress.good_line", count = good).to_string());
            }
            if let Some(bad) = bad_count {
                lines.push(t!("progress.bad_line", count = bad).to_string());
            }
            if let Some(commit) = current_commit {
                lines.push(t!("progress.current", commit = format_commit_full(commit)).to_string());
            }
        }
        InProgressDetails::ApplyingPatches {
            step,
            total_steps,
            subject,
        } => {
            lines.push(t!("progress.operation.am").to_string());
            match (step, total_steps) {
                (Some(step), Some(total)) => {
                    lines.push(t!("progress.patch_line", step = step, total = total).to_string())
                }
                (Some(step), None) => {
                    lines.push(t!("progress.patch_line_single", step = step).to_string())
                }
                _ => {}
            }
            if let Some(subject) = subject {
                lines.push(t!("progress.subject", subject = subject).to_string());
            }
        }
    }
    if conflicts > 0 {
        lines.push(t!("progress.conflicts", count = conflicts).to_string());
    }
    lines.join("\n")
}

fn abort_text(state: RepoState) -> Option<(String, String, String)> {
    Some(match state {
        RepoState::Normal => return None,
        RepoState::Merging => (
            t!("progress.abort_merge").to_string(),
            t!("progress.abort_merge_detail").to_string(),
            t!("progress.abort_merge_button").to_string(),
        ),
        RepoState::Rebasing => (
            t!("progress.abort_rebase").to_string(),
            t!("progress.abort_rebase_detail").to_string(),
            t!("progress.abort_rebase_button").to_string(),
        ),
        RepoState::CherryPicking => (
            t!("progress.abort_cherry_pick").to_string(),
            t!("progress.abort_cherry_pick_detail").to_string(),
            t!("progress.abort_cherry_pick_button").to_string(),
        ),
        RepoState::Reverting => (
            t!("progress.abort_revert").to_string(),
            t!("progress.abort_revert_detail").to_string(),
            t!("progress.abort_revert_button").to_string(),
        ),
        RepoState::ApplyingPatches => (
            t!("progress.abort_am").to_string(),
            t!("progress.abort_am_detail").to_string(),
            t!("progress.abort_button").to_string(),
        ),
        RepoState::Bisecting => (
            t!("progress.reset_bisect_title").to_string(),
            t!("progress.reset_bisect_detail").to_string(),
            t!("progress.reset_bisect_button").to_string(),
        ),
    })
}

/// Why "Resolve conflicts" cannot open, or `None` when it can.
///
/// In-progress details name the commits, but the editor still needs the three
/// index stages and the worktree file. Those reads are not on the wire, so
/// the button stays closed on a remote tab.
pub(crate) fn resolve_conflicts_blocked(ssh: bool) -> Option<String> {
    ssh.then(crate::remote_open::unavailable_reason)
}

impl RepoView {
    /// Re-requests in-progress details whenever a snapshot publishes a
    /// non-normal state, and clears stale details as soon as git finishes.
    pub(crate) fn refresh_in_progress_details(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let state = session.snapshot().state;
        if state == RepoState::Normal {
            if let Some(ticket) = self.in_progress.clear_details() {
                session.cancel_query(ticket);
            }
            return;
        }
        let ticket = session.request_in_progress_details();
        if let Some(replaced) = self.in_progress.begin_details(ticket, state) {
            session.cancel_query(replaced);
        }
    }

    /// The details of the stopped operation, once they have been read.
    ///
    /// The conflict editor names the commits a rebase, cherry-pick or revert
    /// is working with, from the same query the banner uses.
    pub(crate) fn stopped_operation_details(&self) -> Option<&InProgressDetails> {
        let state = self.session.as_ref()?.snapshot().state;
        self.in_progress.details_for(state)
    }

    pub(crate) fn on_in_progress_details_ready(
        &mut self,
        ticket: QueryTicket,
        details: Option<InProgressDetails>,
        cx: &mut Context<Self>,
    ) {
        if self.in_progress.accept_details(ticket, details) {
            self.notify_if_active(cx);
        }
    }

    /// The banner for a stopped operation, or nothing while the repository is
    /// clean. It sits above the current view, so a merge stopped from the
    /// history is visible without opening the working tree.
    pub(crate) fn in_progress_banner(
        &self,
        entity: Entity<RepoView>,
        design: &Design,
    ) -> Option<AnyElement> {
        let session = self.session.as_ref()?;
        let snapshot = session.snapshot();
        let busy = snapshot.busy.is_some() || self.branches.is_running();
        let details = self.in_progress.details_for(snapshot.state);
        let state = banner_state(snapshot.state, details, snapshot.conflicts.len(), busy)?;

        let mut banner = crate::ui::banner(crate::ui::BannerKind::Warning, state.text, design);

        if state.conflicts > 0 {
            // The conflict editor reads index stages and the worktree file.
            // Those reads are not on the wire, so opening it on a remote tab
            // fails with nothing on screen. Abort stays available: it is a
            // supported write.
            let blocked = resolve_conflicts_blocked(self.is_ssh());
            let mut button = crate::ui::button(
                "in-progress-resolve",
                t!("progress.resolve_conflicts").to_string(),
            )
            .disabled(blocked.is_some());
            if let Some(reason) = blocked {
                button = button.tooltip(reason);
            } else {
                let resolve = entity.clone();
                button = button.on_click(move |_, window, cx| {
                    resolve.update(cx, |this, cx| this.open_conflicts(window, cx));
                });
            }
            banner = banner.action(button);
        }

        if let Some(enabled) = state.can_continue {
            let mut button =
                crate::ui::button("in-progress-continue", t!("progress.continue").to_string())
                    .disabled(!enabled);
            if let Some(tooltip) = state.continue_tooltip {
                button = button.tooltip(tooltip);
            }
            let continue_operation = entity.clone();
            banner = banner.action(button.on_click(move |_, window, cx| {
                continue_operation.update(cx, |this, cx| this.continue_in_progress(window, cx));
            }));
        }

        let abort = entity;
        let label = if state.bisecting {
            t!("progress.reset_bisect_button").to_string()
        } else {
            t!("progress.abort_ellipsis").to_string()
        };
        banner = banner.action(
            crate::ui::button("in-progress-abort", label)
                .disabled(state.busy)
                .on_click(move |_, window, cx| {
                    abort.update(cx, |this, cx| this.confirm_abort(window, cx));
                }),
        );

        let mut element = div().id("in-progress-banner").flex_none().child(banner);
        if let Some(tooltip) = state.tooltip {
            element =
                element.tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx));
        }

        Some(element.into_any_element())
    }

    /// Continues the operation the snapshot reports, through the same queue as
    /// every other mutation so the banner shows it running and git's refusal
    /// lands in the status bar.
    pub(crate) fn continue_in_progress(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let snapshot = session.snapshot();
        let label = match snapshot.state {
            RepoState::Merging => t!("progress.continue_merge").to_string(),
            RepoState::Rebasing => t!("progress.continue_rebase").to_string(),
            RepoState::CherryPicking => t!("progress.continue_cherry_pick").to_string(),
            RepoState::Reverting => t!("progress.continue_revert").to_string(),
            RepoState::ApplyingPatches => t!("progress.continue_am").to_string(),
            // A bisect is ended with `git bisect reset`, not continued.
            RepoState::Bisecting | RepoState::Normal => return,
        };
        // The buttons are disabled in both cases; this keeps a stale click from
        // running git's "nothing to continue" path.
        if !snapshot.conflicts.is_empty() || snapshot.busy.is_some() {
            return;
        }
        self.run_operation(Operation::ContinueInProgress, label, window, cx);
    }

    /// Asks before throwing away what the stopped operation did so far.
    fn confirm_abort(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let Some((title, detail, ok_label)) = abort_text(session.snapshot().state) else {
            return;
        };
        if self.is_busy() {
            return;
        }
        let entity = cx.weak_entity();
        crate::ui::confirm(
            window,
            cx,
            crate::ui::Confirm::new(title, detail).ok_label(ok_label),
            move |window, cx| {
                let _ = entity.update(cx, |this, cx| this.abort_in_progress(window, cx));
            },
        );
    }

    /// Runs the abort (or `bisect reset`) the user confirmed.
    pub(crate) fn abort_in_progress(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let label = match session.snapshot().state {
            RepoState::Normal => return,
            RepoState::Bisecting => t!("progress.reset_bisect_op").to_string(),
            RepoState::Merging => t!("progress.abort_merge_op").to_string(),
            RepoState::Rebasing => t!("progress.abort_rebase_op").to_string(),
            RepoState::CherryPicking => t!("progress.abort_cherry_pick_op").to_string(),
            RepoState::Reverting => t!("progress.abort_revert_op").to_string(),
            RepoState::ApplyingPatches => t!("progress.abort_am_op").to_string(),
        };
        self.run_operation(Operation::AbortInProgress, label, window, cx);
    }

    /// Takes the user to the conflicts after an operation stopped on them.
    ///
    /// The session event carries no window and the view is mid-update, so the
    /// switch is deferred to the end of the effect cycle; the window is only
    /// needed to focus the working-tree panel.
    pub(crate) fn reveal_conflicts(&mut self, cx: &mut Context<Self>) {
        let entity = cx.weak_entity();
        let Some(handle) = cx
            .active_window()
            .or_else(|| cx.windows().into_iter().next())
        else {
            return;
        };
        let _ = handle.update(cx, |_, window, cx| {
            window.defer(cx, move |window, cx| {
                let _ = entity.update(cx, |this, cx| this.open_conflicts(window, cx));
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_conflicts_stays_closed_until_a_remote_tab_can_read_stages() {
        assert_eq!(resolve_conflicts_blocked(false), None);
        assert_eq!(
            resolve_conflicts_blocked(true).as_deref(),
            Some(crate::remote_open::unavailable_reason().as_str())
        );
    }

    #[test]
    fn the_banner_names_the_operation_and_the_conflicts_left() {
        let merging = banner_state(RepoState::Merging, None, 2, false).expect("merging banner");
        assert_eq!(merging.text, "Merging · 2 conflicts to resolve");
        assert_eq!(merging.tooltip, None);
        assert_eq!(merging.can_continue, Some(false));
        assert_eq!(
            merging.continue_tooltip.as_deref(),
            Some("2 conflicts left to resolve")
        );

        let resolved = banner_state(RepoState::Merging, None, 0, false).expect("merging banner");
        assert_eq!(resolved.text, "Merging");
        assert_eq!(resolved.can_continue, Some(true));
        assert_eq!(resolved.continue_tooltip, None);

        assert_eq!(
            banner_state(RepoState::Rebasing, None, 1, false)
                .expect("rebase banner")
                .text,
            "Rebasing · 1 conflict to resolve"
        );
        assert_eq!(
            banner_state(RepoState::CherryPicking, None, 0, false)
                .expect("cherry-pick banner")
                .text,
            "Cherry-picking"
        );
        assert_eq!(
            banner_state(RepoState::Reverting, None, 0, false)
                .expect("revert banner")
                .text,
            "Reverting"
        );
        assert_eq!(
            banner_state(RepoState::ApplyingPatches, None, 0, false)
                .expect("am banner")
                .text,
            "Applying patches"
        );
        assert_eq!(banner_state(RepoState::Normal, None, 0, false), None);
    }

    #[test]
    fn a_bisect_is_reset_rather_than_continued() {
        let bisecting = banner_state(RepoState::Bisecting, None, 0, false).expect("bisect banner");
        assert!(bisecting.bisecting);
        assert_eq!(
            bisecting.can_continue, None,
            "core cannot continue a bisect"
        );
        assert_eq!(
            abort_text(RepoState::Bisecting).expect("bisect abort").2,
            "Reset bisect"
        );
        assert!(abort_text(RepoState::Normal).is_none());
    }

    #[test]
    fn continue_waits_for_a_running_operation_too() {
        let busy = banner_state(RepoState::Rebasing, None, 0, true).expect("rebase banner");
        assert_eq!(busy.can_continue, Some(false));
        assert_eq!(
            busy.continue_tooltip.as_deref(),
            Some("An operation is already running")
        );
    }

    #[test]
    fn formats_banner_text_and_tooltip_for_every_in_progress_state() {
        use lamina_core::repo::RebaseOnto;

        let oid_a: lamina_core::Oid = "a1b2c3d000000000000000000000000000000000"
            .parse()
            .expect("oid");
        let oid_b: lamina_core::Oid = "9f8e7d6000000000000000000000000000000000"
            .parse()
            .expect("oid");

        let rebase = InProgressDetails::Rebase {
            branch: Some("feature-x".to_owned()),
            onto: Some(RebaseOnto {
                id: Some(oid_a),
                short_id: "a1b2c3d".to_owned(),
                ref_name: Some("main".to_owned()),
            }),
            step: Some(3),
            total_steps: Some(7),
            current_commit: Some(InProgressCommit {
                id: Some(oid_b),
                short_id: "9f8e7d6".to_owned(),
                summary: Some("Fix parser".to_owned()),
            }),
        };
        let state = banner_state(RepoState::Rebasing, Some(&rebase), 2, false).expect("rebase");
        assert_eq!(
            state.text,
            "Rebasing feature-x onto main (a1b2c3d) · step 3/7 · applying 9f8e7d6 Fix parser · 2 conflicts to resolve"
        );
        let tooltip = state.tooltip.expect("rebase tooltip");
        assert!(tooltip.contains("Operation: Rebase"));
        assert!(tooltip.contains("Branch: feature-x"));
        assert!(tooltip.contains("Onto: main (a1b2c3d000000000000000000000000000000000)"));
        assert!(tooltip.contains("Step: 3 of 7"));
        assert!(tooltip.contains("Applying: 9f8e7d6000000000000000000000000000000000 Fix parser"));
        assert!(tooltip.contains("Conflicts: 2"));

        let merge = InProgressDetails::Merge {
            heads: vec![InProgressCommit {
                id: Some(oid_a),
                short_id: "a1b2c3d".to_owned(),
                summary: Some("Topic work".to_owned()),
            }],
            source: Some("topic".to_owned()),
        };
        let state = banner_state(RepoState::Merging, Some(&merge), 1, false).expect("merge");
        assert_eq!(
            state.text,
            "Merging topic (a1b2c3d) · 1 conflict to resolve"
        );
        let tooltip = state.tooltip.expect("merge tooltip");
        assert!(tooltip.contains("Source: topic"));
        assert!(tooltip.contains("Topic work"));

        let cherry = InProgressDetails::CherryPick {
            commit: Some(InProgressCommit {
                id: Some(oid_b),
                short_id: "9f8e7d6".to_owned(),
                summary: Some("Fix parser".to_owned()),
            }),
            remaining: Some(2),
        };
        let state =
            banner_state(RepoState::CherryPicking, Some(&cherry), 1, false).expect("cherry-pick");
        assert_eq!(
            state.text,
            "Cherry-picking 9f8e7d6 Fix parser · 2 remaining · 1 conflict to resolve"
        );

        let revert = InProgressDetails::Revert {
            commit: Some(InProgressCommit {
                id: Some(oid_b),
                short_id: "9f8e7d6".to_owned(),
                summary: Some("Bad commit".to_owned()),
            }),
            remaining: None,
        };
        let state = banner_state(RepoState::Reverting, Some(&revert), 0, false).expect("revert");
        assert_eq!(state.text, "Reverting 9f8e7d6 Bad commit");

        let bisect = InProgressDetails::Bisect {
            good_count: Some(2),
            bad_count: Some(1),
            current_commit: Some(InProgressCommit {
                id: Some(oid_a),
                short_id: "a1b2c3d".to_owned(),
                summary: Some("Candidate".to_owned()),
            }),
        };
        let state = banner_state(RepoState::Bisecting, Some(&bisect), 0, false).expect("bisect");
        assert_eq!(
            state.text,
            "Bisecting · 2 good, 1 bad · at a1b2c3d Candidate"
        );

        let am = InProgressDetails::ApplyingPatches {
            step: Some(2),
            total_steps: Some(4),
            subject: Some("Handle empty input".to_owned()),
        };
        let state = banner_state(RepoState::ApplyingPatches, Some(&am), 1, false).expect("am");
        assert_eq!(
            state.text,
            "Applying patches · patch 2/4 · Handle empty input · 1 conflict to resolve"
        );
    }

    #[test]
    fn partial_details_and_mismatched_details_fall_back_gracefully() {
        let partial_rebase = InProgressDetails::Rebase {
            branch: None,
            onto: None,
            step: Some(2),
            total_steps: None,
            current_commit: None,
        };
        let state =
            banner_state(RepoState::Rebasing, Some(&partial_rebase), 0, false).expect("partial");
        assert_eq!(state.text, "Rebasing · step 2");

        // Mismatched details (e.g. a stale Merge result after switching to Rebasing) are ignored.
        let stale_merge = InProgressDetails::Merge {
            heads: Vec::new(),
            source: Some("old".to_owned()),
        };
        let state =
            banner_state(RepoState::Rebasing, Some(&stale_merge), 0, false).expect("mismatched");
        assert_eq!(state.text, "Rebasing");
        assert_eq!(state.tooltip, None);
    }

    #[test]
    fn only_a_new_conflict_from_a_user_operation_takes_over_the_view() {
        for operation in [
            FinishedOperation::Merge,
            FinishedOperation::Pull,
            FinishedOperation::CherryPick,
            FinishedOperation::Revert,
            FinishedOperation::Stash,
            FinishedOperation::Rebase,
            FinishedOperation::Continue,
            FinishedOperation::Unreported,
        ] {
            assert!(
                switch_to_conflicts(0, 1, operation, true),
                "{operation:?} that stopped on a conflict must show it"
            );
        }
    }

    #[test]
    fn a_refresh_or_an_earlier_conflict_does_not_take_over_the_view() {
        // A watcher event publishes a snapshot without starting a job.
        assert!(!switch_to_conflicts(0, 2, FinishedOperation::Merge, false));
        // Conflicts that were already in the index are not news.
        assert!(!switch_to_conflicts(2, 2, FinishedOperation::Merge, true));
        assert!(!switch_to_conflicts(2, 3, FinishedOperation::Merge, true));
        // An operation that cannot leave conflicts behind does not switch.
        assert!(!switch_to_conflicts(0, 1, FinishedOperation::Other, true));
        // An operation that resolved the conflicts has nothing to show.
        assert!(!switch_to_conflicts(1, 0, FinishedOperation::Stash, true));
    }

    #[test]
    fn submitted_operations_are_classified_before_their_start_event() {
        assert_eq!(
            FinishedOperation::of(&Operation::Merge {
                branch: "topic".into(),
                no_ff: false,
            }),
            FinishedOperation::Merge
        );
        assert_eq!(
            FinishedOperation::of(&Operation::Pull { rebase: None }),
            FinishedOperation::Pull
        );
        assert_eq!(
            FinishedOperation::of(&Operation::StashPop { index: 0 }),
            FinishedOperation::Stash
        );
        assert_eq!(
            FinishedOperation::of(&Operation::Rebase {
                onto: "a1b2c3d000000000000000000000000000000000"
                    .parse()
                    .expect("oid"),
                upstream: None,
                autostash: false,
            }),
            FinishedOperation::Rebase
        );
        assert_eq!(
            FinishedOperation::of(&Operation::ContinueInProgress),
            FinishedOperation::Continue
        );
        assert_eq!(
            FinishedOperation::of(&Operation::DeleteBranch {
                name: "topic".into(),
                force: false,
            }),
            FinishedOperation::Other
        );
    }

    #[test]
    fn the_finished_event_only_answers_for_the_started_job() {
        let mut progress = InProgress::default();
        progress.submitted(
            OperationId(1),
            FinishedOperation::of(&Operation::Merge {
                branch: "topic".into(),
                no_ff: false,
            }),
        );
        progress.started(OperationId(1), 0, true);
        assert!(progress.finished(OperationId(1), 2));
        // The same id cannot report twice, and an untracked id is ignored.
        assert!(!progress.finished(OperationId(1), 2));
        assert!(!progress.finished(OperationId(2), 2));

        // A job submitted elsewhere is still a user operation.
        progress.started(OperationId(2), 0, true);
        assert!(progress.finished(OperationId(2), 1));
    }
}
