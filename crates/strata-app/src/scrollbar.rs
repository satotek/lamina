//! Scrollbars for the virtualised lists.
//!
//! `uniform_list` draws no scrollbar of its own. Every list in the app goes
//! through [`WithScrollbar::with_scrollbar`], which wraps it in a positioned
//! container with gpui-component's overlay scrollbar. The wrapper is returned,
//! so the sizing a call site chains afterwards (`flex_1`, a fixed width or
//! height, borders) applies to the wrapper and the list fills it.

use std::rc::Rc;

use gpui::{Div, ParentElement, Styled, UniformList, UniformListScrollHandle, div};
use gpui_component::scroll::Scrollbar;

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
            .child(
                div().absolute().inset_0().child(
                    Scrollbar::vertical(handle)
                        .id(scrollbar_id(handle))
                        .viewport_from_layout(),
                ),
            )
    }
}

/// An element id that is unique to the list the handle scrolls.
///
/// The scrollbar keeps its drag and hover state under its element id, and by
/// default that id is the source location that built it. Every list is built
/// on the line above, and the lists' ancestors rarely carry ids, so they all
/// shared one state: grabbing one thumb moved, or lit up, another list's.
/// The handle lives as long as its list, so its address tells lists apart.
fn scrollbar_id(handle: &UniformListScrollHandle) -> (&'static str, usize) {
    ("list-scrollbar", Rc::as_ptr(&handle.0) as usize)
}
