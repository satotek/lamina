//! What every git command the view starts is doing, for the one activity
//! indicator the app draws.
//!
//! The session reports only an [`OperationId`] while it runs a command, so the
//! view records the [`Operation`] it submitted under that id. The record also
//! carries the start time: a command that finishes quickly must not flash the
//! status bar, and a slow one should say how long it has been running.
//!
//! The spinner and the progress line animate, which repaints the window. Only
//! the spinner drives frames, and it is mounted only while a command runs; the
//! tick loop below exists for the reduced-motion case, where the spinner is
//! static but the elapsed seconds still have to keep up.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, IntoElement, ParentElement as _, Pixels,
    Styled as _, Task, div, relative,
};
use strata_core::session::{Operation, OperationId};

use crate::repo_view::RepoView;
use crate::style::Design;

/// A command that finishes before this has run is not worth showing: the
/// indicator would only flicker.
pub(crate) const APPEAR_AFTER: Duration = Duration::from_millis(300);

/// Once a command has been running this long, the status bar says how many
/// seconds it has taken.
pub(crate) const ELAPSED_AFTER: Duration = Duration::from_secs(2);

/// How often the view is woken while a command runs. Frames from the spinner
/// already keep the elapsed seconds current; this covers reduced motion, so it
/// is deliberately slower than a frame.
const TICK: Duration = Duration::from_millis(300);

/// What an operation is doing, in the words the status bar shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ActivityKind {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Reset,
    Checkout,
    Commit,
    Amend,
    Stash,
    Branch,
    Tag,
    Rewrite,
    Patch,
    Worktree,
    /// A fetch, pull or push. The remote module draws its own progress line, so
    /// [`ActivityKind::label`] has nothing to say for it.
    Remote,
    /// Anything else, which still gets the generic label.
    Other,
}

impl ActivityKind {
    /// The kind of work an operation is. The mapping is what lets the status
    /// bar name a command from the id the session reports.
    pub(crate) fn of(operation: &Operation) -> Self {
        match operation {
            Operation::Merge { .. } | Operation::MergeAbort | Operation::MergeContinue => {
                Self::Merge
            }
            Operation::Rebase { .. } | Operation::RebaseEdit { .. } => Self::Rebase,
            Operation::CherryPick { .. } | Operation::CherryPickMany { .. } => Self::CherryPick,
            Operation::Revert { .. } => Self::Revert,
            Operation::Reset { .. } => Self::Reset,
            Operation::Checkout { .. } => Self::Checkout,
            Operation::Commit { amend: false, .. } => Self::Commit,
            Operation::Commit { amend: true, .. } => Self::Amend,
            Operation::StashPush { .. }
            | Operation::StashPushPaths { .. }
            | Operation::StashPop { .. }
            | Operation::StashApply { .. }
            | Operation::StashDrop { .. } => Self::Stash,
            Operation::CreateBranch { .. }
            | Operation::DeleteBranch { .. }
            | Operation::RenameBranch { .. }
            | Operation::SetBranchUpstream { .. }
            | Operation::UnsetBranchUpstream { .. } => Self::Branch,
            Operation::CreateTag { .. } | Operation::DeleteTag { .. } => Self::Tag,
            Operation::UndoReflog { .. } | Operation::RedoReflog { .. } => Self::Rewrite,
            Operation::ApplyPatch { .. } => Self::Patch,
            Operation::WorktreeAdd { .. }
            | Operation::WorktreeRemove { .. }
            | Operation::WorktreePrune => Self::Worktree,
            Operation::Fetch { .. }
            | Operation::FetchUnshallow
            | Operation::Pull { .. }
            | Operation::Push { .. } => Self::Remote,
            _ => Self::Other,
        }
    }

    /// The status bar's label, or `None` for work another surface already
    /// describes.
    pub(crate) fn label(self) -> Option<String> {
        Some(match self {
            Self::Merge => t!("activity.merge").to_string(),
            Self::Rebase => t!("activity.rebase").to_string(),
            Self::CherryPick => t!("activity.cherry_pick").to_string(),
            Self::Revert => t!("activity.revert").to_string(),
            Self::Reset => t!("activity.reset").to_string(),
            Self::Checkout => t!("activity.checkout").to_string(),
            Self::Commit => t!("activity.commit").to_string(),
            Self::Amend => t!("activity.amend").to_string(),
            Self::Stash => t!("activity.stash").to_string(),
            Self::Branch => t!("activity.branch").to_string(),
            Self::Tag => t!("activity.tag").to_string(),
            Self::Rewrite => t!("activity.rewrite").to_string(),
            Self::Patch => t!("activity.patch").to_string(),
            Self::Worktree => t!("activity.worktree").to_string(),
            Self::Remote => return None,
            Self::Other => t!("activity.working").to_string(),
        })
    }
}

/// Whether a command that started `elapsed` ago is worth showing yet.
pub(crate) fn is_visible(elapsed: Duration) -> bool {
    elapsed >= APPEAR_AFTER
}

/// The whole seconds a command has been running, once the wait is noticeable.
pub(crate) fn elapsed_seconds(elapsed: Duration) -> Option<u64> {
    (elapsed >= ELAPSED_AFTER).then_some(elapsed.as_secs())
}

/// The elapsed suffix the status bar appends after the label.
pub(crate) fn elapsed_label(elapsed: Duration) -> Option<String> {
    elapsed_seconds(elapsed).map(|seconds| t!("activity.elapsed", seconds = seconds).to_string())
}

/// The status bar's activity segment.
pub(crate) struct Status {
    pub(crate) label: String,
    pub(crate) elapsed: Option<String>,
}

struct Running {
    kind: ActivityKind,
    started: Instant,
}

/// Every command the view submitted that has not reported back, keyed by the
/// session's id. Ids grow with submission order, so the first entry is the
/// command the status bar names.
#[derive(Default)]
pub(crate) struct Activity {
    running: BTreeMap<OperationId, Running>,
    /// Wakes the view while a command runs, so the elapsed seconds keep up
    /// when reduced motion keeps the spinner from scheduling frames.
    wake: Option<Task<()>>,
}

impl Activity {
    pub(crate) fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    /// Ids the view submitted that have not reported back, oldest first.
    pub(crate) fn ids(&self) -> Vec<OperationId> {
        self.running.keys().copied().collect()
    }

    /// Drops every record. The next finished event is then ignored by the
    /// indicator, which is what a cancel wants: the segment is already gone.
    pub(crate) fn clear(&mut self) {
        self.running.clear();
        self.wake = None;
    }

    /// Records a command this view just submitted and starts the tick loop.
    pub(crate) fn begin(
        &mut self,
        id: OperationId,
        kind: ActivityKind,
        cx: &mut Context<RepoView>,
    ) {
        self.record(id, kind, Instant::now());
        if self.wake.is_none() {
            self.wake = Some(cx.spawn(async move |view, cx| {
                loop {
                    cx.background_executor().timer(TICK).await;
                    let running = view
                        .update(cx, |view, cx| {
                            if view.activity.is_empty() {
                                return false;
                            }
                            cx.notify();
                            true
                        })
                        .unwrap_or(false);
                    if !running {
                        break;
                    }
                }
            }));
        }
    }

    /// The testable half of [`Activity::begin`]: an explicit start time keeps
    /// the delay boundary out of the wall clock.
    fn record(&mut self, id: OperationId, kind: ActivityKind, started: Instant) {
        self.running.insert(id, Running { kind, started });
    }

    /// Forgets a command the session reported finished. Returns whether this
    /// command was one the indicator knew about.
    pub(crate) fn finish(&mut self, id: OperationId) -> bool {
        let removed = self.running.remove(&id).is_some();
        if self.running.is_empty() {
            // Dropping the task cancels the next wake, so an idle window is
            // not repainted for an operation that has ended.
            self.wake = None;
        }
        removed
    }

    fn current(&self) -> Option<&Running> {
        self.running.values().next()
    }

    /// The segment to draw, once the command has run long enough to matter.
    pub(crate) fn status(&self, now: Instant) -> Option<Status> {
        let running = self.current()?;
        let elapsed = now.saturating_duration_since(running.started);
        if !is_visible(elapsed) {
            return None;
        }
        Some(Status {
            label: running.kind.label()?,
            elapsed: elapsed_label(elapsed),
        })
    }

    /// Whether the progress line should be drawn. It follows the same delay,
    /// so a fast command does not make it blink either.
    pub(crate) fn progress_visible(&self, now: Instant) -> bool {
        self.current()
            .is_some_and(|running| is_visible(now.saturating_duration_since(running.started)))
    }
}

impl RepoView {
    /// The hairline under the title bar while any command runs, so progress is
    /// visible even when the status bar is not where the user is looking.
    pub(super) fn activity_progress(&self, design: &Design) -> AnyElement {
        let now = Instant::now();
        // An operation submitted outside this view has no recorded start; it
        // still gets the line rather than nothing.
        let visible = self.activity.progress_visible(now)
            || (self.activity.is_empty() && self.has_operation_in_progress());
        if !visible {
            return div().h(Pixels::ZERO).flex_none().into_any_element();
        }
        div()
            .relative()
            .flex_none()
            .w_full()
            .h(design.px(2.0))
            .overflow_hidden()
            .bg(design.border.opacity(0.35))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .h_full()
                    .w(relative(0.28))
                    .bg(design.accent)
                    .with_animation(
                        "activity-progress-line",
                        Animation::new(Duration::from_millis(1100)).repeat(),
                        |bar, delta| bar.left(relative(delta * 0.72)),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use strata_core::Oid;
    use strata_core::session::ResetMode;

    use super::*;

    fn oid() -> Oid {
        "00".repeat(20).parse().expect("hex object id")
    }

    fn operation_kind(operation: Operation) -> ActivityKind {
        ActivityKind::of(&operation)
    }

    #[test]
    fn each_operation_is_named_by_what_it_does() {
        assert_eq!(
            operation_kind(Operation::Merge {
                branch: "feat/x".to_owned(),
                no_ff: false,
            }),
            ActivityKind::Merge
        );
        assert_eq!(
            operation_kind(Operation::Rebase {
                onto: oid(),
                upstream: None,
                autostash: false,
            }),
            ActivityKind::Rebase
        );
        assert_eq!(
            operation_kind(Operation::CherryPick { commit: oid() }),
            ActivityKind::CherryPick
        );
        assert_eq!(
            operation_kind(Operation::Revert { commit: oid() }),
            ActivityKind::Revert
        );
        assert_eq!(
            operation_kind(Operation::Reset {
                target: oid(),
                mode: ResetMode::Mixed,
            }),
            ActivityKind::Reset
        );
        assert_eq!(
            operation_kind(Operation::Checkout {
                branch: "main".to_owned(),
            }),
            ActivityKind::Checkout
        );
        assert_eq!(
            operation_kind(Operation::Commit {
                message: "subject".to_owned(),
                amend: false,
            }),
            ActivityKind::Commit
        );
        assert_eq!(
            operation_kind(Operation::Commit {
                message: "subject".to_owned(),
                amend: true,
            }),
            ActivityKind::Amend
        );
        assert_eq!(
            operation_kind(Operation::StashPop { index: 0 }),
            ActivityKind::Stash,
            "push, pop and apply are the same wait to the user"
        );
        assert_eq!(
            operation_kind(Operation::CreateBranch {
                name: "feat/x".to_owned(),
                start_point: None,
                checkout: false,
            }),
            ActivityKind::Branch
        );
        assert_eq!(
            operation_kind(Operation::CreateTag {
                name: "v1.0".to_owned(),
                target: oid(),
                message: None,
            }),
            ActivityKind::Tag
        );
        assert_eq!(
            operation_kind(Operation::UndoReflog { entry: None }),
            ActivityKind::Rewrite
        );
        assert_eq!(
            operation_kind(Operation::ApplyPatch { patch: Vec::new() }),
            ActivityKind::Patch
        );
        assert_eq!(
            operation_kind(Operation::WorktreeAdd {
                path: PathBuf::from("/tmp/wt"),
                branch: None,
                new_branch: None,
                commit: None,
            }),
            ActivityKind::Worktree
        );
        assert_eq!(
            operation_kind(Operation::StagePaths(Vec::new())),
            ActivityKind::Other,
            "an operation without a label of its own still gets the fallback"
        );
    }

    #[test]
    fn the_labels_say_what_is_happening() {
        // Read the catalogue by locale instead of the process-wide current one,
        // which another test may switch while this one runs.
        assert_eq!(
            crate::i18n::lookup("en", "activity.merge").as_deref(),
            Some("Merging…")
        );
        assert_eq!(
            crate::i18n::lookup("ja", "activity.merge").as_deref(),
            Some("Merge 中…")
        );
        assert_eq!(
            crate::i18n::lookup("en", "activity.working").as_deref(),
            Some("Working…"),
            "an unknown operation must not leave the status bar blank"
        );
        assert_eq!(
            crate::i18n::lookup("ja", "activity.working").as_deref(),
            Some("実行中…")
        );
        assert_eq!(
            crate::i18n::lookup("en", "activity.elapsed").as_deref(),
            Some("%{seconds}s")
        );

        // Each kind points at its own key, whatever the active locale is.
        assert_eq!(
            ActivityKind::Merge.label(),
            Some(t!("activity.merge").to_string())
        );
        assert_eq!(
            ActivityKind::Stash.label(),
            Some(t!("activity.stash").to_string())
        );
        assert_eq!(
            ActivityKind::Other.label(),
            Some(t!("activity.working").to_string())
        );
        assert_eq!(
            ActivityKind::Remote.label(),
            None,
            "the remote module draws its own progress"
        );
    }

    #[test]
    fn a_fast_command_does_not_flash_the_indicator() {
        assert!(!is_visible(Duration::from_millis(0)));
        assert!(!is_visible(Duration::from_millis(299)));
        assert!(is_visible(APPEAR_AFTER));
    }

    #[test]
    fn elapsed_seconds_appear_after_two_seconds() {
        assert_eq!(elapsed_seconds(Duration::from_millis(1999)), None);
        assert_eq!(elapsed_seconds(ELAPSED_AFTER), Some(2));
        assert_eq!(elapsed_seconds(Duration::from_secs(12)), Some(12));
        assert_eq!(
            elapsed_label(Duration::from_secs(3)),
            Some(t!("activity.elapsed", seconds = 3).to_string())
        );
    }

    #[test]
    fn the_first_submitted_command_is_the_one_named() {
        let now = Instant::now();
        let mut activity = Activity::default();
        activity.record(OperationId(2), ActivityKind::Commit, now);
        assert_eq!(
            activity.status(now).map(|status| status.label),
            None,
            "a command that just started is not shown yet"
        );
        assert_eq!(
            activity
                .status(now + APPEAR_AFTER)
                .map(|status| status.label),
            Some(t!("activity.commit").to_string())
        );
        assert_eq!(
            activity
                .status(now + ELAPSED_AFTER)
                .and_then(|status| status.elapsed),
            Some(t!("activity.elapsed", seconds = 2).to_string())
        );

        activity.record(OperationId(1), ActivityKind::Merge, now);
        assert_eq!(
            activity
                .status(now + APPEAR_AFTER)
                .map(|status| status.label),
            Some(t!("activity.merge").to_string()),
            "the earliest queued command is the one that runs first"
        );

        assert!(activity.finish(OperationId(1)));
        assert_eq!(
            activity
                .status(now + APPEAR_AFTER)
                .map(|status| status.label),
            Some(t!("activity.commit").to_string())
        );
        assert!(activity.finish(OperationId(2)));
        assert!(activity.status(now + APPEAR_AFTER).is_none());
        assert!(!activity.finish(OperationId(2)));
    }
}
