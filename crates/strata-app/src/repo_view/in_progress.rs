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

use gpui::{AnyElement, Context, Entity, IntoElement, Window};
use strata_core::RepoState;
use strata_core::session::{Operation, OperationId};

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

/// What the view knows about the job in flight.
#[derive(Default)]
pub(crate) struct InProgress {
    /// What `run_operation` submitted, waiting for its start event; the session
    /// reports the id only, so the kind has to be kept here.
    submitted: Option<(OperationId, FinishedOperation)>,
    running: Option<Running>,
}

impl InProgress {
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

fn banner_state(state: RepoState, conflicts: usize, busy: bool) -> Option<BannerState> {
    let name = match state {
        RepoState::Normal => return None,
        RepoState::Merging => "Merging",
        RepoState::Rebasing => "Rebasing",
        RepoState::CherryPicking => "Cherry-picking",
        RepoState::Reverting => "Reverting",
        RepoState::ApplyingPatches => "Applying patches",
        RepoState::Bisecting => "Bisecting",
    };
    let text = match conflicts {
        0 => name.to_owned(),
        1 => format!("{name} · 1 conflict to resolve"),
        count => format!("{name} · {count} conflicts to resolve"),
    };
    let can_continue = (state != RepoState::Bisecting).then_some(conflicts == 0 && !busy);
    let continue_tooltip = match (can_continue, conflicts, busy) {
        (Some(false), 1, _) => Some("1 conflict left to resolve".to_owned()),
        (Some(false), count, _) if count > 1 => Some(format!("{count} conflicts left to resolve")),
        (Some(false), _, _) => Some("An operation is already running".to_owned()),
        _ => None,
    };
    Some(BannerState {
        text,
        can_continue,
        continue_tooltip,
        bisecting: state == RepoState::Bisecting,
        conflicts,
        busy,
    })
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
        let state = banner_state(snapshot.state, snapshot.conflicts.len(), busy)?;

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

        Some(banner.into_any_element())
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
        let merging = banner_state(RepoState::Merging, 2, false).expect("merging banner");
        assert_eq!(merging.text, "Merging · 2 conflicts to resolve");
        assert_eq!(merging.can_continue, Some(false));
        assert_eq!(
            merging.continue_tooltip.as_deref(),
            Some("2 conflicts left to resolve")
        );

        let resolved = banner_state(RepoState::Merging, 0, false).expect("merging banner");
        assert_eq!(resolved.text, "Merging");
        assert_eq!(resolved.can_continue, Some(true));
        assert_eq!(resolved.continue_tooltip, None);

        assert_eq!(
            banner_state(RepoState::Rebasing, 1, false)
                .expect("rebase banner")
                .text,
            "Rebasing · 1 conflict to resolve"
        );
        assert_eq!(
            banner_state(RepoState::CherryPicking, 0, false)
                .expect("cherry-pick banner")
                .text,
            "Cherry-picking"
        );
        assert_eq!(
            banner_state(RepoState::Reverting, 0, false)
                .expect("revert banner")
                .text,
            "Reverting"
        );
        assert_eq!(
            banner_state(RepoState::ApplyingPatches, 0, false)
                .expect("am banner")
                .text,
            "Applying patches"
        );
        assert_eq!(banner_state(RepoState::Normal, 0, false), None);
    }

    #[test]
    fn a_bisect_is_reset_rather_than_continued() {
        let bisecting = banner_state(RepoState::Bisecting, 0, false).expect("bisect banner");
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
        let busy = banner_state(RepoState::Rebasing, 0, true).expect("rebase banner");
        assert_eq!(busy.can_continue, Some(false));
        assert_eq!(
            busy.continue_tooltip.as_deref(),
            Some("An operation is already running")
        );
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
