//! Checkboxes.
//!
//! The conflict editor ticks a side per conflict block and one side for a
//! whole file, so the control shows up inside dense rows as well as headers.
//! The component already draws the box; this part only settles the size that
//! fits those rows, plus a framed gutter variant for the tinted side blocks.

use std::sync::Arc;

use gpui::{
    AnyElement, App, ElementId, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::checkbox::Checkbox;
use gpui_component::tooltip::Tooltip;
use gpui_component::{Sizable as _, Size};

use crate::style::Design;

/// A checkbox for a row gutter or a pane header.
pub fn checkbox(id: impl Into<ElementId>, checked: bool) -> Checkbox {
    Checkbox::new(id).checked(checked).with_size(Size::Small)
}

/// A checkbox for a dense row gutter, such as a conflict block's side picker.
///
/// The box sits over a tinted line, where the component's small indicator and
/// light border all but disappear. A frame around it gives the control an edge
/// that reads in both themes, a larger hit area, and a hover wash that says it
/// is clickable.
pub fn gutter_checkbox(
    id: impl Into<ElementId>,
    checked: bool,
    color: Hsla,
    design: &Design,
    tooltip: impl Into<SharedString>,
    on_click: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let id = id.into();
    let frame = ElementId::NamedChild(Arc::new(id.clone()), "frame".into());
    let tooltip: SharedString = tooltip.into();
    let label = tooltip.clone();
    div()
        .id(frame)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(px(18.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(if checked { color } else { design.muted })
        .bg(if checked {
            color.opacity(0.18)
        } else {
            design.background
        })
        .hover(|style| style.bg(design.selected))
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .child(
            Checkbox::new(id)
                .checked(checked)
                .with_size(Size::Small)
                .accessibility_label(label)
                .on_click(on_click),
        )
        .into_any_element()
}
