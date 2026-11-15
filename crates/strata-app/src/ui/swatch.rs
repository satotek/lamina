//! Compact colour swatch strips for theme pickers.
//!
//! A small preview beside a theme name shows its palette at a glance without
//! opening or switching to it first.

use gpui::{
    AnyElement, Hsla, IntoElement as _, ParentElement as _, SharedString, Styled as _, div, px,
};

use crate::style::Design;

const SWATCH_SIZE: f32 = 10.0;
const SWATCH_RADIUS: f32 = 2.5;
const SWATCH_GAP: f32 = 2.0;

/// A tight row of rounded colour chips, outlined so light tints stay visible
/// against a light surface.
pub fn swatch_strip(colors: impl IntoIterator<Item = Hsla>, design: &Design) -> AnyElement {
    let border = design.border;
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(SWATCH_GAP))
        .children(colors.into_iter().map(|color| {
            div()
                .w(px(SWATCH_SIZE))
                .h(px(SWATCH_SIZE))
                .rounded(px(SWATCH_RADIUS))
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
    div()
        .flex()
        .items_center()
        .gap_2()
        .min_w_0()
        .child(swatch_strip(colors, design))
        .child(div().min_w_0().truncate().child(label.into()))
        .into_any_element()
}
