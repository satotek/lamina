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
use gpui_component::{ActiveTheme as _, Colorize as _, Icon, Sizable as _, Theme};
use strata_core::ChangeKind;

use crate::data::format_time;
use crate::settings::{DateFormat, Settings};

/// Lane colours, in the order the graph assigns them.
pub const LANES: usize = 8;
/// Avatar colours; the index comes from the author's email so it never moves.
const AVATARS: usize = 6;

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
        let (colors, lane, avatar) = if theme.is_dark() {
            dark_tables()
        } else {
            light_tables()
        };
        Self {
            background: colors[0],
            chrome: colors[1],
            sidebar: colors[2],
            panel: colors[3],
            field: colors[4],
            border: colors[5],
            text: colors[6],
            muted: colors[7],
            selected: colors[8],
            row_selected: colors[9],
            accent: colors[10],
            accent_soft: colors[11],
            accent_text: colors[12],
            on_accent: hex(0xffffff),
            add_background: colors[13],
            add_text: colors[14],
            remove_background: colors[15],
            remove_text: colors[16],
            remote_background: colors[17],
            remote_text: colors[18],
            tag_background: colors[19],
            tag_text: colors[20],
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
            code: settings.appearance.code_font_size,
            mono: settings
                .appearance
                .code_font
                .as_deref()
                .map_or_else(|| theme.mono_font_family.clone(), SharedString::from),
            lane,
            avatar,
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

/// Puts gpui-component's button palette on the [`Design`] colours.
///
/// A component button paints its rest, hover, pressed and disabled states from
/// the theme, so a fill set on the element itself is replaced as soon as the
/// pointer enters it. Writing the tokens is the supported way to change them;
/// it also keeps the dialogs, menus and inputs beside the buttons on the same
/// palette. The guard makes the write happen once per palette, because a
/// button renders every frame.
pub fn sync_component_buttons(cx: &mut App) {
    let (primary, danger) = {
        let design = Design::new(cx.theme(), Settings::get(cx));
        (design.primary_button(), design.danger_button())
    };
    let theme = cx.theme();
    let in_sync = theme.primary == primary.background
        && theme.primary_hover == primary.hover
        && theme.primary_active == primary.active
        && theme.primary_foreground == primary.foreground
        && theme.button_primary == primary.background
        && theme.button_primary_hover == primary.hover
        && theme.button_primary_active == primary.active
        && theme.button_primary_foreground == primary.foreground
        && theme.danger == danger.background
        && theme.danger_hover == danger.hover
        && theme.danger_active == danger.active
        && theme.danger_foreground == danger.foreground
        && theme.button_danger == danger.background
        && theme.button_danger_hover == danger.hover
        && theme.button_danger_active == danger.active
        && theme.button_danger_foreground == danger.foreground;
    if in_sync {
        return;
    }
    Theme::update(cx, |theme| {
        theme.primary = primary.background;
        theme.primary_hover = primary.hover;
        theme.primary_active = primary.active;
        theme.primary_foreground = primary.foreground;
        theme.button_primary = primary.background;
        theme.button_primary_hover = primary.hover;
        theme.button_primary_active = primary.active;
        theme.button_primary_foreground = primary.foreground;
        theme.danger = danger.background;
        theme.danger_hover = danger.hover;
        theme.danger_active = danger.active;
        theme.danger_foreground = danger.foreground;
        theme.button_danger = danger.background;
        theme.button_danger_hover = danger.hover;
        theme.button_danger_active = danger.active;
        theme.button_danger_foreground = danger.foreground;
    });
}

impl Default for Design {
    fn default() -> Self {
        let (colors, lane, avatar) = light_tables();
        Self {
            background: colors[0],
            chrome: colors[1],
            sidebar: colors[2],
            panel: colors[3],
            field: colors[4],
            border: colors[5],
            text: colors[6],
            muted: colors[7],
            selected: colors[8],
            row_selected: colors[9],
            accent: colors[10],
            accent_soft: colors[11],
            accent_text: colors[12],
            on_accent: hex(0xffffff),
            add_background: colors[13],
            add_text: colors[14],
            remove_background: colors[15],
            remove_text: colors[16],
            remote_background: colors[17],
            remote_text: colors[18],
            tag_background: colors[19],
            tag_text: colors[20],
            ui_font: None,
            date_format: DateFormat::Relative,
            show_remote_branches: true,
            show_tags: true,
            show_stashes: true,
            summary_hint: 72,
            ui: 13.0,
            code: 12.5,
            mono: "monospace".into(),
            lane,
            avatar,
        }
    }
}

fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

type Tables = ([Hsla; 21], [Hsla; LANES], [Hsla; AVATARS]);

fn light_tables() -> Tables {
    (
        [
            hex(0xffffff), // background
            hex(0xf7f8fa), // chrome
            hex(0xf7f8fa), // sidebar
            hex(0xfafbfc), // panel
            hex(0xeef0f3), // field
            hex(0xe3e6eb), // border
            hex(0x1b1d22), // text
            hex(0x5b616e), // muted
            hex(0xe3e9fb), // selected
            hex(0xe9eefc), // row_selected
            hex(0x3d63dd), // accent
            hex(0xe3e9fb), // accent_soft
            hex(0x2a4bb8), // accent_text
            hex(0xe7f6ec), // add_background
            hex(0x1d7a45), // add_text
            hex(0xfdecec), // remove_background
            hex(0xb4352a), // remove_text
            hex(0xeceef2), // remote_background
            hex(0x3f4450), // remote_text
            hex(0xfbf0dc), // tag_background
            hex(0x8a5a12), // tag_text
        ],
        [
            hex(0x3d63dd),
            hex(0x2fa58d),
            hex(0xe0913a),
            hex(0xc2508a),
            hex(0x7a5cd6),
            hex(0x2f9fd0),
            hex(0x8a9a2e),
            hex(0xd0553f),
        ],
        [
            hex(0x3d63dd),
            hex(0x2fa58d),
            hex(0xe0913a),
            hex(0xc2508a),
            hex(0x7a5cd6),
            hex(0x2f9fd0),
        ],
    )
}

fn dark_tables() -> Tables {
    (
        [
            hex(0x16181d), // background
            hex(0x1b1e24), // chrome
            hex(0x1b1e24), // sidebar
            hex(0x1d2027), // panel
            hex(0x262a32), // field
            hex(0x2c3039), // border
            hex(0xe6e8ec), // text
            hex(0xa0a6b1), // muted
            hex(0x2a3350), // selected
            hex(0x243052), // row_selected
            hex(0x6d8cf0), // accent
            hex(0x26314f), // accent_soft
            hex(0xb8c8ff), // accent_text
            hex(0x173022), // add_background
            hex(0x5fd08f), // add_text
            hex(0x3a1f22), // remove_background
            hex(0xf08a80), // remove_text
            hex(0x2a2e36), // remote_background
            hex(0xc9ced8), // remote_text
            hex(0x3b2f1a), // tag_background
            hex(0xf1c27d), // tag_text
        ],
        [
            hex(0x3d63dd),
            hex(0x2fa58d),
            hex(0xe0913a),
            hex(0xc2508a),
            hex(0x7a5cd6),
            hex(0x2f9fd0),
            hex(0x8a9a2e),
            hex(0xd0553f),
        ],
        [
            hex(0x3d63dd),
            hex(0x2fa58d),
            hex(0xe0913a),
            hex(0xc2508a),
            hex(0x7a5cd6),
            hex(0x2f9fd0),
        ],
    )
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
