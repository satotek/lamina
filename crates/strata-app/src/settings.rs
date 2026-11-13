//! Strata's own preferences: appearance, diff display, history display and
//! app behaviour.
//!
//! These are app settings, stored in one JSON file in the platform's config
//! directory. They never write git config; anything that maps to a git option
//! is passed to git per command instead.
//!
//! The current value is a GPUI global. Read it with [`Settings::get`], change it
//! with [`update`] (which saves in the background and notifies observers), and
//! react to changes with `cx.observe_global::<Settings>(…)`.

use std::path::{Path, PathBuf};

use gpui::{App, BorrowAppContext as _, Global, Window};
use gpui_component::Theme;
use serde::{Deserialize, Serialize};

/// Every field has a default, so a file written by an older or newer build
/// still loads: missing keys take the default and unknown keys are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    pub diff: DiffSettings,
    pub history: HistorySettings,
    pub behaviour: Behaviour,
    pub tools: Tools,
    pub privacy: Privacy,
}

impl Global for Settings {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    /// Follow the OS light/dark setting.
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub theme: ThemeMode,
    /// `None` uses the platform's UI font.
    pub ui_font: Option<String>,
    pub ui_font_size: f32,
    /// `None` uses the bundled monospace default.
    pub code_font: Option<String>,
    pub code_font_size: f32,
    pub tab_width: u8,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            ui_font: None,
            ui_font_size: 13.0,
            code_font: None,
            code_font_size: 12.5,
            tab_width: 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DiffLayout {
    #[default]
    Unified,
    Split,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiffSettings {
    pub layout: DiffLayout,
    pub ignore_whitespace: bool,
    /// Unchanged lines shown around each change (git's default is 3).
    pub context_lines: u32,
    pub wrap_lines: bool,
    pub syntax_highlighting: bool,
    pub show_whitespace: bool,
}

impl Default for DiffSettings {
    fn default() -> Self {
        Self {
            layout: DiffLayout::Unified,
            ignore_whitespace: false,
            context_lines: 3,
            wrap_lines: false,
            syntax_highlighting: true,
            show_whitespace: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DateFormat {
    /// "2 hours ago" for recent commits, the date after a week.
    #[default]
    Relative,
    /// `2026-11-13 14:05` in local time.
    Absolute,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistorySettings {
    pub show_remote_branches: bool,
    pub show_tags: bool,
    pub show_stashes: bool,
    pub date_format: DateFormat,
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self {
            show_remote_branches: true,
            show_tags: true,
            show_stashes: true,
            date_format: DateFormat::Relative,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Behaviour {
    /// `None` finds `git` on PATH.
    pub git_executable: Option<PathBuf>,
    /// Background fetch interval; 0 disables it.
    pub auto_fetch_minutes: u32,
    pub prune_on_fetch: bool,
    /// Summary length the commit box warns past.
    pub summary_length_hint: u32,
    /// Ask before discarding changes, deleting branches or force pushing.
    pub confirm_destructive: bool,
    /// Reopen the repositories that were open at quit.
    pub restore_last_repository: bool,
}

impl Default for Behaviour {
    fn default() -> Self {
        Self {
            git_executable: None,
            auto_fetch_minutes: 0,
            prune_on_fetch: true,
            summary_length_hint: 72,
            confirm_destructive: true,
            restore_last_repository: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Tools {
    /// Command to open a file in the user's editor; `{path}` and `{line}`
    /// are substituted. `None` uses the OS default application.
    pub editor: Option<String>,
    /// Command to open a terminal in the repository; `{path}` is substituted.
    /// `None` uses the platform's default terminal.
    pub terminal: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Privacy {
    /// Load author pictures from an external service. Off by default because
    /// it sends author email hashes over the network.
    pub external_avatars: bool,
}

impl Settings {
    pub fn get(cx: &App) -> &Settings {
        cx.global::<Settings>()
    }

    /// Reads `path`, falling back to defaults when it is missing or invalid.
    /// An invalid file is logged and left in place so the user can fix it.
    pub fn load(path: &Path) -> Settings {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                log::warn!("ignoring {}: {error}", path.display());
                Settings::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(error) => {
                log::warn!("cannot read {}: {error}", path.display());
                Settings::default()
            }
        }
    }

    /// Writes atomically (temp file + rename) so a crash mid-write never
    /// leaves a truncated settings file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, json)?;
        std::fs::rename(&temp, path)
    }
}

/// Where Strata keeps its settings and state files:
/// `~/Library/Application Support/Strata` on macOS, `%APPDATA%\Strata` on
/// Windows, `$XDG_CONFIG_HOME/strata` (default `~/.config/strata`) elsewhere.
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Application Support/Strata"))
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("Strata"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|dir| dir.join("strata"))
    }
}

/// `STRATA_CONFIG_DIR` overrides the location, for tests and portable setups.
fn settings_path() -> Option<PathBuf> {
    std::env::var_os("STRATA_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(config_dir)
        .map(|dir| dir.join("settings.json"))
}

/// Loads the settings into the app global. Call once at startup, before any
/// window opens.
pub fn init(cx: &mut App) {
    let settings = settings_path()
        .map(|path| Settings::load(&path))
        .unwrap_or_default();
    cx.set_global(settings);
}

/// Applies the theme setting. `System` follows the OS and is re-applied when
/// the window's appearance changes; the other modes pin the theme.
pub fn apply_theme(window: Option<&mut Window>, cx: &mut App) {
    match Settings::get(cx).appearance.theme {
        ThemeMode::System => Theme::sync_system_appearance(window, cx),
        ThemeMode::Light => Theme::change(gpui_component::ThemeMode::Light, window, cx),
        ThemeMode::Dark => Theme::change(gpui_component::ThemeMode::Dark, window, cx),
    }
}

/// Changes the settings, notifies `observe_global::<Settings>` observers and
/// saves the result off the UI thread.
#[allow(dead_code, reason = "the settings screen is the first caller")]
pub fn update(cx: &mut App, change: impl FnOnce(&mut Settings)) {
    cx.update_global::<Settings, _>(|settings, _| change(settings));
    let snapshot = Settings::get(cx).clone();
    if let Some(path) = settings_path() {
        cx.background_executor()
            .spawn(async move {
                if let Err(error) = snapshot.save(&path) {
                    log::warn!("cannot save {}: {error}", path.display());
                }
            })
            .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_file_loads_back_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/settings.json");
        let mut settings = Settings::default();
        settings.appearance.theme = ThemeMode::Dark;
        settings.diff.layout = DiffLayout::Split;
        settings.behaviour.git_executable = Some(PathBuf::from("/opt/git/bin/git"));
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path), settings);
    }

    #[test]
    fn missing_keys_take_defaults_and_unknown_keys_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"appearance":{"theme":"light"},"from_the_future":{"x":1}}"#,
        )
        .unwrap();
        let settings = Settings::load(&path);
        assert_eq!(settings.appearance.theme, ThemeMode::Light);
        assert_eq!(settings.appearance.code_font_size, 12.5);
        assert_eq!(settings.diff, DiffSettings::default());
    }

    #[test]
    fn an_unreadable_file_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
        assert!(path.exists(), "a broken file is left for the user to fix");
        assert_eq!(
            Settings::load(&dir.path().join("absent.json")),
            Settings::default()
        );
    }
}
