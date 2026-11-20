//! Segmented controls: one choice among a few, drawn as joined buttons.
//!
//! The compare header offers "No base" against "Middle as base"; a pair of
//! unrelated toggles would let both look off. This part ties one selection to
//! one group, so the current choice always reads as the active one.

use std::rc::Rc;

use gpui::{
    App, ClickEvent, ElementId, InteractiveElement as _, IntoElement, ParentElement as _,
    RenderOnce, SharedString, Styled as _, Window, div, px,
};
use gpui_component::Selectable as _;

use super::{ButtonKind, ButtonSize, button};
use crate::style::Design;

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

struct Segment {
    id: ElementId,
    label: SharedString,
    selected: bool,
    tooltip: Option<SharedString>,
    on_click: Option<ClickHandler>,
}

/// A joined row of buttons where exactly one is selected.
#[derive(IntoElement)]
pub struct SegmentedControl {
    id: ElementId,
    segments: Vec<Segment>,
    design: Design,
}

/// A segmented control named by `id`; segments are added in reading order.
pub fn segmented_control(id: impl Into<ElementId>, design: &Design) -> SegmentedControl {
    SegmentedControl {
        id: id.into(),
        segments: Vec::new(),
        design: design.clone(),
    }
}

impl SegmentedControl {
    pub fn segment(
        mut self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        selected: bool,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.segments.push(Segment {
            id: id.into(),
            label: label.into(),
            selected,
            tooltip: None,
            on_click: Some(Rc::new(on_click)),
        });
        self
    }

    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        if let Some(segment) = self.segments.last_mut() {
            segment.tooltip = Some(tooltip.into());
        }
        self
    }
}

impl RenderOnce for SegmentedControl {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let design = &self.design;
        let mut row = div()
            .id(self.id)
            .flex()
            .flex_none()
            .items_center()
            .gap_0p5()
            .p(px(1.0))
            .rounded(design.px(6.0))
            .border_1()
            .border_color(design.border)
            .bg(design.background);

        for segment in self.segments {
            let mut part = button(segment.id, segment.label)
                .size(ButtonSize::Tiny)
                .kind(ButtonKind::Ghost)
                .selected(segment.selected);
            if let Some(tooltip) = segment.tooltip {
                part = part.tooltip(tooltip);
            }
            if let Some(on_click) = segment.on_click {
                part = part.on_click(move |event, window, cx| on_click(event, window, cx));
            }
            row = row.child(part);
        }
        row
    }
}
