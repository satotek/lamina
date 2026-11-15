//! The small labels that sit in rows and headers: change-kind badges, ref
//! badges, status badges and count pills.
//!
//! They all paint from [`Design`](crate::style::Design) and share one type size,
//! so a `M` badge and a `main` badge read as the same kind of thing.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Div, ElementId, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    OperatingSystem, ParentElement as _, RenderOnce, SharedString, Stateful, Styled as _, Window,
    div, px,
};
use gpui_component::Selectable;
use gpui_component::tooltip::Tooltip;
use strata_core::forge::ChecksState;
use strata_core::{ChangeKind, RefKind};

use super::row::RowTipExt as _;
use crate::style::{self, Design};

pub const REF_BADGE_HEIGHT: f32 = 18.0;
pub const REF_BADGE_PADDING_X: f32 = 7.0;
pub const REF_BADGE_GAP: f32 = 4.0;
pub const REF_BADGE_GLYPH: f32 = 11.0;
pub const REF_BADGE_FONT_SIZE: f32 = 11.0;
pub const REF_BADGE_MAX_LABEL_WIDTH: f32 = 130.0;
const REF_OVERFLOW_PADDING_X: f32 = 6.0;

/// Approximate width of a [`ref_badge`] / [`ref_chip`] rendered with `label`,
/// matching the badge's padding, icon, gap, and clamped label width.
pub fn estimate_ref_badge_width(label: &str, max_label_width: f32) -> f32 {
    let chrome = REF_BADGE_PADDING_X * 2.0 + REF_BADGE_GLYPH + REF_BADGE_GAP;
    let label_width = estimate_label_width(label).min(max_label_width.max(0.0));
    chrome + label_width
}

/// Approximate width of the `+N` overflow chip for `hidden_count` refs.
pub fn estimate_ref_overflow_width(hidden_count: usize) -> f32 {
    let label = format!("+{hidden_count}");
    REF_OVERFLOW_PADDING_X * 2.0 + estimate_label_width(&label) + 2.0
}

fn estimate_label_width(label: &str) -> f32 {
    label
        .chars()
        .map(|ch| {
            if ch.is_ascii() {
                match ch {
                    'i' | 'l' | 'j' | 't' | 'f' | 'r' | '.' | ',' | ':' | ';' | '!' | '|' | '/'
                    | '-' | '\'' => 4.2,
                    'm' | 'w' | 'M' | 'W' => 9.5,
                    'A'..='Z' => 7.8,
                    _ => 6.6,
                }
            } else {
                // Wide / CJK glyphs render roughly square at the 11 px font size.
                11.0
            }
        })
        .sum()
}

/// The 18 px letter for a change kind: `A`, `M`, `D`, `R`, `C`, `T` or `!`.
pub fn change_badge(kind: ChangeKind, design: &Design) -> AnyElement {
    let (background, foreground) = style::kind_colors(kind, design);
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(18.0))
        .h(px(18.0))
        .rounded(px(4.0))
        .bg(background)
        .text_size(px(10.0))
        .font_weight(FontWeight::BOLD)
        .text_color(foreground)
        .child(style::kind_marker(kind))
        .into_any_element()
}

fn ref_badge_body(
    kind: RefKind,
    head: bool,
    label: SharedString,
    max_label_width: f32,
    design: &Design,
) -> Div {
    let (background, foreground, glyph_data) = match kind {
        RefKind::LocalBranch if head => (design.accent, design.on_accent, style::glyph::BRANCH),
        RefKind::LocalBranch => (design.accent_soft, design.accent_text, style::glyph::BRANCH),
        RefKind::RemoteBranch => (
            design.remote_background,
            design.remote_text,
            style::glyph::CLOUD,
        ),
        RefKind::Tag => (design.tag_background, design.tag_text, style::glyph::TAG),
    };

    div()
        .flex()
        .items_center()
        .gap(px(REF_BADGE_GAP))
        .flex_none()
        .h(px(REF_BADGE_HEIGHT))
        .px(px(REF_BADGE_PADDING_X))
        .rounded_full()
        .bg(background)
        .text_size(px(REF_BADGE_FONT_SIZE))
        .font_weight(if head || kind == RefKind::Tag {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::MEDIUM
        })
        .text_color(foreground)
        .child(style::glyph_icon(glyph_data, REF_BADGE_GLYPH, foreground))
        .child(div().max_w(px(max_label_width)).truncate().child(label))
}

/// A branch, remote branch or tag chip. The branch `HEAD` is on is filled with
/// the accent so it stands out from its siblings.
pub fn ref_badge(
    kind: RefKind,
    head: bool,
    label: impl Into<SharedString>,
    design: &Design,
) -> AnyElement {
    ref_badge_body(kind, head, label.into(), REF_BADGE_MAX_LABEL_WIDTH, design).into_any_element()
}

/// An identifiable, truncated ref chip so callers can attach tooltips or drop
/// targets without repeating the badge styling.
pub fn ref_chip(
    id: impl Into<ElementId>,
    kind: RefKind,
    head: bool,
    label: impl Into<SharedString>,
    max_label_width: f32,
    design: &Design,
) -> Stateful<Div> {
    ref_badge_body(
        kind,
        head,
        label.into(),
        max_label_width.clamp(24.0, REF_BADGE_MAX_LABEL_WIDTH),
        design,
    )
    .id(id)
}

/// Compact `+N` chip for overflowing commit refs, usable as a `Popover` trigger.
#[derive(IntoElement)]
pub struct RefOverflowChip {
    id: ElementId,
    count: usize,
    design: Design,
    tooltip: Option<SharedString>,
    open: bool,
    selected: bool,
}

pub fn ref_overflow_chip(
    id: impl Into<ElementId>,
    count: usize,
    design: &Design,
) -> RefOverflowChip {
    RefOverflowChip {
        id: id.into(),
        count,
        design: design.clone(),
        tooltip: None,
        open: false,
        selected: false,
    }
}

impl RefOverflowChip {
    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }
}

impl Selectable for RefOverflowChip {
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

impl RenderOnce for RefOverflowChip {
    fn render(self, _window: &mut Window, _cx: &mut gpui::App) -> impl IntoElement {
        let design = &self.design;
        let bg = if self.open || self.selected {
            design.selected
        } else {
            design.field
        };
        let fg = if self.open || self.selected {
            design.text
        } else {
            design.muted
        };
        div()
            .id(self.id)
            .flex()
            .items_center()
            .justify_center()
            .flex_none()
            .h(px(REF_BADGE_HEIGHT))
            .px(px(REF_OVERFLOW_PADDING_X))
            .rounded_full()
            .border_1()
            .border_color(design.border)
            .bg(bg)
            .hover(|style| style.bg(design.selected).text_color(design.text))
            .cursor_pointer()
            .text_size(px(REF_BADGE_FONT_SIZE))
            .font_weight(FontWeight::MEDIUM)
            .text_color(fg)
            .child(format!("+{}", self.count))
            .when_some(self.tooltip, |chip, tooltip| {
                chip.row_tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            })
    }
}

/// How strongly a status badge reads.
///
/// A status badge names a state rather than a ref: a submodule that needs
/// initialising, a worktree that is locked or missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadgeTone {
    /// Plain information, such as a state that needs no action.
    Neutral,
    /// A state worth noticing, in the app's accent.
    Accent,
    /// Something that usually needs the user, in the tag colours.
    Warning,
    /// A problem, in the app's danger colour.
    Danger,
}

/// A short state word, as a chip. Unlike [`ref_badge`] it carries no glyph: the
/// word is the whole message.
pub fn status_badge(
    label: impl Into<SharedString>,
    tone: BadgeTone,
    design: &Design,
) -> AnyElement {
    let (background, foreground) = match tone {
        BadgeTone::Neutral => (design.field, design.muted),
        BadgeTone::Accent => (design.accent_soft, design.accent_text),
        BadgeTone::Warning => (design.tag_background, design.tag_text),
        BadgeTone::Danger => (design.remove_background, design.remove_text),
    };
    div()
        .flex()
        .flex_none()
        .items_center()
        .h(px(16.0))
        .px(px(6.0))
        .rounded_full()
        .bg(background)
        .text_size(px(design.tiny()))
        .font_weight(FontWeight::MEDIUM)
        .text_color(foreground)
        .child(label.into())
        .into_any_element()
}

/// A small dot that says what a pull request's checks are doing.
///
/// `None` returns nothing: a pull request with no checks shows only its
/// number badge, the way a branch with no upstream shows only its name.
pub fn check_dot(state: ChecksState, design: &Design) -> Option<AnyElement> {
    let color = match state {
        ChecksState::Passing => design.add_text,
        ChecksState::Failing => design.remove_text,
        ChecksState::Pending => design.tag_text,
        ChecksState::None => return None,
    };
    Some(
        div()
            .flex_none()
            .w(px(6.0))
            .h(px(6.0))
            .rounded_full()
            .bg(color)
            .into_any_element(),
    )
}

/// A small count, for a header or a title-bar badge.
pub fn count_pill(count: impl ToString, design: &Design) -> AnyElement {
    div()
        .flex_none()
        .min_w(px(14.0))
        .h(px(14.0))
        .px(px(3.0))
        .rounded_full()
        .bg(design.accent)
        .text_color(design.on_accent)
        .text_size(px(9.0))
        .font_weight(FontWeight::SEMIBOLD)
        .flex()
        .items_center()
        .justify_center()
        .child(count.to_string())
        .into_any_element()
}

/// A subtle muted pill showing how many files a commit changed.
pub fn changed_file_count_badge(
    id: impl Into<ElementId>,
    count: usize,
    design: &Design,
) -> AnyElement {
    let tooltip = SharedString::from(if count == 1 {
        t!("history.changed_files.one", count = count).to_string()
    } else {
        t!("history.changed_files.other", count = count).to_string()
    });
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .min_w(px(18.0))
        .h(px(16.0))
        .px(px(5.0))
        .rounded_full()
        .bg(design.field)
        .text_size(px(design.tiny()))
        .font_weight(FontWeight::MEDIUM)
        .text_color(design.muted)
        .child(count.to_string())
        .row_tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .into_any_element()
}

/// A round initials avatar, coloured by the author's email.
pub fn avatar(text: impl Into<SharedString>, color: Hsla, size: f32) -> AnyElement {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(size))
        .h(px(size))
        .rounded_full()
        .bg(color)
        .text_color(gpui::rgb(0xffffff))
        .text_size(px(size * 0.5))
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.into())
        .into_any_element()
}

/// A keyboard hint chip, such as `Ctrl F`.
pub fn kbd(label: impl Into<SharedString>, design: &Design) -> AnyElement {
    div()
        .flex_none()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(design.border)
        .text_size(px(11.0))
        .child(label.into())
        .into_any_element()
}

/// One shortcut as a row of caps: modifiers first, the key last.
///
/// A cap per key is what makes a shortcut readable at a glance, and it is the
/// only way the macOS symbols (`⌘`, `⌥`, `⇧`, `⌃`) stay distinguishable; on the
/// other platforms the modifiers are words.
pub fn keycaps(labels: Vec<SharedString>, design: &Design) -> AnyElement {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(3.0))
        .children(labels.into_iter().map(|label| kbd(label, design)))
        .into_any_element()
}

/// The caps of one keystroke, in reading order.
pub fn shortcut_caps(keystroke: &gpui::Keystroke, os: OperatingSystem) -> Vec<String> {
    let modifiers = keystroke.modifiers;
    let mut caps = Vec::with_capacity(5);
    if modifiers.function {
        caps.push("fn".to_owned());
    }
    if modifiers.control {
        caps.push(symbol_or_word("⌃", "Ctrl", os));
    }
    if modifiers.alt {
        caps.push(symbol_or_word("⌥", "Alt", os));
    }
    if modifiers.shift {
        caps.push(symbol_or_word("⇧", "Shift", os));
    }
    if modifiers.platform {
        caps.push(symbol_or_word("⌘", "Super", os));
    }
    caps.push(key_cap(&keystroke.key, os));
    caps
}

fn symbol_or_word(symbol: &str, word: &str, os: OperatingSystem) -> String {
    if os == OperatingSystem::Mac {
        symbol.to_owned()
    } else {
        word.to_owned()
    }
}

/// What one key is called on a cap: symbols for the named keys on macOS, the
/// familiar short names everywhere.
fn key_cap(key: &str, os: OperatingSystem) -> String {
    let mac = os == OperatingSystem::Mac;
    match key {
        "escape" => if mac { "⎋" } else { "Esc" }.to_owned(),
        "tab" => if mac { "⇥" } else { "Tab" }.to_owned(),
        "enter" => if mac { "↩" } else { "Enter" }.to_owned(),
        "backspace" => if mac { "⌫" } else { "Backspace" }.to_owned(),
        "delete" => if mac { "⌦" } else { "Delete" }.to_owned(),
        "up" => if mac { "↑" } else { "Up" }.to_owned(),
        "down" => if mac { "↓" } else { "Down" }.to_owned(),
        "left" => if mac { "←" } else { "Left" }.to_owned(),
        "right" => if mac { "→" } else { "Right" }.to_owned(),
        "space" => "Space".to_owned(),
        "pageup" => "PgUp".to_owned(),
        "pagedown" => "PgDn".to_owned(),
        "home" => if mac { "↖" } else { "Home" }.to_owned(),
        "end" => if mac { "↘" } else { "End" }.to_owned(),
        key if key.len() == 1 => key.to_ascii_uppercase(),
        key if key.len() > 1
            && key.starts_with('f')
            && key[1..].chars().all(|c| c.is_ascii_digit()) =>
        {
            key.to_ascii_uppercase()
        }
        other => other.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Keystroke, OperatingSystem};

    fn caps(key: &str, os: OperatingSystem) -> Vec<String> {
        let keystroke = Keystroke::parse_with_os(key, os).expect("a keystroke");
        shortcut_caps(&keystroke, os)
    }

    #[test]
    fn a_shortcut_is_split_into_one_cap_per_key() {
        // Apple's modifier order in menus is Control, Option, Shift, Command,
        // so the symbols follow it rather than the order the keys were typed.
        assert_eq!(
            caps("secondary-shift-p", OperatingSystem::Mac),
            ["⇧", "⌘", "P"]
        );
        assert_eq!(
            caps("secondary-shift-p", OperatingSystem::Linux),
            ["Ctrl", "Shift", "P"]
        );
        assert_eq!(caps("secondary-f", OperatingSystem::Mac), ["⌘", "F"]);
        assert_eq!(caps("escape", OperatingSystem::Mac), ["⎋"]);
        assert_eq!(caps("escape", OperatingSystem::Windows), ["Esc"]);
        assert_eq!(caps("alt-1", OperatingSystem::Linux), ["Alt", "1"]);
        assert_eq!(caps("f12", OperatingSystem::Linux), ["F12"]);
        assert_eq!(caps("ctrl-`", OperatingSystem::Linux), ["Ctrl", "`"]);
    }
}
