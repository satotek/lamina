//! The credential prompt git asks for through the askpass bridge.
//!
//! One input, masked for passwords and passphrases and plain for a username or
//! a host-key question. The answer and a cancel travel through the same
//! closure: the caller decides what cancelling means, and for git it means the
//! helper exits non-zero and the command fails with git's own message.
//!
//! The dialog is an alert dialog, so it traps focus, answers `Esc` with cancel
//! and hands focus back when it closes, like the confirmations.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    App, AppContext as _, Focusable as _, ParentElement as _, SharedString, Styled as _, Window,
};
use gpui_component::WindowExt as _;
use gpui_component::dialog::DialogFooter;
use gpui_component::input::{Input, InputState};

use super::button::{ButtonKind, ButtonSize, button};

/// What to do with one answer; the dialog calls it once. It gets the window and
/// app so the caller can update its own entity, like the other dialog parts.
type AnswerFn = Rc<dyn Fn(Option<String>, &mut Window, &mut App)>;

/// Opens the credential dialog for one askpass prompt.
///
/// `on_answer` is called exactly once, with the typed text or `None` when the
/// user cancels (the Cancel button or `Esc`). It runs on the UI thread.
pub fn askpass_prompt(
    window: &mut Window,
    cx: &mut App,
    title: impl Into<SharedString>,
    prompt: impl Into<SharedString>,
    secret: bool,
    on_answer: impl Fn(Option<String>, &mut Window, &mut App) + 'static,
) {
    let title = title.into();
    let prompt = prompt.into();
    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(if secret {
                t!("askpass.password").to_string()
            } else {
                t!("askpass.answer").to_string()
            })
            .masked(secret)
    });
    let input_focus = input.focus_handle(cx);

    // The dialog rebuilds its layer and both the footer and `Enter` can answer,
    // so only the first answer counts.
    let answered = Rc::new(Cell::new(false));
    let finish: AnswerFn = {
        let answered = Rc::clone(&answered);
        Rc::new(
            move |answer: Option<String>, window: &mut Window, cx: &mut App| {
                if !answered.replace(true) {
                    on_answer(answer, window, cx);
                }
            },
        )
    };
    let first_frame = Cell::new(true);

    window.open_alert_dialog(cx, {
        let input = input.clone();
        move |dialog, window, cx| {
            if first_frame.replace(false) {
                input_focus.focus(window, cx);
            }

            let cancel_click = Rc::clone(&finish);
            let ok_click = Rc::clone(&finish);
            let ok_enter = Rc::clone(&finish);
            let cancel_escape = Rc::clone(&finish);
            let ok_input = input.clone();
            let enter_input = input.clone();

            dialog
                .title(title.clone())
                .description(prompt.clone())
                .child(Input::new(&input).w_full())
                .footer(
                    DialogFooter::new()
                        .child(
                            button("askpass-cancel", t!("common.cancel").to_string())
                                .kind(ButtonKind::Normal)
                                .size(ButtonSize::Medium)
                                .on_click(move |_, window, cx| {
                                    cancel_click(None, window, cx);
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            button("askpass-ok", t!("common.ok").to_string())
                                .kind(ButtonKind::Primary)
                                .size(ButtonSize::Medium)
                                .on_click(move |_, window, cx| {
                                    let answer = ok_input.read(cx).value().to_string();
                                    ok_click(Some(answer), window, cx);
                                    window.close_dialog(cx);
                                }),
                        ),
                )
                .on_ok(move |_, window, cx| {
                    let answer = enter_input.read(cx).value().to_string();
                    ok_enter(Some(answer), window, cx);
                    true
                })
                .on_cancel(move |_, window, cx| {
                    // `Esc` and a close request cancel rather than hang git.
                    cancel_escape(None, window, cx);
                    true
                })
        }
    });
}
