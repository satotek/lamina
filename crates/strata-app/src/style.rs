//! The visual language shared by every pane.
//!
//! One palette, taken from the design's light and dark tables, plus the small
//! helpers that colour avatars, badges and dates the same way everywhere. Each
//! module still defines the subset of colours it paints with, but the values
//! come from here so a change lands in one place.

use gpui::{
    AnyElement, App, Hsla, IntoElement, ParentElement as _, SharedString, Styled as _, div, px, rgb,
};
use gpui::{Bounds, canvas, fill, point};
use gpui_component::{Colorize as _, Icon, Sizable as _, Theme};
use strata_core::ChangeKind;
use strata_core::highlight::HighlightKind;

use crate::data::format_time;
use crate::settings::{DateFormat, Settings};

/// Lane colours, in the order the graph assigns them.
pub const LANES: usize = 8;
/// Avatar colours; the index comes from the author's email so it never moves.
const AVATARS: usize = 6;
/// The range the settings page offers for the code font. Every pane clamps to
/// it, so a hand-edited file cannot make one view's text a different size.
pub const CODE_FONT_MIN: f32 = 9.0;
pub const CODE_FONT_MAX: f32 = 24.0;

/// Colours and type sizes for one frame, resolved from the theme and settings.
///
/// Clone is cheap enough to hand a copy to each list closure, which is how the
/// palette reaches code that has no `App` of its own.
#[derive(Clone)]
pub struct Design {
    pub background: Hsla,
    pub chrome: Hsla,
    pub sidebar: Hsla,
    pub panel: Hsla,
    pub field: Hsla,
    pub border: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    /// Background of a selected nav entry or chip.
    pub selected: Hsla,
    /// Background of the selected commit or file row.
    pub row_selected: Hsla,
    pub accent: Hsla,
    pub accent_soft: Hsla,
    pub accent_text: Hsla,
    /// Text and icons drawn on top of `accent`.
    pub on_accent: Hsla,
    pub add_background: Hsla,
    pub add_text: Hsla,
    pub remove_background: Hsla,
    pub remove_text: Hsla,
    pub remote_background: Hsla,
    pub remote_text: Hsla,
    pub tag_background: Hsla,
    pub tag_text: Hsla,
    /// The body tint behind an added or removed diff line.
    pub diff_added_bg: Hsla,
    pub diff_removed_bg: Hsla,
    /// The stronger tint behind the changed words inside those lines.
    pub diff_added_word: Hsla,
    pub diff_removed_word: Hsla,
    /// The band behind a hunk header.
    pub diff_hunk_header: Hsla,
    /// Line numbers and the `+`/`-` marker column of a diff.
    pub diff_gutter_fg: Hsla,
    /// The `·` and `→` markers that reveal whitespace.
    pub diff_whitespace: Hsla,
    /// Syntax colours; see [`Design::syntax`].
    pub syntax_keyword: Hsla,
    pub syntax_string: Hsla,
    pub syntax_comment: Hsla,
    pub syntax_function: Hsla,
    pub syntax_type: Hsla,
    pub syntax_number: Hsla,
    pub syntax_tag: Hsla,
    /// UI font family, when the user picked one.
    pub ui_font: Option<SharedString>,
    /// `history.date_format`.
    pub date_format: DateFormat,
    /// `history.show_remote_branches`; hides matching badges.
    pub show_remote_branches: bool,
    /// `history.show_tags`; hides matching badges.
    pub show_tags: bool,
    /// `history.show_stashes`; hides the sidebar section.
    pub show_stashes: bool,
    /// `behaviour.summary_length_hint`, the commit box's counter target.
    pub summary_hint: u32,
    /// UI text size, from `appearance.ui_font_size`.
    pub ui: f32,
    /// Monospace size, from `appearance.code_font_size`.
    pub code: f32,
    pub mono: SharedString,
    pub lane: [Hsla; LANES],
    avatar: [Hsla; AVATARS],
}

impl Design {
    pub fn new(theme: &Theme, settings: &Settings) -> Self {
        let palette = if theme.is_dark() {
            Palette::dark()
        } else {
            Palette::light()
        };
        Self::from_palette(palette, settings, theme.mono_font_family.clone())
    }

    /// Fills the settings-dependent fields around one appearance's colours.
    fn from_palette(palette: Palette, settings: &Settings, fallback_mono: SharedString) -> Self {
        Self {
            background: palette.background,
            chrome: palette.chrome,
            sidebar: palette.sidebar,
            panel: palette.panel,
            field: palette.field,
            border: palette.border,
            text: palette.text,
            muted: palette.muted,
            selected: palette.selected,
            row_selected: palette.row_selected,
            accent: palette.accent,
            accent_soft: palette.accent_soft,
            accent_text: palette.accent_text,
            on_accent: hex(0xffffff),
            add_background: palette.add_background,
            add_text: palette.add_text,
            remove_background: palette.remove_background,
            remove_text: palette.remove_text,
            remote_background: palette.remote_background,
            remote_text: palette.remote_text,
            tag_background: palette.tag_background,
            tag_text: palette.tag_text,
            diff_added_bg: palette.diff_added_bg,
            diff_removed_bg: palette.diff_removed_bg,
            diff_added_word: palette.diff_added_word,
            diff_removed_word: palette.diff_removed_word,
            diff_hunk_header: palette.diff_hunk_header,
            diff_gutter_fg: palette.diff_gutter_fg,
            diff_whitespace: palette.diff_whitespace,
            syntax_keyword: palette.syntax_keyword,
            syntax_string: palette.syntax_string,
            syntax_comment: palette.syntax_comment,
            syntax_function: palette.syntax_function,
            syntax_type: palette.syntax_type,
            syntax_number: palette.syntax_number,
            syntax_tag: palette.syntax_tag,
            ui_font: settings
                .appearance
                .ui_font
                .as_deref()
                .map(SharedString::from),
            date_format: settings.history.date_format,
            show_remote_branches: settings.history.show_remote_branches,
            show_tags: settings.history.show_tags,
            show_stashes: settings.history.show_stashes,
            summary_hint: settings.behaviour.summary_length_hint,
            ui: settings.appearance.ui_font_size,
            code: settings
                .appearance
                .code_font_size
                .clamp(CODE_FONT_MIN, CODE_FONT_MAX),
            mono: settings
                .appearance
                .code_font
                .as_deref()
                .map_or(fallback_mono, SharedString::from),
            lane: palette.lane,
            avatar: palette.avatar,
        }
    }

    /// Secondary text, a little under the UI size.
    pub fn small(&self) -> f32 {
        (self.ui - 1.5).max(9.0)
    }

    /// Captions, badges and status details.
    pub fn tiny(&self) -> f32 {
        (self.ui - 2.5).max(8.0)
    }

    /// The colour of one syntax class.
    ///
    /// Classes with no colour of their own borrow the body text or the muted
    /// colour, so a grammar with a coarse query still looks deliberate.
    pub fn syntax(&self, kind: HighlightKind) -> Hsla {
        match kind {
            HighlightKind::Keyword | HighlightKind::Attribute => self.syntax_keyword,
            HighlightKind::String => self.syntax_string,
            HighlightKind::Comment => self.syntax_comment,
            HighlightKind::Function | HighlightKind::Property | HighlightKind::Label => {
                self.syntax_function
            }
            HighlightKind::Type | HighlightKind::Constant => self.syntax_type,
            HighlightKind::Number | HighlightKind::Embedded => self.syntax_number,
            HighlightKind::Tag | HighlightKind::Escape => self.syntax_tag,
            HighlightKind::Variable => self.text,
            HighlightKind::Operator | HighlightKind::Punctuation => self.muted,
        }
    }

    /// The colour of one author's avatar, stable for the same email.
    pub fn avatar_color(&self, email: &str) -> Hsla {
        self.avatar[avatar_index(email)]
    }

    /// The fill of a filled primary button.
    pub fn primary_button(&self) -> ButtonColors {
        ButtonColors::filled(self.accent, self.on_accent)
    }

    /// The fill of a destructive button, in the app's own danger colour.
    ///
    /// The surface colour is the fill's high-contrast partner in either theme:
    /// white under the dark red of the light theme, near-black under the pale
    /// salmon of the dark one.
    pub fn danger_button(&self) -> ButtonColors {
        ButtonColors::filled(self.remove_text, self.background)
    }
}

/// The colours of a filled button, one per pointer state.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ButtonColors {
    /// The fill at rest.
    pub background: Hsla,
    /// Text and icons drawn on the fill.
    pub foreground: Hsla,
    /// The fill under the pointer.
    pub hover: Hsla,
    /// The fill while pressed.
    pub active: Hsla,
}

/// How far hover and pressed move away from the rest fill.
const HOVER_DARKEN: f32 = 0.08;
const ACTIVE_DARKEN: f32 = 0.16;

impl ButtonColors {
    fn filled(background: Hsla, foreground: Hsla) -> Self {
        Self {
            background,
            foreground,
            // Darkening keeps the foreground readable; lightening a fill that
            // already carries white text would wash the two together.
            hover: background.darken(HOVER_DARKEN),
            active: background.darken(ACTIVE_DARKEN),
        }
    }
}

/// Puts gpui-component's palette on the [`Design`] colours.
///
/// The whole theme is written now, not only the buttons; this stays for the
/// callers that predate [`crate::component_theme::sync`].
pub fn sync_component_buttons(cx: &mut App) {
    crate::component_theme::sync(cx);
}

impl Default for Design {
    fn default() -> Self {
        // The fallback font matches `Settings::default`, so the default design
        // is the light palette with no user choices applied.
        Self::from_palette(Palette::light(), &Settings::default(), "monospace".into())
    }
}

fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

/// WCAG 2.x contrast ratio between two opaque colours, from 1.0 to 21.0.
///
/// The diff is the one place where body text sits on a coloured band, so its
/// tokens are held to the AA threshold here instead of being eyeballed on a
/// screenshot. Alpha is ignored: every token this checks is opaque. Only the
/// palette tests need it, so it stays out of the shipped build.
#[cfg(test)]
fn contrast_ratio(first: Hsla, second: Hsla) -> f32 {
    fn luminance(color: Hsla) -> f32 {
        let rgb = color.to_rgb();
        let linear = |channel: f32| {
            if channel <= 0.039_28 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(rgb.r) + 0.7152 * linear(rgb.g) + 0.0722 * linear(rgb.b)
    }
    let (first, second) = (luminance(first), luminance(second));
    (first.max(second) + 0.05) / (first.min(second) + 0.05)
}

/// The colour half of one appearance.
///
/// Named fields rather than an array of slots: a new token is one line per
/// theme, and reading a value does not mean counting indices.
struct Palette {
    background: Hsla,
    chrome: Hsla,
    sidebar: Hsla,
    panel: Hsla,
    field: Hsla,
    border: Hsla,
    text: Hsla,
    muted: Hsla,
    selected: Hsla,
    row_selected: Hsla,
    accent: Hsla,
    accent_soft: Hsla,
    accent_text: Hsla,
    add_background: Hsla,
    add_text: Hsla,
    remove_background: Hsla,
    remove_text: Hsla,
    remote_background: Hsla,
    remote_text: Hsla,
    tag_background: Hsla,
    tag_text: Hsla,
    diff_added_bg: Hsla,
    diff_removed_bg: Hsla,
    diff_added_word: Hsla,
    diff_removed_word: Hsla,
    diff_hunk_header: Hsla,
    diff_gutter_fg: Hsla,
    diff_whitespace: Hsla,
    syntax_keyword: Hsla,
    syntax_string: Hsla,
    syntax_comment: Hsla,
    syntax_function: Hsla,
    syntax_type: Hsla,
    syntax_number: Hsla,
    syntax_tag: Hsla,
    lane: [Hsla; LANES],
    avatar: [Hsla; AVATARS],
}

impl Palette {
    fn light() -> Self {
        Self {
            background: hex(0xffffff),
            chrome: hex(0xf7f8fa),
            sidebar: hex(0xf7f8fa),
            panel: hex(0xfafbfc),
            field: hex(0xeef0f3),
            border: hex(0xe3e6eb),
            text: hex(0x1b1d22),
            muted: hex(0x5b616e),
            selected: hex(0xe3e9fb),
            row_selected: hex(0xe9eefc),
            accent: hex(0x3d63dd),
            accent_soft: hex(0xe3e9fb),
            accent_text: hex(0x2a4bb8),
            add_background: hex(0xe7f6ec),
            add_text: hex(0x1d7a45),
            remove_background: hex(0xfdecec),
            remove_text: hex(0xb4352a),
            remote_background: hex(0xeceef2),
            remote_text: hex(0x3f4450),
            tag_background: hex(0xfbf0dc),
            tag_text: hex(0x8a5a12),
            // The washes are light enough that the body text keeps 12:1 or
            // better, and the word tint is one step stronger so the changed
            // token still reads as the change.
            diff_added_bg: hex(0xe6f5eb),
            diff_removed_bg: hex(0xfcebea),
            diff_added_word: hex(0xd2efdd),
            diff_removed_word: hex(0xfde1df),
            diff_hunk_header: hex(0xf0f2f6),
            // Darker than `muted` so line numbers clear 4.5:1 on the washes.
            diff_gutter_fg: hex(0x5f6674),
            // Decorative, but still 3:1 on the line it marks.
            diff_whitespace: hex(0x7d8592),
            // Each class clears 4.5:1 on the washes and on the word tints.
            syntax_keyword: hex(0xa03a75),
            syntax_string: hex(0x18703d),
            syntax_comment: hex(0x5b616e),
            syntax_function: hex(0x2a4bb8),
            syntax_type: hex(0x8a5a12),
            syntax_number: hex(0x1f6f8b),
            syntax_tag: hex(0xb4352a),
            lane: lanes(),
            avatar: avatars(),
        }
    }

    fn dark() -> Self {
        Self {
            background: hex(0x16181d),
            chrome: hex(0x1b1e24),
            sidebar: hex(0x1b1e24),
            panel: hex(0x1d2027),
            field: hex(0x262a32),
            border: hex(0x2c3039),
            text: hex(0xe6e8ec),
            muted: hex(0xa0a6b1),
            selected: hex(0x2a3350),
            row_selected: hex(0x243052),
            accent: hex(0x6d8cf0),
            accent_soft: hex(0x26314f),
            accent_text: hex(0xb8c8ff),
            add_background: hex(0x173022),
            add_text: hex(0x5fd08f),
            remove_background: hex(0x3a1f22),
            remove_text: hex(0xf08a80),
            remote_background: hex(0x2a2e36),
            remote_text: hex(0xc9ced8),
            tag_background: hex(0x3b2f1a),
            tag_text: hex(0xf1c27d),
            // Deep enough to stay a wash under the light body text, strong
            // enough to separate an added line from the panel behind it.
            diff_added_bg: hex(0x12291c),
            diff_removed_bg: hex(0x331a1e),
            diff_added_word: hex(0x173a24),
            diff_removed_word: hex(0x4a2126),
            diff_hunk_header: hex(0x23262e),
            // Pale enough for 6:1 on the washes, as line numbers are small.
            diff_gutter_fg: hex(0x9aa2af),
            // Decorative, but still 3:1 on the line it marks.
            diff_whitespace: hex(0x78818f),
            // Each class clears 4.5:1 on the washes and on the word tints.
            syntax_keyword: hex(0xe59ac9),
            syntax_string: hex(0x5fd08f),
            syntax_comment: hex(0xb0b6c1),
            syntax_function: hex(0xb8c8ff),
            syntax_type: hex(0xf1c27d),
            syntax_number: hex(0x7fd6e8),
            syntax_tag: hex(0xf49a91),
            lane: lanes(),
            avatar: avatars(),
        }
    }
}

/// Lane colours, in the order the graph assigns them; the same in both
/// appearances, because a lane keeps its identity across a theme change.
fn lanes() -> [Hsla; LANES] {
    [
        hex(0x3d63dd),
        hex(0x2fa58d),
        hex(0xe0913a),
        hex(0xc2508a),
        hex(0x7a5cd6),
        hex(0x2f9fd0),
        hex(0x8a9a2e),
        hex(0xd0553f),
    ]
}

/// Avatar colours, kept in the same order in both appearances.
fn avatars() -> [Hsla; AVATARS] {
    [
        hex(0x3d63dd),
        hex(0x2fa58d),
        hex(0xe0913a),
        hex(0xc2508a),
        hex(0x7a5cd6),
        hex(0x2f9fd0),
    ]
}

/// FNV-1a over the email, so every appearance gives an author the same colour
/// without keeping state between frames.
fn avatar_index(email: &str) -> usize {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in email.trim().to_lowercase().bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    (hash as usize) % AVATARS
}

/// Up to two initials: the first letter of the first and last word. A single
/// name keeps one letter. Non-ASCII letters are uppercased, not dropped.
pub fn initials(name: &str) -> String {
    let mut words = name.split_whitespace().filter(|word| !word.is_empty());
    let first = words.next().and_then(|word| word.chars().next());
    let last = words.next_back().and_then(|word| word.chars().next());
    match (first, last) {
        (Some(first), Some(last)) => first.to_uppercase().chain(last.to_uppercase()).collect(),
        (Some(first), None) => first.to_uppercase().collect(),
        (None, _) => "?".to_owned(),
    }
}

/// "2 hours ago" for anything younger than a week, then a plain date.
pub fn relative_time(epoch: i64, now: i64) -> String {
    let seconds = now.saturating_sub(epoch);
    if seconds < 60 {
        return "just now".to_owned();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes} min ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return plural(hours, "hour");
    }
    let days = hours / 24;
    if days < 7 {
        return plural(days, "day");
    }
    date_part(epoch)
}

fn plural(count: i64, unit: &str) -> String {
    if count == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{count} {unit}s ago")
    }
}

/// The `YYYY-MM-DD` part of a timestamp, in the same calendar the absolute
/// column uses.
fn date_part(epoch: i64) -> String {
    let stamp = format_time(epoch);
    stamp
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// The date column's label, following `history.date_format`.
pub fn date_label(epoch: i64, now: i64, format: DateFormat) -> String {
    match format {
        DateFormat::Relative => relative_time(epoch, now),
        DateFormat::Absolute => format_time(epoch),
    }
}

/// Seconds since the Unix epoch, saturating at zero for a clock before 1970.
pub fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// One letter for a change kind, matching git's porcelain codes.
pub fn kind_marker(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added | ChangeKind::Untracked => "A",
        ChangeKind::Modified => "M",
        ChangeKind::Deleted => "D",
        ChangeKind::Renamed => "R",
        ChangeKind::Copied => "C",
        ChangeKind::TypeChanged => "T",
        ChangeKind::Conflicted => "!",
    }
}

/// `(background, foreground)` for a change kind: A green, M amber, D red,
/// R blue, with the remaining kinds on the neutral or accent pills.
pub fn kind_colors(kind: ChangeKind, design: &Design) -> (Hsla, Hsla) {
    match kind {
        ChangeKind::Added | ChangeKind::Untracked => (design.add_background, design.add_text),
        ChangeKind::Modified | ChangeKind::TypeChanged => (design.tag_background, design.tag_text),
        ChangeKind::Deleted | ChangeKind::Conflicted => {
            (design.remove_background, design.remove_text)
        }
        ChangeKind::Renamed | ChangeKind::Copied => (design.accent_soft, design.accent_text),
    }
}

/// Splits `crates/app/src/main.rs` into `("main.rs", "crates/app/src")`.
pub fn path_parts(path: &str) -> (&str, Option<&str>) {
    match path.rsplit_once('/') {
        Some((dir, name)) => (name, Some(dir)),
        None => (path, None),
    }
}

/// Icons the default `gpui_component::IconName` bundle does not carry, embedded
/// from `assets/icons` so they need no change to the app's asset source.
pub mod glyph {
    pub const BRANCH: &[u8] = include_bytes!("../assets/icons/branch.svg");
    pub const CLOUD: &[u8] = include_bytes!("../assets/icons/cloud.svg");
    pub const TAG: &[u8] = include_bytes!("../assets/icons/tag.svg");
    pub const ARCHIVE: &[u8] = include_bytes!("../assets/icons/archive.svg");
    pub const COMMIT: &[u8] = include_bytes!("../assets/icons/commit.svg");
    pub const SUBMODULE: &[u8] = include_bytes!("../assets/icons/submodule.svg");
    pub const WORKTREE: &[u8] = include_bytes!("../assets/icons/worktree.svg");
    /// "STRATA" as outlines; see resources/icon/wordmark.py.
    pub const WORDMARK: &[u8] = include_bytes!("../assets/icons/wordmark.svg");
}

/// The title bar's "STRATA" wordmark at `height`, in `color`.
pub fn wordmark(height: f32, color: Hsla) -> Icon {
    // The SVG's viewBox is 10400 x 1550; keep that aspect so the letters are
    // not squeezed into the icon's default square.
    const ASPECT: f32 = 10400.0 / 1550.0;
    // No `with_size`: it forces a square that overrides the width set here.
    Icon::default()
        .data(glyph::WORDMARK)
        .w(px(height * ASPECT))
        .h(px(height))
        .text_color(color)
}

/// A [`glyph`] at `size`, in `color`.
pub fn glyph_icon(data: &'static [u8], size: f32, color: Hsla) -> Icon {
    Icon::default()
        .data(data)
        .with_size(px(size))
        .text_color(color)
}

/// The app mark at `size`: the design's rounded square with three bands.
///
/// Drawn with quads rather than the SVG asset because GPUI renders an SVG as a
/// single-colour alpha mask, which flattens the bands into one silhouette.
pub fn app_mark(size: f32) -> AnyElement {
    let unit = size / 256.0;
    div()
        .flex_none()
        .w(px(size))
        .h(px(size))
        .child(
            canvas(
                move |_, _, _| (),
                move |bounds, _, window, _| {
                    let mut mark = |x: f32, y: f32, width: f32, height: f32, radius: f32, color| {
                        let origin = point(
                            bounds.origin.x + px(x * unit),
                            bounds.origin.y + px(y * unit),
                        );
                        let size = gpui::size(px(width * unit), px(height * unit));
                        window.paint_quad(
                            fill(Bounds::new(origin, size), color).corner_radii(px(radius * unit)),
                        );
                    };
                    mark(8.0, 8.0, 240.0, 240.0, 56.0, hex(0x1b1e25));
                    mark(40.0, 76.0, 176.0, 36.0, 6.0, hex(0x3d63dd));
                    mark(40.0, 128.0, 176.0, 36.0, 6.0, hex(0x2fa58d));
                    mark(40.0, 180.0, 176.0, 36.0, 6.0, hex(0xe0913a));
                },
            )
            .size_full(),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_component::ThemeMode;

    #[test]
    fn relative_dates_cover_every_boundary() {
        let now = 1_800_000_000;
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(relative_time(now + 30, now), "just now", "clock skew");
        assert_eq!(relative_time(now - 59, now), "just now");
        assert_eq!(relative_time(now - 60, now), "1 min ago");
        assert_eq!(relative_time(now - 119, now), "1 min ago");
        assert_eq!(relative_time(now - 120, now), "2 min ago");
        assert_eq!(relative_time(now - 3599, now), "59 min ago");
        assert_eq!(relative_time(now - 3600, now), "1 hour ago");
        assert_eq!(relative_time(now - 7200, now), "2 hours ago");
        assert_eq!(relative_time(now - 86_399, now), "23 hours ago");
        assert_eq!(relative_time(now - 86_400, now), "1 day ago");
        assert_eq!(relative_time(now - 6 * 86_400, now), "6 days ago");
        assert_eq!(
            relative_time(now - 7 * 86_400, now),
            date_part(now - 7 * 86_400),
            "a week old commits show the date instead"
        );
    }

    #[test]
    fn absolute_dates_are_the_iso_stamp() {
        assert_eq!(date_label(0, 0, DateFormat::Absolute), "1970-01-01 00:00");
        assert_eq!(date_label(0, 90, DateFormat::Relative), "1 min ago");
    }

    #[test]
    fn avatar_colours_are_stable_and_case_insensitive() {
        let design = Design::default();
        let first = design.avatar_color("mio@example.invalid");
        assert_eq!(first, design.avatar_color("mio@example.invalid"));
        assert_eq!(
            first,
            design.avatar_color("  MIO@example.invalid "),
            "the same email keeps its colour however it is spelled"
        );
        assert_ne!(
            design.avatar_color("a@example.invalid"),
            design.avatar_color("b@example.invalid"),
            "different authors can differ"
        );
    }

    #[test]
    fn initials_take_the_first_and_last_word() {
        assert_eq!(initials("Mio Tanaka"), "MT");
        assert_eq!(initials("Ren"), "R");
        assert_eq!(initials("ana maria lopez"), "AL");
        assert_eq!(initials("  Mio   Tanaka  "), "MT");
        assert_eq!(initials("山田 太郎"), "山太");
        assert_eq!(initials("éclair noir"), "ÉN");
        assert_eq!(initials(""), "?");
    }

    #[test]
    fn paths_split_into_a_name_and_its_directory() {
        assert_eq!(path_parts("src/main.rs"), ("main.rs", Some("src")));
        assert_eq!(path_parts("main.rs"), ("main.rs", None));
        assert_eq!(
            path_parts("crates/app/src/a/b.rs"),
            ("b.rs", Some("crates/app/src/a"))
        );
    }

    #[test]
    fn kind_badges_follow_the_design_colours() {
        let design = Design::default();
        assert_eq!(
            kind_colors(ChangeKind::Added, &design),
            (design.add_background, design.add_text)
        );
        assert_eq!(
            kind_colors(ChangeKind::Modified, &design),
            (design.tag_background, design.tag_text)
        );
        assert_eq!(
            kind_colors(ChangeKind::Deleted, &design),
            (design.remove_background, design.remove_text)
        );
        assert_eq!(kind_marker(ChangeKind::Renamed), "R");
    }

    fn dark_design() -> Design {
        // `Design` reads only the mode from the theme; loading the dark theme
        // file would need a running app.
        let mut theme = Theme::default();
        theme.mode = ThemeMode::Dark;
        Design::new(&theme, &Settings::default())
    }

    const SYNTAX_CLASSES: [HighlightKind; 16] = [
        HighlightKind::Keyword,
        HighlightKind::String,
        HighlightKind::Comment,
        HighlightKind::Function,
        HighlightKind::Type,
        HighlightKind::Constant,
        HighlightKind::Number,
        HighlightKind::Variable,
        HighlightKind::Property,
        HighlightKind::Operator,
        HighlightKind::Punctuation,
        HighlightKind::Attribute,
        HighlightKind::Tag,
        HighlightKind::Label,
        HighlightKind::Escape,
        HighlightKind::Embedded,
    ];

    #[test]
    fn contrast_ratio_matches_the_wcag_reference_values() {
        assert!((contrast_ratio(hex(0x000000), hex(0xffffff)) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(hex(0xffffff), hex(0xffffff)) - 1.0).abs() < 0.001);
        // #767676 on white is the familiar AA boundary example, at 4.54:1.
        assert!((contrast_ratio(hex(0x767676), hex(0xffffff)) - 4.54).abs() < 0.02);
        assert_eq!(
            contrast_ratio(hex(0x123456), hex(0xabcdef)),
            contrast_ratio(hex(0xabcdef), hex(0x123456)),
            "the ratio does not depend on the argument order"
        );
    }

    #[test]
    fn diff_text_is_readable_on_every_line_tint() {
        for design in [Design::default(), dark_design()] {
            for background in [
                design.diff_added_bg,
                design.diff_removed_bg,
                design.diff_added_word,
                design.diff_removed_word,
            ] {
                let ratio = contrast_ratio(design.text, background);
                assert!(ratio >= 4.5, "body text is {ratio:.2}:1 on a diff tint");
            }
            for background in [design.diff_added_bg, design.diff_removed_bg] {
                let ratio = contrast_ratio(design.diff_gutter_fg, background);
                assert!(ratio >= 4.5, "line numbers are {ratio:.2}:1 on a diff tint");
                let ratio = contrast_ratio(design.diff_whitespace, background);
                assert!(
                    ratio >= 3.0,
                    "the whitespace markers are {ratio:.2}:1 on a diff tint"
                );
            }
        }
    }

    #[test]
    fn syntax_colours_stay_readable_on_the_diff_tints() {
        for design in [Design::default(), dark_design()] {
            let tints = [
                design.diff_added_bg,
                design.diff_removed_bg,
                design.diff_added_word,
                design.diff_removed_word,
            ];
            for kind in SYNTAX_CLASSES {
                for background in tints {
                    let ratio = contrast_ratio(design.syntax(kind), background);
                    assert!(ratio >= 4.5, "{kind:?} is {ratio:.2}:1 on a diff tint");
                }
            }
        }
    }

    #[test]
    fn the_diff_tokens_follow_the_appearance() {
        let light = Design::default();
        let dark = dark_design();
        for (light, dark) in [
            (light.diff_added_bg, dark.diff_added_bg),
            (light.diff_removed_bg, dark.diff_removed_bg),
            (light.diff_added_word, dark.diff_added_word),
            (light.diff_removed_word, dark.diff_removed_word),
            (light.diff_hunk_header, dark.diff_hunk_header),
            (light.diff_gutter_fg, dark.diff_gutter_fg),
            (light.diff_whitespace, dark.diff_whitespace),
        ] {
            assert_ne!(light, dark, "each appearance has its own value");
        }
        assert_ne!(
            light.syntax(HighlightKind::Keyword),
            dark.syntax(HighlightKind::Keyword)
        );

        // The tint strengthens away from the surface: darker in the light
        // theme, lighter in the dark one.
        for (design, darker) in [(light, true), (dark, false)] {
            assert_eq!(
                design.diff_added_word.l < design.diff_added_bg.l,
                darker,
                "the changed word is a stronger tint than its line"
            );
            assert_eq!(
                design.diff_removed_word.l < design.diff_removed_bg.l,
                darker,
                "the changed word is a stronger tint than its line"
            );
        }
    }

    #[test]
    fn the_code_size_is_clamped_like_every_other_pane() {
        let mut settings = Settings::default();
        settings.appearance.code_font_size = 40.0;
        let design = Design::new(&Theme::default(), &settings);
        assert_eq!(design.code, CODE_FONT_MAX);
        settings.appearance.code_font_size = 2.0;
        assert_eq!(
            Design::new(&Theme::default(), &settings).code,
            CODE_FONT_MIN
        );
    }

    #[test]
    fn filled_buttons_use_the_design_accents_in_both_themes() {
        for design in [Design::default(), dark_design()] {
            let primary = design.primary_button();
            assert_eq!(primary.background, design.accent);
            assert_eq!(primary.foreground, design.on_accent);
            assert!(
                primary.hover.l < primary.background.l,
                "hover darkens the fill"
            );
            assert!(
                primary.active.l < primary.hover.l,
                "pressed darkens it further than hover"
            );

            let danger = design.danger_button();
            assert_eq!(danger.background, design.remove_text);
            assert_eq!(
                danger.foreground, design.background,
                "the surface colour keeps the label readable on the danger fill"
            );
            assert_eq!(danger.hover, danger.background.darken(HOVER_DARKEN));
            assert_eq!(danger.active, danger.background.darken(ACTIVE_DARKEN));
        }

        assert_ne!(
            Design::default().primary_button().background,
            dark_design().primary_button().background,
            "each theme has its own accent"
        );
    }
}
