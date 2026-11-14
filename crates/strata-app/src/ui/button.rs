//! Buttons: the one place the app decides what a button looks like.
//!
//! Every button is a [`gpui_component::button::Button`] underneath, so it keeps
//! that component's focus ring, keyboard activation, disabled styling and
//! accessibility metadata — and its palette, which this module points at the
//! app's [`Design`](crate::style::Design) colours. This module only names the
//! four roles the app uses and the four sizes it draws, so a caller writes
//! `ui::button(...).danger()` instead of picking colours.

use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, ElementId, FocusHandle, IntoElement, ParentElement as _,
    RenderOnce, SharedString, StyleRefinement, Styled, Window,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Disableable as _, Icon, Selectable as _, Sizable as _, Size, StyledExt as _};

/// What a button means, which decides its fill.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ButtonKind {
    /// A filled button: the one thing the surface wants the user to do.
    #[default]
    Primary,
    /// A destructive action, in the theme's danger colour, so it never reads as
    /// the safe default.
    Danger,
    /// An ordinary bordered button.
    Normal,
    /// A borderless button for toolbars and header actions.
    Ghost,
}

/// How much room a button takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ButtonSize {
    /// The component's extra-small size (20 px), for a button inside a diff row.
    Tiny,
    /// 24 px: list headers and inline actions.
    #[default]
    Small,
    /// 32 px: dialog footers and the commit box.
    Medium,
}

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// A button built from a [`ButtonKind`] and a [`ButtonSize`].
///
/// The builder stores intent and builds the component at render time, so the
/// order of `.kind()` and `.on_click()` never matters.
#[derive(IntoElement)]
pub struct UiButton {
    id: ElementId,
    label: Option<SharedString>,
    icon: Option<Icon>,
    kind: ButtonKind,
    size: ButtonSize,
    disabled: bool,
    /// The button is selected in a set, as a toggle would be.
    selected: bool,
    /// The button is the trigger of an open popover or menu, which the
    /// component paints like a selection.
    open: bool,
    tooltip: Option<SharedString>,
    focus: Option<FocusHandle>,
    on_click: Option<ClickHandler>,
    children: Vec<AnyElement>,
    style: StyleRefinement,
}

/// A labelled button; `Primary` unless the caller says otherwise.
pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> UiButton {
    UiButton::new(id).label(label)
}

/// A square, icon-only button. The accessible name comes from its tooltip.
pub fn icon_button(id: impl Into<ElementId>, icon: impl Into<Icon>) -> UiButton {
    UiButton::new(id).icon(icon)
}

/// A button a caller fills with its own content, for the few shapes that are
/// more than an icon and a label (the title bar's icon-over-label actions).
pub fn content_button(id: impl Into<ElementId>) -> UiButton {
    UiButton::new(id)
}

impl UiButton {
    fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            label: None,
            icon: None,
            kind: ButtonKind::default(),
            size: ButtonSize::default(),
            disabled: false,
            selected: false,
            open: false,
            tooltip: None,
            focus: None,
            on_click: None,
            children: Vec::new(),
            style: StyleRefinement::default(),
        }
    }

    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn icon(mut self, icon: impl Into<Icon>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    pub fn kind(mut self, kind: ButtonKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn size(mut self, size: ButtonSize) -> Self {
        self.size = size;
        self
    }

    /// Shorthand for [`UiButton::kind`] with [`ButtonKind::Danger`].
    pub fn danger(self) -> Self {
        self.kind(ButtonKind::Danger)
    }

    /// Shorthand for [`UiButton::kind`] with [`ButtonKind::Ghost`].
    pub fn ghost(self) -> Self {
        self.kind(ButtonKind::Ghost)
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    /// Uses a caller-owned focus handle instead of the button's own.
    ///
    /// A feature that has to move focus onto this button, or read whether it
    /// has it, keeps the handle it handed over (the confirmation footer does
    /// both).
    pub fn track_focus(mut self, focus: &FocusHandle) -> Self {
        self.focus = Some(focus.clone());
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }

    /// Appends content after the label, for a key hint or a badge.
    pub fn child(mut self, child: impl IntoElement) -> Self {
        self.children.push(child.into_any_element());
        self
    }
}

impl Styled for UiButton {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl gpui_component::Selectable for UiButton {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }

    fn open(mut self, open: bool) -> Self {
        self.open = open;
        self
    }

    fn is_open(&self) -> bool {
        self.open
    }
}

impl RenderOnce for UiButton {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            label,
            icon,
            kind,
            size,
            disabled,
            selected,
            open,
            tooltip,
            focus,
            on_click,
            children,
            style,
        } = self;

        let mut button = Button::new(id);
        let labelled = label.is_some();
        if let Some(label) = label {
            button = button.label(label);
        }
        if let Some(icon) = icon {
            button = button.icon(icon);
        }
        button = match kind {
            ButtonKind::Primary => button.primary(),
            ButtonKind::Danger => button.danger(),
            ButtonKind::Normal => button,
            ButtonKind::Ghost => button.ghost(),
        };
        button = button.with_size(match size {
            ButtonSize::Tiny => Size::XSmall,
            ButtonSize::Small => Size::Small,
            ButtonSize::Medium => Size::Medium,
        });
        if let Some(focus) = focus {
            button = button.track_focus(&focus);
        }
        button = button.disabled(disabled);
        button = button.selected(selected);
        button = button.open(open);
        if let Some(tooltip) = tooltip {
            button = button.tooltip(tooltip.clone());
            // An icon-only button has no label for a screen reader to announce,
            // so its tooltip doubles as the accessible name.
            if !labelled {
                button = button.accessibility_label(tooltip);
            }
        }
        if let Some(on_click) = on_click {
            button = button.on_click(move |event, window, cx| on_click(event, window, cx));
        }
        button.children(children).refine_style(&style)
    }
}
