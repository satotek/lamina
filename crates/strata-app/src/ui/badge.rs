//! The small labels that sit in rows and headers: change-kind badges, ref
//! badges, status badges and count pills.
//!
//! They all paint from [`Design`](crate::style::Design) and share one type size,
//! so a `M` badge and a `main` badge read as the same kind of thing.

use gpui::{
    AnyElement, FontWeight, Hsla, IntoElement, ParentElement as _, SharedString, Styled as _, div,
    px,
};
use strata_core::{ChangeKind, RefKind};

use crate::style::{self, Design};

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

/// A branch, remote branch or tag chip. The branch `HEAD` is on is filled with
/// the accent so it stands out from its siblings.
pub fn ref_badge(
    kind: RefKind,
    head: bool,
    label: impl Into<SharedString>,
    design: &Design,
) -> AnyElement {
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
        .gap(px(4.0))
        .flex_none()
        .h(px(18.0))
        .px(px(7.0))
        .rounded_full()
        .bg(background)
        .text_size(px(11.0))
        .font_weight(if head || kind == RefKind::Tag {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::MEDIUM
        })
        .text_color(foreground)
        .child(style::glyph_icon(glyph_data, 11.0, foreground))
        .child(div().max_w(px(130.0)).truncate().child(label.into()))
        .into_any_element()
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
