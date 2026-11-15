//! Data-driven colour themes parsed from TOML definitions.
//!
//! Bundled themes are embedded at compile time from `resources/themes/*.toml`,
//! while user-supplied `*.toml` themes are loaded from `<config dir>/themes/`
//! (honouring `STRATA_CONFIG_DIR`) using the same [`ThemePalette::from_toml`]
//! parser.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use gpui::{App, Hsla, rgb, rgba};
use serde::de::{self, Visitor};
use serde::{Deserialize, Serialize};

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

const EXPORT_HEADER: &str = "\
# Strata custom colour theme.\n\
# Edit any #rrggbb or #rrggbbaa hex colour below.\n\
# Keep `id` unique (different from bundled themes) and set `name` for the picker.\n\
# Every token in [colors], [terminal], and [graph] is required.\n\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeAppearance {
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeSource {
    Bundled,
    User,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThemeEntry {
    pub palette: Arc<ThemePalette>,
    pub source: ThemeSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeErrorKind {
    ReadDirFailed(String),
    ReadFailed(String),
    ParseFailed(String),
    CollidesWithBundled(String),
    DuplicateUserId(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeLoadError {
    pub file: String,
    pub kind: ThemeErrorKind,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct LoadedUserThemes {
    pub themes: Vec<Arc<ThemePalette>>,
    pub errors: Vec<ThemeLoadError>,
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

    fn from_hsla(color: Hsla) -> Self {
        let rgba = color.to_rgb();
        Self {
            r: (rgba.r.clamp(0.0, 1.0) * 255.0).round() as u8,
            g: (rgba.g.clamp(0.0, 1.0) * 255.0).round() as u8,
            b: (rgba.b.clamp(0.0, 1.0) * 255.0).round() as u8,
            a: (rgba.a.clamp(0.0, 1.0) * 255.0).round() as u8,
        }
    }

    fn format(self) -> String {
        if self.a == 0xff {
            format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            format!("#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
        }
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

impl Serialize for HexColor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.format())
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTheme {
    id: String,
    name: String,
    appearance: ThemeAppearance,
    colors: RawColors,
    terminal: RawTerminal,
    graph: RawGraph,
}

#[derive(Serialize, Deserialize)]
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

#[derive(Serialize, Deserialize)]
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGraph {
    lanes: Vec<HexColor>,
}

/// Complete colour palette for one theme, resolved once from TOML.
#[derive(Debug, Clone, PartialEq)]
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
    pub terminal_foreground: Hsla,
    pub terminal_background: Hsla,
    pub terminal_cursor: Hsla,
    pub terminal_selection: Hsla,
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

    /// Serialises the complete palette as TOML with a header comment so users
    /// can start from a valid file containing every required token.
    pub fn to_toml(&self) -> String {
        let raw = RawTheme {
            id: self.id.clone(),
            name: self.name.clone(),
            appearance: self.appearance,
            colors: RawColors {
                background: HexColor::from_hsla(self.background),
                chrome: HexColor::from_hsla(self.chrome),
                sidebar: HexColor::from_hsla(self.sidebar),
                panel: HexColor::from_hsla(self.panel),
                field: HexColor::from_hsla(self.field),
                border: HexColor::from_hsla(self.border),
                text: HexColor::from_hsla(self.text),
                muted: HexColor::from_hsla(self.muted),
                selected: HexColor::from_hsla(self.selected),
                row_selected: HexColor::from_hsla(self.row_selected),
                accent: HexColor::from_hsla(self.accent),
                accent_soft: HexColor::from_hsla(self.accent_soft),
                accent_text: HexColor::from_hsla(self.accent_text),
                on_accent: HexColor::from_hsla(self.on_accent),
                add_background: HexColor::from_hsla(self.add_background),
                add_text: HexColor::from_hsla(self.add_text),
                remove_background: HexColor::from_hsla(self.remove_background),
                remove_text: HexColor::from_hsla(self.remove_text),
                remote_background: HexColor::from_hsla(self.remote_background),
                remote_text: HexColor::from_hsla(self.remote_text),
                tag_background: HexColor::from_hsla(self.tag_background),
                tag_text: HexColor::from_hsla(self.tag_text),
                diff_added_bg: HexColor::from_hsla(self.diff_added_bg),
                diff_removed_bg: HexColor::from_hsla(self.diff_removed_bg),
                diff_added_word: HexColor::from_hsla(self.diff_added_word),
                diff_removed_word: HexColor::from_hsla(self.diff_removed_word),
                diff_hunk_header: HexColor::from_hsla(self.diff_hunk_header),
                diff_gutter_fg: HexColor::from_hsla(self.diff_gutter_fg),
                diff_whitespace: HexColor::from_hsla(self.diff_whitespace),
                syntax_keyword: HexColor::from_hsla(self.syntax_keyword),
                syntax_string: HexColor::from_hsla(self.syntax_string),
                syntax_comment: HexColor::from_hsla(self.syntax_comment),
                syntax_function: HexColor::from_hsla(self.syntax_function),
                syntax_type: HexColor::from_hsla(self.syntax_type),
                syntax_number: HexColor::from_hsla(self.syntax_number),
                syntax_tag: HexColor::from_hsla(self.syntax_tag),
                avatar: self.avatar.map(HexColor::from_hsla),
            },
            terminal: RawTerminal {
                foreground: HexColor::from_hsla(self.terminal_foreground),
                background: HexColor::from_hsla(self.terminal_background),
                cursor: HexColor::from_hsla(self.terminal_cursor),
                selection: HexColor::from_hsla(self.terminal_selection),
                black: HexColor::from_hsla(self.terminal_ansi[0]),
                red: HexColor::from_hsla(self.terminal_ansi[1]),
                green: HexColor::from_hsla(self.terminal_ansi[2]),
                yellow: HexColor::from_hsla(self.terminal_ansi[3]),
                blue: HexColor::from_hsla(self.terminal_ansi[4]),
                magenta: HexColor::from_hsla(self.terminal_ansi[5]),
                cyan: HexColor::from_hsla(self.terminal_ansi[6]),
                white: HexColor::from_hsla(self.terminal_ansi[7]),
                bright_black: HexColor::from_hsla(self.terminal_ansi[8]),
                bright_red: HexColor::from_hsla(self.terminal_ansi[9]),
                bright_green: HexColor::from_hsla(self.terminal_ansi[10]),
                bright_yellow: HexColor::from_hsla(self.terminal_ansi[11]),
                bright_blue: HexColor::from_hsla(self.terminal_ansi[12]),
                bright_magenta: HexColor::from_hsla(self.terminal_ansi[13]),
                bright_cyan: HexColor::from_hsla(self.terminal_ansi[14]),
                bright_white: HexColor::from_hsla(self.terminal_ansi[15]),
            },
            graph: RawGraph {
                lanes: self.lane.into_iter().map(HexColor::from_hsla).collect(),
            },
        };
        let body = toml::to_string_pretty(&raw).expect("theme serialization cannot fail");
        format!("{EXPORT_HEADER}{body}")
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

static BUNDLED_THEMES: LazyLock<Vec<Arc<ThemePalette>>> = LazyLock::new(|| {
    BUNDLED_SOURCES
        .iter()
        .map(|(file, source)| {
            let palette = ThemePalette::from_toml(source)
                .unwrap_or_else(|error| panic!("bundled theme {file} is invalid: {error}"));
            Arc::new(palette)
        })
        .collect()
});

static USER_THEMES: RwLock<LoadedUserThemes> = RwLock::new(LoadedUserThemes {
    themes: Vec::new(),
    errors: Vec::new(),
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

fn bundled_by_id(id: &str) -> Option<&'static Arc<ThemePalette>> {
    BUNDLED_THEMES.iter().find(|theme| theme.id == id)
}

/// Directory where user-supplied `*.toml` themes live (`<config dir>/themes`).
pub fn user_themes_dir() -> Option<PathBuf> {
    crate::settings::config_file("themes")
}

/// Loads all `*.toml` user themes from `dir`, rejecting bundled id collisions
/// and collecting per-file errors without panicking.
pub fn load_from_dir(dir: &Path) -> LoadedUserThemes {
    let mut loaded = LoadedUserThemes::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return loaded,
        Err(error) => {
            log::warn!("cannot read themes directory {}: {error}", dir.display());
            loaded.errors.push(ThemeLoadError {
                file: dir.display().to_string(),
                kind: ThemeErrorKind::ReadDirFailed(error.to_string()),
            });
            return loaded;
        }
    };

    let mut files = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) == Some("toml") && path.is_file() {
                    files.push((entry.file_name(), path));
                }
            }
            Err(error) => {
                log::warn!("cannot inspect themes entry in {}: {error}", dir.display());
                loaded.errors.push(ThemeLoadError {
                    file: dir.display().to_string(),
                    kind: ThemeErrorKind::ReadDirFailed(error.to_string()),
                });
            }
        }
    }
    files.sort_by(|(left, _), (right, _)| left.cmp(right));

    let mut seen_user_ids = BTreeSet::new();
    for (os_name, path) in files {
        let file = os_name.to_string_lossy().into_owned();
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) => {
                log::warn!("cannot read theme {}: {error}", path.display());
                loaded.errors.push(ThemeLoadError {
                    file,
                    kind: ThemeErrorKind::ReadFailed(error.to_string()),
                });
                continue;
            }
        };
        let palette = match ThemePalette::from_toml(&source) {
            Ok(palette) => palette,
            Err(error) => {
                log::warn!("invalid theme {}: {error}", path.display());
                loaded.errors.push(ThemeLoadError {
                    file,
                    kind: ThemeErrorKind::ParseFailed(error),
                });
                continue;
            }
        };
        if bundled_by_id(&palette.id).is_some() {
            log::warn!(
                "ignoring user theme {}: id {:?} collides with a bundled theme",
                path.display(),
                palette.id
            );
            loaded.errors.push(ThemeLoadError {
                file,
                kind: ThemeErrorKind::CollidesWithBundled(palette.id),
            });
            continue;
        }
        if !seen_user_ids.insert(palette.id.clone()) {
            log::warn!(
                "ignoring user theme {}: duplicate theme id {:?}",
                path.display(),
                palette.id
            );
            loaded.errors.push(ThemeLoadError {
                file,
                kind: ThemeErrorKind::DuplicateUserId(palette.id),
            });
            continue;
        }
        loaded.themes.push(Arc::new(palette));
    }

    loaded.themes.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    loaded
}

fn sanitize_id_stem(id: &str) -> String {
    let cleaned: String = id
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "theme".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Writes `palette` into `dir` as `<id>-custom.toml` (or `-2`, `-3`, … without
/// overwriting existing files) with a unique non-colliding `id` so the user
/// starts from a valid, immediately loadable theme file.
pub fn export_palette_to_dir(palette: &ThemePalette, dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let existing = load_from_dir(dir);
    let existing_ids: BTreeSet<&str> = existing
        .themes
        .iter()
        .map(|theme| theme.id.as_str())
        .collect();

    let stem_base = sanitize_id_stem(&palette.id);
    let mut index = 1usize;
    let (stem, path) = loop {
        let stem = if index == 1 {
            format!("{stem_base}-custom")
        } else {
            format!("{stem_base}-custom-{index}")
        };
        let path = dir.join(format!("{stem}.toml"));
        if !path.exists() && bundled_by_id(&stem).is_none() && !existing_ids.contains(stem.as_str())
        {
            break (stem, path);
        }
        index += 1;
    };

    let mut exported = palette.clone();
    exported.id = stem;
    exported.name = if index == 1 {
        format!("{} (Custom)", palette.name)
    } else {
        format!("{} (Custom {index})", palette.name)
    };
    std::fs::write(&path, exported.to_toml()).map_err(|error| error.to_string())?;
    Ok(path)
}

/// Exports `palette` into the active user themes directory.
pub fn export_to_user_dir(palette: &ThemePalette) -> Result<PathBuf, String> {
    let dir = user_themes_dir().ok_or_else(|| "configuration directory is unknown".to_owned())?;
    export_palette_to_dir(palette, &dir)
}

/// Reloads user themes from disk into the global registry without touching GPUI.
pub fn reload_from_disk() {
    let next = user_themes_dir()
        .map(|dir| load_from_dir(&dir))
        .unwrap_or_default();
    if let Ok(mut guard) = USER_THEMES.write() {
        *guard = next;
    }
}

/// Loads user themes once at startup before any window opens.
pub fn init(_cx: &mut App) {
    reload_from_disk();
}

/// Reloads user themes from disk, re-applies the active theme, and refreshes
/// every open window.
pub fn reload(cx: &mut App) {
    reload_from_disk();
    crate::settings::apply_theme(None, cx);
    cx.refresh_windows();
}

type DirFingerprint = Vec<(String, Option<SystemTime>, u64)>;

fn dir_fingerprint(dir: &Path) -> DirFingerprint {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut fingerprint = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        fingerprint.push((
            entry.file_name().to_string_lossy().into_owned(),
            meta.modified().ok(),
            meta.len(),
        ));
    }
    fingerprint.sort_by(|left, right| left.0.cmp(&right.0));
    fingerprint
}

/// Polls the user themes directory on the background executor and live-reloads
/// whenever a `*.toml` theme file is added, modified, or removed.
pub fn watch(cx: &mut App) {
    let Some(dir) = user_themes_dir() else {
        return;
    };
    cx.spawn(async move |cx| {
        let initial_dir = dir.clone();
        let mut last = cx
            .background_executor()
            .spawn(async move { dir_fingerprint(&initial_dir) })
            .await;
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(700))
                .await;
            let scan_dir = dir.clone();
            let next = cx
                .background_executor()
                .spawn(async move { dir_fingerprint(&scan_dir) })
                .await;
            if next == last {
                continue;
            }
            // Wait briefly for editors that write through temp files or
            // truncate-then-write bursts before parsing.
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            let settled_dir = dir.clone();
            last = cx
                .background_executor()
                .spawn(async move { dir_fingerprint(&settled_dir) })
                .await;
            cx.update(reload);
        }
    })
    .detach();
}

/// Every loaded theme (bundled first, then user themes) in display order.
pub fn all() -> Vec<ThemeEntry> {
    let mut result: Vec<ThemeEntry> = BUNDLED_THEMES
        .iter()
        .map(|palette| ThemeEntry {
            palette: Arc::clone(palette),
            source: ThemeSource::Bundled,
        })
        .collect();
    if let Ok(guard) = USER_THEMES.read() {
        result.extend(guard.themes.iter().map(|palette| ThemeEntry {
            palette: Arc::clone(palette),
            source: ThemeSource::User,
        }));
    }
    result
}

/// Themes matching `appearance`, bundled first then user themes.
pub fn for_appearance(appearance: ThemeAppearance) -> Vec<ThemeEntry> {
    all()
        .into_iter()
        .filter(|entry| entry.palette.appearance == appearance)
        .collect()
}

/// Looks up a bundled or loaded user theme by `id`.
pub fn by_id(id: &str) -> Option<ThemeEntry> {
    if let Some(palette) = bundled_by_id(id) {
        return Some(ThemeEntry {
            palette: Arc::clone(palette),
            source: ThemeSource::Bundled,
        });
    }
    if let Ok(guard) = USER_THEMES.read()
        && let Some(palette) = guard.themes.iter().find(|theme| theme.id == id)
    {
        return Some(ThemeEntry {
            palette: Arc::clone(palette),
            source: ThemeSource::User,
        });
    }
    None
}

/// Errors collected during the most recent user theme load.
pub fn load_errors() -> Vec<ThemeLoadError> {
    USER_THEMES
        .read()
        .map(|guard| guard.errors.clone())
        .unwrap_or_default()
}

/// Default bundled theme for the given appearance.
pub fn default_for(is_dark: bool) -> Arc<ThemePalette> {
    let id = if is_dark {
        DEFAULT_DARK_THEME_ID
    } else {
        DEFAULT_LIGHT_THEME_ID
    };
    Arc::clone(bundled_by_id(id).expect("default bundled theme must exist"))
}

/// Resolves `id` to a theme entry, warning once and falling back to the default
/// bundled theme for `is_dark` when `id` is unknown.
pub fn resolve_entry(id: &str, is_dark: bool) -> ThemeEntry {
    if let Some(entry) = by_id(id) {
        return entry;
    }
    let fallback = default_for(is_dark);
    warn_unknown_once(id, &fallback.id);
    ThemeEntry {
        palette: fallback,
        source: ThemeSource::Bundled,
    }
}

/// Resolves `id` to a theme palette, warning once and falling back to the
/// default for `is_dark` when `id` is unknown.
pub fn resolve(id: &str, is_dark: bool) -> Arc<ThemePalette> {
    resolve_entry(id, is_dark).palette
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
        assert_eq!(for_appearance(ThemeAppearance::Light).len(), 2);
        assert_eq!(for_appearance(ThemeAppearance::Dark).len(), 4);
    }

    #[test]
    fn missing_tokens_are_rejected_without_silent_defaults() {
        // A Windows checkout may convert the bundled files to CRLF, and the
        // needles below are written with plain newlines.
        let normalized = BUNDLED_SOURCES[0].1.replace("\r\n", "\n");
        let valid = normalized.as_str();
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
    fn loading_from_dir_handles_valid_invalid_colliding_and_non_toml_files() {
        let dir = tempfile::tempdir().expect("temp dir");

        // Valid user theme (modified copy of bundled dark theme).
        let mut custom = by_id(DEFAULT_DARK_THEME_ID)
            .expect("strata-dark")
            .palette
            .as_ref()
            .clone();
        custom.id = "midnight-custom".to_owned();
        custom.name = "Midnight Custom".to_owned();
        std::fs::write(dir.path().join("midnight.toml"), custom.to_toml()).expect("write valid");

        // Invalid TOML / bad hex / missing token.
        std::fs::write(
            dir.path().join("broken.toml"),
            BUNDLED_SOURCES[0].1.replacen("#ffffff", "#zzzzzz", 1),
        )
        .expect("write invalid");

        // Id colliding with a bundled theme must be rejected and reported.
        std::fs::write(dir.path().join("override.toml"), BUNDLED_SOURCES[0].1)
            .expect("write colliding");

        // Duplicate id across two user files: second file fails cleanly.
        let mut dup = custom.clone();
        dup.name = "Midnight Duplicate".to_owned();
        std::fs::write(dir.path().join("zzz-dup.toml"), dup.to_toml()).expect("write duplicate");

        // Non-TOML file and subdirectory should both be ignored quietly.
        std::fs::write(dir.path().join("notes.txt"), "not a theme").expect("write txt");
        std::fs::create_dir(dir.path().join("nested.toml")).expect("mkdir");

        let loaded = load_from_dir(dir.path());
        assert_eq!(loaded.themes.len(), 1);
        assert_eq!(loaded.themes[0].id, "midnight-custom");
        assert_eq!(loaded.themes[0].name, "Midnight Custom");

        assert_eq!(loaded.errors.len(), 3);
        assert_eq!(loaded.errors[0].file, "broken.toml");
        assert!(matches!(
            loaded.errors[0].kind,
            ThemeErrorKind::ParseFailed(_)
        ));
        assert_eq!(loaded.errors[1].file, "override.toml");
        assert_eq!(
            loaded.errors[1].kind,
            ThemeErrorKind::CollidesWithBundled(DEFAULT_LIGHT_THEME_ID.to_owned())
        );
        assert_eq!(loaded.errors[2].file, "zzz-dup.toml");
        assert_eq!(
            loaded.errors[2].kind,
            ThemeErrorKind::DuplicateUserId("midnight-custom".to_owned())
        );
    }

    #[test]
    fn missing_themes_dir_loads_empty_without_errors() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("does-not-exist");
        let loaded = load_from_dir(&missing);
        assert!(loaded.themes.is_empty());
        assert!(loaded.errors.is_empty());
    }

    #[test]
    fn export_and_reparse_round_trip_matches_every_bundled_palette() {
        for (file, _) in BUNDLED_SOURCES {
            let entry = all()
                .into_iter()
                .find(|entry| format!("{}.toml", entry.palette.id) == *file)
                .unwrap_or_else(|| panic!("missing bundled theme for {file}"));
            let serialized = entry.palette.to_toml();
            let reparsed = ThemePalette::from_toml(&serialized)
                .unwrap_or_else(|error| panic!("round-trip parse failed for {file}: {error}"));
            assert_eq!(
                &reparsed,
                entry.palette.as_ref(),
                "round-trip mismatch for {file}"
            );
        }

        let dir = tempfile::tempdir().expect("temp dir");
        let source = &by_id(DEFAULT_DARK_THEME_ID).expect("strata-dark").palette;
        let first = export_palette_to_dir(source, dir.path()).expect("first export");
        assert_eq!(
            first.file_name().and_then(|n| n.to_str()),
            Some("strata-dark-custom.toml")
        );
        let second = export_palette_to_dir(source, dir.path()).expect("second export");
        assert_eq!(
            second.file_name().and_then(|n| n.to_str()),
            Some("strata-dark-custom-2.toml")
        );

        let loaded = load_from_dir(dir.path());
        assert!(loaded.errors.is_empty());
        assert_eq!(loaded.themes.len(), 2);
        let first_loaded = loaded
            .themes
            .iter()
            .find(|theme| theme.id == "strata-dark-custom")
            .expect("strata-dark-custom should be loaded");
        assert!(
            loaded
                .themes
                .iter()
                .any(|theme| theme.id == "strata-dark-custom-2")
        );
        let mut normalized = first_loaded.as_ref().clone();
        normalized.id = source.id.clone();
        normalized.name = source.name.clone();
        assert_eq!(&normalized, source.as_ref());
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

        let light = &by_id(DEFAULT_LIGHT_THEME_ID).expect("strata-light").palette;
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

        let dark = &by_id(DEFAULT_DARK_THEME_ID).expect("strata-dark").palette;
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
