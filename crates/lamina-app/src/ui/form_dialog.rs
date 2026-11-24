//! Form dialogs: a titled card around one dialog entity, with Cancel and a
//! submit button in the footer.
//!
//! Every form in the app is the same shape: the entity is the dialog's body,
//! Cancel closes the card, and submit hands the entity's own `submit` method
//! what the user typed. The entity closes the dialog itself once its operation
//! is under way, so Enter never closes it either: a plan git would refuse must
//! leave the form open. The builder exists so the width, the zoom, the footer
//! and the initial focus are decided once, like [`confirm`](super::confirm).
//!
//! The element ids of the buttons stay with the caller: they are what the
//! visual tests name, and a shared default would make every dialog's buttons
//! indistinguishable there.

use std::rc::Rc;

use gpui::{
    App, Context, Entity, FocusHandle, ParentElement as _, Render, SharedString, Window, px,
};
use gpui_component::WindowExt as _;
use gpui_component::dialog::DialogFooter;

use super::button::{ButtonSize, button};
use crate::settings::Settings;

type SubmitHandler<E> = Rc<dyn Fn(&mut E, &mut Window, &mut Context<E>)>;
type SubmitGate<E> = Rc<dyn Fn(&E, &App) -> bool>;

struct Submit<E> {
    id: SharedString,
    label: SharedString,
    handler: SubmitHandler<E>,
}

/// A form dialog's title, width in unscaled pixels, body and footer buttons.
///
/// [`FormDialog::open`] does nothing when no [`content`](Self::content) was
/// set, so a caller that forgets the body never opens an empty card.
pub struct FormDialog<E: Render> {
    title: SharedString,
    /// Unscaled width; the zoom is applied when the card renders.
    width: f32,
    content: Option<Entity<E>>,
    cancel_id: SharedString,
    submit: Option<Submit<E>>,
    /// Whether the form can be submitted right now. It greys out the submit
    /// button and makes Enter a no-op, which is the same answer the caller's
    /// `submit` would give.
    can_submit: Option<SubmitGate<E>>,
    focus: Option<FocusHandle>,
}

impl<E: Render> FormDialog<E> {
    pub fn new(title: impl Into<SharedString>, width: f32) -> Self {
        Self {
            title: title.into(),
            width,
            content: None,
            cancel_id: "form-dialog-cancel".into(),
            submit: None,
            can_submit: None,
            focus: None,
        }
    }

    /// The dialog entity, drawn as the card's body.
    pub fn content(mut self, content: Entity<E>) -> Self {
        self.content = Some(content);
        self
    }

    /// The Cancel button's element id. The button says `common.cancel`, the
    /// way every form does.
    pub fn cancel(mut self, id: impl Into<SharedString>) -> Self {
        self.cancel_id = id.into();
        self
    }

    /// The submit button and what it runs. `handler` receives the entity the
    /// content was built from, so a form submits through its own method.
    pub fn submit(
        mut self,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        handler: impl Fn(&mut E, &mut Window, &mut Context<E>) + 'static,
    ) -> Self {
        self.submit = Some(Submit {
            id: id.into(),
            label: label.into(),
            handler: Rc::new(handler),
        });
        self
    }

    /// Greys out submit and ignores Enter while this returns false.
    pub fn can_submit(mut self, can_submit: impl Fn(&E, &App) -> bool + 'static) -> Self {
        self.can_submit = Some(Rc::new(can_submit));
        self
    }

    /// The control that takes focus once the card is up. The card takes focus
    /// when it opens, and a text field needs it to receive what the user types.
    pub fn focus(mut self, focus: FocusHandle) -> Self {
        self.focus = Some(focus);
        self
    }

    pub fn open(self, window: &mut Window, cx: &mut App) {
        let Self {
            title,
            width,
            content,
            cancel_id,
            submit,
            can_submit,
            focus,
        } = self;
        let Some(content) = content else {
            return;
        };
        let cancel_label = t!("common.cancel").to_string();
        // The dialog may rebuild its layer, so every render gets its own
        // clones; the handlers themselves are shared.
        window.open_dialog(cx, move |card, _, cx| {
            let element = content.clone();
            let zoom = Settings::get(cx).appearance.zoom();
            let enabled = can_submit
                .as_ref()
                .is_none_or(|can_submit| can_submit(content.read(cx), cx));
            let mut footer = DialogFooter::new().child(
                button(cancel_id.clone(), cancel_label.clone())
                    .size(ButtonSize::Medium)
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            );
            if let Some(submit) = &submit {
                let entity = content.clone();
                let handler = submit.handler.clone();
                footer = footer.child(
                    button(submit.id.clone(), submit.label.clone())
                        .size(ButtonSize::Medium)
                        .disabled(!enabled)
                        .on_click(move |_, window, cx| {
                            entity.update(cx, |entity, cx| handler(entity, window, cx));
                        }),
                );
            }
            let ok_entity = content.clone();
            let ok_handler = submit.as_ref().map(|submit| submit.handler.clone());
            let ok_gate = can_submit.clone();
            card.title(title.clone())
                .w(px(width * zoom))
                .content(move |body, _, _| body.child(element.clone()))
                .footer(footer)
                .on_ok(move |_, window, cx| {
                    let Some(handler) = ok_handler.clone() else {
                        return false;
                    };
                    // Enter reaches the dialog rather than the focused button,
                    // so the gate is asked again here: the entity may have
                    // changed since the frame that greyed the button out.
                    if ok_gate
                        .as_ref()
                        .is_some_and(|can_submit| !can_submit(ok_entity.read(cx), cx))
                    {
                        return false;
                    }
                    // The entity closes the dialog once its operation is on its
                    // way, so the confirm does not close it a second time.
                    ok_entity.update(cx, |entity, cx| handler(entity, window, cx));
                    false
                })
        });
        if let Some(focus) = focus {
            window.defer(cx, move |window, cx| focus.focus(window, cx));
        }
    }
}
