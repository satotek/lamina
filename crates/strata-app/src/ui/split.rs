//! Draggable dividers between two panes.
//!
//! A handle is a thin band along the edge of the pane before the boundary: the
//! band, its resize cursor, its hover and drag states, and the drag itself come
//! from gpui-component, whose divider colours are the [`Design`] tokens
//! (component_theme points them at the app's palette). This module adds the one
//! thing the app needs on top: a payload the owning view follows to resize its
//! panes, and a double click that puts the boundary back where it started.
//!
//! The split itself is not a layout this module owns. Panes are sized by a
//! fraction of the area they share, so the owner only has to know how many
//! pixels the area had when the pointer moved; [`SplitExtent`] carries that
//! measurement.
//!
//! [`Design`]: crate::style::Design

use gpui::{
    App, AppContext as _, Axis, Bounds, ClickEvent, DragMoveEvent, ElementId,
    InteractiveElement as _, IntoElement, Pixels, Render, Window,
};
use gpui_component::InteractiveElementExt as _;
use gpui_component::resizable::{HandleEdge, resize_handle};

/// The payload a split handle carries while it is dragged.
///
/// GPUI routes a drag to every listener for its payload type, not only to the
/// element the drag started on, so the payload names the axis it belongs to and
/// each handle ignores drags of the other axis. The two History boundaries are
/// one of each, which is what keeps dragging the list from also moving the
/// file column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SplitDrag {
    axis: Axis,
}

impl Render for SplitDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// A measured span along one axis, in the coordinate space of the window.
///
/// The owning view records it after its panes are laid out and reads it while a
/// handle is dragged, so a drag can turn the pointer's position into a fraction
/// of the area rather than counting deltas that drift.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SplitExtent {
    start: f32,
    size: f32,
}

impl SplitExtent {
    /// The span covered by `leading` and `trailing`, measured along `axis`.
    pub fn between(leading: Bounds<Pixels>, trailing: Bounds<Pixels>, axis: Axis) -> Self {
        let (start, end) = match axis {
            Axis::Horizontal => (
                f32::from(leading.origin.x),
                f32::from(trailing.origin.x + trailing.size.width),
            ),
            Axis::Vertical => (
                f32::from(leading.origin.y),
                f32::from(trailing.origin.y + trailing.size.height),
            ),
        };
        Self {
            start,
            size: (end - start).max(0.0),
        }
    }

    pub fn size(&self) -> f32 {
        self.size
    }

    /// How far into the area a pointer at `position` sits, as a fraction.
    pub fn fraction_at(&self, position: Pixels) -> f32 {
        let position = f32::from(position);
        if self.size <= 0.0 {
            return 0.0;
        }
        ((position - self.start) / self.size).clamp(0.0, 1.0)
    }
}

/// The fraction of `total` the leading pane may take, given both minimums.
///
/// A window too small for both minimums cannot honour either, so the space is
/// split in the minimums' own proportion instead of collapsing one side to
/// zero.
pub fn clamp_fraction(fraction: f32, total: f32, min_leading: f32, min_trailing: f32) -> f32 {
    if !(total.is_finite() && total > 0.0) {
        return fraction.clamp(0.0, 1.0);
    }
    let lower = (min_leading / total).clamp(0.0, 1.0);
    let upper = (1.0 - min_trailing / total).clamp(0.0, 1.0);
    if lower > upper {
        let minimums = min_leading + min_trailing;
        if minimums <= 0.0 {
            return 0.5;
        }
        return (min_leading / minimums).clamp(0.0, 1.0);
    }
    fraction.clamp(lower, upper)
}

/// The leading pane's size in pixels, clamped the way a drag would clamp it.
pub fn split_pixels(fraction: f32, total: f32, min_leading: f32, min_trailing: f32) -> f32 {
    if !(total.is_finite() && total > 0.0) {
        return 0.0;
    }
    clamp_fraction(fraction, total, min_leading, min_trailing) * total
}

/// A divider the user can drag, and double-click to restore its default.
///
/// `axis` is the direction the panes move along: [`Axis::Vertical`] for a
/// horizontal boundary between stacked panes, [`Axis::Horizontal`] for a
/// vertical one between side-by-side panes. The handle hugs the trailing edge of
/// the pane it is added to, so that pane has to be positioned (`relative`).
pub fn split_handle(
    id: impl Into<ElementId>,
    axis: Axis,
    on_drag_move: impl Fn(&DragMoveEvent<SplitDrag>, &mut Window, &mut App) + 'static,
    on_reset: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    resize_handle::<SplitDrag, SplitDrag>(id, axis)
        .inside(HandleEdge::Trailing)
        .on_drag(SplitDrag { axis }, |drag, _, _, cx| cx.new(|_| *drag))
        .on_drag_move(move |event: &DragMoveEvent<SplitDrag>, window, cx| {
            if event.drag(cx).axis == axis {
                on_drag_move(event, window, cx);
            }
        })
        .on_double_click(move |event, window, cx| on_reset(event, window, cx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, point, px, size};

    const MIN_LIST: f32 = 112.0;
    const MIN_DETAILS: f32 = 160.0;

    #[test]
    fn a_drag_never_takes_either_pane_below_its_minimum() {
        let total = 1000.0;
        assert_eq!(clamp_fraction(0.55, total, MIN_LIST, MIN_DETAILS), 0.55);
        assert!(
            (clamp_fraction(0.0, total, MIN_LIST, MIN_DETAILS) - 0.112).abs() < 1e-6,
            "dragging to the top leaves the list its minimum"
        );
        assert!(
            (clamp_fraction(1.0, total, MIN_LIST, MIN_DETAILS) - 0.84).abs() < 1e-6,
            "dragging to the bottom leaves the details their minimum"
        );
        assert!((clamp_fraction(-1.0, total, MIN_LIST, MIN_DETAILS) - 0.112).abs() < 1e-6);
        assert!((clamp_fraction(2.0, total, MIN_LIST, MIN_DETAILS) - 0.84).abs() < 1e-6);
    }

    #[test]
    fn a_window_too_small_for_both_minimums_still_shows_both() {
        // 200 px cannot hold 112 + 160; the space divides in the minimums'
        // proportion rather than collapsing one pane to nothing.
        let fraction = clamp_fraction(0.9, 200.0, MIN_LIST, MIN_DETAILS);
        assert!((fraction - MIN_LIST / (MIN_LIST + MIN_DETAILS)).abs() < 1e-6);
        assert!(fraction > 0.0 && fraction < 1.0);
    }

    #[test]
    fn a_measured_area_keeps_the_same_proportion_at_any_size() {
        let total = 1000.0;
        assert!((split_pixels(0.4, total, MIN_LIST, MIN_DETAILS) - 400.0).abs() < 1e-4);
        assert!(
            (split_pixels(0.4, total, MIN_LIST, MIN_DETAILS) / total
                - split_pixels(0.4, total / 2.0, MIN_LIST, MIN_DETAILS) / (total / 2.0))
                .abs()
                < 1e-4,
            "a smaller window keeps the proportion the user chose"
        );
    }

    #[test]
    fn a_pointer_maps_back_to_the_fraction_it_sits_on() {
        let extent = SplitExtent::between(
            Bounds::new(point(px(0.0), px(100.0)), size(px(10.0), px(800.0))),
            Bounds::new(point(px(0.0), px(900.0)), size(px(10.0), px(0.0))),
            Axis::Vertical,
        );
        assert_eq!(extent.size(), 800.0);
        assert!((extent.fraction_at(px(400.0)) - 0.375).abs() < 1e-6);
        assert_eq!(extent.fraction_at(px(0.0)), 0.0);
        assert_eq!(extent.fraction_at(px(5000.0)), 1.0);
    }

    #[test]
    fn a_horizontal_area_measures_from_its_first_pane_to_its_last() {
        let extent = SplitExtent::between(
            Bounds::new(point(px(250.0), px(0.0)), size(px(320.0), px(10.0))),
            Bounds::new(point(px(570.0), px(0.0)), size(px(680.0), px(10.0))),
            Axis::Horizontal,
        );
        assert_eq!(extent.size(), 1000.0);
        assert!((extent.fraction_at(px(250.0)) - 0.0).abs() < 1e-6);
        assert!((extent.fraction_at(px(570.0)) - 0.32).abs() < 1e-6);
    }

    #[test]
    fn an_unmeasured_or_empty_area_is_not_a_division_by_zero() {
        assert_eq!(clamp_fraction(0.3, 0.0, MIN_LIST, MIN_DETAILS), 0.3);
        assert_eq!(split_pixels(0.3, 0.0, MIN_LIST, MIN_DETAILS), 0.0);
        let empty = SplitExtent::default();
        assert_eq!(empty.fraction_at(px(10.0)), 0.0);
    }
}
