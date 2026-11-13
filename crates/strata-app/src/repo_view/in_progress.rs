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
use strata_core::RepoState;
use strata_core::repo::{InProgressCommit, InProgressDetails};
use strata_core::session::{Operation, OperationId, QueryTicket};

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
            Operation::CherryPick { .. } => Self::CherryPick,
            Operation::Revert { .. } => Self::Revert,
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

    fn details_for(&self, state: RepoState) -> Option<&InProgressDetails> {
        self.details
            .as_ref()
            .filter(|details| details_match_state(details, state))
    }
}

fn details_match_state(details: &InProgressDetails, state: RepoState) -> bool {
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
        (Some(false), 1, _) => Some("1 conflict left to resolve".to_owned()),
        (Some(false), count, _) if count > 1 => Some(format!("{count} conflicts left to resolve")),
        (Some(false), _, _) => Some("An operation is already running".to_owned()),
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
            let mut head = String::from("Rebasing");
            if let Some(branch) = branch {
                head.push(' ');
                head.push_str(branch);
            }
            if let Some(onto) = onto {
                match &onto.ref_name {
                    Some(ref_name) => {
                        head.push_str(&format!(" onto {ref_name} ({})", onto.short_id));
                    }
                    None => {
                        head.push_str(&format!(" onto {}", onto.short_id));
                    }
                }
            }
            let mut parts = vec![head];
            match (step, total_steps) {
                (Some(step), Some(total)) => parts.push(format!("step {step}/{total}")),
                (Some(step), None) => parts.push(format!("step {step}")),
                _ => {}
            }
            if let Some(commit) = current_commit {
                parts.push(format!("applying {}", format_commit_brief(commit)));
            }
            parts
        }
        Some(InProgressDetails::Merge { heads, source }) => {
            let ids: Vec<&str> = heads.iter().map(|head| head.short_id.as_str()).collect();
            let head = match (source.as_deref(), ids.is_empty()) {
                (Some(source), false) => format!("Merging {source} ({})", ids.join(", ")),
                (Some(source), true) => format!("Merging {source}"),
                (None, false) => format!("Merging {}", ids.join(", ")),
                (None, true) => "Merging".to_owned(),
            };
            vec![head]
        }
        Some(InProgressDetails::CherryPick { commit, remaining }) => {
            let head = match commit {
                Some(commit) => format!("Cherry-picking {}", format_commit_brief(commit)),
                None => "Cherry-picking".to_owned(),
            };
            let mut parts = vec![head];
            if let Some(remaining) = remaining {
                parts.push(format!("{remaining} remaining"));
            }
            parts
        }
        Some(InProgressDetails::Revert { commit, remaining }) => {
            let head = match commit {
                Some(commit) => format!("Reverting {}", format_commit_brief(commit)),
                None => "Reverting".to_owned(),
            };
            let mut parts = vec![head];
            if let Some(remaining) = remaining {
                parts.push(format!("{remaining} remaining"));
            }
            parts
        }
        Some(InProgressDetails::Bisect {
            good_count,
            bad_count,
            current_commit,
        }) => {
            let mut parts = vec!["Bisecting".to_owned()];
            match (good_count, bad_count) {
                (Some(good), Some(bad)) => parts.push(format!("{good} good, {bad} bad")),
                (Some(good), None) => parts.push(format!("{good} good")),
                (None, Some(bad)) => parts.push(format!("{bad} bad")),
                (None, None) => {}
            }
            if let Some(commit) = current_commit {
                parts.push(format!("at {}", format_commit_brief(commit)));
            }
            parts
        }
        Some(InProgressDetails::ApplyingPatches {
            step,
            total_steps,
            subject,
        }) => {
            let mut parts = vec!["Applying patches".to_owned()];
            match (step, total_steps) {
                (Some(step), Some(total)) => parts.push(format!("patch {step}/{total}")),
                (Some(step), None) => parts.push(format!("patch {step}")),
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
                RepoState::Merging => "Merging",
                RepoState::Rebasing => "Rebasing",
                RepoState::CherryPicking => "Cherry-picking",
                RepoState::Reverting => "Reverting",
                RepoState::ApplyingPatches => "Applying patches",
                RepoState::Bisecting => "Bisecting",
            };
            vec![name.to_owned()]
        }
    };

    match conflicts {
        0 => {}
        1 => parts.push("1 conflict to resolve".to_owned()),
        count => parts.push(format!("{count} conflicts to resolve")),
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
            lines.push("Operation: Rebase".to_owned());
            if let Some(branch) = branch {
                lines.push(format!("Branch: {branch}"));
            }
            if let Some(onto) = onto {
                let hash = onto
                    .id
                    .map(|id| id.to_hex().to_string())
                    .unwrap_or_else(|| onto.short_id.clone());
                match &onto.ref_name {
                    Some(ref_name) => lines.push(format!("Onto: {ref_name} ({hash})")),
                    None => lines.push(format!("Onto: {hash}")),
                }
            }
            match (step, total_steps) {
                (Some(step), Some(total)) => lines.push(format!("Step: {step} of {total}")),
                (Some(step), None) => lines.push(format!("Step: {step}")),
                _ => {}
            }
            if let Some(commit) = current_commit {
                lines.push(format!("Applying: {}", format_commit_full(commit)));
            }
        }
        InProgressDetails::Merge { heads, source } => {
            lines.push("Operation: Merge".to_owned());
            if let Some(source) = source {
                lines.push(format!("Source: {source}"));
            }
            for head in heads {
                lines.push(format!("Commit: {}", format_commit_full(head)));
            }
        }
        InProgressDetails::CherryPick { commit, remaining } => {
            lines.push("Operation: Cherry-pick".to_owned());
            if let Some(commit) = commit {
                lines.push(format!("Commit: {}", format_commit_full(commit)));
            }
            if let Some(remaining) = remaining {
                lines.push(format!("Remaining: {remaining}"));
            }
        }
        InProgressDetails::Revert { commit, remaining } => {
            lines.push("Operation: Revert".to_owned());
            if let Some(commit) = commit {
                lines.push(format!("Commit: {}", format_commit_full(commit)));
            }
            if let Some(remaining) = remaining {
                lines.push(format!("Remaining: {remaining}"));
            }
        }
        InProgressDetails::Bisect {
            good_count,
            bad_count,
            current_commit,
        } => {
            lines.push("Operation: Bisect".to_owned());
            if let Some(good) = good_count {
                lines.push(format!("Good: {good}"));
            }
            if let Some(bad) = bad_count {
                lines.push(format!("Bad: {bad}"));
            }
            if let Some(commit) = current_commit {
                lines.push(format!("Current: {}", format_commit_full(commit)));
            }
        }
        InProgressDetails::ApplyingPatches {
            step,
            total_steps,
            subject,
        } => {
            lines.push("Operation: Applying patches (git am)".to_owned());
            match (step, total_steps) {
                (Some(step), Some(total)) => lines.push(format!("Patch: {step} of {total}")),
                (Some(step), None) => lines.push(format!("Patch: {step}")),
                _ => {}
            }
            if let Some(subject) = subject {
                lines.push(format!("Subject: {subject}"));
            }
        }
    }
    if conflicts > 0 {
        lines.push(format!("Conflicts: {conflicts}"));
    }
    lines.join("\n")
}

fn abort_text(state: RepoState) -> Option<(&'static str, &'static str, &'static str)> {
    Some(match state {
        RepoState::Normal => return None,
        RepoState::Merging => (
            "Abort the merge?",
            "Git restores HEAD and the working tree from before the merge. Conflicts resolved so far are lost.",
            "Abort merge",
        ),
        RepoState::Rebasing => (
            "Abort the rebase?",
            "Git returns the branch to where it was before the rebase. Commits already replayed and conflicts resolved so far are lost.",
            "Abort rebase",
        ),
        RepoState::CherryPicking => (
            "Abort the cherry-pick?",
            "Git returns the branch and working tree to before the cherry-pick. Conflicts resolved so far are lost.",
            "Abort cherry-pick",
        ),
        RepoState::Reverting => (
            "Abort the revert?",
            "Git returns the branch and working tree to before the revert. Conflicts resolved so far are lost.",
            "Abort revert",
        ),
        RepoState::ApplyingPatches => (
            "Abort applying patches?",
            "Git returns the branch and working tree to before the first patch. Conflicts resolved so far are lost.",
            "Abort",
        ),
        RepoState::Bisecting => (
            "Reset the bisect?",
            "Git ends the bisect and returns to the branch it started from. The commits tested so far are forgotten.",
            "Reset bisect",
        ),
    })
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
            let resolve = entity.clone();
            banner = banner.action(
                crate::ui::button("in-progress-resolve", "Resolve conflicts").on_click(
                    move |_, window, cx| {
                        resolve.update(cx, |this, cx| this.open_conflicts(window, cx));
                    },
                ),
            );
        }

        if let Some(enabled) = state.can_continue {
            let mut button =
                crate::ui::button("in-progress-continue", "Continue").disabled(!enabled);
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
            "Reset bisect"
        } else {
            "Abort…"
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
            RepoState::Merging => "continue the merge",
            RepoState::Rebasing => "continue the rebase",
            RepoState::CherryPicking => "continue the cherry-pick",
            RepoState::Reverting => "continue the revert",
            RepoState::ApplyingPatches => "continue applying patches",
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
            RepoState::Bisecting => "reset the bisect",
            RepoState::Merging => "abort the merge",
            RepoState::Rebasing => "abort the rebase",
            RepoState::CherryPicking => "abort the cherry-pick",
            RepoState::Reverting => "abort the revert",
            RepoState::ApplyingPatches => "abort applying patches",
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
        use strata_core::repo::RebaseOnto;

        let oid_a: strata_core::Oid = "a1b2c3d000000000000000000000000000000000"
            .parse()
            .expect("oid");
        let oid_b: strata_core::Oid = "9f8e7d6000000000000000000000000000000000"
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
