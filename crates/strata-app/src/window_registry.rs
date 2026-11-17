//! The app's own list of open windows, and the decision to quit from it.
//!
//! GPUI's `QuitMode::LastWindowClosed` asks whether its window list is empty,
//! but `App::update_window` takes the window being updated out of that list for
//! the duration of the update. A window closed from inside another window's
//! update therefore leaves the list looking empty while other windows are still
//! open, and the app quits with all of them. This list only changes when a
//! window opens or a close is reported, so a nested update cannot make it lie.

use std::collections::HashSet;

use gpui::{App, Global, QuitMode, WindowId};

/// The windows Strata opened and has not seen closed yet.
#[derive(Default)]
pub(crate) struct OpenWindows {
    ids: HashSet<WindowId>,
}

impl Global for OpenWindows {}

/// Starts tracking windows. Call once at startup, before the first window.
pub(crate) fn init(cx: &mut App) {
    cx.set_global(OpenWindows::default());
    // Every platform leaves the decision to `on_window_closed` below. macOS
    // keeps running after its last window so the dock can reopen one.
    cx.set_quit_mode(QuitMode::Explicit);
    cx.on_window_closed(|cx, closed| {
        let Some(remaining) = forget(closed, cx) else {
            return;
        };
        log::debug!("window {closed:?} closed; {remaining} still open");
        if should_quit(remaining) {
            cx.quit();
        }
    })
    .detach();
}

/// Adds a window the app opened to the list. A window whose `open_window` call
/// failed never reaches this and so never counts.
pub(crate) fn track(window: WindowId, cx: &mut App) {
    with_windows(cx, |windows| windows.ids.insert(window));
}

/// Drops one closed window from the list and returns how many remain, or
/// `None` when the window was never tracked.
fn forget(closed: WindowId, cx: &mut App) -> Option<usize> {
    with_windows(cx, |windows| {
        if !windows.ids.remove(&closed) {
            return None;
        }
        Some(windows.ids.len())
    })
    .flatten()
}

/// Whether the app should end now that `remaining` of its windows are open.
///
/// macOS stays alive without windows: the dock reopens one through the reopen
/// handler in `main`. Elsewhere the last window ends the session, as it did
/// when GPUI made the decision itself.
fn should_quit(remaining: usize) -> bool {
    cfg!(not(target_os = "macos")) && remaining == 0
}

/// Runs `f` against the list, or does nothing before [`init`] created it. A
/// window opened in that state (a test that builds one view directly) simply
/// goes untracked instead of panicking.
fn with_windows<R>(cx: &mut App, f: impl FnOnce(&mut OpenWindows) -> R) -> Option<R> {
    if !cx.has_global::<OpenWindows>() {
        return None;
    }
    Some(f(cx.global_mut::<OpenWindows>()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AnyWindowHandle, AppContext as _, Empty, TestAppContext};

    fn open(cx: &mut TestAppContext) -> AnyWindowHandle {
        cx.update(|cx| cx.open_window(Default::default(), |_, cx| cx.new(|_| Empty)))
            .expect("open a test window")
            .into()
    }

    fn count(cx: &App) -> usize {
        cx.global::<OpenWindows>().ids.len()
    }

    /// Reproduces the report: a secondary window closed while another window
    /// was being updated ended the whole process on X11. GPUI's own window
    /// list is empty at that point, because it holds the updated window aside.
    #[gpui::test]
    fn closing_a_window_inside_another_windows_update_does_not_quit(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            init(cx);
        });
        let kept = open(cx);
        let closing = open(cx);
        cx.update(|cx| {
            track(kept.window_id(), cx);
            track(closing.window_id(), cx);
        });

        cx.update(|cx| {
            kept.update(cx, |_, _, cx| {
                assert_eq!(count(cx), 2);
                closing
                    .update(cx, |_, window, _| window.remove_window())
                    .expect("close the second window");
                // The window the close was nested in is still open, so the
                // remaining count has to say so rather than "nothing left".
                assert_eq!(count(cx), 1);
                assert!(!should_quit(count(cx)));
            })
            .expect("update the still-open window");
        });

        assert_eq!(cx.windows().len(), 1, "the other window has to survive");
        cx.update(|cx| assert_eq!(count(cx), 1));
    }

    #[gpui::test]
    fn closing_the_last_window_quits_only_off_macos(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            init(cx);
        });
        let only = open(cx);
        cx.update(|cx| track(only.window_id(), cx));
        cx.update(|cx| {
            only.update(cx, |_, window, _| window.remove_window())
                .expect("close the window");
            assert_eq!(count(cx), 0);
        });
        assert_eq!(should_quit(0), !cfg!(target_os = "macos"));
        assert!(!should_quit(1));
    }

    /// A window whose `open_window` failed never reaches `track`, and neither
    /// does anything else the app did not open: closing it changes nothing.
    #[gpui::test]
    fn an_untracked_window_does_not_change_the_count(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            init(cx);
        });
        let tracked = open(cx);
        let untracked = open(cx);
        cx.update(|cx| track(tracked.window_id(), cx));
        cx.update(|cx| {
            untracked
                .update(cx, |_, window, _| window.remove_window())
                .expect("close the untracked window");
            assert_eq!(count(cx), 1);
        });
    }
}
