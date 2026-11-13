//! Stashing from the UI: the stash dialog and the sidebar's stash entries.
//!
//! Nothing here runs git. The dialog and the sidebar's context menu build an
//! [`Operation::Stash…`] and hand it to [`RepoView::run_operation`], which gives
//! it to the session worker; the sidebar is redrawn from the snapshot the
//! session publishes afterwards.
//!
//! A pop or apply that conflicts leaves the working tree in a conflicted state,
//! which the staging view already lists; nothing extra is needed here.

use gpui::{
    App, AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement, Render,
    Styled, Subscription, Window, div, px,
};
use gpui_component::checkbox::Checkbox;
use gpui_component::dialog::DialogFooter;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::notification::Notification;
use gpui_component::{ActiveTheme as _, WindowExt as _};
use strata_core::ChangeKind;
use strata_core::session::Operation;

use crate::repo_view::RepoView;

/// Title of the stash dialog and of the notifications it raises.
const STASH_TITLE: &str = "Stash changes";

impl RepoView {
    /// `StashChanges`: asks for a message and stashes the working tree.
    pub(crate) fn stash_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_none() {
            notify(
                window,
                cx,
                Notification::warning("Stashing needs an open repository."),
            );
            return;
        }
        if self.branches.is_running()
            || self
                .session
                .as_ref()
                .is_some_and(|session| session.snapshot().busy.is_some())
        {
            notify(
                window,
                cx,
                Notification::warning("An operation is already running."),
            );
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let snapshot = session.snapshot();
        let status = &snapshot.status;
        if status.staged.is_empty() && status.unstaged.is_empty() {
            notify(
                window,
                cx,
                Notification::info("The working tree has no changes to stash."),
            );
            return;
        }
        // `git stash push` saves nothing when the only changes are untracked
        // files and `-u` is missing, so start with the box ticked in exactly
        // that case.
        let include_default = status.staged.is_empty()
            && status
                .unstaged
                .iter()
                .all(|change| change.kind == ChangeKind::Untracked);

        let view = self.entity.clone();
        let dialog = cx.new(|cx| StashDialog::new(view, include_default, window, cx));
        let focus = dialog.read(cx).input_focus(cx);
        let content = dialog.clone();
        let footer = dialog.clone();
        let confirm = dialog;
        window.open_dialog(cx, move |card, _, _| {
            let content = content.clone();
            let footer = footer.clone();
            let confirm = confirm.clone();
            card.title(STASH_TITLE)
                .w(px(520.0))
                .content(move |body, _, _| body.child(content.clone()))
                .footer(
                    DialogFooter::new()
                        .child(
                            crate::ui::button("stash-cancel", "Cancel")
                                .size(crate::ui::ButtonSize::Medium)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            crate::ui::button("stash-submit", "Stash changes")
                                .size(crate::ui::ButtonSize::Medium)
                                .on_click(move |_, window, cx| {
                                    footer.update(cx, |dialog, cx| dialog.submit(window, cx));
                                }),
                        ),
                )
                .on_ok(move |_, window, cx| {
                    // The dialog closes itself once the operation is on its
                    // way, so the confirm does not close it a second time.
                    confirm.update(cx, |dialog, cx| dialog.submit(window, cx));
                    false
                })
        });
        // The input owns `Enter` while it has focus, which is where the user
        // starts typing; `Escape` and the footer still belong to the dialog.
        window.defer(cx, move |window, cx| focus.focus(window, cx));
    }

    /// The working-tree changes, stashed. The dialog has already validated that
    /// there is something to stash.
    fn stash_push(
        &mut self,
        message: Option<String>,
        include_untracked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = match &message {
            Some(message) => format!("stash \"{message}\""),
            None => "stash changes".to_owned(),
        };
        self.run_operation(
            Operation::StashPush {
                message,
                include_untracked,
            },
            label,
            window,
            cx,
        );
    }

    /// Applies `stash@{index}` and keeps the stash.
    pub(crate) fn stash_apply(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = format!("apply stash@{{{index}}}");
        self.run_operation(Operation::StashApply { index }, label, window, cx);
    }

    /// Applies `stash@{index}` and drops it when the apply succeeded.
    pub(crate) fn stash_pop(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let label = format!("pop stash@{{{index}}}");
        self.run_operation(Operation::StashPop { index }, label, window, cx);
    }

    /// Asks before deleting a stash entry; dropping is the one stash action git
    /// cannot undo.
    pub(crate) fn confirm_stash_drop(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The menu that offered this is already closing, and it would otherwise
        // leave the dialog with a focus handle that dies with it; the dialog
        // hands focus back to whatever was focused when it opened.
        self.focus_root(window, cx);
        let entity = self.entity.clone();
        crate::ui::confirm(
            window,
            cx,
            crate::ui::Confirm::new(
                format!("Drop stash@{{{index}}}?"),
                "The stashed changes are deleted. Unlike a pop, the changes are not \
                 applied to the working tree first.",
            )
            .ok_label("Drop stash"),
            move |window, cx| {
                entity.update(cx, |view, cx| view.stash_drop(index, window, cx));
            },
        );
    }

    fn stash_drop(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let label = format!("drop stash@{{{index}}}");
        self.run_operation(Operation::StashDrop { index }, label, window, cx);
    }
}

/// The stash dialog's form: a message and the untracked-files checkbox.
///
/// It is its own entity so the form state lives as long as the dialog, without
/// `RepoView` needing a field for a dialog that is open only briefly.
pub struct StashDialog {
    view: Entity<RepoView>,
    input: Entity<InputState>,
    include_untracked: bool,
    _input_events: Subscription,
}

impl StashDialog {
    fn new(
        view: Entity<RepoView>,
        include_untracked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Stash message (optional)"));
        let _input_events = cx.subscribe(&input, |this, _input, event: &InputEvent, cx| {
            if matches!(
                event,
                InputEvent::PressEnter {
                    secondary: false,
                    ..
                }
            ) {
                let Some(handle) = cx.active_window() else {
                    return;
                };
                let _ = handle.update(cx, |_, window, cx| this.submit(window, cx));
            }
        });
        Self {
            view,
            input,
            include_untracked,
            _input_events,
        }
    }

    fn input_focus(&self, cx: &App) -> gpui::FocusHandle {
        self.input.focus_handle(cx)
    }

    fn submit(&mut self, window: &mut Window, cx: &mut App) {
        let message = self.input.read(cx).value().trim().to_owned();
        let message = (!message.is_empty()).then_some(message);
        let include_untracked = self.include_untracked;
        self.view.update(cx, |view, cx| {
            view.stash_push(message, include_untracked, window, cx);
        });
        // Closing here would drop this dialog while it is being updated.
        window.defer(cx, |window, cx| window.close_dialog(cx));
    }
}

impl Render for StashDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let toggle = cx.listener(|this, checked: &bool, _window, cx| {
            this.include_untracked = *checked;
            cx.notify();
        });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .child(div().text_size(px(11.0)).text_color(muted).child(
                "The staged and unstaged changes move to a new stash entry, and the \
                         working tree returns to HEAD.",
            ))
            .child(Input::new(&self.input))
            .child(
                Checkbox::new("stash-include-untracked")
                    .checked(self.include_untracked)
                    .label("Include untracked files")
                    .on_change(toggle),
            )
    }
}

fn notify(window: &mut Window, cx: &mut App, note: Notification) {
    window.push_notification(note.title(STASH_TITLE), cx);
}
