//! Framed panels: a bordered box with an optional header bar, a body and an
//! optional footer bar.
//!
//! The header and footer share one bar style, so a boxed control reads the same
//! wherever it sits. The body is whatever the caller places between the two
//! bars, and it is clipped to the frame's rounded corners.

use gpui::{
    AnyElement, App, Div, ElementId, InteractiveElement as _, IntoElement, ParentElement,
    RenderOnce, SharedString, Stateful, StatefulInteractiveElement as _, StyleRefinement, Styled,
    Window, div, px,
};
use gpui_component::StyledExt as _;

use crate::style::Design;

/// A framed box. Each [`Panel::header`] and [`Panel::footer`] call adds one item
/// to its bar; children added with `.child` fill the body in order.
pub fn panel(design: &Design) -> Panel {
    Panel {
        header: Vec::new(),
        body: Vec::new(),
        footer: Vec::new(),
        design: design.clone(),
        style: StyleRefinement::default(),
    }
}

#[derive(IntoElement)]
pub struct Panel {
    header: Vec<AnyElement>,
    body: Vec<AnyElement>,
    footer: Vec<AnyElement>,
    design: Design,
    style: StyleRefinement,
}

impl Panel {
    /// Adds an item to the header bar, left to right.
    pub fn header(mut self, item: impl IntoElement) -> Self {
        self.header.push(item.into_any_element());
        self
    }

    /// Adds an item to the footer bar. The bar lays its items out from the
    /// trailing edge, which is where a dialog's actions belong.
    pub fn footer(mut self, item: impl IntoElement) -> Self {
        self.footer.push(item.into_any_element());
        self
    }
}

impl ParentElement for Panel {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.body.extend(elements);
    }
}

impl Styled for Panel {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Panel {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let design = &self.design;
        let mut frame = div()
            .flex()
            .flex_col()
            .w_full()
            .rounded(design.px(6.0))
            .border_1()
            .border_color(design.border)
            .bg(design.background)
            .overflow_hidden();
        if !self.header.is_empty() {
            frame = frame.child(bar(design).border_b_1().children(self.header));
        }
        frame = frame.children(self.body);
        if !self.footer.is_empty() {
            frame = frame.child(bar(design).justify_end().border_t_1().children(self.footer));
        }
        frame.refine_style(&self.style)
    }
}

/// The tinted strip a header or footer sits in.
fn bar(design: &Design) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .bg(design.panel)
        .border_color(design.border)
}

/// A folder path for a header bar: monospace, one line, cut at the end when it
/// does not fit. It takes the spare width, so badges and buttons placed after it
/// stay at the bar's edge.
pub fn path_label(path: impl Into<SharedString>, design: &Design) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .truncate()
        .font_family(design.mono.clone())
        .text_size(px(design.tiny()))
        .text_color(design.text)
        .child(path.into())
}

/// A list of rows that scrolls once it grows past `max_height`. The rows are
/// spaced apart and the list keeps a little padding above and below, so a row
/// at either end does not touch the frame.
pub fn scrolling_list(id: impl Into<ElementId>, max_height: f32, design: &Design) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap(design.px(2.0))
        .py_1()
        .max_h(design.px(max_height))
        .overflow_y_scroll()
}
