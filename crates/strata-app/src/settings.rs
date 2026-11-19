//! Strata's own preferences: appearance, diff display, history display and
//! app behaviour.
//!
//! These are app settings, stored in one JSON file in the platform's config
//! directory. They never write git config; anything that maps to a git option
//! is passed to git per command instead.
//!
//! The current value is a GPUI global. Read it with [`Settings::get`], change it
//! with [`update`] (which notifies observers at once and saves a moment later),
//! and react to changes with `cx.observe_global::<Settings>(…)`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use gpui::{App, BorrowAppContext as _, Global, Window};
use gpui_component::Theme;
use serde::{Deserialize, Serialize};

use crate::i18n::Language;

/// Every field has a default, so a file written by an older or newer build
/// still loads: missing keys take the default and unknown keys are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    pub diff: DiffSettings,
    pub history: HistorySettings,
    pub layout: LayoutSettings,
    pub behaviour: Behaviour,
    pub tools: Tools,
    pub privacy: Privacy,
    pub commit_graph: CommitGraphSettings,
    pub updates: UpdateSettings,
    pub proxy: ProxySettings,
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
    /// Theme id selected when the resolved appearance is light.
    pub light_theme: String,
    /// Theme id selected when the resolved appearance is dark.
    pub dark_theme: String,
    /// The interface language. `System` follows the operating system; a file
    /// written before this key existed takes the default.
    pub language: Language,
    /// `None` uses the platform's UI font.
    pub ui_font: Option<String>,
    pub ui_font_size: f32,
    /// `None` uses the bundled monospace default.
    pub code_font: Option<String>,
    pub code_font_size: f32,
    pub tab_width: u8,
    /// The whole-window zoom in percent; see [`ZOOM_STEPS`].
    pub zoom_percent: u16,
}

/// The zoom steps the settings window offers, in percent.
///
/// They are fixed rather than free: every window is laid out from a handful of
/// type sizes, and a step keeps the same proportions in all of them while the
/// lists still fit their rows.
pub const ZOOM_STEPS: [u16; 8] = [80, 90, 100, 110, 125, 150, 175, 200];
/// The zoom a settings file without the key opens at.
pub const DEFAULT_ZOOM: u16 = 100;

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            light_theme: crate::style::theme::DEFAULT_LIGHT_THEME_ID.to_owned(),
            dark_theme: crate::style::theme::DEFAULT_DARK_THEME_ID.to_owned(),
            language: Language::System,
            ui_font: None,
            ui_font_size: 13.0,
            code_font: None,
            code_font_size: 12.5,
            tab_width: 4,
            zoom_percent: DEFAULT_ZOOM,
        }
    }
}

impl Appearance {
    /// The factor every window is drawn at: 1.0 at 100%.
    pub fn zoom(&self) -> f32 {
        f32::from(normalize_zoom(self.zoom_percent)) / 100.0
    }
}

/// The offered step nearest to `percent`.
///
/// A hand-edited file can name any number; snapping keeps every window on one
/// of the sizes the settings window can show.
pub fn normalize_zoom(percent: u16) -> u16 {
    ZOOM_STEPS
        .into_iter()
        .min_by_key(|step| step.abs_diff(percent))
        .unwrap_or(DEFAULT_ZOOM)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DiffLayout {
    #[default]
    Unified,
    Split,
}

/// How the image diff view lays its two sides out.
///
/// The mode is a preference rather than per-file state: a user who compares
/// pictures wants the same arrangement for the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ImageDiffMode {
    /// Old beside new.
    #[default]
    SideBySide,
    /// The two pictures on top of each other, split by a draggable divider.
    Swipe,
    /// The new picture over the old one, at an adjustable opacity.
    OnionSkin,
}

impl ImageDiffMode {
    /// The order the mode buttons appear in.
    pub const ALL: [ImageDiffMode; 3] = [
        ImageDiffMode::SideBySide,
        ImageDiffMode::Swipe,
        ImageDiffMode::OnionSkin,
    ];

    pub fn label(self) -> String {
        match self {
            ImageDiffMode::SideBySide => t!("image_diff.mode.side_by_side"),
            ImageDiffMode::Swipe => t!("image_diff.mode.swipe"),
            ImageDiffMode::OnionSkin => t!("image_diff.mode.onion_skin"),
        }
        .to_string()
    }

    /// The stable id used in element ids and the saved file.
    pub fn id(self) -> &'static str {
        match self {
            ImageDiffMode::SideBySide => "side-by-side",
            ImageDiffMode::Swipe => "swipe",
            ImageDiffMode::OnionSkin => "onion-skin",
        }
    }
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
    /// The last image diff mode the user picked.
    pub image_mode: ImageDiffMode,
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
            image_mode: ImageDiffMode::SideBySide,
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

/// How the details pane's changed-file list is arranged.
///
/// A preference rather than per-commit state: a user who reads changes as a
/// tree wants the next commit's files that way too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FileListMode {
    /// One row per file, with its directory trailing the name.
    #[default]
    List,
    /// Rows grouped under collapsible directories.
    Tree,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistorySettings {
    pub show_remote_branches: bool,
    pub show_tags: bool,
    pub show_stashes: bool,
    pub show_commit_signatures: bool,
    pub show_changed_file_count: bool,
    pub show_uncommitted_changes: bool,
    pub date_format: DateFormat,
    pub file_list_mode: FileListMode,
    /// Refs hidden from the history graph, one entry per repository.
    pub hidden_refs: Vec<HiddenHistoryRefs>,
}

/// The refs one repository's history graph leaves out.
///
/// A ref name alone would be ambiguous across repositories: `topic` is common,
/// and the same checkout can be opened under two paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiddenHistoryRefs {
    /// Canonical working-tree root, the key the commit-graph offer also uses.
    pub repository: PathBuf,
    /// Full ref names, e.g. `refs/heads/topic`.
    pub refs: Vec<String>,
}

impl HistorySettings {
    /// The full ref names hidden for the repository at `repository`.
    pub fn hidden_refs(&self, repository: &Path) -> Vec<String> {
        self.hidden_refs
            .iter()
            .find(|entry| entry.repository == repository)
            .map(|entry| entry.refs.clone())
            .unwrap_or_default()
    }

    /// Hides one more ref for `repository`, keeping the list sorted so the
    /// saved file does not depend on the order branches were hidden in.
    pub fn hide_ref(&mut self, repository: &Path, full_name: String) {
        let entry = match self
            .hidden_refs
            .iter_mut()
            .find(|entry| entry.repository == repository)
        {
            Some(entry) => entry,
            None => {
                self.hidden_refs.push(HiddenHistoryRefs {
                    repository: repository.to_path_buf(),
                    refs: Vec::new(),
                });
                self.hidden_refs.last_mut().expect("the entry just pushed")
            }
        };
        if !entry.refs.contains(&full_name) {
            entry.refs.push(full_name);
            entry.refs.sort();
        }
    }

    /// Shows every ref of `repository` again and forgets its entry.
    pub fn show_all_refs(&mut self, repository: &Path) {
        self.hidden_refs
            .retain(|entry| entry.repository != repository);
    }
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self {
            show_remote_branches: true,
            show_tags: true,
            show_stashes: true,
            show_commit_signatures: true,
            show_changed_file_count: true,
            show_uncommitted_changes: true,
            date_format: DateFormat::Relative,
            file_list_mode: FileListMode::List,
            hidden_refs: Vec::new(),
        }
    }
}

/// Fraction of the History area's height the commit list takes when the user
/// has not dragged the boundary.
pub const DEFAULT_HISTORY_LIST_FRACTION: f32 = 0.55;
/// Fraction of the details area's width the file list column takes by default.
pub const DEFAULT_HISTORY_FILES_FRACTION: f32 = 0.28;

/// The History view's draggable layout.
///
/// The fractions are the user's intent, not the pixels that happen to be on
/// screen: a smaller window keeps the same proportions. The splitters clamp
/// them to each side's minimum as they are applied, so a window too small for
/// both minimums never makes one side disappear.
///
/// One value each for the whole app rather than one per tab: a boundary dragged
/// in one repository is where the user expects it in the next.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutSettings {
    /// Share of the history area's height taken by the commit list.
    pub history_list_fraction: f32,
    /// Share of the details area's width taken by the file list column.
    pub history_files_fraction: f32,
    /// Whether the details area fills the history view and the commit list
    /// collapses to a one-row strip.
    pub history_details_maximized: bool,
    /// Whether the sidebar is folded to its narrow icon rail, giving the
    /// content the width it would take.
    pub sidebar_collapsed: bool,
}

impl Default for LayoutSettings {
    fn default() -> Self {
        Self {
            history_list_fraction: DEFAULT_HISTORY_LIST_FRACTION,
            history_files_fraction: DEFAULT_HISTORY_FILES_FRACTION,
            history_details_maximized: false,
            sidebar_collapsed: false,
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
    /// Shell command and optional arguments for the embedded terminal panel.
    /// `None` launches the platform's default shell.
    pub embedded_shell: Option<String>,
    pub ghq: GhqSettings,
    pub gh: GhSettings,
}

/// Optional ghq integration: the repository pickup in the picker and the
/// clone destination.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GhqSettings {
    /// On by default; the UI still disables it when ghq is not installed.
    pub enabled: bool,
    /// `None` uses the root `ghq root` reports.
    pub root: Option<PathBuf>,
}

impl Default for GhqSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            root: None,
        }
    }
}

/// Optional GitHub CLI integration. The app never handles a token: `gh` keeps
/// its own credentials.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GhSettings {
    pub enabled: bool,
}

impl Default for GhSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Privacy {
    /// Load author pictures from an external service. Off by default because
    /// it sends author email hashes over the network.
    pub external_avatars: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CommitGraphSettings {
    /// Canonical working-tree roots where the user chose "Don't ask for this
    /// repository". Empty means every repository may be asked once.
    pub ignored_repositories: Vec<PathBuf>,
}

impl CommitGraphSettings {
    /// Whether the offer may be shown for the repository at `repository`.
    pub fn asks(&self, repository: &Path) -> bool {
        !self
            .ignored_repositories
            .iter()
            .any(|ignored| ignored == repository)
    }

    /// Stops asking for one repository, without touching the others.
    pub fn ignore(&mut self, repository: PathBuf) {
        if !self.ignored_repositories.contains(&repository) {
            self.ignored_repositories.push(repository);
        }
    }
}

/// Application update checking preferences and persisted state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateSettings {
    /// Whether to check for new versions automatically on startup.
    pub auto_check: bool,
    /// Unix timestamp in seconds of the last completed update check.
    pub last_checked_at: Option<u64>,
    /// Release version string the user chose to skip (e.g. `0.1.7`).
    pub skipped_version: Option<String>,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            auto_check: true,
            last_checked_at: None,
            skipped_version: None,
        }
    }
}

/// How Strata routes network traffic through a proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProxyMode {
    /// Follow OS proxy settings (with fallback to standard proxy env vars).
    #[default]
    System,
    /// Disable proxy usage and clear inherited proxy environment variables.
    None,
    /// Route traffic through an explicitly configured proxy endpoint.
    Manual,
}

/// App-wide network proxy configuration.
///
/// The manual proxy password is stored in the OS credential store via
/// `crate::proxy`, never in `settings.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxySettings {
    pub mode: ProxyMode,
    pub scheme: strata_core::proxy::ProxyScheme,
    pub host: String,
    pub port: u16,
    pub username: String,
    /// Comma-separated hosts, domains, or CIDRs that bypass the proxy.
    pub no_proxy: String,
}

impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            mode: ProxyMode::System,
            scheme: strata_core::proxy::ProxyScheme::Http,
            host: String::new(),
            port: 8080,
            username: String::new(),
            no_proxy: String::new(),
        }
    }
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

/// How long a burst of changes settles before it is written. A text field
/// reports one change per keystroke, and `save` renames through one fixed temp
/// file, so two writes started in quick succession could interleave there.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(300);

/// Serialises writes to the settings file and coalesces a burst of changes into
/// a single write: a writer that wakes up after a newer change only records
/// that its generation is stale and writes nothing.
static SAVE: Mutex<SaveCoalescer> = Mutex::new(SaveCoalescer::new());

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct SaveCoalescer {
    /// Generation of the newest recorded change.
    latest: u64,
    /// Generation of the newest change a write has covered.
    saved: u64,
}

impl SaveCoalescer {
    const fn new() -> Self {
        Self {
            latest: 0,
            saved: 0,
        }
    }

    /// Records a change and returns the generation its writer must present.
    fn record(&mut self) -> u64 {
        self.latest += 1;
        self.latest
    }

    /// True when `generation` is still the newest change and no write covered
    /// it yet. Marks it covered, so two writers cannot both write it.
    fn take(&mut self, generation: u64) -> bool {
        if generation != self.latest || self.saved >= generation {
            return false;
        }
        self.saved = generation;
        true
    }

    /// True while a recorded change is still waiting for the debounce.
    fn pending(&self) -> bool {
        self.saved < self.latest
    }
}

fn save_logged(settings: &Settings, path: &Path) {
    if let Err(error) = settings.save(path) {
        log::warn!("cannot save {}: {error}", path.display());
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

/// A file in the configuration directory. `STRATA_CONFIG_DIR` overrides the
/// directory, for tests and portable setups.
pub fn config_file(name: &str) -> Option<PathBuf> {
    std::env::var_os("STRATA_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(config_dir)
        .map(|dir| dir.join(name))
}

/// `STRATA_CONFIG_DIR` overrides the location, for tests and portable setups.
fn settings_path() -> Option<PathBuf> {
    config_file("settings.json")
}

/// The file the settings are saved to, for the settings window to show.
pub fn settings_file() -> Option<PathBuf> {
    settings_path()
}

/// The git executable named by the saved file, for opening a repository from
/// the command line: that happens before a window (and so an [`App`]) exists,
/// so the settings global cannot be read yet.
pub fn saved_git_executable() -> Option<PathBuf> {
    settings_path()
        .map(|path| Settings::load(&path).behaviour.git_executable)
        .unwrap_or_default()
}

/// The proxy mode in the saved settings file, used when opening a repository
/// before GPUI's [`Settings`] global is initialised.
pub fn saved_proxy_mode() -> ProxyMode {
    settings_path()
        .map(|path| Settings::load(&path).proxy.mode)
        .unwrap_or_default()
}

/// Loads the settings into the app global. Call once at startup, before any
/// window opens.
pub fn init(cx: &mut App) {
    let settings = settings_path()
        .map(|path| Settings::load(&path))
        .unwrap_or_default();
    cx.set_global(settings);
    // Changes made in the debounce window would otherwise be lost on quit;
    // GPUI polls this before the process ends.
    cx.on_app_quit(|cx| {
        flush(cx);
        async {}
    })
    .detach();
}

/// Applies the theme setting. `System` follows the OS and is re-applied when
/// the window's appearance changes; the other modes pin the theme.
pub fn apply_theme(window: Option<&mut Window>, cx: &mut App) {
    match Settings::get(cx).appearance.theme {
        ThemeMode::System => Theme::sync_system_appearance(window, cx),
        ThemeMode::Light => Theme::change(gpui_component::ThemeMode::Light, window, cx),
        ThemeMode::Dark => Theme::change(gpui_component::ThemeMode::Dark, window, cx),
    }
    // Every theme change resets gpui-component's palette, so the tokens are
    // re-pointed at the design's colours right after it.
    crate::component_theme::sync(cx);
    // A theme reload also resets the base font size the component library
    // scales from; the saved zoom is written back on top of it.
    crate::zoom::apply(cx);
}

/// Changes the settings and notifies `observe_global::<Settings>` observers at
/// once. The file is written in the background once the changes have stopped
/// for [`SAVE_DEBOUNCE`], so typing in a text field does not start one write
/// per character.
pub fn update(cx: &mut App, change: impl FnOnce(&mut Settings)) {
    cx.update_global::<Settings, _>(|settings, _| change(settings));
    let snapshot = Settings::get(cx).clone();
    let Some(path) = settings_path() else {
        return;
    };
    let Ok(mut state) = SAVE.lock() else {
        return;
    };
    let generation = state.record();
    drop(state);

    let timer = cx.background_executor().timer(SAVE_DEBOUNCE);
    cx.background_executor()
        .spawn(async move {
            timer.await;
            // The lock is held across the write, not only across the decision:
            // a slower, older writer must not still be renaming while a newer
            // one starts on the same temp file.
            if let Ok(mut state) = SAVE.lock()
                && state.take(generation)
            {
                save_logged(&snapshot, &path);
            }
        })
        .detach();
}

/// Writes changes the debounce has not saved yet. Called on app quit.
fn flush(cx: &App) {
    let Some(path) = settings_path() else {
        return;
    };
    let Ok(mut state) = SAVE.lock() else {
        return;
    };
    if !state.pending() {
        return;
    }
    let latest = state.latest;
    if state.take(latest) {
        save_logged(Settings::get(cx), &path);
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
        settings.diff.image_mode = ImageDiffMode::OnionSkin;
        settings.behaviour.git_executable = Some(PathBuf::from("/opt/git/bin/git"));
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path), settings);
    }

    #[test]
    fn the_last_image_mode_is_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut settings = Settings::default();
        settings.diff.image_mode = ImageDiffMode::Swipe;
        settings.save(&path).unwrap();
        assert_eq!(
            Settings::load(&path).diff.image_mode,
            ImageDiffMode::Swipe,
            "the image view opens the way it was left"
        );
        std::fs::write(&path, r#"{"diff":{"layout":"split"}}"#).unwrap();
        assert_eq!(
            Settings::load(&path).diff.image_mode,
            ImageDiffMode::SideBySide,
            "a file without the key takes the default"
        );
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

    #[test]
    fn a_burst_of_changes_only_lets_the_newest_write() {
        let mut coalescer = SaveCoalescer::new();
        assert!(!coalescer.pending());

        let first = coalescer.record();
        let second = coalescer.record();
        let third = coalescer.record();
        assert!(coalescer.pending());

        // The writers of the first two keystrokes wake up after the third one
        // and write nothing.
        assert!(!coalescer.take(first));
        assert!(!coalescer.take(second));
        assert!(!coalescer.take(second), "a write happens at most once");
        assert!(coalescer.pending());

        assert!(coalescer.take(third));
        assert!(!coalescer.take(third));
        assert!(!coalescer.pending());
    }

    #[test]
    fn the_history_layout_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        let mut settings = Settings::default();
        assert_eq!(
            settings.layout.history_list_fraction,
            DEFAULT_HISTORY_LIST_FRACTION
        );
        settings.layout.history_list_fraction = 0.42;
        settings.layout.history_files_fraction = 0.31;
        settings.layout.history_details_maximized = true;
        settings.save(&path).unwrap();

        let loaded = Settings::load(&path);
        assert_eq!(loaded.layout, settings.layout);
        assert!(loaded.layout.history_details_maximized);
        assert!(
            (loaded.layout.history_list_fraction - 0.42).abs() < f32::EPSILON,
            "fractions are stored as fractions, not as the pixels of one window"
        );
    }

    #[test]
    fn the_sidebar_collapsed_state_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        let mut settings = Settings::default();
        assert!(
            !settings.layout.sidebar_collapsed,
            "the sidebar starts open"
        );
        settings.layout.sidebar_collapsed = true;
        settings.save(&path).unwrap();
        assert!(Settings::load(&path).layout.sidebar_collapsed);

        // A file written before the key existed opens the sidebar again.
        std::fs::write(&path, r#"{"layout":{"history_list_fraction":0.4}}"#).unwrap();
        assert!(!Settings::load(&path).layout.sidebar_collapsed);
    }

    #[test]
    fn a_file_without_the_layout_takes_the_default_split() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"appearance":{"theme":"dark"}}"#).unwrap();
        assert_eq!(Settings::load(&path).layout, LayoutSettings::default());
    }

    #[test]
    fn a_file_without_a_language_follows_the_system() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        // The file an older build wrote has no `language` key.
        std::fs::write(
            &path,
            r#"{"appearance":{"theme":"dark","ui_font_size":14.0}}"#,
        )
        .unwrap();
        let settings = Settings::load(&path);
        assert_eq!(settings.appearance.language, crate::i18n::Language::System);
        assert_eq!(settings.appearance.theme, ThemeMode::Dark);
        assert_eq!(settings.appearance.light_theme, "strata-light");
        assert_eq!(settings.appearance.dark_theme, "strata-dark");
    }

    #[test]
    fn selected_themes_survive_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut settings = Settings::default();
        settings.appearance.light_theme = "catppuccin-latte".to_owned();
        settings.appearance.dark_theme = "catppuccin-mocha".to_owned();
        settings.save(&path).unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded.appearance.light_theme, "catppuccin-latte");
        assert_eq!(loaded.appearance.dark_theme, "catppuccin-mocha");
    }

    #[test]
    fn the_language_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut settings = Settings::default();
        settings.appearance.language = crate::i18n::Language::Japanese;
        settings.save(&path).unwrap();
        assert_eq!(
            Settings::load(&path).appearance.language,
            crate::i18n::Language::Japanese
        );
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(
            json.contains("\"language\": \"japanese\""),
            "the file stores a stable id, not a display name: {json}"
        );
    }

    #[test]
    fn commit_graph_ignores_are_per_repository_and_survive_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let asked = PathBuf::from("/work/repo-a");
        let silenced = PathBuf::from("/work/repo-b");

        let mut settings = Settings::default();
        assert!(settings.commit_graph.asks(&asked));
        settings.commit_graph.ignore(silenced.clone());
        assert!(settings.commit_graph.asks(&asked));
        assert!(!settings.commit_graph.asks(&silenced));

        settings.save(&path).unwrap();
        let loaded = Settings::load(&path);
        assert!(loaded.commit_graph.asks(&asked));
        assert!(!loaded.commit_graph.asks(&silenced));
    }

    #[test]
    fn hidden_history_refs_are_per_repository_and_survive_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let first = PathBuf::from("/work/repo-a");
        let second = PathBuf::from("/work/repo-b");

        let mut settings = Settings::default();
        assert!(settings.history.hidden_refs(&first).is_empty());
        settings.history.hide_ref(&first, "refs/heads/topic".into());
        settings
            .history
            .hide_ref(&first, "refs/remotes/origin/topic".into());
        settings
            .history
            .hide_ref(&second, "refs/heads/other".into());

        settings.save(&path).unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(
            loaded.history.hidden_refs(&first),
            ["refs/heads/topic", "refs/remotes/origin/topic"],
            "one repository's hidden refs do not leak into another's"
        );
        assert_eq!(loaded.history.hidden_refs(&second), ["refs/heads/other"]);

        let mut loaded = loaded;
        loaded.history.show_all_refs(&first);
        assert!(loaded.history.hidden_refs(&first).is_empty());
        assert_eq!(
            loaded.history.hidden_refs(&second),
            ["refs/heads/other"],
            "showing all for one repository leaves the other alone"
        );
    }

    #[test]
    fn a_change_after_a_write_is_pending_again() {
        let mut coalescer = SaveCoalescer::new();
        let generation = coalescer.record();
        assert!(coalescer.take(generation));
        assert!(!coalescer.pending());

        let later = coalescer.record();
        assert!(coalescer.pending());
        assert!(
            !coalescer.take(generation),
            "an already covered generation cannot write the newer value"
        );
        assert!(coalescer.take(later));
    }

    #[test]
    fn no_changes_leave_nothing_to_flush() {
        let mut coalescer = SaveCoalescer::new();
        assert!(!coalescer.pending());
        assert!(!coalescer.take(0));
    }

    #[test]
    fn proxy_settings_round_trip_and_omit_password() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        // Missing `proxy` key defaults to System mode.
        std::fs::write(&path, r#"{"appearance":{"theme":"dark"}}"#).unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded.proxy, ProxySettings::default());
        assert_eq!(loaded.proxy.mode, ProxyMode::System);

        let settings = Settings {
            proxy: ProxySettings {
                mode: ProxyMode::Manual,
                scheme: strata_core::proxy::ProxyScheme::Socks5h,
                host: "proxy.example.com".into(),
                port: 1080,
                username: "alice".into(),
                no_proxy: "localhost,10.0.0.0/8".into(),
            },
            ..Settings::default()
        };
        settings.save(&path).unwrap();

        let json = std::fs::read_to_string(&path).unwrap();
        assert!(
            !json.contains("\"password\""),
            "password must never be stored in settings.json: {json}"
        );
        assert_eq!(Settings::load(&path), settings);
    }
}
