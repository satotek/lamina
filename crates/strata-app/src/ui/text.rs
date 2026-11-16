//! Text-level parts: the pane-level message, the empty state and the section
//! header above a list.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, Div, ElementId, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, RenderOnce, SharedString, Stateful, StatefulInteractiveElement as _,
    StyleRefinement, Styled, Window, div, px,
};
use gpui_component::Icon;
use gpui_component::Sizable as _;
use gpui_component::StyledExt as _;
use gpui_kit_assets::IconName;

use crate::style::Design;

/// A full-pane message: loading, empty or failed. Painted in `color` on the
/// pane's own background so it reads as the pane's contents, not a toast.
pub fn message(text: impl Into<SharedString>, color: Hsla, design: &Design) -> Div {
    div()
        .flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .p_4()
        .text_size(design.px(12.0))
        .text_color(color)
        .bg(design.background)
        .child(text.into())
}

/// A pane with nothing in it.
pub fn empty_state(text: impl Into<SharedString>, design: &Design) -> Div {
    message(text, design.muted, design)
}

/// A pane that is still loading: a spinner beside a short label, centred where
/// the contents will appear. Long loads (a large history's first page, a big
/// diff) show this instead of leaving the pane blank.
pub fn loading_state(text: impl Into<SharedString>, design: &Design) -> Div {
    div()
        .flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .gap(design.px(8.0))
        .p_4()
        .text_size(design.px(12.0))
        .text_color(design.muted)
        .bg(design.background)
        .child(super::spinner("pane-loading-spinner", design, 14.0))
        .child(text.into())
}

/// The heading above a group of rows: a small caps label, an optional count
/// and whatever the group lets the user do.
///
/// The default height and padding match the sidebar's ref sections; a pane with
/// its own spacing overrides them on the returned element.
pub fn section_header(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    design: &Design,
) -> SectionHeader {
    SectionHeader {
        id: id.into(),
        label: label.into(),
        icon: None,
        count: None,
        disclosure: None,
        danger: false,
        trailing: Vec::new(),
        on_click: None,
        design: design.clone(),
        style: StyleRefinement::default(),
    }
}

type SectionAction = Box<dyn Fn(&mut Window, &mut App) + 'static>;

#[derive(IntoElement)]
pub struct SectionHeader {
    id: ElementId,
    label: SharedString,
    icon: Option<AnyElement>,
    count: Option<usize>,
    disclosure: Option<bool>,
    danger: bool,
    trailing: Vec<AnyElement>,
    on_click: Option<SectionAction>,
    design: Design,
    style: StyleRefinement,
}

impl SectionHeader {
    /// A leading icon, for a section that names a ref kind.
    pub fn icon(mut self, icon: impl IntoElement) -> Self {
        self.icon = Some(icon.into_any_element());
        self
    }

    /// Show how many entries the section holds.
    pub fn count(mut self, count: usize) -> Self {
        self.count = Some(count);
        self
    }

    /// Draw the chevron of a collapsible section; `open` picks its direction.
    pub fn disclosure(mut self, open: bool) -> Self {
        self.disclosure = Some(open);
        self
    }

    /// The heading is about a problem, so paint it red.
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    /// A control on the right, such as "Stage all".
    pub fn trailing(mut self, element: impl IntoElement) -> Self {
        self.trailing.push(element.into_any_element());
        self
    }

    /// Makes the whole header a control, with the hover tint that implies.
    pub fn on_click(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

impl Styled for SectionHeader {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for SectionHeader {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let color = if self.danger {
            self.design.remove_text
        } else {
            self.design.muted
        };
        let clickable = self.on_click.is_some();
        let mut row: Stateful<Div> = div()
            .id(self.id)
            .flex()
            .items_center()
            .gap(self.design.px(6.0))
            .h(self.design.px(26.0))
            .px_2()
            .rounded(self.design.px(7.0))
            .text_size(px(self.design.tiny()))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(color)
            .when(clickable, |row| {
                row.cursor_pointer()
                    .hover(|style| style.bg(self.design.selected))
            });
        if let Some(open) = self.disclosure {
            row = row.child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .with_size(self.design.px(12.0))
                .text_color(color),
            );
        }
        if let Some(icon) = self.icon {
            row = row.child(icon);
        }
        // The count reads as part of the heading, so it sits next to the
        // label; a group's actions stay on the far edge. An empty label is a
        // strip with no heading, only its controls.
        if !self.label.is_empty() {
            row = row.child(div().flex_none().truncate().child(self.label));
        }
        if let Some(count) = self.count {
            row = row.child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::NORMAL)
                    .text_size(self.design.px(10.0))
                    .child(count.to_string()),
            );
        }
        row = row.child(div().flex_1());
        for element in self.trailing {
            row = row.child(element);
        }
        let row = row.refine_style(&self.style);
        if let Some(on_click) = self.on_click {
            row.on_click(move |_, window, cx| on_click(window, cx))
                .into_any_element()
        } else {
            row.into_any_element()
        }
    }
}
