//! Confirmations for destructive actions.
//!
//! Every one is a `gpui_component` alert dialog: a modal layer that traps
//! focus, answers `Esc` with Cancel and `Enter` with the focused button, and
//! hands focus back to whatever had it. A drawn overlay cannot do any of that,
//! which is how the working-tree discard used to leave `Enter` toggling staging
//! behind it and `Esc` to the pane's own key binding.
//!
//! Cancel and OK are the component's own footer buttons, so they are the same
//! size and the destructive one is the component's danger variant.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{App, SharedString, Window};
use gpui_component::WindowExt as _;
use gpui_component::button::ButtonVariant;

use crate::settings::Settings;

/// What a confirmation says and which button performs it.
pub struct Confirm {
    pub title: SharedString,
    pub detail: Option<SharedString>,
    pub ok_label: SharedString,
    /// Paint OK red. Every destructive confirmation sets this.
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
            danger: true,
            always_ask: false,
        }
    }

    pub fn ok_label(mut self, label: impl Into<SharedString>) -> Self {
        self.ok_label = label.into();
        self
    }

    pub fn always_ask(mut self) -> Self {
        self.always_ask = true;
        self
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

    // The dialog may rebuild its layer, so the builder closure has to be
    // callable more than once and the handler has to stay shared.
    let on_ok = Rc::new(on_ok);
    window.open_alert_dialog(cx, move |dialog, _, _| {
        let on_ok = on_ok.clone();
        let variant = if confirm.danger {
            ButtonVariant::Danger
        } else {
            ButtonVariant::Primary
        };
        dialog
            .title(confirm.title.clone())
            .when_some(confirm.detail.clone(), |dialog, detail| {
                dialog.description(detail)
            })
            .confirm()
            .cancel_text("Cancel")
            .ok_text(confirm.ok_label.clone())
            .ok_variant(variant)
            .on_ok(move |_, window, cx| {
                on_ok(window, cx);
                true
            })
    });
}

#[cfg(test)]
mod tests {
    use super::should_ask;

    #[test]
    fn the_setting_decides_except_where_the_ask_is_mandatory() {
        assert!(should_ask(true, false), "the default asks");
        assert!(!should_ask(false, false), "the setting can turn it off");
        assert!(should_ask(false, true), "a forced ask survives the setting");
        assert!(should_ask(true, true));
    }
}
