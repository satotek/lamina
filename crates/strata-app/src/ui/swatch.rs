//! Compact colour swatch strips for theme pickers.
//!
//! A small preview beside a theme name shows its palette at a glance without
//! opening or switching to it first.

use gpui::{
    AnyElement, Hsla, IntoElement as _, ParentElement as _, SharedString, Styled as _, div,
};

use crate::style::Design;

const SWATCH_SIZE: f32 = 10.0;
const SWATCH_RADIUS: f32 = 2.5;
const SWATCH_GAP: f32 = 2.0;

/// A tight row of rounded colour chips, outlined so light tints stay visible
/// against a light surface.
pub fn swatch_strip(colors: impl IntoIterator<Item = Hsla>, design: &Design) -> AnyElement {
    let border = design.border;
    let size = design.px(SWATCH_SIZE);
    let radius = design.px(SWATCH_RADIUS);
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(design.px(SWATCH_GAP))
        .children(colors.into_iter().map(move |color| {
            div()
                .w(size)
                .h(size)
                .rounded(radius)
                .border_1()
                .border_color(border)
                .bg(color)
        }))
        .into_any_element()
}

/// A [`swatch_strip`] paired with a single-line label for buttons and menus.
pub fn swatch_label(
    label: impl Into<SharedString>,
    colors: impl IntoIterator<Item = Hsla>,
    design: &Design,
) -> AnyElement {
    swatch_label_with_tag(label, None::<SharedString>, colors, design)
}

/// A [`swatch_strip`] paired with a single-line label and an optional trailing
/// tag badge (for distinguishing user-defined themes in pickers).
pub fn swatch_label_with_tag(
    label: impl Into<SharedString>,
    tag: Option<impl Into<SharedString>>,
    colors: impl IntoIterator<Item = Hsla>,
    design: &Design,
) -> AnyElement {
    let mut row = div()
        .flex()
        .items_center()
        .gap_2()
        .min_w_0()
        .child(swatch_strip(colors, design))
        .child(div().min_w_0().truncate().child(label.into()));
    if let Some(tag) = tag {
        row = row.child(super::status_badge(tag, super::BadgeTone::Neutral, design));
    }
    row.into_any_element()
}
