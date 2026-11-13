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
    ]
);

// `KeyBinding::new` parses without an OS, so the `secondary-` modifier is
// rejected there; pick the platform's modifier at compile time instead.
#[cfg(target_os = "macos")]
const PLATFORM_QUIT: &str = "cmd-q";
#[cfg(not(target_os = "macos"))]
const PLATFORM_QUIT: &str = "ctrl-q";

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
        // Global: quitting must work whichever panel has focus.
        KeyBinding::new(PLATFORM_QUIT, Quit, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
}
