//! The reword form: the commit's full message in a multi-line editor.
//!
//! The dialog is its own entity so the draft lives as long as the window, not
//! as long as `RepoView`; only the submit hands the text back. An empty message
//! would make git refuse the amend, so the footer's OK is disabled until the
//! editor has something that survives git's `strip` cleanup.

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, Focusable as _, IntoElement, ParentElement, Render, Styled, Subscription,
    WeakEntity, Window, div, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::WindowExt as _;
use gpui_component::dialog::DialogFooter;
use gpui_component::input::{InputEvent, Textarea, TextareaState};
use strata_core::Oid;

use super::RepoView;
use super::history_ops::EditAction;
use crate::settings::Settings;
use crate::style::Design;
use crate::ui;

/// Opens the editor for `commit`, prefilled with `message`.
///
/// `warning` is the non-blocking note shown when the commit is already
/// published, because the reword will need a force push to reach the remote.
pub(super) fn open(
    view: &mut RepoView,
    commit: Oid,
    short: String,
    message: String,
    warning: Option<String>,
    window: &mut Window,
    cx: &mut Context<RepoView>,
) {
    // The context menu that offered this is closing; the dialog restores the
    // focus it was opened with, which must outlive the menu.
    view.focus_root(window, cx);
    let handle = cx.weak_entity();
    let dialog =
        cx.new(|cx| RewordDialog::new(handle, commit, short, message, warning, window, cx));
    let focus = dialog.read(cx).message_focus(cx);
    let content = dialog.clone();
    let footer = dialog.clone();
    let confirm = dialog;
    window.open_dialog(cx, move |card, _, cx| {
        let zoom = Settings::get(cx).appearance.zoom();
        let content = content.clone();
        let footer = footer.clone();
        let confirm = confirm.clone();
        let empty = confirm.read(cx).is_empty(cx);
        card.title(t!("edit_dialog.title"))
            .w(px(520.0 * zoom))
            .content(move |body, _, _| body.child(content.clone()))
            .footer(
                DialogFooter::new()
                    .child(
                        ui::button("reword-cancel", t!("common.cancel"))
                            .size(ui::ButtonSize::Medium)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        ui::button("reword-ok", t!("edit_dialog.reword").to_string())
                            .size(ui::ButtonSize::Medium)
                            .disabled(empty)
                            .on_click(move |_, window, cx| {
                                footer.update(cx, |dialog, cx| dialog.submit(window, cx));
                            }),
                    ),
            )
            .on_ok(move |_, window, cx| {
                // Enter reaches the dialog rather than the focused button; an
                // empty editor keeps it open instead of closing with nothing.
                if confirm.read(cx).is_empty(cx) {
                    return false;
                }
                confirm.update(cx, |dialog, cx| dialog.submit(window, cx));
                false
            })
    });
    window.defer(cx, move |window, cx| focus.focus(window, cx));
}

struct RewordDialog {
    /// Weak: the dialog outlives a `RepoView` the workspace has replaced.
    view: WeakEntity<RepoView>,
    commit: Oid,
    short: String,
    message: Entity<TextareaState>,
    /// Shown while the commit is already on its upstream.
    warning: Option<String>,
    _events: Vec<Subscription>,
}

impl RewordDialog {
    fn new(
        view: WeakEntity<RepoView>,
        commit: Oid,
        short: String,
        message: String,
        warning: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| TextareaState::new(window, cx).default_value(message));
        let events = vec![
            cx.subscribe(&input, |_this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    // Repaints the footer so OK follows the editor between enabled
                    // and disabled while the text is being cleared.
                    cx.notify();
                }
            }),
        ];
        Self {
            view,
            commit,
            short,
            message: input,
            warning,
            _events: events,
        }
    }

    fn message_focus(&self, cx: &App) -> gpui::FocusHandle {
        self.message.focus_handle(cx)
    }

    fn is_empty(&self, cx: &App) -> bool {
        self.message.read(cx).value().trim().is_empty()
    }

    fn submit(&mut self, window: &mut Window, cx: &mut App) {
        let message = self.message.read(cx).value().to_string();
        if message.trim().is_empty() {
            return;
        }
        let commit = self.commit;
        let _ = self.view.update(cx, |view, cx| {
            view.run_edit(EditAction::Reword, commit, Some(message), window, cx);
        });
        // Closing here would drop this dialog while it is being updated.
        window.defer(cx, |window, cx| window.close_dialog(cx));
    }
}

impl Render for RewordDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .child(
                div()
                    .text_size(px(design.small()))
                    .text_color(design.muted)
                    .child(t!("edit_dialog.description", short = self.short)),
            )
            .when_some(self.warning.clone(), |form, warning| {
                form.child(ui::banner(ui::BannerKind::Warning, warning, &design).wrap())
            })
            .child(
                Textarea::new(&self.message)
                    .h(design.px(220.0))
                    .w_full()
                    .text_size(px(design.code)),
            )
    }
}
