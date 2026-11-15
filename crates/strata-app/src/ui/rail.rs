//! The narrow icon rail a pane falls back to when it is collapsed.
//!
//! A pane that is usually a list of named sections can give most of its width
//! to the content instead of closing: the same sections stay reachable as one
//! icon each, with the number of entries underneath. The rail only paints what
//! the caller hands it — an icon, a name, a count and what a click means — so
//! it is not tied to one pane's sections.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, ClickEvent, Div, ElementId, InteractiveElement as _, IntoElement,
    ParentElement as _, RenderOnce, SharedString, Stateful, StatefulInteractiveElement as _,
    Styled, Window, div, px,
};
use gpui_component::tooltip::Tooltip;

use crate::style::Design;

/// The width of a collapsed pane: one 44 px icon, the same on every platform.
pub const RAIL_WIDTH: f32 = 44.0;

/// Height of one rail button, icon and count together.
const RAIL_ITEM_HEIGHT: f32 = 44.0;

/// The rail's column: a narrow strip with the pane's own surface and edge.
pub fn rail(design: &Design) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(2.0))
        .w(px(RAIL_WIDTH))
        .h_full()
        .flex_none()
        .py_3()
        .px(px(4.0))
        .overflow_hidden()
        .bg(design.sidebar)
        .border_r_1()
        .border_color(design.border)
}

/// One entry of a rail: an icon with its count, named by its tooltip.
pub fn rail_item(
    id: impl Into<ElementId>,
    icon: impl IntoElement,
    label: impl Into<SharedString>,
    design: &Design,
) -> RailItem {
    RailItem {
        id: id.into(),
        icon: icon.into_any_element(),
        label: label.into(),
        count: None,
        selected: false,
        on_click: None,
        design: design.clone(),
    }
}

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

#[derive(IntoElement)]
pub struct RailItem {
    id: ElementId,
    icon: AnyElement,
    label: SharedString,
    count: Option<usize>,
    selected: bool,
    on_click: Option<ClickHandler>,
    design: Design,
}

impl RailItem {
    /// The number of entries the icon stands for, drawn under it.
    pub fn count(mut self, count: usize) -> Self {
        self.count = Some(count);
        self
    }

    /// Whether this entry is what the pane is showing right now.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for RailItem {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let design = self.design;
        let tooltip = self.label.clone();
        let mut item: Stateful<Div> = div()
            .id(self.id)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(2.0))
            .w_full()
            .h(px(RAIL_ITEM_HEIGHT))
            .flex_none()
            .rounded(px(8.0))
            .cursor_pointer()
            .text_color(design.text)
            .when(self.selected, |item| item.bg(design.selected))
            .hover(move |style| style.bg(design.selected))
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .child(self.icon);
        if let Some(count) = self.count {
            item = item.child(
                div()
                    .text_size(px(10.0))
                    .text_color(design.muted)
                    .child(count.to_string()),
            );
        }
        if let Some(on_click) = self.on_click {
            item = item.on_click(move |event, window, cx| on_click(event, window, cx));
        }
        item
    }
}
