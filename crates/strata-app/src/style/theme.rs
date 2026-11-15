//! Data-driven colour themes parsed from TOML definitions.
//!
//! Bundled themes are embedded at compile time from `resources/themes/*.toml`,
//! while [`ThemePalette::from_toml`] stays independent of the embedded source
//! so user-supplied theme files can use the same parser later.

use std::collections::BTreeSet;
use std::sync::{LazyLock, Mutex};

use gpui::{Hsla, rgb, rgba};
use serde::Deserialize;
use serde::de::{self, Visitor};

use super::{AVATARS, LANES};

pub const DEFAULT_LIGHT_THEME_ID: &str = "strata-light";
pub const DEFAULT_DARK_THEME_ID: &str = "strata-dark";

const BUNDLED_SOURCES: &[(&str, &str)] = &[
    (
        "strata-light.toml",
        include_str!("../../resources/themes/strata-light.toml"),
    ),
    (
        "catppuccin-latte.toml",
        include_str!("../../resources/themes/catppuccin-latte.toml"),
    ),
    (
        "strata-dark.toml",
        include_str!("../../resources/themes/strata-dark.toml"),
    ),
    (
        "catppuccin-frappe.toml",
        include_str!("../../resources/themes/catppuccin-frappe.toml"),
    ),
    (
        "catppuccin-macchiato.toml",
        include_str!("../../resources/themes/catppuccin-macchiato.toml"),
    ),
    (
        "catppuccin-mocha.toml",
        include_str!("../../resources/themes/catppuccin-mocha.toml"),
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeAppearance {
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HexColor {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

impl HexColor {
    fn parse(input: &str) -> Result<Self, String> {
        let Some(hex) = input.strip_prefix('#') else {
            return Err(format!("colour {input:?} must start with '#'"));
        };
        let bytes = match hex.len() {
            6 => {
                let r = u8::from_str_radix(&hex[0..2], 16)
                    .map_err(|_| format!("invalid hex colour {input:?}"))?;
                let g = u8::from_str_radix(&hex[2..4], 16)
                    .map_err(|_| format!("invalid hex colour {input:?}"))?;
                let b = u8::from_str_radix(&hex[4..6], 16)
                    .map_err(|_| format!("invalid hex colour {input:?}"))?;
                (r, g, b, 0xff)
            }
            8 => {
                let r = u8::from_str_radix(&hex[0..2], 16)
                    .map_err(|_| format!("invalid hex colour {input:?}"))?;
                let g = u8::from_str_radix(&hex[2..4], 16)
                    .map_err(|_| format!("invalid hex colour {input:?}"))?;
                let b = u8::from_str_radix(&hex[4..6], 16)
                    .map_err(|_| format!("invalid hex colour {input:?}"))?;
                let a = u8::from_str_radix(&hex[6..8], 16)
                    .map_err(|_| format!("invalid hex colour {input:?}"))?;
                (r, g, b, a)
            }
            _ => {
                return Err(format!("colour {input:?} must be #rrggbb or #rrggbbaa"));
            }
        };
        Ok(Self {
            r: bytes.0,
            g: bytes.1,
            b: bytes.2,
            a: bytes.3,
        })
    }

    fn to_hsla(self) -> Hsla {
        if self.a == 0xff {
            let value = (u32::from(self.r) << 16) | (u32::from(self.g) << 8) | u32::from(self.b);
            rgb(value).into()
        } else {
            let value = (u32::from(self.r) << 24)
                | (u32::from(self.g) << 16)
                | (u32::from(self.b) << 8)
                | u32::from(self.a);
            rgba(value).into()
        }
    }
}

impl<'de> Deserialize<'de> for HexColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct HexVisitor;

        impl Visitor<'_> for HexVisitor {
            type Value = HexColor;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a hex colour string (#rrggbb or #rrggbbaa)")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                HexColor::parse(value).map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_str(HexVisitor)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTheme {
    id: String,
    name: String,
    appearance: ThemeAppearance,
    colors: RawColors,
    terminal: RawTerminal,
    graph: RawGraph,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawColors {
    background: HexColor,
    chrome: HexColor,
    sidebar: HexColor,
    panel: HexColor,
    field: HexColor,
    border: HexColor,
    text: HexColor,
    muted: HexColor,
    selected: HexColor,
    row_selected: HexColor,
    accent: HexColor,
    accent_soft: HexColor,
    accent_text: HexColor,
    on_accent: HexColor,
    add_background: HexColor,
    add_text: HexColor,
    remove_background: HexColor,
    remove_text: HexColor,
    remote_background: HexColor,
    remote_text: HexColor,
    tag_background: HexColor,
    tag_text: HexColor,
    diff_added_bg: HexColor,
    diff_removed_bg: HexColor,
    diff_added_word: HexColor,
    diff_removed_word: HexColor,
    diff_hunk_header: HexColor,
    diff_gutter_fg: HexColor,
    diff_whitespace: HexColor,
    syntax_keyword: HexColor,
    syntax_string: HexColor,
    syntax_comment: HexColor,
    syntax_function: HexColor,
    syntax_type: HexColor,
    syntax_number: HexColor,
    syntax_tag: HexColor,
    avatar: [HexColor; AVATARS],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTerminal {
    foreground: HexColor,
    background: HexColor,
    cursor: HexColor,
    selection: HexColor,
    black: HexColor,
    red: HexColor,
    green: HexColor,
    yellow: HexColor,
    blue: HexColor,
    magenta: HexColor,
    cyan: HexColor,
    white: HexColor,
    bright_black: HexColor,
    bright_red: HexColor,
    bright_green: HexColor,
    bright_yellow: HexColor,
    bright_blue: HexColor,
    bright_magenta: HexColor,
    bright_cyan: HexColor,
    bright_white: HexColor,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGraph {
    lanes: Vec<HexColor>,
}

/// Complete colour palette for one theme, resolved once from TOML.
#[derive(Debug, Clone)]
pub struct ThemePalette {
    pub id: String,
    pub name: String,
    pub appearance: ThemeAppearance,
    pub background: Hsla,
    pub chrome: Hsla,
    pub sidebar: Hsla,
    pub panel: Hsla,
    pub field: Hsla,
    pub border: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub selected: Hsla,
    pub row_selected: Hsla,
    pub accent: Hsla,
    pub accent_soft: Hsla,
    pub accent_text: Hsla,
    pub on_accent: Hsla,
    pub add_background: Hsla,
    pub add_text: Hsla,
    pub remove_background: Hsla,
    pub remove_text: Hsla,
    pub remote_background: Hsla,
    pub remote_text: Hsla,
    pub tag_background: Hsla,
    pub tag_text: Hsla,
    pub diff_added_bg: Hsla,
    pub diff_removed_bg: Hsla,
    pub diff_added_word: Hsla,
    pub diff_removed_word: Hsla,
    pub diff_hunk_header: Hsla,
    pub diff_gutter_fg: Hsla,
    pub diff_whitespace: Hsla,
    pub syntax_keyword: Hsla,
    pub syntax_string: Hsla,
    pub syntax_comment: Hsla,
    pub syntax_function: Hsla,
    pub syntax_type: Hsla,
    pub syntax_number: Hsla,
    pub syntax_tag: Hsla,
    pub avatar: [Hsla; AVATARS],
    pub lane: [Hsla; LANES],
    #[cfg_attr(not(feature = "terminal"), allow(dead_code))]
    pub terminal_foreground: Hsla,
    #[cfg_attr(not(feature = "terminal"), allow(dead_code))]
    pub terminal_background: Hsla,
    #[cfg_attr(not(feature = "terminal"), allow(dead_code))]
    pub terminal_cursor: Hsla,
    #[cfg_attr(not(feature = "terminal"), allow(dead_code))]
    pub terminal_selection: Hsla,
    #[cfg_attr(not(feature = "terminal"), allow(dead_code))]
    pub terminal_ansi: [Hsla; 16],
}

impl ThemePalette {
    /// Parses one theme from TOML text. Every token must be explicitly defined.
    pub fn from_toml(source: &str) -> Result<Self, String> {
        let raw: RawTheme = toml::from_str(source).map_err(|error| error.to_string())?;
        if raw.id.trim().is_empty() {
            return Err("theme id must not be empty".to_owned());
        }
        if raw.name.trim().is_empty() {
            return Err("theme name must not be empty".to_owned());
        }
        if raw.graph.lanes.is_empty() {
            return Err("theme graph.lanes must contain at least one colour".to_owned());
        }

        let lane =
            std::array::from_fn(|index| raw.graph.lanes[index % raw.graph.lanes.len()].to_hsla());
        let avatar = raw.colors.avatar.map(HexColor::to_hsla);
        let terminal_ansi = [
            raw.terminal.black,
            raw.terminal.red,
            raw.terminal.green,
            raw.terminal.yellow,
            raw.terminal.blue,
            raw.terminal.magenta,
            raw.terminal.cyan,
            raw.terminal.white,
            raw.terminal.bright_black,
            raw.terminal.bright_red,
            raw.terminal.bright_green,
            raw.terminal.bright_yellow,
            raw.terminal.bright_blue,
            raw.terminal.bright_magenta,
            raw.terminal.bright_cyan,
            raw.terminal.bright_white,
        ]
        .map(HexColor::to_hsla);

        Ok(Self {
            id: raw.id,
            name: raw.name,
            appearance: raw.appearance,
            background: raw.colors.background.to_hsla(),
            chrome: raw.colors.chrome.to_hsla(),
            sidebar: raw.colors.sidebar.to_hsla(),
            panel: raw.colors.panel.to_hsla(),
            field: raw.colors.field.to_hsla(),
            border: raw.colors.border.to_hsla(),
            text: raw.colors.text.to_hsla(),
            muted: raw.colors.muted.to_hsla(),
            selected: raw.colors.selected.to_hsla(),
            row_selected: raw.colors.row_selected.to_hsla(),
            accent: raw.colors.accent.to_hsla(),
            accent_soft: raw.colors.accent_soft.to_hsla(),
            accent_text: raw.colors.accent_text.to_hsla(),
            on_accent: raw.colors.on_accent.to_hsla(),
            add_background: raw.colors.add_background.to_hsla(),
            add_text: raw.colors.add_text.to_hsla(),
            remove_background: raw.colors.remove_background.to_hsla(),
            remove_text: raw.colors.remove_text.to_hsla(),
            remote_background: raw.colors.remote_background.to_hsla(),
            remote_text: raw.colors.remote_text.to_hsla(),
            tag_background: raw.colors.tag_background.to_hsla(),
            tag_text: raw.colors.tag_text.to_hsla(),
            diff_added_bg: raw.colors.diff_added_bg.to_hsla(),
            diff_removed_bg: raw.colors.diff_removed_bg.to_hsla(),
            diff_added_word: raw.colors.diff_added_word.to_hsla(),
            diff_removed_word: raw.colors.diff_removed_word.to_hsla(),
            diff_hunk_header: raw.colors.diff_hunk_header.to_hsla(),
            diff_gutter_fg: raw.colors.diff_gutter_fg.to_hsla(),
            diff_whitespace: raw.colors.diff_whitespace.to_hsla(),
            syntax_keyword: raw.colors.syntax_keyword.to_hsla(),
            syntax_string: raw.colors.syntax_string.to_hsla(),
            syntax_comment: raw.colors.syntax_comment.to_hsla(),
            syntax_function: raw.colors.syntax_function.to_hsla(),
            syntax_type: raw.colors.syntax_type.to_hsla(),
            syntax_number: raw.colors.syntax_number.to_hsla(),
            syntax_tag: raw.colors.syntax_tag.to_hsla(),
            avatar,
            lane,
            terminal_foreground: raw.terminal.foreground.to_hsla(),
            terminal_background: raw.terminal.background.to_hsla(),
            terminal_cursor: raw.terminal.cursor.to_hsla(),
            terminal_selection: raw.terminal.selection.to_hsla(),
            terminal_ansi,
        })
    }

    /// Five representative colours for compact theme picker swatches.
    pub fn swatches(&self) -> [Hsla; 5] {
        [
            self.background,
            self.panel,
            self.accent,
            self.add_text,
            self.remove_text,
        ]
    }
}

static BUNDLED_THEMES: LazyLock<Vec<ThemePalette>> = LazyLock::new(|| {
    BUNDLED_SOURCES
        .iter()
        .map(|(file, source)| {
            ThemePalette::from_toml(source)
                .unwrap_or_else(|error| panic!("bundled theme {file} is invalid: {error}"))
        })
        .collect()
});

/// Deduplicates warnings for unknown theme ids because `Design::new` runs on
/// every rendered frame.
static WARNED_UNKNOWN_IDS: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

fn warn_unknown_once(id: &str, fallback_id: &str) {
    let first = WARNED_UNKNOWN_IDS
        .lock()
        .map(|mut set| set.insert(id.to_owned()))
        .unwrap_or(true);
    if first {
        log::warn!("unknown theme id {id:?}; falling back to {fallback_id:?}");
    }
}

/// Every bundled theme in display order.
pub fn all() -> &'static [ThemePalette] {
    &BUNDLED_THEMES
}

/// Bundled themes matching `appearance`.
pub fn for_appearance(appearance: ThemeAppearance) -> impl Iterator<Item = &'static ThemePalette> {
    all()
        .iter()
        .filter(move |theme| theme.appearance == appearance)
}

/// Looks up a bundled theme by `id`.
pub fn by_id(id: &str) -> Option<&'static ThemePalette> {
    all().iter().find(|theme| theme.id == id)
}

/// Default bundled theme for the given appearance.
pub fn default_for(is_dark: bool) -> &'static ThemePalette {
    let id = if is_dark {
        DEFAULT_DARK_THEME_ID
    } else {
        DEFAULT_LIGHT_THEME_ID
    };
    by_id(id).expect("default bundled theme must exist")
}

/// Resolves `id` to a bundled theme, warning once and falling back to the
/// default for `is_dark` when `id` is unknown.
pub fn resolve(id: &str, is_dark: bool) -> &'static ThemePalette {
    if let Some(theme) = by_id(id) {
        return theme;
    }
    let fallback = default_for(is_dark);
    warn_unknown_once(id, &fallback.id);
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(value: u32) -> Hsla {
        rgb(value).into()
    }

    #[test]
    fn every_bundled_theme_parses_with_all_lanes_and_unique_ids() {
        let mut ids = BTreeSet::new();
        for (file, source) in BUNDLED_SOURCES {
            let raw: RawTheme =
                toml::from_str(source).unwrap_or_else(|e| panic!("{file} failed: {e}"));
            assert_eq!(
                raw.graph.lanes.len(),
                LANES,
                "{file} must define {LANES} graph lane colours"
            );
            assert!(ids.insert(raw.id.clone()), "duplicate theme id {}", raw.id);
            let parsed = ThemePalette::from_toml(source).expect("bundled theme parses");
            assert!(!parsed.name.is_empty());
        }
        assert_eq!(all().len(), 6);
        assert_eq!(for_appearance(ThemeAppearance::Light).count(), 2);
        assert_eq!(for_appearance(ThemeAppearance::Dark).count(), 4);
    }

    #[test]
    fn missing_tokens_are_rejected_without_silent_defaults() {
        let valid = BUNDLED_SOURCES[0].1;
        for needle in [
            "on_accent = \"#ffffff\"\n",
            "bright_white = \"#3f4450\"\n",
            "diff_whitespace = \"#7d8592\"\n",
        ] {
            let stripped = valid.replacen(needle, "", 1);
            assert_ne!(stripped, valid, "expected needle {needle:?} in template");
            assert!(
                ThemePalette::from_toml(&stripped).is_err(),
                "removing {needle:?} must fail parsing"
            );
        }
    }

    #[test]
    fn hex_colours_validate_format() {
        assert!(HexColor::parse("#1e1e2e").is_ok());
        assert!(HexColor::parse("#1e1e2ecc").is_ok());
        assert!(HexColor::parse("1e1e2e").is_err());
        assert!(HexColor::parse("#1e1e2").is_err());
        assert!(HexColor::parse("#zzzzzz").is_err());
    }

    #[test]
    fn unknown_theme_ids_fall_back_to_defaults() {
        assert_eq!(resolve("does-not-exist", false).id, DEFAULT_LIGHT_THEME_ID);
        assert_eq!(resolve("does-not-exist", true).id, DEFAULT_DARK_THEME_ID);
        assert_eq!(resolve("catppuccin-mocha", true).id, "catppuccin-mocha");
        assert_eq!(resolve("catppuccin-latte", false).id, "catppuccin-latte");
    }

    #[test]
    fn strata_light_and_dark_reproduce_original_colours_pixel_for_pixel() {
        let expected_lanes = [
            hex(0x3d63dd),
            hex(0x2fa58d),
            hex(0xe0913a),
            hex(0xc2508a),
            hex(0x7a5cd6),
            hex(0x2f9fd0),
            hex(0x8a9a2e),
            hex(0xd0553f),
        ];
        let expected_avatars = [
            hex(0x3d63dd),
            hex(0x2fa58d),
            hex(0xe0913a),
            hex(0xc2508a),
            hex(0x7a5cd6),
            hex(0x2f9fd0),
        ];

        let light = by_id(DEFAULT_LIGHT_THEME_ID).expect("strata-light");
        assert_eq!(light.appearance, ThemeAppearance::Light);
        assert_eq!(light.background, hex(0xffffff));
        assert_eq!(light.chrome, hex(0xf7f8fa));
        assert_eq!(light.sidebar, hex(0xf7f8fa));
        assert_eq!(light.panel, hex(0xfafbfc));
        assert_eq!(light.field, hex(0xeef0f3));
        assert_eq!(light.border, hex(0xe3e6eb));
        assert_eq!(light.text, hex(0x1b1d22));
        assert_eq!(light.muted, hex(0x5b616e));
        assert_eq!(light.selected, hex(0xe3e9fb));
        assert_eq!(light.row_selected, hex(0xe9eefc));
        assert_eq!(light.accent, hex(0x3d63dd));
        assert_eq!(light.accent_soft, hex(0xe3e9fb));
        assert_eq!(light.accent_text, hex(0x2a4bb8));
        assert_eq!(light.on_accent, hex(0xffffff));
        assert_eq!(light.add_background, hex(0xe7f6ec));
        assert_eq!(light.add_text, hex(0x1d7a45));
        assert_eq!(light.remove_background, hex(0xfdecec));
        assert_eq!(light.remove_text, hex(0xb4352a));
        assert_eq!(light.remote_background, hex(0xeceef2));
        assert_eq!(light.remote_text, hex(0x3f4450));
        assert_eq!(light.tag_background, hex(0xfbf0dc));
        assert_eq!(light.tag_text, hex(0x8a5a12));
        assert_eq!(light.diff_added_bg, hex(0xe6f5eb));
        assert_eq!(light.diff_removed_bg, hex(0xfcebea));
        assert_eq!(light.diff_added_word, hex(0xd2efdd));
        assert_eq!(light.diff_removed_word, hex(0xfde1df));
        assert_eq!(light.diff_hunk_header, hex(0xf0f2f6));
        assert_eq!(light.diff_gutter_fg, hex(0x5f6674));
        assert_eq!(light.diff_whitespace, hex(0x7d8592));
        assert_eq!(light.syntax_keyword, hex(0xa03a75));
        assert_eq!(light.syntax_string, hex(0x18703d));
        assert_eq!(light.syntax_comment, hex(0x5b616e));
        assert_eq!(light.syntax_function, hex(0x2a4bb8));
        assert_eq!(light.syntax_type, hex(0x8a5a12));
        assert_eq!(light.syntax_number, hex(0x1f6f8b));
        assert_eq!(light.syntax_tag, hex(0xb4352a));
        assert_eq!(light.lane, expected_lanes);
        assert_eq!(light.avatar, expected_avatars);
        assert_eq!(light.terminal_foreground, hex(0x1b1d22));
        assert_eq!(light.terminal_background, hex(0xffffff));
        assert_eq!(light.terminal_cursor, hex(0x3d63dd));
        assert_eq!(light.terminal_selection, hex(0xe3e9fb).opacity(0.8));
        assert_eq!(
            light.terminal_ansi,
            [
                hex(0x1b1d22),
                hex(0xb4352a),
                hex(0x1d7a45),
                hex(0x8a5a12),
                hex(0x2a4bb8),
                hex(0xa03a75),
                hex(0x1f6f8b),
                hex(0x5b616e),
                hex(0x636a78),
                hex(0xc93b2f),
                hex(0x1a884d),
                hex(0x9b640e),
                hex(0x3d63dd),
                hex(0xb24384),
                hex(0x197e9e),
                hex(0x3f4450),
            ]
        );

        let dark = by_id(DEFAULT_DARK_THEME_ID).expect("strata-dark");
        assert_eq!(dark.appearance, ThemeAppearance::Dark);
        assert_eq!(dark.background, hex(0x16181d));
        assert_eq!(dark.chrome, hex(0x1b1e24));
        assert_eq!(dark.sidebar, hex(0x1b1e24));
        assert_eq!(dark.panel, hex(0x1d2027));
        assert_eq!(dark.field, hex(0x262a32));
        assert_eq!(dark.border, hex(0x2c3039));
        assert_eq!(dark.text, hex(0xe6e8ec));
        assert_eq!(dark.muted, hex(0xa0a6b1));
        assert_eq!(dark.selected, hex(0x2a3350));
        assert_eq!(dark.row_selected, hex(0x243052));
        assert_eq!(dark.accent, hex(0x6d8cf0));
        assert_eq!(dark.accent_soft, hex(0x26314f));
        assert_eq!(dark.accent_text, hex(0xb8c8ff));
        assert_eq!(dark.on_accent, hex(0xffffff));
        assert_eq!(dark.add_background, hex(0x173022));
        assert_eq!(dark.add_text, hex(0x5fd08f));
        assert_eq!(dark.remove_background, hex(0x3a1f22));
        assert_eq!(dark.remove_text, hex(0xf08a80));
        assert_eq!(dark.remote_background, hex(0x2a2e36));
        assert_eq!(dark.remote_text, hex(0xc9ced8));
        assert_eq!(dark.tag_background, hex(0x3b2f1a));
        assert_eq!(dark.tag_text, hex(0xf1c27d));
        assert_eq!(dark.diff_added_bg, hex(0x12291c));
        assert_eq!(dark.diff_removed_bg, hex(0x331a1e));
        assert_eq!(dark.diff_added_word, hex(0x173a24));
        assert_eq!(dark.diff_removed_word, hex(0x4a2126));
        assert_eq!(dark.diff_hunk_header, hex(0x23262e));
        assert_eq!(dark.diff_gutter_fg, hex(0x9aa2af));
        assert_eq!(dark.diff_whitespace, hex(0x78818f));
        assert_eq!(dark.syntax_keyword, hex(0xe59ac9));
        assert_eq!(dark.syntax_string, hex(0x5fd08f));
        assert_eq!(dark.syntax_comment, hex(0xb0b6c1));
        assert_eq!(dark.syntax_function, hex(0xb8c8ff));
        assert_eq!(dark.syntax_type, hex(0xf1c27d));
        assert_eq!(dark.syntax_number, hex(0x7fd6e8));
        assert_eq!(dark.syntax_tag, hex(0xf49a91));
        assert_eq!(dark.lane, expected_lanes);
        assert_eq!(dark.avatar, expected_avatars);
        assert_eq!(dark.terminal_foreground, hex(0xe6e8ec));
        assert_eq!(dark.terminal_background, hex(0x16181d));
        assert_eq!(dark.terminal_cursor, hex(0x6d8cf0));
        assert_eq!(dark.terminal_selection, hex(0x2a3350).opacity(0.8));
        assert_eq!(
            dark.terminal_ansi,
            [
                hex(0x1d2027),
                hex(0xf08a80),
                hex(0x5fd08f),
                hex(0xf1c27d),
                hex(0x6d8cf0),
                hex(0xe59ac9),
                hex(0x7fd6e8),
                hex(0xc9ced8),
                hex(0x737a88),
                hex(0xff9e96),
                hex(0x7ee0a8),
                hex(0xfad493),
                hex(0x9cb2f8),
                hex(0xf0b3da),
                hex(0x9ce3f2),
                hex(0xe6e8ec),
            ]
        );
    }
}
