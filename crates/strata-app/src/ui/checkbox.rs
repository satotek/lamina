//! Checkboxes.
//!
//! The conflict editor ticks a side per conflict block and one side for a
//! whole file, so the control shows up inside dense rows as well as headers.
//! The component already draws the box; this part only settles the size that
//! fits those rows.

use gpui::ElementId;
use gpui_component::Sizable as _;
use gpui_component::Size;
use gpui_component::checkbox::Checkbox;

/// A checkbox for a row gutter or a pane header.
pub fn checkbox(id: impl Into<ElementId>, checked: bool) -> Checkbox {
    Checkbox::new(id).checked(checked).with_size(Size::Small)
}
