//! Repository tabs: the strip and each tab item inside it.
//!
//! Styled from [`Design`] tokens so the strip reads as an extension of the
//! 44 px chrome title bar above it. Each tab renders the repository name, an
//! optional accent dot while an operation is in progress, a hover-revealed
//! close button, middle-click close, and drag-and-drop reordering.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, ClickEvent, Context, Div, ElementId, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Render, RenderOnce,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::tooltip::Tooltip;

use crate::settings::Settings;
use crate::style::Design;

/// Height of the repository tab strip when two or more tabs are open.
pub const TAB_STRIP_HEIGHT: f32 = 32.0;

#[derive(Clone)]
struct DraggedTab {
    index: usize,
    label: SharedString,
}

impl Render for DraggedTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        div()
            .px_3()
            .py_1()
            .rounded(px(6.0))
            .bg(design.background)
            .border_1()
            .border_color(design.accent)
            .text_size(px(design.small()))
            .font_weight(FontWeight::MEDIUM)
            .text_color(design.text)
            .shadow_md()
            .child(self.label.clone())
    }
}

/// The horizontal container sitting directly below the title bar when
/// multiple repository tabs are open.
pub fn tab_strip(id: impl Into<ElementId>, design: &Design) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(4.0))
        .h(px(TAB_STRIP_HEIGHT))
        .w_full()
        .overflow_x_scroll()
        .px_2()
        .bg(design.chrome)
        .border_b_1()
        .border_color(design.border)
}

#[derive(Clone, Copy)]
struct TabColors {
    background: Hsla,
    field: Hsla,
    border: Hsla,
    text: Hsla,
    muted: Hsla,
    accent: Hsla,
    accent_soft: Hsla,
    font_size: f32,
}

impl From<&Design> for TabColors {
    fn from(design: &Design) -> Self {
        Self {
            background: design.background,
            field: design.field,
            border: design.border,
            text: design.text,
            muted: design.muted,
            accent: design.accent,
            accent_soft: design.accent_soft,
            font_size: design.small(),
        }
    }
}

type SelectHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
type CloseHandler = Rc<dyn Fn(&mut Window, &mut App)>;
type ReorderHandler = Rc<dyn Fn(usize, usize, &mut Window, &mut App)>;

/// One repository tab inside [`tab_strip`].
#[derive(IntoElement)]
pub struct UiTab {
    id: ElementId,
    index: usize,
    label: SharedString,
    active: bool,
    busy: bool,
    tooltip: Option<SharedString>,
    on_select: Option<SelectHandler>,
    on_close: Option<CloseHandler>,
    on_reorder: Option<ReorderHandler>,
    colors: TabColors,
}

/// Builds a repository tab at position `index`.
pub fn tab(
    id: impl Into<ElementId>,
    index: usize,
    label: impl Into<SharedString>,
    active: bool,
    design: &Design,
) -> UiTab {
    UiTab {
        id: id.into(),
        index,
        label: label.into(),
        active,
        busy: false,
        tooltip: None,
        on_select: None,
        on_close: None,
        on_reorder: None,
        colors: TabColors::from(design),
    }
}

impl UiTab {
    pub fn busy(mut self, busy: bool) -> Self {
        self.busy = busy;
        self
    }

    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    pub fn on_select(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_select = Some(Rc::new(handler));
        self
    }

    pub fn on_close(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_close = Some(Rc::new(handler));
        self
    }

    pub fn on_reorder(
        mut self,
        handler: impl Fn(usize, usize, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_reorder = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for UiTab {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let colors = self.colors;
        let index = self.index;
        let group: SharedString = format!("ui-tab-group-{index}").into();
        let label = self.label.clone();

        let mut tab = div()
            .id(self.id)
            .group(group.clone())
            .flex()
            .flex_1()
            .items_center()
            .gap(px(6.0))
            .min_w(px(76.0))
            .max_w(px(220.0))
            .h(px(24.0))
            .px_2()
            .rounded(px(6.0))
            .border_1()
            .cursor_pointer()
            .text_size(px(colors.font_size));

        tab = if self.active {
            tab.bg(colors.background)
                .border_color(colors.border)
                .text_color(colors.text)
                .font_weight(FontWeight::SEMIBOLD)
        } else {
            tab.border_color(Hsla::transparent_black())
                .text_color(colors.muted)
                .font_weight(FontWeight::MEDIUM)
                .hover(move |style| style.bg(colors.field).text_color(colors.text))
        };

        if let Some(tip) = self.tooltip {
            tab = tab.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx));
        }

        if let Some(on_select) = self.on_select {
            tab = tab.on_click(move |event, window, cx| on_select(event, window, cx));
        }

        if let Some(on_close) = self.on_close.clone() {
            tab = tab.on_mouse_down(MouseButton::Middle, move |_, window, cx| {
                cx.stop_propagation();
                on_close(window, cx);
            });
        }

        if let Some(on_reorder) = self.on_reorder {
            let drag = DraggedTab {
                index,
                label: label.clone(),
            };
            let drop_bg = colors.accent_soft;
            tab = tab
                .on_drag(drag, |dragged, _, _, cx| cx.new(|_| dragged.clone()))
                .drag_over::<DraggedTab>(move |style, _, _, _| style.bg(drop_bg))
                .on_drop(move |dragged: &DraggedTab, window, cx| {
                    on_reorder(dragged.index, index, window, cx);
                });
        }

        tab = tab
            .when(self.busy, |tab| {
                tab.child(
                    div()
                        .id(("ui-tab-busy", index))
                        .flex_none()
                        .w(px(6.0))
                        .h(px(6.0))
                        .rounded_full()
                        .bg(colors.accent),
                )
            })
            .child(div().flex_1().min_w_0().truncate().child(label));

        if let Some(on_close) = self.on_close {
            let close = div()
                .id(("ui-tab-close", index))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(px(16.0))
                .h(px(16.0))
                .rounded(px(4.0))
                .text_size(px(12.0))
                .text_color(colors.muted)
                .opacity(0.0)
                .group_hover(group, |style| style.opacity(1.0))
                .hover(move |style| style.bg(colors.border).text_color(colors.text))
                .cursor_pointer()
                // Do not let pressing the close button start a tab drag or select the tab.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    on_close(window, cx);
                })
                .child("×");
            tab = tab.child(close);
        }

        tab
    }
}
