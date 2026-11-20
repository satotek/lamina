//! Handing the keyboard to a text field.
//!
//! An `Input` registers its text handler for the focus handle its state owns on
//! the frame it paints. Focusing the view or dialog around it instead leaves
//! the field looking ready while what the user types goes nowhere until a click
//! lands on the field itself.
//!
//! The focus is deferred so it lands after the caller's update: a window being
//! built, a dialog opening or a closing overlay can move the focus again in the
//! same effect cycle, and an immediate call here would be overwritten before the
//! field is painted.

use gpui::{App, Entity, Focusable as _, Window};
use gpui_component::input::InputState;

/// Gives the keyboard to `input`'s text field.
pub(crate) fn focus_input(input: &Entity<InputState>, window: &mut Window, cx: &mut App) {
    let focus = input.focus_handle(cx);
    window.defer(cx, move |window, cx| focus.focus(window, cx));
}
