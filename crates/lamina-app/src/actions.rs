//! Window-level commands and their default key bindings.
//!
//! Every user command is a GPUI action rather than a raw key match, so the
//! same command can later be reached from a menu, a command palette or a
//! user keymap without touching the view that handles it. Bindings are scoped
//! to the `RepoView` key context; text inputs add an `Input` context, which
//! excludes them from the single-letter navigation bindings.

use gpui::{App, actions};

/// Key context set on the repository view's root element.
pub const REPO_VIEW: &str = "RepoView";
/// [`REPO_VIEW`] minus text fields, for bindings on unmodified keys.
/// gpui-component puts the `Input` context on every text field (`Input` and
/// `Textarea`); bare letters are text while it has focus, so the list
/// navigation must not see them there.
pub const REPO_VIEW_LIST: &str = "RepoView && !Input";
/// Key context set on the main window's workspace root element, keeping
/// tab shortcuts out of secondary windows such as Settings.
pub const WORKSPACE: &str = "Workspace";

actions!(
    lamina,
    [
        SelectPrevious,
        SelectNext,
        /// Moves the focused commit row and extends the selected range.
        ExtendSelectionPrevious,
        /// Moves the focused commit row and extends the selected range.
        ExtendSelectionNext,
        SelectPageUp,
        SelectPageDown,
        SelectFirst,
        SelectLast,
        ToggleFrameStats,
        /// Gives the details area the whole history view, or restores the
        /// split. Handled on `RepoView` by `repo_view::split`.
        ToggleDetailsMaximized,
        /// Folds the sidebar to its icon rail, or expands it again. Handled on
        /// `RepoView` by `repo_view::sidebar`.
        ToggleSidebar,
        Quit,
        /// Opens the settings window. Handled globally by `settings_view`.
        OpenSettings,
        /// Opens the read-only git config viewer for the active repository.
        /// Handled globally by `git_config_view`.
        OpenGitConfig,
        /// Opens another repository (picker, recent list). Handled by the
        /// workspace.
        OpenRepository,
        /// Opens a repository on another machine. Handled by the workspace.
        OpenRemoteRepository,
        /// Opens the clone dialog to create a repository from a URL. Handled
        /// by the workspace.
        CloneRepository,
        /// Opens the init dialog to create an empty repository. Handled by the
        /// workspace.
        NewRepository,
        /// Opens the dialog to compare arbitrary files or folders. Handled by
        /// the workspace.
        OpenCompare,
        /// Opens the repository picker to add a new tab. Handled by `Workspace`.
        NewTab,
        /// Closes the active tab, or the window when it is the last tab.
        CloseTab,
        /// Reopens the most recently closed repository tab at its old position.
        /// Handled by `Workspace`.
        ReopenClosedTab,
        /// Switches to the next repository tab.
        NextTab,
        /// Switches to the previous repository tab.
        PreviousTab,
        SelectTab1,
        SelectTab2,
        SelectTab3,
        SelectTab4,
        SelectTab5,
        SelectTab6,
        SelectTab7,
        SelectTab8,
        SelectTab9,
        /// Stashes the working tree's changes. Handled on `RepoView` by
        /// `stash.rs`.
        StashChanges,
        /// Restores the state before the newest reflog action lamina can undo.
        /// Handled on `RepoView` by `reflog.rs`.
        UndoLastAction,
        /// Re-applies the last action this view undid. Handled on `RepoView`
        /// by `reflog.rs`.
        RedoLastAction,
        /// Compares the branch the sidebar revealed last with the current
        /// branch across their merge base. Handled on `RepoView` by
        /// `repo_view::compare`.
        CompareBranchThreeWay,
        /// Compares exactly three selected commits with one as the base.
        /// Handled on `RepoView` by `repo_view::compare`.
        CompareCommitsThreeWay,
        /// Opens a terminal in the repository's working tree. Handled on
        /// `RepoView` by `title_bar.rs`.
        OpenTerminalHere,
        /// Opens the selected file in the editor. Handled on `RepoView` by
        /// `title_bar.rs`.
        OpenInEditor,
        /// Opens the command palette over the active window. Handled globally
        /// by the palette module.
        TogglePalette,
        /// Opens the branch switcher under the title bar's branch chip.
        /// Handled on `RepoView` by `branch_switcher.rs`.
        SwitchBranch,
        /// Returns to the previous commit jump, or the parent repository this
        /// one was opened from. Handled on `RepoView` by `navigation.rs`.
        NavigateBack,
        /// Returns to the commit jump Back left. Handled on `RepoView` by
        /// `navigation.rs`.
        NavigateForward,
        /// Selects the selected commit's first parent in the history list.
        GoToParent,
        /// Selects the second parent of a merge.
        GoToSecondParent,
        /// Selects the first child of the selected commit in display order.
        GoToChild,
        /// Selects the commit HEAD points at.
        GoToHead,
        /// Closes the compare view. Handled on `RepoView` by
        /// `repo_view::compare`; a no-op while no comparison is open.
        CloseCompare,
        /// Shows every line of the diff on screen, or only the changes again.
        /// Handled on `RepoView` by the diff panes.
        ToggleWholeFile,
        /// Applies a patch file chosen by the user. Handled on `RepoView` by
        /// `repo_view::patch`.
        ApplyPatch,
        /// Stops the git command that is running, local or remote. Handled on
        /// `RepoView`. Escape reaches it through search dismiss, because that
        /// binding already owns the key.
        CancelRunningOperation,
        /// Asks the session for a fresh status. A remote helper that is watching
        /// already pushes outside edits; this is still how the user asks, and
        /// how an unwatched helper catches up. Handled on `RepoView`.
        RefreshWorkingTree,
    ]
);

#[cfg(feature = "terminal")]
actions!(
    lamina,
    [
        /// Opens or closes the embedded terminal panel for the active tab.
        ToggleTerminal,
        /// Opens a new session tab inside the embedded terminal panel.
        NewTerminalTab,
        /// Closes the active session tab inside the embedded terminal panel.
        CloseTerminalTab,
        /// Switches to the next session tab inside the embedded terminal panel.
        NextTerminalTab,
        /// Switches to the previous session tab inside the embedded terminal panel.
        PreviousTerminalTab,
    ]
);

pub fn bind(cx: &mut App) {
    crate::keymap::bind_group(cx, crate::keymap::Group::Actions);
    cx.on_action(|_: &Quit, cx| cx.quit());
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::{
        AppContext as _, Context, Entity, FocusHandle, Focusable as _, InteractiveElement as _,
        IntoElement, ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext,
        Window, div,
    };
    use gpui_component::input::{Input, InputState};

    use super::{REPO_VIEW, SelectNext, SelectPrevious, bind};

    /// Stands in for the repository view's list: the root carries
    /// [`REPO_VIEW`], as `RepoView` does, and holds a text field beneath it.
    struct Probe {
        focus: FocusHandle,
        input: Entity<InputState>,
        selected_commit: usize,
        navigations: Rc<Cell<usize>>,
    }

    impl Probe {
        fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
            Self {
                focus: cx.focus_handle(),
                input: cx.new(|cx| InputState::new(window, cx).placeholder("Filter commits")),
                // A middle row, so a step in either direction is observable.
                selected_commit: 1,
                navigations: Rc::new(Cell::new(0)),
            }
        }

        fn select(&mut self, delta: isize) {
            self.selected_commit = self.selected_commit.saturating_add_signed(delta);
            self.navigations.set(self.navigations.get() + 1);
        }
    }

    impl Render for Probe {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .flex()
                .flex_col()
                .size_full()
                .track_focus(&self.focus)
                .key_context(REPO_VIEW)
                .on_action(cx.listener(|this, _: &SelectNext, _, _| this.select(1)))
                .on_action(cx.listener(|this, _: &SelectPrevious, _, _| this.select(-1)))
                .child(Input::new(&self.input))
        }
    }

    fn probe(cx: &mut TestAppContext) -> (Entity<Probe>, &mut VisualTestContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            bind(cx);
        });
        let (probe, cx) = cx.add_window_view(Probe::new);
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        (probe, cx)
    }

    fn focus(cx: &mut VisualTestContext, handle: &FocusHandle) {
        cx.update(|window, cx| window.focus(handle, cx));
    }

    /// The bug: `RepoView`'s unmodified letters matched the whole view, so a
    /// text field inside it never received `j` or `k`; the list moved instead.
    #[gpui::test]
    fn typing_into_a_field_inside_the_repo_view_does_not_navigate_the_list(
        cx: &mut TestAppContext,
    ) {
        let (probe, cx) = probe(cx);
        let input = probe.read_with(cx, |this, cx| this.input.read(cx).focus_handle(cx));
        focus(cx, &input);

        cx.simulate_keystrokes("k j");

        let (text, selected, navigations) = probe.read_with(cx, |this, cx| {
            (
                this.input.read(cx).value().to_string(),
                this.selected_commit,
                this.navigations.get(),
            )
        });
        assert_eq!(text, "kj", "both letters must reach the focused field");
        assert_eq!(selected, 1, "the field's keystroke moved the selection");
        assert_eq!(navigations, 0, "no list action may fire from the field");
    }

    /// The bindings are narrowed to `!Input`, not removed: with the list
    /// focused they still walk the commit rows.
    #[gpui::test]
    fn j_and_k_still_navigate_the_list_when_it_has_focus(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let focus_handle = probe.read_with(cx, |this, _| this.focus.clone());
        focus(cx, &focus_handle);

        cx.simulate_keystrokes("j j k");

        let (selected, navigations) =
            probe.read_with(cx, |this, _| (this.selected_commit, this.navigations.get()));
        assert_eq!(selected, 2, "j twice and k once must land one row down");
        assert_eq!(navigations, 3, "every keystroke must reach the list");
    }
}
