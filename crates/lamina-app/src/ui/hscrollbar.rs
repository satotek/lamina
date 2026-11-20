//! A thin horizontal scrollbar shared by several panes.
//!
//! The Compare view's three panes each pan their code with one
//! [`ScrollHandle`], so none of them can own a scrollbar that also shows the
//! other panes' overflow. This part draws one bar under the row of panes whose
//! thumb is sized from a single pane's viewport and whose track spans the whole
//! row, and drives the shared handle directly. The built-in scrollbar derives
//! its thumb from the element it overlays, which would make the overflow look
//! three times smaller than it is.

use gpui::{
    App, Bounds, CursorStyle, DispatchPhase, ElementId, Entity, Hsla, InteractiveElement as _,
    IntoElement, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels,
    RenderOnce, ScrollHandle, Styled as _, Window, canvas, div, fill, point, px,
};

use crate::style::Design;

/// The row the bar occupies under the panes.
const SHARED_SCROLLBAR_HEIGHT: f32 = 10.0;

/// A thumb shorter than this is too hard to grab even when the content is
/// enormous.
const MIN_THUMB_LENGTH: f32 = 24.0;

/// Ink the bar's thumb never expands past.
const THUMB_INSET: f32 = 2.0;

/// One shared bar under a row of panes.
///
/// `viewport` is the width of the code area in one pane and `content` the
/// width of the widest line in that pane; the caller opens the bar only when
/// `content` is wider than `viewport`.
pub fn h_scrollbar(
    id: impl Into<ElementId>,
    handle: &ScrollHandle,
    viewport: Pixels,
    content: Pixels,
    design: &Design,
) -> UiHScrollbar {
    UiHScrollbar {
        id: id.into(),
        handle: handle.clone(),
        viewport: f32::from(viewport),
        content: f32::from(content),
        track: design.background,
        thumb: design.border,
        thumb_hover: design.muted,
    }
}

#[derive(Default)]
struct Drag {
    active: bool,
    /// Pointer distance from the thumb's leading edge when the drag started.
    grab: f32,
}

#[derive(IntoElement)]
pub struct UiHScrollbar {
    id: ElementId,
    handle: ScrollHandle,
    viewport: f32,
    content: f32,
    track: Hsla,
    thumb: Hsla,
    thumb_hover: Hsla,
}

impl RenderOnce for UiHScrollbar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let id = self.id;
        let drag = window.use_keyed_state(id.clone(), cx, |_, _| Drag::default());
        let handle = self.handle;
        let viewport = self.viewport;
        let content = self.content;
        let track = self.track;
        let thumb = self.thumb;
        let thumb_hover = self.thumb_hover;

        div()
            .id(id)
            .debug_selector(|| "shared-hscrollbar".to_string())
            .flex()
            .flex_none()
            .items_center()
            .w_full()
            .h(px(SHARED_SCROLLBAR_HEIGHT))
            .cursor(CursorStyle::PointingHand)
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, (), window, _cx| {
                        paint_and_watch(
                            bounds,
                            window,
                            &drag,
                            &handle,
                            viewport,
                            content,
                            [track, thumb, thumb_hover],
                        );
                    },
                )
                .size_full(),
            )
    }
}

/// The bar's geometry from the shared handle's current offset.
///
/// Returns `None` when every pane already shows its widest line, which is the
/// same condition the caller uses to omit the row.
fn thumb_bounds(
    track: Bounds<Pixels>,
    viewport: f32,
    content: f32,
    offset: f32,
) -> Option<Bounds<Pixels>> {
    let track_width = f32::from(track.size.width);
    if content <= viewport + 0.5 || track_width <= 0.0 {
        return None;
    }
    let length = (track_width * (viewport / content).clamp(0.0, 1.0))
        .max(MIN_THUMB_LENGTH)
        .min(track_width);
    let travel = track_width - length;
    let max_offset = content - viewport;
    let progress = (offset / max_offset).clamp(0.0, 1.0);
    let x = f32::from(track.origin.x) + progress * travel;
    Some(Bounds::new(
        point(px(x), track.origin.y),
        gpui::size(px(length), track.size.height),
    ))
}

/// The largest offset the shared panes can take, in pixels.
fn max_offset(viewport: f32, content: f32) -> f32 {
    (content - viewport).max(0.0)
}

#[cfg(test)]
fn offset_to_progress(offset: f32, viewport: f32, content: f32) -> f32 {
    let max = max_offset(viewport, content);
    if max <= 0.0 {
        0.0
    } else {
        (offset / max).clamp(0.0, 1.0)
    }
}

fn paint_and_watch(
    bounds: Bounds<Pixels>,
    window: &mut Window,
    drag: &Entity<Drag>,
    handle: &ScrollHandle,
    viewport: f32,
    content: f32,
    colors: [Hsla; 3],
) {
    let [track_color, thumb_color, thumb_hover] = colors;
    let offset = (-f32::from(handle.offset().x)).max(0.0);
    let inset = THUMB_INSET;
    let track = Bounds::new(
        point(bounds.origin.x + px(inset), bounds.origin.y + px(inset)),
        gpui::size(
            (bounds.size.width - px(inset * 2.0)).max(px(0.0)),
            (bounds.size.height - px(inset * 2.0)).max(px(0.0)),
        ),
    );

    window.paint_quad(fill(track, track_color).corner_radii(px(2.0)));

    let Some(thumb) = thumb_bounds(track, viewport, content, offset) else {
        return;
    };
    let hovered = bounds.contains(&window.mouse_position());
    let color = if hovered { thumb_hover } else { thumb_color };
    window.paint_quad(fill(thumb, color).corner_radii(px(2.0)));

    let view_id = window.current_view();
    let max = max_offset(viewport, content);
    let travel = (f32::from(track.size.width) - f32::from(thumb.size.width)).max(0.0);

    // A press either grabs the thumb or centres it on the pointer, then every
    // move maps the pointer back onto the shared offset. The listeners live on
    // the window because the row is thinner than a comfortable drag, and only
    // one of them runs while the button is down.
    let start_drag = {
        let drag = drag.clone();
        let handle = handle.clone();
        move |event: &MouseDownEvent, phase: DispatchPhase, _: &mut Window, cx: &mut App| {
            if phase != DispatchPhase::Bubble || !bounds.contains(&event.position) {
                return;
            }
            cx.stop_propagation();
            let x = f32::from(event.position.x - track.origin.x);
            let grab = if thumb.contains(&event.position) {
                f32::from(event.position.x - thumb.origin.x)
            } else {
                f32::from(thumb.size.width) / 2.0
            };
            let thumb_start = (x - grab).clamp(0.0, travel);
            let progress = if travel <= 0.0 {
                0.0
            } else {
                thumb_start / travel
            };
            handle.set_offset(point(px(-progress * max), handle.offset().y));
            drag.update(cx, |drag, cx| {
                drag.active = true;
                drag.grab = grab;
                cx.notify();
            });
            cx.notify(view_id);
        }
    };

    let move_drag = {
        let drag = drag.clone();
        let handle = handle.clone();
        move |event: &MouseMoveEvent, _: DispatchPhase, _: &mut Window, cx: &mut App| {
            if !drag.read(cx).active || !event.dragging() {
                return;
            }
            let grab = drag.read(cx).grab;
            cx.stop_propagation();
            let x = f32::from(event.position.x - track.origin.x);
            let thumb_start = (x - grab).clamp(0.0, travel);
            let progress = if travel <= 0.0 {
                0.0
            } else {
                thumb_start / travel
            };
            handle.set_offset(point(px(-progress * max), handle.offset().y));
            cx.notify(view_id);
        }
    };

    let end_drag = {
        let drag = drag.clone();
        move |_: &MouseUpEvent, _: DispatchPhase, _: &mut Window, cx: &mut App| {
            if !drag.read(cx).active {
                return;
            }
            drag.update(cx, |drag, cx| {
                drag.active = false;
                cx.notify();
            });
            cx.notify(view_id);
        }
    };

    window.on_mouse_event(start_drag);
    window.on_mouse_event(move_drag);
    window.on_mouse_event(end_drag);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track_start() -> f32 {
        100.0
    }

    #[test]
    fn the_thumb_sits_at_both_ends_and_matches_the_content_ratio() {
        let track = Bounds::new(
            point(px(track_start()), px(0.0)),
            gpui::size(px(600.0), px(6.0)),
        );
        let first = thumb_bounds(track, 300.0, 1200.0, 0.0).expect("the content overflows");
        let last = thumb_bounds(track, 300.0, 1200.0, 900.0).expect("the content overflows");
        assert_eq!(f32::from(first.origin.x), track_start());
        assert!((f32::from(first.size.width) - 150.0).abs() < 0.5);
        assert!((f32::from(last.right()) - 700.0).abs() < 0.5);
        assert_eq!(offset_to_progress(900.0, 300.0, 1200.0), 1.0);
    }

    #[test]
    fn the_thumb_stays_grabbable_and_the_bar_hides_when_nothing_overflows() {
        let track = Bounds::new(point(px(0.0), px(0.0)), gpui::size(px(600.0), px(6.0)));
        let tiny = thumb_bounds(track, 10.0, 100_000.0, 0.0).expect("the content overflows");
        assert_eq!(f32::from(tiny.size.width), MIN_THUMB_LENGTH);
        assert!(thumb_bounds(track, 600.0, 500.0, 0.0).is_none());
    }
}
