//! The loading spinner: a small arc that turns while a command runs.
//!
//! It is drawn as a canvas rather than a rotating element so the arc itself
//! turns and only this small element is repainted. [`gpui`]'s animation
//! wrapper respects the system's reduced-motion setting, so the same spinner
//! stays a static, visible arc for a user who asked for no motion.

use std::f32::consts::{FRAC_PI_2, TAU};
use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, App, Bounds, ElementId, Hsla, IntoElement, PathBuilder, Pixels,
    RenderOnce, Styled as _, Window, canvas, linear, point, px,
};

use crate::style::Design;

/// One turn.
const TURN: Duration = Duration::from_millis(900);

/// How much of the ring the arc covers, leaving a gap that shows the turn.
const ARC: f32 = 0.75;

/// A spinner in the app's zoomed dimensions, in the accent colour.
pub fn spinner(id: impl Into<ElementId>, design: &Design, diameter: f32) -> Spinner {
    Spinner::new(id, design, diameter, design.accent)
}

/// A spinner built at an explicit size and colour, for a surface that follows
/// the component library's fixed sizes rather than the app's zoom.
#[derive(IntoElement)]
pub struct Spinner {
    id: ElementId,
    diameter: Pixels,
    color: Hsla,
}

impl Spinner {
    /// The app's zoomed size and an explicit colour.
    pub fn new(id: impl Into<ElementId>, design: &Design, diameter: f32, color: Hsla) -> Self {
        Self::fixed(id, design.px(diameter), color)
    }

    /// An explicit pixel size and colour.
    pub fn fixed(id: impl Into<ElementId>, diameter: Pixels, color: Hsla) -> Self {
        Self {
            id: id.into(),
            diameter,
            color,
        }
    }
}

impl RenderOnce for Spinner {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            diameter,
            color,
        } = self;
        Arc {
            diameter,
            color,
            turn: 0.0,
        }
        .with_animation(
            id,
            Animation::new(TURN).repeat().with_easing(linear),
            |arc, delta| Arc { turn: delta, ..arc },
        )
    }
}

#[derive(IntoElement)]
struct Arc {
    diameter: Pixels,
    color: Hsla,
    turn: f32,
}

impl RenderOnce for Arc {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let color = self.color;
        let turn = self.turn;
        canvas(
            |_bounds, _, _| (),
            move |bounds, (), window, _| paint(bounds, color, turn, window),
        )
        .w(self.diameter)
        .h(self.diameter)
    }
}

/// Paints the arc and the faint ring it travels around.
fn paint(bounds: Bounds<Pixels>, color: Hsla, turn: f32, window: &mut Window) {
    let stroke = (bounds.size.width.as_f32() * 0.16).clamp(1.5, 3.0);
    let radius = (bounds.size.width.min(bounds.size.height).as_f32() - stroke) / 2.0;
    if radius <= 0.0 {
        return;
    }
    let center = bounds.center();
    let ring = |from: f32, to: f32| {
        let mut path = PathBuilder::stroke(px(stroke));
        let steps = 32;
        for step in 0..=steps {
            let angle = from + (to - from) * step as f32 / steps as f32;
            let at = point(
                center.x + px(radius * angle.cos()),
                center.y + px(radius * angle.sin()),
            );
            if step == 0 {
                path.move_to(at);
            } else {
                path.line_to(at);
            }
        }
        path.build()
    };
    // The track makes the moving arc read as one ring rather than a partial
    // disc.
    if let Ok(track) = ring(0.0, TAU) {
        window.paint_path(track, color.opacity(0.18));
    }
    let start = -FRAC_PI_2 + turn * TAU;
    if let Ok(arc) = ring(start, start + ARC * TAU) {
        window.paint_path(arc, color);
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Render, TestAppContext, Window, px, size};

    use super::*;

    struct Host;

    impl Render for Host {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            spinner("test-spinner", &Design::default(), 16.0)
        }
    }

    #[gpui::test]
    fn the_spinner_animates_only_when_motion_is_allowed(cx: &mut TestAppContext) {
        let window = cx.open_window(size(px(60.0), px(60.0)), |_, _| Host);
        cx.run_until_parked();
        let frames = window
            .update(cx, |_, window, cx| window.simulate_next_frame(cx))
            .expect("a window");
        assert!(frames > 0, "a visible spinner must drive its own frames");

        cx.update(|cx| cx.set_reduce_motion(true));
        let window = cx.open_window(size(px(60.0), px(60.0)), |_, _| Host);
        cx.run_until_parked();
        let frames = window
            .update(cx, |_, window, cx| window.simulate_next_frame(cx))
            .expect("a window");
        assert_eq!(
            frames, 0,
            "reduced motion keeps the arc still and schedules no frame"
        );
    }
}
