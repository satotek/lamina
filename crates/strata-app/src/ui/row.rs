//! List rows: one shape for every selectable row in the app.
//!
//! The background and the left indicator follow one precedence, so a row means
//! the same thing in the branch list, the file lists and the commit list:
//!
//! 1. `selected` — the user is on this row (accent indicator, selected tint).
//! 2. `revealed` — the row is shown but not checked out (neutral tint; no
//!    caller sets it yet).
//! 3. `head` — the checked-out branch (nav tint, semibold text).
//! 4. neither — plain, with the hover tint supplied by [`list_row`].
//!
//! Row height and padding come from the same scale as the rest of the app; a
//! list whose rows must be shorter (the 20 px conflict list) overrides the
//! height on the returned element.
//!
//! Anything that has to blend into a row's fill — the commit graph's clip fade
//! — asks [`row_surface`] for the same colour instead of repeating the
//! precedence above.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyView, App, AppContext as _, Context, Div, ElementId, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, Stateful,
    StatefulInteractiveElement, Styled as _, Window, div, px,
};

use crate::style::Design;

/// Height of a standard list row, shared by the file, branch and conflict lists.
pub const ROW_HEIGHT: f32 = 28.0;

/// What a row is, for the background/indicator precedence.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct RowState {
    pub selected: bool,
    pub revealed: bool,
    pub head: bool,
}

impl RowState {
    /// The state a plain row starts in.
    pub const NORMAL: RowState = RowState {
        selected: false,
        revealed: false,
        head: false,
    };

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn revealed(mut self, revealed: bool) -> Self {
        self.revealed = revealed;
        self
    }

    pub fn head(mut self, head: bool) -> Self {
        self.head = head;
        self
    }
}

/// Which step of the precedence a row lands on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowTint {
    Selected,
    Revealed,
    Head,
    Normal,
}

/// The winning state: selected beats revealed beats head beats plain.
pub fn row_tint(state: RowState) -> RowTint {
    if state.selected {
        RowTint::Selected
    } else if state.revealed {
        RowTint::Revealed
    } else if state.head {
        RowTint::Head
    } else {
        RowTint::Normal
    }
}

/// The row's fill.
pub fn row_background(state: RowState, design: &Design) -> Hsla {
    match row_tint(state) {
        RowTint::Selected => design.row_selected,
        RowTint::Revealed => design.field,
        RowTint::Head => design.selected,
        // Transparent, so whatever surface the list sits on shows through.
        RowTint::Normal => Hsla::transparent_black(),
    }
}

/// The colour the row reads as against the list surface: a plain row is that
/// surface itself. Anything painted underneath a row's contents (the graph's
/// clip fade in the commit list) needs this, not the transparent fill.
pub fn row_surface(state: RowState, design: &Design) -> Hsla {
    match row_tint(state) {
        RowTint::Normal => design.background,
        RowTint::Selected | RowTint::Revealed | RowTint::Head => row_background(state, design),
    }
}

/// The 2 px bar down the row's leading edge.
pub fn row_indicator(state: RowState, design: &Design) -> Hsla {
    match row_tint(state) {
        RowTint::Selected => design.accent,
        RowTint::Revealed => design.border,
        RowTint::Head | RowTint::Normal => Hsla::transparent_black(),
    }
}

/// A selectable row. The caller appends its cells and, usually, `.on_click`.
pub fn list_row(id: impl Into<ElementId>, state: RowState, design: &Design) -> Stateful<Div> {
    let tint = row_tint(state);
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .h(px(ROW_HEIGHT))
        .mx(px(6.0))
        .px_2()
        .rounded(px(6.0))
        .border_l_2()
        .border_color(row_indicator(state, design))
        .bg(row_background(state, design))
        .text_size(px(design.small() + 0.5))
        .text_color(design.text)
        .when(state.head, |row| row.font_weight(FontWeight::SEMIBOLD))
        // A painted row keeps its own fill while hovered; only a plain row
        // lights up, so hovering never hides why a row is highlighted.
        .when(tint == RowTint::Normal, |row| {
            row.hover(|style| style.bg(design.field))
        })
}

/// A row's detail tip: what GPUI shows on hover, laid out so that any press
/// dismisses it.
///
/// GPUI keeps its native tooltip up while the pointer is inside the tooltip
/// itself, and paints it after every deferred layer. A right press inside the
/// tip therefore leaves it showing, and the context menu that press opens is
/// painted *underneath* it: the menu looks like it never opened. The tip here
/// reports a box of one pixel at the pointer, which keeps the pointer outside
/// it (so the press dismisses the tip, and the menu is the only thing left on
/// screen) while the content itself is drawn at the pointer as before.
///
/// The cost of the smaller box is GPUI's edge flipping: a tip that would
/// overflow the window is no longer moved to the other side of the pointer.
/// The sidebar's tips are one to three short lines and stay inside the window,
/// which is why the trade is worth it here.
pub trait RowTipExt: StatefulInteractiveElement + Sized {
    fn row_tooltip(
        self,
        build_tooltip: impl Fn(&mut Window, &mut App) -> AnyView + 'static,
    ) -> Self {
        self.tooltip(move |window, cx| {
            let content = build_tooltip(window, cx);
            cx.new(|_| RowTip { content }).into()
        })
    }
}

impl<E: StatefulInteractiveElement> RowTipExt for E {}

/// The view behind [`RowTipExt::row_tooltip`]: a one-pixel box at the pointer
/// with the tip's content drawn from its corner.
struct RowTip {
    content: AnyView,
}

impl Render for RowTip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().w(px(1.0)).h(px(1.0)).child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .child(self.content.clone()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_beats_revealed_beats_head() {
        let all = RowState {
            selected: true,
            revealed: true,
            head: true,
        };
        assert_eq!(row_tint(all), RowTint::Selected);
        assert_eq!(
            row_tint(RowState {
                selected: false,
                revealed: true,
                head: true,
            }),
            RowTint::Revealed
        );
        assert_eq!(
            row_tint(RowState {
                selected: false,
                revealed: false,
                head: true,
            }),
            RowTint::Head
        );
        assert_eq!(row_tint(RowState::NORMAL), RowTint::Normal);
    }

    #[test]
    fn each_tint_paints_a_distinct_fill() {
        let design = Design::default();
        assert_eq!(
            row_background(RowState::NORMAL.selected(true), &design),
            design.row_selected
        );
        assert_eq!(
            row_background(RowState::NORMAL.revealed(true), &design),
            design.field
        );
        assert_eq!(
            row_background(RowState::NORMAL.head(true), &design),
            design.selected
        );
        assert_eq!(
            row_indicator(RowState::NORMAL.selected(true), &design),
            design.accent
        );
        assert_ne!(
            row_background(RowState::NORMAL.revealed(true), &design),
            row_background(RowState::NORMAL.selected(true), &design),
            "a revealed row must not read as the selection"
        );
    }

    #[test]
    fn a_plain_row_reads_as_the_list_surface() {
        let design = Design::default();
        assert_eq!(
            row_surface(RowState::NORMAL, &design),
            design.background,
            "a plain row blends into the surface behind it"
        );
        for state in [
            RowState::NORMAL.selected(true),
            RowState::NORMAL.revealed(true),
            RowState::NORMAL.head(true),
        ] {
            assert_eq!(
                row_surface(state, &design),
                row_background(state, &design),
                "a painted row keeps its own fill"
            );
        }
    }

    /// A row's tip must be gone by the time the frame that draws the context
    /// menu the press opened is painted.
    ///
    /// GPUI draws its native tooltip after every deferred layer, so a tip left
    /// showing while the menu opens hides the menu instead: the user sees a
    /// right click that does nothing. The pointer here stays inside the tip,
    /// which is the case GPUI does not dismiss on its own.
    #[gpui::test]
    fn a_right_press_dismisses_the_row_tip(cx: &mut gpui::TestAppContext) {
        use std::cell::Cell;
        use std::rc::Rc;
        use std::time::Duration;

        use gpui::{
            AppContext as _, Background, Context, IntoElement, MouseButton, ParentElement as _,
            Render, Styled as _, VisualTestContext, Window, div, point, px, rgb,
        };
        use gpui_component::Root;
        use gpui_component::menu::PopupMenuItem;

        const TIP: u32 = 0x7a135e;

        struct Tip;
        impl Render for Tip {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().w(px(120.0)).h(px(24.0)).bg(rgb(TIP))
            }
        }

        struct Probe {
            opened: Rc<Cell<usize>>,
        }
        impl Render for Probe {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let opened = self.opened.clone();
                div().size_full().child(crate::ui::context_menu(
                    "tip-row-menu",
                    &Design::default(),
                    list_row("tip-row", RowState::NORMAL, &Design::default())
                        .w(px(220.0))
                        .row_tooltip(|_, cx| cx.new(|_| Tip).into()),
                    move |menu, _, _| {
                        opened.set(opened.get() + 1);
                        menu.item(PopupMenuItem::new("Item"))
                    },
                ))
            }
        }

        let paint = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
        };
        let showing = |cx: &mut VisualTestContext| {
            let marker: Background = rgb(TIP).into();
            cx.update(|window, _| {
                window
                    .painted_quads()
                    .iter()
                    .any(|quad| quad.background == marker)
            })
        };

        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_reduce_motion(true);
        });
        let opened = Rc::new(Cell::new(0));
        let probe_opened = opened.clone();
        let window = cx
            .update(|cx| {
                cx.open_window(Default::default(), |window, cx| {
                    let probe = cx.new(|_| Probe {
                        opened: probe_opened,
                    });
                    cx.new(|cx| Root::new(probe, window, cx))
                })
            })
            .expect("a window");
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        paint(&mut cx);

        // Hover the row's top-left corner: the tip drops to the pointer plus
        // 1 px and therefore covers the rest of the row.
        cx.simulate_mouse_move(point(px(10.0), px(2.0)), None, Default::default());
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        paint(&mut cx);
        assert!(showing(&mut cx), "the hovered row must show its tip");

        // Stay inside the tip, which is also inside the row.
        let inside_tip = point(px(20.0), px(12.0));
        cx.simulate_mouse_move(inside_tip, None, Default::default());
        cx.run_until_parked();
        paint(&mut cx);
        assert!(showing(&mut cx), "the tip stays while it is hovered");

        cx.simulate_mouse_down(inside_tip, MouseButton::Right, Default::default());
        cx.simulate_mouse_up(inside_tip, MouseButton::Right, Default::default());
        cx.run_until_parked();
        paint(&mut cx);

        assert_eq!(opened.get(), 1, "the right press must open the menu");
        assert!(
            !showing(&mut cx),
            "the tip must be dismissed or it paints over the menu"
        );
    }
}
