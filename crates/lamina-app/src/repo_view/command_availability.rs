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

use lamina_core::HeadInfo;

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
            "lamina::CompareCommitsThreeWay",
            compare::three_commit_blocked(repo_open, &selected),
        );
        map.block(
            "lamina::CompareBranchThreeWay",
            self.three_way_branch_blocked(),
        );

        self.block_graph_moves(&mut map);
        self.block_history_writes(&mut map);
        self.block_navigation(&mut map);
        self.block_local_machine_commands(&mut map);
        map.block(
            "lamina::CancelRunningOperation",
            (!self.can_cancel_running_operation())
                .then(|| t!("status.nothing_running").to_string()),
        );
        map.block("lamina::RefreshWorkingTree", self.refresh_blocked());
        map
    }

    /// Refresh needs a session. A remote tab that has not connected yet says
    /// so, instead of looking like a command that ran and did nothing.
    fn refresh_blocked(&self) -> Option<String> {
        if self.session.is_some() {
            return None;
        }
        Some(if self.is_ssh() {
            t!("title_bar.not_connected").to_string()
        } else {
            t!("compare.no_repository").to_string()
        })
    }

    /// Commands that need this machine: a terminal or an editor. A remote tab
    /// can run both against the host through `ssh`, but only while the
    /// connection is up. The config viewer needs no local checkout: a connected
    /// tab reads and writes the host's files through the helper.
    fn block_local_machine_commands(&self, map: &mut AvailabilityMap) {
        if !self.is_ssh() {
            return;
        }
        let disconnected = if self.remote_connected() {
            None
        } else {
            Some(t!("title_bar.not_connected").to_string())
        };
        map.block("lamina::OpenGitConfig", disconnected.clone());
        map.block("lamina::OpenInEditor", disconnected.clone());
        // Without the embedded terminal nothing can run `ssh` for the user, so
        // the external terminal stays closed on a remote tab in that build.
        #[cfg(feature = "terminal")]
        let terminal = disconnected;
        #[cfg(not(feature = "terminal"))]
        let terminal = Some(t!("remote_repo.no_terminal").to_string());
        map.block("lamina::OpenTerminalHere", terminal.clone());
        #[cfg(feature = "terminal")]
        map.block("lamina::ToggleTerminal", terminal);
    }

    /// The selected rows' real commits, oldest first. The synthetic benchmark
    /// has ids but no trees, so its rows contribute nothing, exactly as they do
    /// to the command's own gatherer.
    fn palette_selected_commits(&self) -> Vec<lamina_core::Oid> {
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
                "lamina::GoToParent",
                "lamina::GoToSecondParent",
                "lamina::GoToChild",
                "lamina::GoToHead",
            ] {
                map.block(action, Some(t!("compare.no_repository").to_string()));
            }
            return;
        };
        let snapshot = session.snapshot();
        let head = head_target(snapshot.head.as_ref());
        for (action, movement) in [
            ("lamina::GoToParent", GraphMove::Parent),
            ("lamina::GoToSecondParent", GraphMove::SecondParent),
            ("lamina::GoToChild", GraphMove::Child),
            ("lamina::GoToHead", GraphMove::Head),
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
        map.block("lamina::ApplyPatch", self.patch_blocked());
        map.block("lamina::StashChanges", self.stash_blocked());
        map.block("lamina::CreateBranchFromHead", self.create_branch_blocked());
        let remote = self.remote_blocked();
        for action in [
            "lamina::FetchRemotes",
            "lamina::PullBranch",
            "lamina::PushBranch",
        ] {
            map.block(action, remote.clone());
        }
        map.block("lamina::UndoLastAction", self.undo_blocked());
        map.block("lamina::RedoLastAction", self.undo_blocked());
    }

    fn patch_blocked(&self) -> Option<String> {
        self.session
            .is_none()
            .then(|| self.no_session_reason())
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
            return Some(self.no_session_reason());
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
    ///
    /// A remote tab uses the same gate. The helper runs the restore, so being
    /// on another machine is not itself a reason to close the command.
    fn undo_blocked(&self) -> Option<String> {
        if self.session.is_none() {
            return Some(self.no_session_reason());
        }
        let gate = self.undo_gate();
        if gate.busy {
            return Some(t!("branch.reason.busy").to_string());
        }
        gate.pending.then(|| t!("reflog.finish_first").to_string())
    }

    fn block_navigation(&self, map: &mut AvailabilityMap) {
        if self.session.is_none() {
            for action in ["lamina::NavigateBack", "lamina::NavigateForward"] {
                map.block(action, Some(t!("compare.no_repository").to_string()));
            }
            return;
        }
        map.block(
            "lamina::NavigateBack",
            (!self.can_navigate_back()).then(|| t!("navigation.no_back").to_string()),
        );
        map.block(
            "lamina::NavigateForward",
            (!self.can_navigate_forward()).then(|| t!("navigation.no_forward").to_string()),
        );
    }
}

#[cfg(test)]
mod tests {
    use gpui::{QuitMode, VisualTestContext};
    use lamina_core::test_support::TestRepo;

    use super::*;
    use crate::repo_view::Launch;

    const THREE_WAY_COMMITS: &str = "lamina::CompareCommitsThreeWay";
    const THREE_WAY_BRANCH: &str = "lamina::CompareBranchThreeWay";

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
                view.reveal_ref(lamina_core::RefKind::LocalBranch, "feature", window, cx);
            });
        });
        let reason = blocked_reason(&view, &mut vcx, THREE_WAY_BRANCH);
        assert_eq!(
            reason, None,
            "a non-current branch with a merge base can be compared"
        );
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::OpenGitConfig"),
            None,
            "a local repository can show its config"
        );
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::OpenTerminalHere"),
            None
        );
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::OpenInEditor"),
            None
        );
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::CancelRunningOperation").as_deref(),
            Some(t!("status.nothing_running").as_ref()),
            "an idle repository has nothing to cancel"
        );
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::RefreshWorkingTree"),
            None,
            "a local repository can reload its working tree"
        );
    }

    #[gpui::test]
    fn a_remote_tab_names_why_local_machine_commands_cannot_run(cx: &mut gpui::TestAppContext) {
        cx.update(prepare);
        let window = cx.add_window(|window, cx| {
            RepoView::new(
                Launch::Remote(crate::remote_open::RemoteLaunch {
                    spec: crate::remote_open::RemoteSpec {
                        host: "git@example.com".into(),
                        port: None,
                        path: "/home/repo".into(),
                    },
                    auto_connect: false,
                    ssh: None,
                }),
                window,
                cx,
            )
        });
        let view = window.root(cx).expect("the window's root view");
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        assert_eq!(
            view.read_with(&vcx, |view, _| view.no_session_reason()),
            t!("remote.not_connected").to_string(),
            "a restored tab points at Connect, not at the generic no-repository reason"
        );
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::OpenGitConfig").as_deref(),
            Some(t!("title_bar.not_connected").as_ref()),
            "the config window needs the host's session"
        );
        #[cfg(feature = "terminal")]
        let terminal = Some(t!("title_bar.not_connected").to_string());
        #[cfg(not(feature = "terminal"))]
        let terminal = Some(t!("remote_repo.no_terminal").to_string());
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::OpenTerminalHere").as_deref(),
            terminal.as_deref()
        );
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::OpenInEditor").as_deref(),
            Some(t!("title_bar.not_connected").as_ref())
        );
        #[cfg(feature = "terminal")]
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::ToggleTerminal").as_deref(),
            Some(t!("title_bar.not_connected").as_ref())
        );
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::CancelRunningOperation").as_deref(),
            Some(t!("status.nothing_running").as_ref()),
            "a disconnected tab is not running a command"
        );
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::RefreshWorkingTree").as_deref(),
            Some(t!("title_bar.not_connected").as_ref()),
            "refresh cannot run until the tab is connected"
        );
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::UndoLastAction").as_deref(),
            Some(t!("remote.not_connected").as_ref()),
            "undo needs the session a restored tab has not opened yet"
        );
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::ApplyPatch"),
            Some(t!("remote.not_connected").to_string()),
            "a patch needs the host's session"
        );
        assert_eq!(
            blocked_reason(&view, &mut vcx, THREE_WAY_COMMITS).as_deref(),
            Some(t!("compare.no_repository").as_ref()),
            "a disconnected tab has no commits to compare"
        );
        assert_eq!(
            blocked_reason(&view, &mut vcx, THREE_WAY_BRANCH).as_deref(),
            Some(t!("compare.select_branch").as_ref()),
            "a disconnected tab has no revealed branch"
        );
    }

    /// A connected remote tab runs the editor and the terminal against the
    /// host, so those rows must lose their reason once the session is up.
    #[cfg(unix)]
    #[gpui::test]
    fn a_connected_remote_tab_can_run_commands_that_follow_it_to_the_host(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::remote_open::{RemoteLaunch, RemoteSpec, SshOverride};
        use crate::repo_view::remote_fixture::{install_host_helper, layout_fake_ssh, scratch_dir};

        cx.update(prepare);
        let repo = TestRepo::new();
        repo.write("README.md", "alpha\n");
        repo.commit("alpha");
        // The helper's reader thread delivers events.
        cx.dispatcher.allow_parking();

        let scratch = scratch_dir("lamina-app-availability");
        let laid = layout_fake_ssh(&scratch);
        let helper = crate::repo_view::remote_fixture::wait_for_helper();
        install_host_helper(&helper, &laid.helpers);

        let window = cx.add_window(|window, cx| {
            RepoView::new(
                Launch::Remote(RemoteLaunch {
                    spec: RemoteSpec {
                        host: "git.example.com".into(),
                        port: None,
                        path: repo.path().to_string_lossy().into_owned(),
                    },
                    auto_connect: true,
                    ssh: Some(SshOverride {
                        program: laid.ssh.clone(),
                        helper_dir: laid.helpers.clone(),
                    }),
                }),
                window,
                cx,
            )
        });
        let view = window.root(cx).expect("the window's root view");
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        while vcx.read(|cx| view.read(cx).commit_count()) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the remote history did not load"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
            vcx.run_until_parked();
        }

        #[cfg(feature = "terminal")]
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::OpenTerminalHere"),
            None,
            "a connected tab can open a terminal on the host"
        );
        #[cfg(not(feature = "terminal"))]
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::OpenTerminalHere").as_deref(),
            Some(t!("remote_repo.no_terminal").as_ref()),
            "this build has no way to reach the host"
        );
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::OpenInEditor"),
            None,
            "a connected tab can fetch the file for the editor"
        );
        #[cfg(feature = "terminal")]
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::ToggleTerminal"),
            None
        );
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::OpenGitConfig"),
            None,
            "a connected tab reads and writes the host's config"
        );
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::UndoLastAction"),
            None,
            "undo runs on the helper; an empty reflog is not a block"
        );
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::RedoLastAction"),
            None,
            "redo uses the same helper write as undo"
        );
        assert_eq!(
            blocked_reason(&view, &mut vcx, "lamina::ApplyPatch"),
            None,
            "a connected tab sends the patch bytes to the helper"
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }
}
