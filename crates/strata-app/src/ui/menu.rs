//! Context menus: one trigger shape for every right-click menu.
//!
//! gpui-component builds a context menu inside the trigger's element subtree,
//! so every text property the menu does not set itself is inherited from the
//! element that was pressed. A menu opened on the checked-out branch — whose
//! row is semibold — therefore painted every entry bold, and the same menu on a
//! monospace row would paint monospace.
//!
//! [`context_menu`] puts a wrapper between the row and the menu and states the
//! UI text on that wrapper, so the menu reads the same wherever it opens. The
//! row keeps its own styling: it sits inside the wrapper and states the weight,
//! size and colour it wants, as every part in this module does.
//!
//! The wrapper takes its width from its content, like a list item does. A row
//! that filled its list says `.w_full()` on the returned element as well, so
//! wrapping it does not change the list's layout.
//!
//! The entry size is gpui-component's own (`text_sm`): the library styles each
//! item, so only text an item does not size itself picks up the wrapper's.

use gpui::{
    Context, Div, ElementId, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    Stateful, Styled as _, Window, div,
};
use gpui_component::menu::{ContextMenu, ContextMenuExt as _, PopupMenu};

use crate::style::Design;

/// Attaches `build`'s menu to `trigger`, on a wrapper that carries the UI text.
///
/// The same element ID must be passed for the same trigger on every frame, or
/// the open menu is forgotten when the row re-renders. The returned element is
/// the wrapper, so a row that filled its list can fill the wrapper too.
pub fn context_menu<E>(
    id: impl Into<ElementId>,
    design: &Design,
    trigger: E,
    build: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
) -> ContextMenu<Stateful<Div>>
where
    E: IntoElement,
{
    div()
        .id(id)
        .flex_none()
        .font_family(design.ui_font_family.clone())
        .font_weight(FontWeight::NORMAL)
        .text_color(design.text)
        .context_menu(build)
        .child(trigger)
}

/// A menu label that says why it is disabled; a popup item has no tooltip, so
/// the reason has to travel in the text.
pub fn menu_label(base: impl Into<String>, blocked: Option<&'static str>) -> String {
    let base = base.into();
    match blocked {
        Some(reason) => format!("{base} ({reason})"),
        None => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blocked_entry_names_the_reason_in_parentheses() {
        assert_eq!(menu_label("Checkout", None), "Checkout");
        assert_eq!(
            menu_label("Checkout", Some("already checked out")),
            "Checkout (already checked out)"
        );
    }
}
