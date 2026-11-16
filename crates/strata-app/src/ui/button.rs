//! Buttons: the one place the app decides what a button looks like.
//!
//! Every button is a [`gpui_component::button::Button`] underneath, so it keeps
//! that component's focus ring, keyboard activation, disabled styling and
//! accessibility metadata — and its palette, which this module points at the
//! app's [`Design`](crate::style::Design) colours. This module only names the
//! four roles the app uses and the four sizes it draws, so a caller writes
//! `ui::button(...).danger()` instead of picking colours.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, ClickEvent, ElementId, FocusHandle, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, RenderOnce, SharedString, StatefulInteractiveElement as _,
    StyleRefinement, Styled, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, Size, StyledExt as _,
};
use gpui_kit_assets::IconName;

use crate::style::Design;

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
    /// The button ran the command that is still in flight: its icon becomes a
    /// spinner and it refuses further clicks until the operation reports back.
    loading: bool,
    /// The button is selected in a set, as a toggle would be.
    selected: bool,
    /// Accessible toggle state (`aria-pressed`), for buttons that stay latched.
    toggled: bool,
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

/// A square, icon-only toggle button. Active uses `Primary`, inactive uses `Ghost`.
pub fn icon_toggle(id: impl Into<ElementId>, icon: impl Into<Icon>, active: bool) -> UiButton {
    icon_button(id, icon)
        .kind(if active {
            ButtonKind::Primary
        } else {
            ButtonKind::Ghost
        })
        .selected(active)
        .toggled(active)
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
            loading: false,
            selected: false,
            toggled: false,
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

    /// The action this button started has not finished. The button stays
    /// disabled and shows a spinner in place of its icon.
    pub fn loading(mut self, loading: bool) -> Self {
        self.loading = loading;
        self
    }

    /// Marks the button's accessible toggle state (`aria-pressed`) without
    /// changing its colour role.
    pub fn toggled(mut self, toggled: bool) -> Self {
        self.toggled = toggled;
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

    /// The role the button was built with, for tests that guard a dialog's
    /// action hierarchy.
    #[cfg(test)]
    pub(crate) fn button_kind(&self) -> ButtonKind {
        self.kind
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
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            label,
            icon,
            kind,
            size,
            disabled,
            loading,
            selected,
            toggled,
            open,
            tooltip,
            focus,
            on_click,
            children,
            style,
        } = self;

        let has_icon = icon.is_some();
        let spinner_id = ElementId::Name(format!("{id}·loading").into());
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
        // Loading is a disabled state of its own: the command has already been
        // submitted, so a second click would be a second operation.
        button = button.disabled(disabled || loading);
        button = button.loading(loading);
        button = button.selected(selected);
        button = button.toggled(toggled);
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
        let children = if loading && !has_icon {
            // A label-only button has no icon slot for the component to swap,
            // so the spinner goes in front of the label's row instead.
            let color = match kind {
                ButtonKind::Primary => cx.theme().button_primary_foreground,
                ButtonKind::Danger => cx.theme().button_danger_foreground,
                ButtonKind::Normal => cx.theme().button_foreground,
                ButtonKind::Ghost => cx.theme().foreground,
            };
            let diameter = match size {
                ButtonSize::Tiny => px(11.0),
                ButtonSize::Small => px(12.0),
                ButtonSize::Medium => px(14.0),
            };
            let mut children = children;
            children.insert(
                0,
                crate::ui::Spinner::fixed(spinner_id, diameter, color).into_any_element(),
            );
            children
        } else {
            children
        };
        button.children(children).refine_style(&style)
    }
}

/// A compact action tile with an accent icon, optional shortcut keycaps, and a
/// label, used in equal-width grids such as the welcome screen.
#[derive(IntoElement)]
pub struct ActionTile {
    id: ElementId,
    icon: IconName,
    label: SharedString,
    keycaps: Vec<SharedString>,
    primary: bool,
    design: Design,
    on_click: Option<ClickHandler>,
}

/// The narrowest an action tile may get before the row wraps.
///
/// The width covers the longest welcome label ("Open from path…") plus the
/// tile's padding, so a wrapped row never cuts a label instead.
const ACTION_TILE_MIN_WIDTH: f32 = 120.0;

pub fn action_tile(
    id: impl Into<ElementId>,
    icon: IconName,
    label: impl Into<SharedString>,
    design: &Design,
) -> ActionTile {
    ActionTile {
        id: id.into(),
        icon,
        label: label.into(),
        keycaps: Vec::new(),
        primary: false,
        design: design.clone(),
        on_click: None,
    }
}

impl ActionTile {
    pub fn primary(mut self, primary: bool) -> Self {
        self.primary = primary;
        self
    }

    pub fn keycaps(mut self, keycaps: Vec<SharedString>) -> Self {
        self.keycaps = keycaps;
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for ActionTile {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let design = &self.design;
        let (bg, hover_bg, border, label_color) = if self.primary {
            (
                design.accent_soft,
                design.selected,
                design.accent.opacity(0.35),
                design.accent_text,
            )
        } else {
            (design.panel, design.field, design.border, design.text)
        };
        let keycap_color = if self.primary {
            design.accent_text
        } else {
            design.muted
        };

        div()
            .id(self.id)
            .flex()
            .flex_col()
            .flex_1()
            .min_w(design.px(ACTION_TILE_MIN_WIDTH))
            .justify_between()
            .gap(design.px(8.0))
            .px(design.px(10.0))
            .py(design.px(9.0))
            .rounded(design.px(8.0))
            .border_1()
            .border_color(border)
            .bg(bg)
            .cursor_pointer()
            .hover(move |style| style.bg(hover_bg))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_1()
                    .w_full()
                    .child(
                        Icon::new(self.icon)
                            .with_size(design.px(15.0))
                            .text_color(design.accent),
                    )
                    .when(!self.keycaps.is_empty(), |row| {
                        row.child(
                            div()
                                .text_color(keycap_color)
                                .child(super::badge::keycaps(self.keycaps, design)),
                        )
                    }),
            )
            .child(
                div()
                    .w_full()
                    .truncate()
                    .text_size(design.px(11.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(label_color)
                    .child(self.label),
            )
            .when_some(self.on_click, |tile, on_click| {
                tile.on_click(move |event, window, cx| on_click(event, window, cx))
            })
    }
}
