//! Why the palette's repository commands cannot run right now.
//!
//! The palette lists every command the view handles, including the ones whose
//! handler would return without doing anything: a compare command without the
//! right selection, a write while git is busy, or a branch command whose
//! branch is not the one on screen. The palette reads this map when it opens,
//! so an unavailable row can say why instead of disappearing or failing
//! silently.
//!
//! The reasons come from the same rules the menus and buttons use. Keeping one
//! source for them means a row and the button beside it never disagree.

use strata_core::HeadInfo;

use super::RepoView;
use super::compare;
use super::history_ops::head_target;
use super::navigation::{self, GraphMove};
use crate::palette::AvailabilityMap;

impl RepoView {
    /// The reasons the palette's repository commands are unavailable, keyed by
    /// the action's registry name. An action with no entry can run.
    pub(crate) fn command_availability(&self) -> AvailabilityMap {
        let mut map = AvailabilityMap::default();
        let repo_open = self.session.is_some();

        let selected = self.palette_selected_commits();
        map.block(
            "strata::CompareCommitsThreeWay",
            compare::three_commit_blocked(repo_open, &selected),
        );
        map.block(
            "strata::CompareBranchThreeWay",
            self.three_way_branch_blocked(),
        );

        self.block_graph_moves(&mut map);
        self.block_history_writes(&mut map);
        self.block_navigation(&mut map);
        map
    }

    /// The selected rows' real commits, oldest first. The synthetic benchmark
    /// has ids but no trees, so its rows contribute nothing, exactly as they do
    /// to the command's own gatherer.
    fn palette_selected_commits(&self) -> Vec<strata_core::Oid> {
        let Some(source) = self.source() else {
            return Vec::new();
        };
        self.selection
            .oldest_first()
            .into_iter()
            .filter_map(|index| source.commit(index).map(|commit| commit.id))
            .collect()
    }

    /// The revealed branch's three-way compare. Without a revealed branch the
    /// command has nothing to open, which is why the branch row's menu is the
    /// only other way in.
    fn three_way_branch_blocked(&self) -> Option<String> {
        let Some(branch) = self.branches.revealed_local_branch() else {
            return Some(t!("compare.select_branch").to_string());
        };
        let Some(session) = self.session.as_ref() else {
            return Some(t!("compare.no_repository").to_string());
        };
        let snapshot = session.snapshot();
        let is_current = matches!(
            snapshot.head.as_ref(),
            Some(HeadInfo::Branch { short_name, .. }) if short_name == branch
        );
        let busy = self.is_busy();
        compare::three_way_with_current_branch_blocked(
            true,
            is_current,
            busy,
            self.compare_head(),
            self.compare_branch_tip(branch),
            // The ancestry read only matters once the permanent reasons pass.
            if is_current || busy {
                None
            } else {
                self.compare_branch_merge_base(branch)
            },
        )
    }

    /// The graph jumps, which need a selected commit and a commit to land on.
    fn block_graph_moves(&self, map: &mut AvailabilityMap) {
        let Some(session) = self.session.as_ref() else {
            // `--fake` and the error screen handle the actions but have no
            // session to move through.
            for action in [
                "strata::GoToParent",
                "strata::GoToSecondParent",
                "strata::GoToChild",
                "strata::GoToHead",
            ] {
                map.block(action, Some(t!("compare.no_repository").to_string()));
            }
            return;
        };
        let snapshot = session.snapshot();
        let head = head_target(snapshot.head.as_ref());
        for (action, movement) in [
            ("strata::GoToParent", GraphMove::Parent),
            ("strata::GoToSecondParent", GraphMove::SecondParent),
            ("strata::GoToChild", GraphMove::Child),
            ("strata::GoToHead", GraphMove::Head),
        ] {
            let blocked =
                navigation::graph_target(&snapshot.commits, head, self.selection_commit, movement)
                    .is_none()
                    .then(|| movement.blocked_reason());
            map.block(action, blocked);
        }
    }

    /// The write commands the menus disable with a reason: the history and
    /// remote operations, stashing and patching.
    fn block_history_writes(&self, map: &mut AvailabilityMap) {
        map.block("strata::ApplyPatch", self.patch_blocked());
        map.block("strata::StashChanges", self.stash_blocked());
        map.block("strata::CreateBranchFromHead", self.create_branch_blocked());
        let remote = self.remote_blocked();
        for action in [
            "strata::FetchRemotes",
            "strata::PullBranch",
            "strata::PushBranch",
        ] {
            map.block(action, remote.clone());
        }
        map.block("strata::UndoLastAction", self.undo_blocked());
        map.block("strata::RedoLastAction", self.undo_blocked());
    }

    fn patch_blocked(&self) -> Option<String> {
        self.session
            .is_none()
            .then(|| t!("remote.no_session").to_string())
            .or_else(|| self.is_busy().then(|| t!("branch.reason.busy").to_string()))
    }

    fn stash_blocked(&self) -> Option<String> {
        let Some(session) = self.session.as_ref() else {
            return Some(t!("stash.needs_repo").to_string());
        };
        if self.is_busy() {
            return Some(t!("stash.busy").to_string());
        }
        let status = &session.snapshot().status;
        if status.staged.is_empty() && status.unstaged.is_empty() {
            return Some(t!("stash.nothing").to_string());
        }
        None
    }

    fn create_branch_blocked(&self) -> Option<String> {
        if self.session.is_none() {
            return Some(t!("remote.no_session").to_string());
        }
        if self.is_busy() {
            return Some(t!("branch.reason.busy").to_string());
        }
        self.branches
            .dialog_open()
            .then(|| t!("branch.reason.dialog_open").to_string())
    }

    /// Undo only reports a reason when the run would refuse outright. An empty
    /// reflog is not one: running the command loads the rows first, and the
    /// handler says what happened once they arrive.
    fn undo_blocked(&self) -> Option<String> {
        if self.session.is_none() {
            return Some(t!("remote.no_session").to_string());
        }
        let gate = self.undo_gate();
        if gate.busy {
            return Some(t!("branch.reason.busy").to_string());
        }
        gate.pending.then(|| t!("reflog.finish_first").to_string())
    }

    fn block_navigation(&self, map: &mut AvailabilityMap) {
        if self.session.is_none() {
            for action in ["strata::NavigateBack", "strata::NavigateForward"] {
                map.block(action, Some(t!("compare.no_repository").to_string()));
            }
            return;
        }
        map.block(
            "strata::NavigateBack",
            (!self.can_navigate_back()).then(|| t!("navigation.no_back").to_string()),
        );
        map.block(
            "strata::NavigateForward",
            (!self.can_navigate_forward()).then(|| t!("navigation.no_forward").to_string()),
        );
    }
}

#[cfg(test)]
mod tests {
    use gpui::{QuitMode, VisualTestContext};
    use strata_core::test_support::TestRepo;

    use super::*;
    use crate::repo_view::Launch;

    const THREE_WAY_COMMITS: &str = "strata::CompareCommitsThreeWay";
    const THREE_WAY_BRANCH: &str = "strata::CompareBranchThreeWay";

    fn prepare(cx: &mut gpui::App) {
        cx.set_quit_mode(QuitMode::Explicit);
        gpui_component::init(cx);
        cx.set_global(crate::settings::Settings::default());
        crate::actions::bind(cx);
    }

    fn wait_for_commits(view: &gpui::Entity<RepoView>, cx: &mut VisualTestContext, count: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while view.read_with(cx, |view, _| view.commit_count()) < count {
            assert!(
                std::time::Instant::now() < deadline,
                "the history did not load"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn blocked_reason(
        view: &gpui::Entity<RepoView>,
        cx: &mut VisualTestContext,
        action: &str,
    ) -> Option<String> {
        view.read_with(cx, |view, _| {
            view.command_availability()
                .reason(action)
                .map(str::to_owned)
        })
    }

    #[gpui::test]
    fn three_selected_commits_make_the_three_way_compare_available(cx: &mut gpui::TestAppContext) {
        cx.update(prepare);
        let repo = TestRepo::new();
        for message in ["one", "two", "three", "four"] {
            repo.write("file.txt", &format!("{message}\n"));
            repo.commit(message);
        }
        let window = cx.add_window(|window, cx| {
            RepoView::new(Launch::open_quiet(repo.path(), None), window, cx)
        });
        let view = window.root(cx).expect("the window's root view");
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        wait_for_commits(&view, &mut vcx, 4);

        let reason = blocked_reason(&view, &mut vcx, THREE_WAY_COMMITS);
        assert_eq!(
            reason.as_deref(),
            Some(t!("compare.select_three").as_ref()),
            "one row is not three commits"
        );

        vcx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let order = [0, 1, 2, 3];
                view.selection.click(0, false, false, &order);
                view.selection.click(1, false, true, &order);
                view.selection.click(2, false, true, &order);
                view.selection_commit = view.selection.focused().and_then(|index| {
                    view.source()
                        .and_then(|source| source.commit(index).map(|c| c.id))
                });
                cx.notify();
            });
        });

        let reason = blocked_reason(&view, &mut vcx, THREE_WAY_COMMITS);
        assert_eq!(reason, None, "three selected commits are enough");
    }

    #[gpui::test]
    fn the_three_way_branch_command_names_why_it_cannot_run(cx: &mut gpui::TestAppContext) {
        cx.update(prepare);
        let repo = TestRepo::new();
        repo.write("file.txt", "one\n");
        repo.commit("one");
        repo.git(&["branch", "feature"]);
        repo.write("file.txt", "two\n");
        repo.commit("two");
        let window = cx.add_window(|window, cx| {
            RepoView::new(Launch::open_quiet(repo.path(), None), window, cx)
        });
        let view = window.root(cx).expect("the window's root view");
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        wait_for_commits(&view, &mut vcx, 2);

        let reason = blocked_reason(&view, &mut vcx, THREE_WAY_BRANCH);
        assert_eq!(
            reason.as_deref(),
            Some(t!("compare.select_branch").as_ref()),
            "no branch has been revealed yet"
        );

        vcx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.reveal_ref(strata_core::RefKind::LocalBranch, "feature", window, cx);
            });
        });
        let reason = blocked_reason(&view, &mut vcx, THREE_WAY_BRANCH);
        assert_eq!(
            reason, None,
            "a non-current branch with a merge base can be compared"
        );
    }
}
