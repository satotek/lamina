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
//!
//! [`menu_open`] reports whether any menu is up. GPUI paints its native
//! tooltips above every deferred layer, so a row tip that appears while a menu
//! is open hides the menu; [`row_tooltip`](super::row::RowTipExt::row_tooltip)
//! consults this state and skips its content while a menu is open.

use std::cell::RefCell;

use gpui::{
    Context, DismissEvent, Div, ElementId, EntityId, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, Stateful, Styled as _, Subscription, WeakEntity, Window, div,
};
use gpui_component::menu::{ContextMenu, ContextMenuExt as _, PopupMenu};

use crate::style::Design;

thread_local! {
    /// The context menus up on this thread, kept in step with their close.
    ///
    /// Menus live on the UI thread and a thread-local cannot outlive them, so
    /// the list starts empty in every process and every test.
    static OPEN_MENUS: RefCell<OpenMenus> = RefCell::new(OpenMenus::default());
}

/// The menus currently open, as weak handles so a menu lost without a dismiss
/// (its window closed) stops counting.
#[derive(Default)]
struct OpenMenus {
    menus: Vec<OpenMenu>,
}

struct OpenMenu {
    menu: WeakEntity<PopupMenu>,
    /// Held only for its side effect: the menu's `DismissEvent` removes this
    /// entry.
    _dismiss: Subscription,
}

impl OpenMenus {
    fn is_open(&mut self) -> bool {
        self.menus.retain(|menu| menu.menu.is_upgradable());
        !self.menus.is_empty()
    }

    fn add(&mut self, menu: WeakEntity<PopupMenu>, dismiss: Subscription) {
        self.menus.push(OpenMenu {
            menu,
            _dismiss: dismiss,
        });
    }

    fn remove(&mut self, id: EntityId) {
        self.menus.retain(|menu| menu.menu.entity_id() != id);
    }
}

/// Whether a context menu from [`context_menu`] is open right now.
pub(crate) fn menu_open() -> bool {
    OPEN_MENUS.with(|menus| menus.borrow_mut().is_open())
}

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
        .context_menu(move |menu, window, cx| {
            note_menu_open(window, cx);
            build(menu, window, cx)
        })
        .child(trigger)
}

/// Records that `cx`'s menu is up until it is dismissed, so row tips stay
/// hidden while it is.
fn note_menu_open(window: &mut Window, cx: &mut Context<PopupMenu>) {
    let id = cx.entity_id();
    let menu = cx.weak_entity();
    // The entity is not in the app until its builder returns, so the listener
    // is installed on the next effect flush, which still runs inside the press
    // that opened the menu: no frame can build a tip in between.
    window.defer(cx, move |_window, cx| {
        let Some(menu_entity) = menu.upgrade() else {
            return;
        };
        let dismiss = cx.subscribe(&menu_entity, move |_, _: &DismissEvent, _| {
            OPEN_MENUS.with(|menus| menus.borrow_mut().remove(id));
        });
        OPEN_MENUS.with(|menus| menus.borrow_mut().add(menu, dismiss));
    });
}

/// A menu label that says why it is disabled; a popup item has no tooltip, so
/// the reason has to travel in the text.
pub fn menu_label(base: impl Into<String>, blocked: Option<impl Into<String>>) -> String {
    let base = base.into();
    match blocked {
        Some(reason) => format!("{base} ({})", reason.into()),
        None => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blocked_entry_names_the_reason_in_parentheses() {
        assert_eq!(menu_label("Checkout", None::<&str>), "Checkout");
        assert_eq!(
            menu_label("Checkout", Some("already checked out")),
            "Checkout (already checked out)"
        );
    }
}
