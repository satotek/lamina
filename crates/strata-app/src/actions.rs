//! Window-level commands and their default key bindings.
//!
//! Every user command is a GPUI action rather than a raw key match, so the
//! same command can later be reached from a menu, a command palette or a
//! user keymap without touching the view that handles it. Bindings are scoped
//! to the `RepoView` key context; panels added later (text inputs, dialogs)
//! push their own contexts and take precedence while focused.

use gpui::{App, KeyBinding, actions};

/// Key context set on the repository view's root element.
pub const REPO_VIEW: &str = "RepoView";

actions!(
    strata,
    [
        SelectPrevious,
        SelectNext,
        SelectPageUp,
        SelectPageDown,
        SelectFirst,
        SelectLast,
        ToggleFrameStats,
        Quit,
        /// Opens the settings window. Handled globally by `settings_view`.
        OpenSettings,
        /// Opens another repository (picker, recent list). Handled globally
        /// by the repository-picker module.
        OpenRepository,
        /// Stashes the working tree's changes. Handled on `RepoView` by
        /// `stash.rs`.
        StashChanges,
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
const PLATFORM_STASH: &str = "cmd-shift-s";
#[cfg(not(target_os = "macos"))]
const PLATFORM_STASH: &str = "ctrl-shift-s";

pub fn bind(cx: &mut App) {
    let scope = Some(REPO_VIEW);
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevious, scope),
        KeyBinding::new("k", SelectPrevious, scope),
        KeyBinding::new("down", SelectNext, scope),
        KeyBinding::new("j", SelectNext, scope),
        KeyBinding::new("pageup", SelectPageUp, scope),
        KeyBinding::new("pagedown", SelectPageDown, scope),
        KeyBinding::new("home", SelectFirst, scope),
        KeyBinding::new("end", SelectLast, scope),
        KeyBinding::new("f12", ToggleFrameStats, scope),
        KeyBinding::new(PLATFORM_STASH, StashChanges, scope),
        // Global: these work whichever window or panel has focus.
        KeyBinding::new(PLATFORM_SETTINGS, OpenSettings, None),
        KeyBinding::new(PLATFORM_OPEN, OpenRepository, None),
        // Global: quitting must work whichever panel has focus.
        KeyBinding::new(PLATFORM_QUIT, Quit, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
}
