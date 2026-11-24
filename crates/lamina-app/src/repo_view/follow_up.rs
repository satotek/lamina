//! The one more step some operations want after they succeed.
//!
//! A new worktree is offered in a new tab, an applied patch is reported in the
//! status bar. Both are registered under the id the session returned for the
//! command that asked for them, so another operation finishing first cannot
//! run them: the id is the only thing that says which command ended.

use std::collections::HashMap;
use std::path::PathBuf;

use lamina_core::session::OperationId;

/// What to do once an operation reports success.
pub(super) enum FollowUp {
    /// The worktree a `worktree add` created, to offer in a new tab.
    OpenWorktree(PathBuf),
    /// The "Apply patch…" command, whose success the command log alone would
    /// not surface.
    PatchApplied,
}

#[derive(Default)]
pub(super) struct FollowUps(HashMap<OperationId, FollowUp>);

impl FollowUps {
    /// Registers the step for the operation that was just started.
    pub(super) fn expect(&mut self, id: OperationId, follow_up: FollowUp) {
        self.0.insert(id, follow_up);
    }

    /// Takes the step registered for `id`, if any, once it reports back.
    ///
    /// A failure drops the step: it exists to continue a successful command,
    /// and the failure is already reported through the status bar.
    pub(super) fn finished(&mut self, id: OperationId, succeeded: bool) -> Option<FollowUp> {
        let follow_up = self.0.remove(&id)?;
        succeeded.then_some(follow_up)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{FollowUp, FollowUps};
    use lamina_core::session::OperationId;

    #[test]
    fn a_follow_up_runs_only_for_the_operation_that_registered_it() {
        let mut follow_ups = FollowUps::default();
        follow_ups.expect(OperationId(1), FollowUp::OpenWorktree("/tmp/wt".into()));

        assert!(follow_ups.finished(OperationId(2), true).is_none());
        let Some(FollowUp::OpenWorktree(path)) = follow_ups.finished(OperationId(1), true) else {
            panic!("the registered follow-up runs when its own operation ends");
        };
        assert_eq!(path, PathBuf::from("/tmp/wt"));
    }

    #[test]
    fn a_failed_operation_drops_its_follow_up() {
        let mut follow_ups = FollowUps::default();
        follow_ups.expect(OperationId(3), FollowUp::PatchApplied);

        assert!(follow_ups.finished(OperationId(3), false).is_none());
        assert!(follow_ups.finished(OperationId(3), true).is_none());
    }

    /// Two commands in flight: the unrelated one finishes first and must leave
    /// the other's step alone, which is the bug the unkeyed field caused.
    #[test]
    fn another_operation_finishing_first_keeps_the_offer_waiting() {
        let mut follow_ups = FollowUps::default();
        follow_ups.expect(OperationId(4), FollowUp::OpenWorktree("/tmp/wt".into()));

        assert!(follow_ups.finished(OperationId(5), true).is_none());
        assert!(
            follow_ups.finished(OperationId(5), false).is_none(),
            "a failure elsewhere must not drop another operation's step"
        );
        let Some(FollowUp::OpenWorktree(path)) = follow_ups.finished(OperationId(4), true) else {
            panic!("the worktree offer still waits for its own operation");
        };
        assert_eq!(path, PathBuf::from("/tmp/wt"));
    }
}
