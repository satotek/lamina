//! Window-level commands and their default key bindings.
//!
//! Every user command is a GPUI action rather than a raw key match, so the
//! same command can later be reached from a menu, a command palette or a
//! user keymap without touching the view that handles it. Bindings are scoped
//! to the `RepoView` key context; text inputs add an `Input` context, which
//! excludes them from the single-letter navigation bindings.

use gpui::{App, KeyBinding, actions};

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
    strata,
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
        Quit,
        /// Opens the settings window. Handled globally by `settings_view`.
        OpenSettings,
        /// Opens another repository (picker, recent list). Handled by the
        /// workspace.
        OpenRepository,
        /// Opens the clone dialog to create a repository from a URL. Handled
        /// by the workspace.
        CloneRepository,
        /// Opens the init dialog to create an empty repository. Handled by the
        /// workspace.
        NewRepository,
        /// Opens the repository picker to add a new tab. Handled by `Workspace`.
        NewTab,
        /// Closes the active tab, or the window when it is the last tab.
        CloseTab,
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
        /// Restores the state before the newest reflog action strata can undo.
        /// Handled on `RepoView` by `reflog.rs`.
        UndoLastAction,
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
        /// Returns to the parent repository this one was opened from. Handled
        /// on `RepoView` when a parent exists on the navigation stack.
        NavigateBack,
        /// Closes the compare view. Handled on `RepoView` by
        /// `repo_view::compare`; a no-op while no comparison is open.
        CloseCompare,
    ]
);

// `KeyBinding::new` parses without an OS, so the `secondary-` modifier is
// rejected there; pick the platform's modifier at compile time instead.
#[cfg(target_os = "macos")]
const PLATFORM_QUIT: &str = "cmd-q";
#[cfg(not(target_os = "macos"))]
const PLATFORM_QUIT: &str = "ctrl-q";
#[cfg(target_os = "macos")]
const PLATFORM_SETTINGS: &str = "cmd-,";
#[cfg(not(target_os = "macos"))]
const PLATFORM_SETTINGS: &str = "ctrl-,";
#[cfg(target_os = "macos")]
const PLATFORM_OPEN: &str = "cmd-o";
#[cfg(not(target_os = "macos"))]
const PLATFORM_OPEN: &str = "ctrl-o";
#[cfg(target_os = "macos")]
const PLATFORM_NEW_TAB: &str = "cmd-t";
#[cfg(not(target_os = "macos"))]
const PLATFORM_NEW_TAB: &str = "ctrl-t";
#[cfg(target_os = "macos")]
const PLATFORM_CLOSE_TAB: &str = "cmd-w";
#[cfg(not(target_os = "macos"))]
const PLATFORM_CLOSE_TAB: &str = "ctrl-w";
#[cfg(target_os = "macos")]
const PLATFORM_TAB_KEYS: [&str; 9] = [
    "cmd-1", "cmd-2", "cmd-3", "cmd-4", "cmd-5", "cmd-6", "cmd-7", "cmd-8", "cmd-9",
];
#[cfg(not(target_os = "macos"))]
const PLATFORM_TAB_KEYS: [&str; 9] = [
    "ctrl-1", "ctrl-2", "ctrl-3", "ctrl-4", "ctrl-5", "ctrl-6", "ctrl-7", "ctrl-8", "ctrl-9",
];
#[cfg(target_os = "macos")]
const PLATFORM_STASH: &str = "cmd-shift-s";
#[cfg(not(target_os = "macos"))]
const PLATFORM_STASH: &str = "ctrl-shift-s";
#[cfg(target_os = "macos")]
const PLATFORM_PALETTE: &str = "cmd-shift-p";
#[cfg(not(target_os = "macos"))]
const PLATFORM_PALETTE: &str = "ctrl-shift-p";
#[cfg(target_os = "macos")]
const PLATFORM_SWITCH_BRANCH: &str = "cmd-b";
#[cfg(not(target_os = "macos"))]
const PLATFORM_SWITCH_BRANCH: &str = "ctrl-b";
#[cfg(target_os = "macos")]
const PLATFORM_UNDO_LAST: &str = "cmd-z";
#[cfg(not(target_os = "macos"))]
const PLATFORM_UNDO_LAST: &str = "ctrl-z";

pub fn bind(cx: &mut App) {
    let scope = Some(REPO_VIEW);
    // Unmodified letters are text while a field has focus, so the list
    // navigation must not see them there.
    let navigation = Some(REPO_VIEW_LIST);
    let workspace = Some(WORKSPACE);
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevious, scope),
        KeyBinding::new("k", SelectPrevious, navigation),
        KeyBinding::new("down", SelectNext, scope),
        KeyBinding::new("j", SelectNext, navigation),
        // `z` is a letter, so it is text while a field has focus: the details
        // toggle must not swallow it there.
        KeyBinding::new("z", ToggleDetailsMaximized, navigation),
        // Escape leaves the compare view; every dialog and popover sets its
        // own deeper context, so it keeps its own binding.
        KeyBinding::new("escape", CloseCompare, scope),
        KeyBinding::new("shift-up", ExtendSelectionPrevious, scope),
        KeyBinding::new("shift-down", ExtendSelectionNext, scope),
        KeyBinding::new("pageup", SelectPageUp, scope),
        KeyBinding::new("pagedown", SelectPageDown, scope),
        KeyBinding::new("home", SelectFirst, scope),
        KeyBinding::new("end", SelectLast, scope),
        KeyBinding::new("f12", ToggleFrameStats, scope),
        KeyBinding::new(PLATFORM_STASH, StashChanges, scope),
        KeyBinding::new(PLATFORM_SWITCH_BRANCH, SwitchBranch, scope),
        // `z` is a letter, so a focused text field keeps `Cmd/Ctrl+Z` for its
        // own undo; the repository action only runs outside one.
        KeyBinding::new(PLATFORM_UNDO_LAST, UndoLastAction, navigation),
        KeyBinding::new("alt-left", NavigateBack, scope),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-[", NavigateBack, scope),
        // Workspace tabs: scoped to the main window so `SettingsView` keeps
        // its own `CloseSettings` binding on `Cmd/Ctrl+W`.
        KeyBinding::new(PLATFORM_NEW_TAB, NewTab, workspace),
        KeyBinding::new(PLATFORM_CLOSE_TAB, CloseTab, workspace),
        KeyBinding::new("ctrl-tab", NextTab, workspace),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, workspace),
        KeyBinding::new(PLATFORM_TAB_KEYS[0], SelectTab1, workspace),
        KeyBinding::new(PLATFORM_TAB_KEYS[1], SelectTab2, workspace),
        KeyBinding::new(PLATFORM_TAB_KEYS[2], SelectTab3, workspace),
        KeyBinding::new(PLATFORM_TAB_KEYS[3], SelectTab4, workspace),
        KeyBinding::new(PLATFORM_TAB_KEYS[4], SelectTab5, workspace),
        KeyBinding::new(PLATFORM_TAB_KEYS[5], SelectTab6, workspace),
        KeyBinding::new(PLATFORM_TAB_KEYS[6], SelectTab7, workspace),
        KeyBinding::new(PLATFORM_TAB_KEYS[7], SelectTab8, workspace),
        KeyBinding::new(PLATFORM_TAB_KEYS[8], SelectTab9, workspace),
        // Global: these work whichever window or panel has focus.
        KeyBinding::new(PLATFORM_SETTINGS, OpenSettings, None),
        KeyBinding::new(PLATFORM_OPEN, OpenRepository, None),
        KeyBinding::new(PLATFORM_PALETTE, TogglePalette, None),
        // `F1` is the command palette on platforms that do not spell shortcuts
        // with the primary modifier, and a second way in everywhere else.
        KeyBinding::new("f1", TogglePalette, None),
        // Global: quitting must work whichever panel has focus.
        KeyBinding::new(PLATFORM_QUIT, Quit, None),
    ]);
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
