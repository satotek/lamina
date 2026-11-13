//! Inline notices: a tinted bar with a message and, optionally, the actions
//! that resolve it.
//!
//! The merge banner, the conflict-marker warnings and the settings hints all
//! say the same three things — informational, a warning, or a failure — so the
//! three tints live here instead of once per pane.

use gpui::{
    AnyElement, App, Hsla, IntoElement, ParentElement as _, RenderOnce, SharedString,
    StyleRefinement, Styled, Window, div, px,
};
use gpui_component::StyledExt as _;

use crate::style::Design;

/// How loud a banner is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BannerKind {
    Info,
    Warning,
    Danger,
}

impl BannerKind {
    fn colors(self, design: &Design) -> (Hsla, Hsla) {
        match self {
            BannerKind::Info => (design.accent_soft, design.accent_text),
            BannerKind::Warning => (design.tag_background, design.tag_text),
            BannerKind::Danger => (design.remove_background, design.remove_text),
        }
    }
}

/// A tinted bar with a message. Use [`Banner::action`] for the button that
/// resolves whatever the message is about.
pub fn banner(kind: BannerKind, message: impl Into<SharedString>, design: &Design) -> Banner {
    Banner {
        kind,
        message: message.into(),
        detail: None,
        actions: Vec::new(),
        design: design.clone(),
        style: StyleRefinement::default(),
    }
}

#[derive(IntoElement)]
pub struct Banner {
    kind: BannerKind,
    message: SharedString,
    detail: Option<SharedString>,
    actions: Vec<AnyElement>,
    design: Design,
    style: StyleRefinement,
}

impl Banner {
    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.actions.push(action.into_any_element());
        self
    }

    /// A quieter second line under the message, for context that should not
    /// compete with the action the banner asks for.
    pub fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

impl Styled for Banner {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Banner {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let (background, foreground) = self.kind.colors(&self.design);
        let mut actions = div().flex().items_center().gap_2().flex_none();
        for action in self.actions {
            actions = actions.child(action);
        }
        let mut text = div().flex_1().min_w_0().flex().flex_col();
        text = text.child(div().truncate().child(self.message));
        if let Some(detail) = self.detail {
            text = text.child(
                div()
                    .truncate()
                    .text_size(px(self.design.tiny()))
                    .opacity(0.8)
                    .child(detail),
            );
        }
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .flex_none()
            .px_3()
            .py_1()
            .bg(background)
            .text_size(px(self.design.small()))
            .text_color(foreground)
            .child(text)
            .child(actions)
            .refine_style(&self.style)
    }
}
