//! Scrollbars for the virtualised lists.
//!
//! `uniform_list` draws no scrollbar of its own. Every list in the app goes
//! through [`WithScrollbar::with_scrollbar`], which wraps it in a positioned
//! container with gpui-component's overlay scrollbar. The wrapper is returned,
//! so the sizing a call site chains afterwards (`flex_1`, a fixed width or
//! height, borders) applies to the wrapper and the list fills it.

use gpui::{Div, ParentElement, Styled, UniformList, UniformListScrollHandle, div};
use gpui_component::scroll::ScrollableElement as _;

pub trait WithScrollbar {
    fn with_scrollbar(self, handle: &UniformListScrollHandle) -> Div;
}

impl WithScrollbar for UniformList {
    fn with_scrollbar(self, handle: &UniformListScrollHandle) -> Div {
        // The thumb's position and size come from the handle's item count and
        // row height, so even a 200k-row list costs nothing extra here.
        div()
            .relative()
            .min_h_0()
            .min_w_0()
            .child(self.size_full())
            .vertical_scrollbar(handle)
    }
}
