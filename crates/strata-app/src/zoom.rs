//! The whole-window UI zoom.
//!
//! GPUI draws its own lengths in logical pixels and has no window scale it lets
//! the app set, but it does expose one global lever: the window's rem size. The
//! component library already treats it as the zoom control — `Root` copies the
//! theme's base font size into `Window::set_rem_size` before every frame, and
//! every rem-based length, every component widget and every text size built
//! from the theme's typography tokens follows it.
//!
//! The app's own panes paint from [`Design`](crate::style::Design) instead of
//! rems, so [`Design`](crate::style::Design) multiplies its two type sizes by
//! the same factor (see `style::Design::from_palette`). Together those are the
//! whole window: list text, diff and terminal fonts, icons drawn at the theme's
//! size, and the component library's padding and control heights.
//!
//! The factor is stored in `appearance.zoom_percent`, so a step taken with the
//! keyboard, the menu or the settings page is the same preference and comes
//! back at the next launch.

use gpui::{App, px};
use gpui_component::notification::Notification;
use gpui_component::{ActiveTheme as _, Theme, WindowExt as _};

use crate::keymap::{self, Group};
use crate::settings::{self, Settings, ZOOM_STEPS};

use gpui::actions;

/// gpui-component's base font size, which is the rem size its rem lengths are
/// written against before the zoom is applied.
const BASE_FONT_SIZE: f32 = 16.0;
/// The matching monospace base.
const BASE_MONO_FONT_SIZE: f32 = 13.0;

actions!(
    strata,
    [
        /// Steps the whole window up one zoom step.
        ZoomIn,
        /// Steps the whole window down one zoom step.
        ZoomOut,
        /// Returns the whole window to 100%.
        ResetZoom,
    ]
);

/// Registers the zoom keys and their handlers. Call once at startup.
pub fn bind(cx: &mut App) {
    keymap::bind_group(cx, Group::Zoom);
    cx.on_action(|_: &ZoomIn, cx: &mut App| step(cx, 1));
    cx.on_action(|_: &ZoomOut, cx: &mut App| step(cx, -1));
    cx.on_action(|_: &ResetZoom, cx: &mut App| {
        set(cx, settings::DEFAULT_ZOOM);
        report(cx);
    });
    // The window that was just opened inherits the saved zoom like every other
    // one that observes the settings, but the theme the window starts from is
    // still the unzoomed one until this runs.
    apply(cx);
}

/// Stores `percent` (snapped to an offered step) and applies it everywhere.
pub fn set(cx: &mut App, percent: u16) {
    let percent = settings::normalize_zoom(percent);
    if Settings::get(cx).appearance.zoom_percent != percent {
        settings::update(cx, |settings| settings.appearance.zoom_percent = percent);
    }
    apply(cx);
    cx.refresh_windows();
}

/// The step above (`direction` 1) or below (-1) the current one.
fn step(cx: &mut App, direction: isize) {
    let current = settings::normalize_zoom(Settings::get(cx).appearance.zoom_percent);
    let index = ZOOM_STEPS
        .iter()
        .position(|step| *step == current)
        .unwrap_or_else(|| {
            ZOOM_STEPS
                .iter()
                .position(|step| *step == settings::DEFAULT_ZOOM)
                .unwrap_or(0)
        });
    let target = (index as isize + direction).clamp(0, ZOOM_STEPS.len() as isize - 1) as usize;
    set(cx, ZOOM_STEPS[target]);
    report(cx);
}

/// Writes the zoom onto the theme the component library draws from.
///
/// The early return keeps a window opening (and the appearance callback) from
/// rebuilding every window's token set for a value that has not changed.
pub fn apply(cx: &mut App) {
    let zoom = Settings::get(cx).appearance.zoom();
    let font = px(BASE_FONT_SIZE * zoom);
    let mono = px(BASE_MONO_FONT_SIZE * zoom);
    if cx.theme().font_size == font && cx.theme().mono_font_size == mono {
        return;
    }
    Theme::update(cx, |theme| {
        theme.font_size = font;
        theme.mono_font_size = mono;
    });
}

/// Tells the user which size is on screen now. A zoom change is otherwise only
/// visible as a redraw, which is easy to miss while a list is scrolled.
fn report(cx: &mut App) {
    let percent = Settings::get(cx).appearance.zoom_percent;
    let Some(handle) = cx.active_window() else {
        return;
    };
    let _ = handle.update(cx, |_, window, cx| {
        window.push_notification(Notification::info(format!("{percent}%")), cx);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_step_is_offered_in_order() {
        let mut sorted = ZOOM_STEPS;
        sorted.sort_unstable();
        assert_eq!(sorted, ZOOM_STEPS, "the steps read small to large");
        assert!(ZOOM_STEPS.contains(&settings::DEFAULT_ZOOM));
    }

    #[test]
    fn a_step_outside_the_table_snaps_to_the_nearest_one() {
        assert_eq!(settings::normalize_zoom(100), 100);
        assert_eq!(settings::normalize_zoom(120), 125);
        assert_eq!(settings::normalize_zoom(0), 80);
        assert_eq!(settings::normalize_zoom(u16::MAX), 200);
    }

    /// The `secondary-` keys the registry names resolve to the state's primary
    /// modifier, so no platform loses its zoom.
    #[test]
    fn the_zoom_keys_are_bound_on_both_platforms() {
        for (key, action) in [
            ("secondary-=", "ZoomIn"),
            ("secondary-+", "ZoomIn"),
            ("secondary--", "ZoomOut"),
            ("secondary-0", "ResetZoom"),
        ] {
            let spec = keymap::resolve(action).expect("the zoom action is in the registry");
            assert!(
                spec.defaults.iter().any(|binding| binding.key == key),
                "{action} has no {key} default"
            );
            assert_eq!(spec.context, None, "{action} works in every window");
        }
    }
}
