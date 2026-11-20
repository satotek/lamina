//! Element helpers the chrome modules share: an icon by name and an icon-font
//! glyph by its asset bytes.

use gpui::{Hsla, Styled, px};
use gpui_component::{Icon, Sizable as _};
use gpui_kit_assets::IconName;

use crate::style;

pub(super) fn icon(name: IconName, size: f32, color: Hsla) -> Icon {
    Icon::new(name).with_size(px(size)).text_color(color)
}

pub(super) fn glyph(data: &'static [u8], size: f32, color: Hsla) -> Icon {
    style::glyph_icon(data, size, color)
}
