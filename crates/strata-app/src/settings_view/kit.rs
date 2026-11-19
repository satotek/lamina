//! The building blocks every settings page is made of: a page of groups, each
//! group a short heading over flat rows, each row a name on the left and its
//! control on the right.
//!
//! A row's explanation sits behind an info icon instead of under the name, so
//! a page reads as a list of settings rather than a wall of sentences. Hovering
//! the icon shows the text; clicking it pins the text under the row, which is
//! the path for anyone not using a mouse hover. Only a [`Item::note`] stays on
//! the row: the few cases where changing the value without reading it would
//! surprise (where a password is kept, which repositories a change reaches).
//!
//! Search reads the same text the info icons hold, so hiding an explanation
//! does not hide the setting from a query that only matches its explanation.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Entity, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, ParentElement, Pixels, Role, SharedString, StatefulInteractiveElement as _,
    Styled, Subscription, Window, div, px, relative,
};
use gpui_component::input::{InputEvent, InputState, NumberInput};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::switch::Switch;
use gpui_component::tooltip::Tooltip;
use gpui_component::{Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit_assets::IconName;

use super::change::Section;
use crate::style::Design;

type Getter<T> = Rc<dyn Fn(&App) -> T>;
type Setter<T> = Rc<dyn Fn(T, &mut App)>;
type RenderFn = Rc<dyn Fn(&ItemCx, &mut Window, &mut App) -> AnyElement>;

/// Where an item sits, so a control that keeps state between frames (a text
/// field, a select) can key it by position.
#[derive(Clone, Copy, Debug)]
pub(super) struct ItemCx {
    pub page: &'static str,
    pub group: usize,
    pub item: usize,
    /// Whether the row is disabled, for controls that draw themselves.
    pub disabled: bool,
}

impl ItemCx {
    /// A key unique to this item, for `window.use_keyed_state`.
    pub fn key(&self, prefix: &str) -> SharedString {
        SharedString::from(format!(
            "settings-{prefix}-{}-{}-{}",
            self.page, self.group, self.item
        ))
    }

    fn id(&self, part: &str) -> SharedString {
        self.key(part)
    }

    /// The size every control on a settings row uses.
    pub fn size(&self) -> gpui_component::Size {
        gpui_component::Size::Small
    }

    pub fn is_disabled(&self) -> bool {
        self.disabled
    }
}

/// A value range and step for [`Field::number_input`].
#[derive(Clone, Copy, Debug)]
pub(super) struct NumberOptions {
    pub min: f64,
    pub max: f64,
    pub step: f64,
}

enum FieldKind {
    Switch {
        get: Getter<bool>,
        set: Setter<bool>,
    },
    Dropdown {
        options: Vec<(SharedString, SharedString)>,
        get: Getter<SharedString>,
        set: Setter<SharedString>,
    },
    Segmented {
        options: Vec<(SharedString, SharedString)>,
        get: Getter<SharedString>,
        set: Setter<SharedString>,
    },
    Number {
        options: NumberOptions,
        get: Getter<f64>,
        set: Setter<f64>,
    },
    Custom(RenderFn),
}

/// The control on the right of a row.
pub(super) struct Field {
    kind: FieldKind,
    width: Option<Pixels>,
}

impl Field {
    pub fn switch(
        get: impl Fn(&App) -> bool + 'static,
        set: impl Fn(bool, &mut App) + 'static,
    ) -> Self {
        Self::new(FieldKind::Switch {
            get: Rc::new(get),
            set: Rc::new(set),
        })
    }

    /// A button that opens a menu of `options` (`(value, label)` pairs). For
    /// lists longer than a few entries or labels too long to sit side by side.
    pub fn dropdown(
        options: Vec<(SharedString, SharedString)>,
        get: impl Fn(&App) -> SharedString + 'static,
        set: impl Fn(SharedString, &mut App) + 'static,
    ) -> Self {
        Self::new(FieldKind::Dropdown {
            options,
            get: Rc::new(get),
            set: Rc::new(set),
        })
    }

    /// Joined buttons, one per option, for two to four short choices: every
    /// choice is visible without opening anything.
    pub fn segmented(
        options: Vec<(SharedString, SharedString)>,
        get: impl Fn(&App) -> SharedString + 'static,
        set: impl Fn(SharedString, &mut App) + 'static,
    ) -> Self {
        Self::new(FieldKind::Segmented {
            options,
            get: Rc::new(get),
            set: Rc::new(set),
        })
    }

    pub fn number_input(
        options: NumberOptions,
        get: impl Fn(&App) -> f64 + 'static,
        set: impl Fn(f64, &mut App) + 'static,
    ) -> Self {
        Self::new(FieldKind::Number {
            options,
            get: Rc::new(get),
            set: Rc::new(set),
        })
    }

    pub fn render<E: IntoElement>(
        render: impl Fn(&ItemCx, &mut Window, &mut App) -> E + 'static,
    ) -> Self {
        Self::new(FieldKind::Custom(Rc::new(move |cx, window, app| {
            render(cx, window, app).into_any_element()
        })))
    }

    /// Fixes the control's width, so controls down a page line up.
    pub fn w(mut self, width: Pixels) -> Self {
        self.width = Some(width);
        self
    }

    fn new(kind: FieldKind) -> Self {
        Self { kind, width: None }
    }

    fn render_field(
        &self,
        title: &SharedString,
        disabled: bool,
        item: &ItemCx,
        design: &Design,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let element = match &self.kind {
            FieldKind::Switch { get, set } => {
                let set = set.clone();
                Switch::new(item.id("switch"))
                    .accessibility_label(title.clone())
                    .checked(get(cx))
                    .disabled(disabled)
                    .on_click(move |checked: &bool, _, cx: &mut App| set(*checked, cx))
                    .into_any_element()
            }
            FieldKind::Dropdown { options, get, set } => {
                dropdown(item, title, options, get(cx), set.clone(), disabled)
            }
            FieldKind::Segmented { options, get, set } => {
                segmented(item, options, get(cx), set.clone(), disabled, design)
            }
            FieldKind::Number { options, get, set } => number(
                item,
                title,
                *options,
                get(cx),
                set.clone(),
                disabled,
                window,
                cx,
            ),
            FieldKind::Custom(render) => render(item, window, cx),
        };
        match self.width {
            Some(width) => div().w(width).flex_none().child(element).into_any_element(),
            None => element,
        }
    }
}

fn dropdown(
    item: &ItemCx,
    title: &SharedString,
    options: &[(SharedString, SharedString)],
    current: SharedString,
    set: Setter<SharedString>,
    disabled: bool,
) -> AnyElement {
    let label = options
        .iter()
        .find(|(value, _)| *value == current)
        .map_or_else(|| current.clone(), |(_, label)| label.clone());
    let options = options.to_vec();
    gpui_component::button::Button::new(item.id("dropdown"))
        .w_full()
        .accessibility_label(format!("{title}: {label}"))
        .label(label)
        .dropdown_caret(true)
        .outline()
        .small()
        .disabled(disabled)
        .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |menu, _, _| {
            options.iter().fold(menu, |menu, (value, label)| {
                let set = set.clone();
                let value = value.clone();
                menu.item(
                    PopupMenuItem::new(label.clone())
                        .checked(value == current)
                        .on_click(move |_, _, cx| set(value.clone(), cx)),
                )
            })
        })
        .into_any_element()
}

fn segmented(
    item: &ItemCx,
    options: &[(SharedString, SharedString)],
    current: SharedString,
    set: Setter<SharedString>,
    disabled: bool,
    design: &Design,
) -> AnyElement {
    let control = options.iter().enumerate().fold(
        crate::ui::segmented_control(item.id("segmented"), design),
        |control, (index, (value, label))| {
            let set = set.clone();
            let value = value.clone();
            let selected = value == current;
            control.segment(
                SharedString::from(format!("{}-{index}", item.id("segment"))),
                label.clone(),
                selected,
                move |_, _, cx| {
                    if !disabled {
                        set(value.clone(), cx);
                    }
                },
            )
        },
    );
    div()
        .when(disabled, |this| this.opacity(0.5))
        .child(control)
        .into_any_element()
}

/// One preview card's outer size, its padding, its border and the gap between
/// two cards, at 100% zoom. The card's content box is sized to leave room for
/// its padding and border, so the outer card keeps this size whatever the
/// theme draws inside it.
const PREVIEW_CARD_W: f32 = 140.0;
const PREVIEW_CARD_H: f32 = 60.0;
const PREVIEW_CARD_PADDING: f32 = 8.0;
const PREVIEW_CARD_BORDER: f32 = 1.0;
const PREVIEW_CARD_GAP: f32 = 12.0;

/// The width [`preview_cards`] draws for `count` cards with their gaps, for a
/// page deciding whether the cards fit before it offers them.
pub(super) fn preview_cards_width(count: usize, design: &Design) -> f32 {
    let gaps = count.saturating_sub(1) as f32;
    count as f32 * design.scale(PREVIEW_CARD_W) + gaps * design.scale(PREVIEW_CARD_GAP)
}

type PreviewFn = Rc<dyn Fn(&Design) -> AnyElement>;

/// One choice of a [`preview_cards`] row: the value a click writes, the label
/// under the card and the picture that tells the choices apart.
pub(super) struct PreviewCard {
    value: SharedString,
    label: SharedString,
    preview: PreviewFn,
}

impl PreviewCard {
    pub fn new(
        value: impl Into<SharedString>,
        label: impl Into<SharedString>,
        preview: impl Fn(&Design) -> AnyElement + 'static,
    ) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            preview: Rc::new(preview),
        }
    }
}

/// A choice picked by look rather than by name: one card per option, side by
/// side, each showing a small picture of what choosing it means, its label
/// under it and a check on the selected one. Dropdowns and segmented buttons
/// name a choice; these show it.
pub(super) fn preview_cards(
    item: &ItemCx,
    cards: Vec<PreviewCard>,
    current: SharedString,
    set: impl Fn(SharedString, &mut App) + 'static,
    design: &Design,
) -> AnyElement {
    let disabled = item.is_disabled();
    let inset = PREVIEW_CARD_PADDING + PREVIEW_CARD_BORDER;
    let set = Rc::new(set);
    let mut row = h_flex().gap(design.px(PREVIEW_CARD_GAP)).flex_none();
    for (index, card) in cards.into_iter().enumerate() {
        let selected = card.value == current;
        let value = card.value.clone();
        let on_click = set.clone();
        let frame = div()
            .id(SharedString::from(format!(
                "{}-{index}",
                item.id("preview")
            )))
            .cursor_pointer()
            .w(design.px(PREVIEW_CARD_W - 2.0 * inset))
            .h(design.px(PREVIEW_CARD_H - 2.0 * inset))
            .p(design.px(PREVIEW_CARD_PADDING))
            .rounded(design.px(6.0))
            .bg(design.background)
            .overflow_hidden()
            .border_1()
            .border_color(if selected {
                design.accent
            } else {
                design.border
            })
            .when(selected, |this| this.border_2())
            .when(!selected && !disabled, |this| {
                this.hover(|this| this.border_color(design.accent))
            })
            .child((card.preview)(design))
            .on_click(move |_, _, cx| {
                if !disabled {
                    on_click(value.clone(), cx);
                }
            });
        row = row.child(
            v_flex().flex_none().gap(design.px(4.0)).child(frame).child(
                h_flex()
                    .w(design.px(PREVIEW_CARD_W))
                    .justify_center()
                    .gap(design.px(4.0))
                    .when(selected, |this| {
                        this.child(
                            Icon::new(IconName::Check)
                                .size(design.px(12.0))
                                .text_color(design.accent),
                        )
                    })
                    .child(
                        div()
                            .text_size(px(design.tiny()))
                            .text_color(if selected { design.text } else { design.muted })
                            .child(card.label),
                    ),
            ),
        );
    }
    row.when(disabled, |this| this.opacity(0.5))
        .into_any_element()
}

struct NumberState {
    input: Entity<InputState>,
    value: f64,
    _subscription: Subscription,
}

#[allow(clippy::too_many_arguments)]
fn number(
    item: &ItemCx,
    title: &SharedString,
    options: NumberOptions,
    value: f64,
    set: Setter<f64>,
    disabled: bool,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let state = window.use_keyed_state(item.key("number"), cx, |window, cx| {
        // Stepping and bounds live on the input itself: `+`/`-` and Up/Down
        // step with its precision handling, and out-of-range text is allowed
        // while typing and clamped on blur.
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(value.to_string())
                .step(options.step)
                .min(options.min)
                .max(options.max)
        });
        let subscription = cx.subscribe_in(
            &input,
            window,
            move |state: &mut NumberState, input, event: &InputEvent, _, cx| {
                if !matches!(event, InputEvent::Change) {
                    return;
                }
                let text = input.read(cx).value();
                // Unparsable intermediates ("-", "", "1.") wait for the next
                // keystroke; the setting only ever receives a value in range.
                if let Ok(parsed) = text.parse::<f64>() {
                    let clamped = parsed.clamp(options.min, options.max);
                    if clamped != state.value {
                        state.value = clamped;
                        set(clamped, cx);
                    }
                }
            },
        );
        NumberState {
            input,
            value,
            _subscription: subscription,
        }
    });
    // A reset or another window can change the value under the field.
    state.update(cx, |state, cx| {
        if state.value != value {
            state.value = value;
            state.input.update(cx, |input, cx| {
                input.set_value(SharedString::from(value.to_string()), window, cx);
            });
        }
    });
    NumberInput::new(&state.read(cx).input)
        .accessibility_label(title.clone())
        .small()
        .disabled(disabled)
        .w_full()
        .into_any_element()
}

/// One choice of a [`card_picker`]: the look its card draws, and the value
/// choosing it writes back.
pub(super) struct CardOption {
    /// The value written back when this card is chosen.
    pub value: SharedString,
    /// The name shown under the card.
    pub label: SharedString,
    /// The card's fill, drawn from the palette the choice stands for rather
    /// than the active one.
    pub fill: Hsla,
    /// Horizontal bars over the fill, as `(colour, share of the card width)`.
    pub bars: Vec<(Hsla, f32)>,
    /// A small badge beside the name, for choices the user supplied.
    pub tag: Option<SharedString>,
}

/// One preview card at 100% zoom: 112 wide, and 60 tall with the standard
/// four bars, because the border, the padding and the bars sum to it. The
/// card sizes itself from that content rather than a fixed height, so it
/// never clips and every card in a row is as tall as its neighbours.
const CARD_W: f32 = 112.0;
const CARD_BAR_H: f32 = 8.0;
const CARD_BAR_GAP: f32 = 4.0;
const CARD_PAD: f32 = 6.0;

/// The gap between the cards of one row.
const CARD_GAP: f32 = 10.0;

/// A wrapping row of preview cards, for choices judged by look rather than
/// by name: every option is visible at once, each drawn in its own colours,
/// so telling two choices apart takes no round trip through the setting.
///
/// The selected card is outlined in the accent colour and its name carries a
/// check mark; every card also carries its name as an accessibility label,
/// so the picker can be read without sight.
pub(super) fn card_picker(
    item: &ItemCx,
    options: Vec<CardOption>,
    current: SharedString,
    set: impl Fn(SharedString, &mut App) + 'static,
    design: &Design,
) -> AnyElement {
    let disabled = item.is_disabled();
    let set: Setter<SharedString> = Rc::new(set);
    div()
        .flex()
        .flex_wrap()
        .items_start()
        .gap(design.px(CARD_GAP))
        .when(disabled, |this| this.opacity(0.5))
        .children(options.into_iter().map(|option| {
            let selected = option.value == current;
            let on_click = (!disabled).then(|| {
                let set = set.clone();
                let value = option.value.clone();
                move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| set(value.clone(), cx)
            });
            v_flex()
                .flex_none()
                .w(design.px(CARD_W))
                .gap(design.px(3.0))
                .child(
                    div()
                        // The element id is the choice's own value, so the
                        // node a screen reader announces stays the same node
                        // while the palette around it changes.
                        .id(item.key(&format!("card-{}", option.value)))
                        .role(Role::RadioButton)
                        .aria_label(option.label.clone())
                        .aria_selected(selected)
                        // The name under the card truncates at the card's
                        // width, so the full one is a hover away.
                        .tooltip({
                            let label = option.label.clone();
                            move |window, cx| Tooltip::new(label.clone()).build(window, cx)
                        })
                        .cursor_pointer()
                        .rounded(design.px(6.0))
                        // The border width never changes: the colour alone
                        // says which card is chosen, so choosing one does not
                        // move the row.
                        .border_2()
                        .border_color(if selected {
                            design.accent
                        } else {
                            design.border
                        })
                        .bg(option.fill)
                        .p(design.px(CARD_PAD))
                        .child(v_flex().gap(design.px(CARD_BAR_GAP)).children(
                            option.bars.iter().map(|(color, share)| {
                                div()
                                    .flex_none()
                                    .h(design.px(CARD_BAR_H))
                                    .w(relative(*share))
                                    .rounded(design.px(2.0))
                                    .bg(*color)
                            }),
                        ))
                        .when_some(on_click, |this, on_click| this.on_click(on_click)),
                )
                .child(
                    h_flex()
                        .items_center()
                        .gap(design.px(4.0))
                        .when(selected, |this| {
                            this.text_color(design.text)
                                .child(Icon::new(IconName::Check).size(design.px(12.0)))
                        })
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(design.tiny()))
                                .text_color(if selected { design.text } else { design.muted })
                                .child(option.label.clone()),
                        )
                        .when_some(option.tag, |this, tag| {
                            this.child(crate::ui::status_badge(
                                tag,
                                crate::ui::BadgeTone::Neutral,
                                design,
                            ))
                        }),
                )
                .into_any_element()
        }))
        .into_any_element()
}

enum ItemKind {
    Row {
        title: SharedString,
        info: Option<SharedString>,
        note: Option<SharedString>,
        field: Field,
        disabled: bool,
    },
    Custom(RenderFn),
}

/// One entry of a group: a named row with a control, or a free-form element
/// that takes the group's full width.
pub(super) struct Item {
    kind: ItemKind,
    keywords: Vec<SharedString>,
}

impl Item {
    pub fn new(title: impl Into<SharedString>, field: Field) -> Self {
        Self {
            kind: ItemKind::Row {
                title: title.into(),
                info: None,
                note: None,
                field,
                disabled: false,
            },
            keywords: Vec::new(),
        }
    }

    /// A free-form element. It is hidden while searching unless one of its
    /// [`Item::keywords`] matches.
    pub fn render<E: IntoElement>(
        render: impl Fn(&ItemCx, &mut Window, &mut App) -> E + 'static,
    ) -> Self {
        Self {
            kind: ItemKind::Custom(Rc::new(move |cx, window, app| {
                render(cx, window, app).into_any_element()
            })),
            keywords: Vec::new(),
        }
    }

    /// The explanation behind the row's info icon.
    pub fn description(mut self, text: impl Into<SharedString>) -> Self {
        if let ItemKind::Row { info, .. } = &mut self.kind {
            *info = Some(text.into());
        }
        self
    }

    /// A line that stays visible under the row's name. Keep it for what the
    /// user must know before changing the value.
    pub fn note(mut self, text: impl Into<SharedString>) -> Self {
        if let ItemKind::Row { note, .. } = &mut self.kind {
            *note = Some(text.into());
        }
        self
    }

    pub fn disabled(mut self, value: bool) -> Self {
        if let ItemKind::Row { disabled, .. } = &mut self.kind {
            *disabled = value;
        }
        self
    }

    pub fn keywords<I, S>(mut self, words: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<SharedString>,
    {
        self.keywords.extend(words.into_iter().map(Into::into));
        self
    }

    fn matches(&self, query: &str) -> bool {
        let hit = |text: &str| text.to_lowercase().contains(query);
        let keyword = self.keywords.iter().any(|word| hit(word));
        match &self.kind {
            ItemKind::Row {
                title, info, note, ..
            } => {
                keyword
                    || hit(title)
                    || info.as_deref().is_some_and(hit)
                    || note.as_deref().is_some_and(hit)
            }
            ItemKind::Custom(_) => keyword,
        }
    }

    fn render_item(
        &self,
        item: ItemCx,
        last: bool,
        design: &Design,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let body = match &self.kind {
            ItemKind::Custom(render) => render(&item, window, cx),
            ItemKind::Row {
                title,
                info,
                note,
                field,
                disabled,
            } => {
                let pinned = info
                    .as_ref()
                    .map(|_| {
                        *window
                            .use_keyed_state(item.key("info-pinned"), cx, |_, _| false)
                            .read(cx)
                    })
                    .unwrap_or(false);
                let field_cx = ItemCx {
                    disabled: *disabled,
                    ..item
                };
                let control = field.render_field(title, *disabled, &field_cx, design, window, cx);
                let icon = info
                    .clone()
                    .map(|text| info_icon(&item, text, design, window, cx));
                v_flex()
                    .w_full()
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .gap(design.px(16.0))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        h_flex()
                                            .gap(design.px(6.0))
                                            .items_center()
                                            .when(*disabled, |this| this.opacity(0.5))
                                            .child(title.clone())
                                            .children(icon),
                                    )
                                    .when_some(note.clone(), |this, note| {
                                        this.child(
                                            div()
                                                .text_size(px(design.small()))
                                                .text_color(design.muted)
                                                .child(note),
                                        )
                                    }),
                            )
                            .child(div().flex_none().child(control)),
                    )
                    .when(pinned, |this| {
                        this.when_some(info.clone(), |this, text| {
                            this.child(
                                div()
                                    .pt(design.px(6.0))
                                    .text_size(px(design.small()))
                                    .text_color(design.muted)
                                    .child(text),
                            )
                        })
                    })
                    .into_any_element()
            }
        };
        div()
            .w_full()
            .py(design.px(9.0))
            .when(!last, |this| this.border_b_1().border_color(design.border))
            .child(body)
            .into_any_element()
    }
}

/// The info icon: hover shows the explanation, a click pins it under the row.
fn info_icon(
    item: &ItemCx,
    text: SharedString,
    design: &Design,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let pinned = window.use_keyed_state(item.key("info-pinned"), cx, |_, _| false);
    let tooltip = text.clone();
    div()
        .id(item.id("info"))
        .flex_none()
        .cursor_pointer()
        .text_color(design.muted)
        .hover(|this| this.text_color(design.text))
        .child(Icon::new(IconName::Info).size(design.px(14.0)))
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .on_click(move |_, _, cx| {
            pinned.update(cx, |pinned, cx| {
                *pinned = !*pinned;
                cx.notify();
            });
        })
        .into_any_element()
}

/// A short muted heading inside a group, above rows that are not
/// [`Item::new`] rows — a wrapping card row has no title of its own to name
/// it. Its explanation sits behind the same info icon as a row title's, so
/// the heading keeps the group's one-thing-per-line reading.
pub(super) fn sub_heading(
    item: &ItemCx,
    title: SharedString,
    info: Option<SharedString>,
    design: &Design,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let pinned = info
        .as_ref()
        .map(|_| {
            *window
                .use_keyed_state(item.key("info-pinned"), cx, |_, _| false)
                .read(cx)
        })
        .unwrap_or(false);
    v_flex()
        .w_full()
        .child(
            h_flex()
                .gap(design.px(6.0))
                .items_center()
                .text_size(px(design.tiny()))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(design.muted)
                .child(title)
                .when_some(info.clone(), |this, text| {
                    this.child(info_icon(item, text, design, window, cx))
                }),
        )
        .when(pinned, |this| {
            this.when_some(info, |this, text| {
                this.child(
                    div()
                        .pt(design.px(2.0))
                        .text_size(px(design.small()))
                        .text_color(design.muted)
                        .child(text),
                )
            })
        })
        .into_any_element()
}

/// A heading over flat rows.
pub(super) struct Group {
    title: Option<SharedString>,
    info: Option<SharedString>,
    note: Option<SharedString>,
    items: Vec<Item>,
}

impl Group {
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: Some(title.into()),
            info: None,
            note: None,
            items: Vec::new(),
        }
    }

    pub fn item(mut self, item: Item) -> Self {
        self.items.push(item);
        self
    }

    pub fn items(mut self, items: impl IntoIterator<Item = Item>) -> Self {
        self.items.extend(items);
        self
    }

    /// The explanation behind the heading's info icon.
    pub fn description(mut self, text: impl Into<SharedString>) -> Self {
        self.info = Some(text.into());
        self
    }

    /// A line that stays visible under the heading.
    pub fn note(mut self, text: impl Into<SharedString>) -> Self {
        self.note = Some(text.into());
        self
    }

    fn render_group(
        &self,
        page: &'static str,
        group: usize,
        query: Option<&str>,
        design: &Design,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let visible: Vec<(usize, &Item)> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| query.is_none_or(|query| item.matches(query)))
            .collect();
        if visible.is_empty() {
            return None;
        }
        let count = visible.len();
        let rows: Vec<AnyElement> = visible
            .into_iter()
            .enumerate()
            .map(|(position, (index, item))| {
                item.render_item(
                    ItemCx {
                        page,
                        group,
                        item: index,
                        disabled: false,
                    },
                    position + 1 == count,
                    design,
                    window,
                    cx,
                )
            })
            .collect();
        let heading_cx = ItemCx {
            page,
            group,
            item: usize::MAX,
            disabled: false,
        };
        Some(
            v_flex()
                .w_full()
                .when_some(self.title.clone(), |this, title| {
                    this.child(
                        h_flex()
                            .gap(design.px(6.0))
                            .items_center()
                            .text_size(px(design.tiny()))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(design.muted)
                            .child(title)
                            .when_some(self.info.clone(), |this, text| {
                                this.child(info_icon(&heading_cx, text, design, window, cx))
                            }),
                    )
                })
                .when_some(self.note.clone(), |this, note| {
                    this.child(
                        div()
                            .pt(design.px(2.0))
                            .text_size(px(design.small()))
                            .text_color(design.muted)
                            .child(note),
                    )
                })
                .when(
                    self.info.is_some()
                        && *window
                            .use_keyed_state(heading_cx.key("info-pinned"), cx, |_, _| false)
                            .read(cx),
                    |this| {
                        this.when_some(self.info.clone(), |this, text| {
                            this.child(
                                div()
                                    .pt(design.px(2.0))
                                    .text_size(px(design.small()))
                                    .text_color(design.muted)
                                    .child(text),
                            )
                        })
                    },
                )
                .child(v_flex().w_full().pt(design.px(2.0)).children(rows))
                .into_any_element(),
        )
    }
}

impl IntoIterator for Group {
    type Item = Group;
    type IntoIter = std::iter::Once<Group>;

    fn into_iter(self) -> Self::IntoIter {
        std::iter::once(self)
    }
}

/// One settings page: the section it edits and its groups.
pub(super) struct Page {
    pub section: Section,
    info: Option<SharedString>,
    resettable: bool,
    groups: Vec<Group>,
}

impl Page {
    pub fn new(section: Section) -> Self {
        Self {
            section,
            info: None,
            resettable: true,
            groups: Vec::new(),
        }
    }

    /// The page's explanation, behind the info icon beside its title.
    pub fn description(mut self, text: impl Into<SharedString>) -> Self {
        self.info = Some(text.into());
        self
    }

    /// Whether the title row offers to reset the section. A page that edits a
    /// file of its own has nothing to restore.
    pub fn resettable(mut self, value: bool) -> Self {
        self.resettable = value;
        self
    }

    pub fn group(mut self, group: Group) -> Self {
        self.groups.push(group);
        self
    }

    pub fn groups(mut self, groups: impl IntoIterator<Item = Group>) -> Self {
        self.groups.extend(groups);
        self
    }

    pub fn is_resettable(&self) -> bool {
        self.resettable
    }

    /// The page's groups, or only the rows matching `query` (lower-case).
    /// `None` when a query leaves nothing on the page.
    pub fn render_body(
        &self,
        query: Option<&str>,
        design: &Design,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let page = self.section.id();
        let groups: Vec<AnyElement> = self
            .groups
            .iter()
            .enumerate()
            .filter_map(|(index, group)| group.render_group(page, index, query, design, window, cx))
            .collect();
        if groups.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .w_full()
                .gap(design.px(24.0))
                .children(groups)
                .into_any_element(),
        )
    }

    /// The page's explanation as an info icon, for the title row.
    pub fn info_icon(
        &self,
        design: &Design,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let item = ItemCx {
            page: self.section.id(),
            group: usize::MAX,
            item: usize::MAX,
            disabled: false,
        };
        self.info
            .clone()
            .map(|text| info_icon(&item, text, design, window, cx))
    }
}
