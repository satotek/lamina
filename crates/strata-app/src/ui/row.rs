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

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Div, ElementId, FontWeight, Hsla, InteractiveElement as _, Stateful, Styled as _, div, px,
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
        RowTint::Normal => Hsla::transparent_black(),
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
}
