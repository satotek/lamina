//! Confirmations for destructive actions.
//!
//! Every one is a `gpui_component` alert dialog: a modal layer that traps
//! focus, answers `Esc` with Cancel and hands focus back to whatever had it. A
//! drawn overlay cannot do any of that, which is how the working-tree discard
//! used to leave `Enter` toggling staging behind it and `Esc` to the pane's own
//! key binding.
//!
//! The footer is ours rather than the component's: its buttons cannot be
//! focused by name, and its `Enter` binding always confirms. Here the
//! destructive confirmation opens on Cancel, so a stray `Enter` cancels and
//! `Tab` then `Enter` is a deliberate second step; a non-destructive one opens
//! on OK. Both buttons are the same size, and the destructive one carries the
//! app's danger colour.

use std::cell::Cell;
use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{App, ParentElement as _, SharedString, Window};
use gpui_component::WindowExt as _;
use gpui_component::dialog::DialogFooter;

use super::button::{ButtonKind, ButtonSize, button};
use crate::settings::Settings;

/// What a confirmation says and which button performs it.
pub struct Confirm {
    pub title: SharedString,
    pub detail: Option<SharedString>,
    pub ok_label: SharedString,
    /// Text of the button that cancels; "Cancel" unless the caller names it.
    pub cancel_label: Option<SharedString>,
    /// Paint OK red and open on Cancel. Every destructive confirmation sets
    /// this.
    pub danger: bool,
    /// Ask even when the user turned `behaviour.confirm_destructive` off.
    pub always_ask: bool,
}

impl Confirm {
    pub fn new(title: impl Into<SharedString>, detail: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            detail: Some(detail.into()),
            ok_label: "Delete".into(),
            cancel_label: None,
            danger: true,
            always_ask: false,
        }
    }

    pub fn ok_label(mut self, label: impl Into<SharedString>) -> Self {
        self.ok_label = label.into();
        self
    }

    /// Names the way out, for a confirmation whose cancel is a next step
    /// rather than a refusal ("Keep resolving" while a merge is stopped).
    pub fn cancel_label(mut self, label: impl Into<SharedString>) -> Self {
        self.cancel_label = Some(label.into());
        self
    }

    pub fn always_ask(mut self) -> Self {
        self.always_ask = true;
        self
    }

    /// The Cancel button's text.
    fn cancel_text(&self) -> SharedString {
        self.cancel_label.clone().unwrap_or_else(|| "Cancel".into())
    }
}

/// Whether this confirmation should be shown at all.
///
/// A confirmation the user switched off in the settings is skipped and the
/// action runs directly; `always_ask` is the escape hatch for the few cases
/// where the action is already a recovery step (the force-delete offered after
/// git refused to delete an unmerged branch) and skipping it would silently
/// escalate what the user asked for.
pub fn should_ask(confirm_destructive: bool, always_ask: bool) -> bool {
    always_ask || confirm_destructive
}

/// Asks, then runs `on_ok`, or runs `on_ok` straight away when the settings
/// turned confirmations off.
pub fn confirm(
    window: &mut Window,
    cx: &mut App,
    confirm: Confirm,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
) {
    if !should_ask(
        Settings::get(cx).behaviour.confirm_destructive,
        confirm.always_ask,
    ) {
        on_ok(window, cx);
        return;
    }

    let cancel_label = confirm.cancel_text();
    let Confirm {
        title,
        detail,
        ok_label,
        danger,
        ..
    } = confirm;
    let ok_kind = if danger {
        ButtonKind::Danger
    } else {
        ButtonKind::Primary
    };

    // The dialog may rebuild its layer, so the builder closure has to be
    // callable more than once and the handlers have to stay shared.
    let on_ok = Rc::new(on_ok);

    // The first frame is the only one allowed to set focus: the dialog layer
    // renders again on every resize, and focus belongs to the user after that.
    let cancel_focus = cx.focus_handle();
    let ok_focus = cx.focus_handle();
    let initial_focus = if danger {
        cancel_focus.clone()
    } else {
        ok_focus.clone()
    };
    let first_frame = Cell::new(true);

    window.open_alert_dialog(cx, move |dialog, window, cx| {
        if first_frame.replace(false) {
            initial_focus.focus(window, cx);
        }

        let ok_click = on_ok.clone();
        let ok_enter = on_ok.clone();
        let ok_has_focus = ok_focus.clone();
        dialog
            .title(title.clone())
            .when_some(detail.clone(), |dialog, detail| dialog.description(detail))
            .footer(
                DialogFooter::new()
                    .child(
                        button("confirm-cancel", cancel_label.clone())
                            .kind(ButtonKind::Normal)
                            .size(ButtonSize::Medium)
                            .track_focus(&cancel_focus)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        button("confirm-ok", ok_label.clone())
                            .kind(ok_kind)
                            .size(ButtonSize::Medium)
                            .track_focus(&ok_focus)
                            .on_click(move |_, window, cx| {
                                ok_click(window, cx);
                                window.close_dialog(cx);
                            }),
                    ),
            )
            .on_ok(move |_, window, cx| {
                // Enter reaches the dialog, not the focused button, so the
                // button that has focus decides what it means. Anything but a
                // focused OK is the safe reading of a stray Enter.
                if danger && !ok_has_focus.is_focused(window) {
                    return true;
                }
                ok_enter(window, cx);
                true
            })
    });
}

#[cfg(test)]
mod tests {
    use super::Confirm;

    #[test]
    fn the_setting_decides_except_where_the_ask_is_mandatory() {
        use super::should_ask;
        assert!(should_ask(true, false), "the default asks");
        assert!(!should_ask(false, false), "the setting can turn it off");
        assert!(should_ask(false, true), "a forced ask survives the setting");
        assert!(should_ask(true, true));
    }

    #[test]
    fn cancel_says_cancel_unless_the_caller_names_it() {
        let plain = Confirm::new("Discard changes?", "The changes are lost.");
        assert_eq!(plain.cancel_text().as_ref(), "Cancel");

        let named = plain.cancel_label("Keep resolving");
        assert_eq!(named.cancel_text().as_ref(), "Keep resolving");
    }
}
