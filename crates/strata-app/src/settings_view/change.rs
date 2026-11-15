//! The pure half of the settings window: one function per control, mapping the
//! value the control produced onto [`Settings`].
//!
//! The field closures in [`super::pages`] do nothing else, which keeps rules
//! such as "sizes are clamped to what the editor can draw" testable without a
//! window, and keeps every control's write to the settings in one place.

use std::path::PathBuf;

use crate::settings::{
    Appearance, Behaviour, DateFormat, DiffLayout, DiffSettings, HistorySettings, Privacy,
    Settings, ThemeMode, Tools,
};

/// One page of the settings window. It is also the unit of "Reset section".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Appearance,
    Diff,
    History,
    Behaviour,
    Tools,
    Privacy,
    Keyboard,
    Commands,
}

impl Section {
    pub const ALL: [Section; 8] = [
        Section::Appearance,
        Section::Diff,
        Section::History,
        Section::Behaviour,
        Section::Tools,
        Section::Privacy,
        Section::Keyboard,
        Section::Commands,
    ];

    /// The stable id, which element ids and the reset button's name use.
    pub fn id(self) -> &'static str {
        match self {
            Section::Appearance => "appearance",
            Section::Diff => "diff",
            Section::History => "history",
            Section::Behaviour => "behaviour",
            Section::Tools => "tools",
            Section::Privacy => "privacy",
            Section::Keyboard => "keyboard",
            Section::Commands => "commands",
        }
    }

    /// The translated page name.
    pub fn title(self) -> String {
        match self {
            Section::Appearance => t!("settings.appearance.title").into_owned(),
            Section::Diff => t!("settings.diff.title").into_owned(),
            Section::History => t!("settings.history.title").into_owned(),
            Section::Behaviour => t!("settings.behaviour.title").into_owned(),
            Section::Tools => t!("settings.tools.title").into_owned(),
            Section::Privacy => t!("settings.privacy.title").into_owned(),
            Section::Keyboard => t!("settings.keyboard.title").into_owned(),
            Section::Commands => t!("settings.commands.title").into_owned(),
        }
    }
}

/// Restores one section, leaving the others alone.
pub fn reset_section(settings: &mut Settings, section: Section) {
    match section {
        Section::Appearance => settings.appearance = Appearance::default(),
        Section::Diff => settings.diff = DiffSettings::default(),
        Section::History => settings.history = HistorySettings::default(),
        Section::Behaviour => settings.behaviour = Behaviour::default(),
        Section::Tools => settings.tools = Tools::default(),
        Section::Privacy => settings.privacy = Privacy::default(),
        // The keymap and the command list are files of their own; the page's
        // buttons reset them where the user can see the result.
        Section::Keyboard | Section::Commands => {}
    }
}

/// Font sizes the panes can draw; the same range as the settings window.
pub const FONT_SIZE: std::ops::RangeInclusive<f64> = 9.0..=24.0;
pub const TAB_WIDTH: std::ops::RangeInclusive<f64> = 1.0..=8.0;
pub const CONTEXT_LINES: std::ops::RangeInclusive<f64> = 0.0..=20.0;
pub const SUMMARY_LENGTH: std::ops::RangeInclusive<f64> = 20.0..=200.0;

/// The intervals the auto-fetch control offers; 0 turns it off.
pub const AUTO_FETCH_MINUTES: [u32; 6] = [0, 5, 10, 15, 30, 60];

fn clamp(value: f64, range: std::ops::RangeInclusive<f64>) -> f64 {
    // A number input can hand back a value outside its bounds while the user is
    // still typing, so every number goes through this before it is stored.
    if value.is_nan() {
        return *range.start();
    }
    value.clamp(*range.start(), *range.end())
}

/// A blank field means "use the platform default", not an empty font name.
fn optional_text(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

pub fn theme(settings: &mut Settings, theme: ThemeMode) {
    settings.appearance.theme = theme;
}

pub fn light_theme(settings: &mut Settings, id: &str) {
    let id = id.trim();
    if !id.is_empty() {
        settings.appearance.light_theme = id.to_owned();
    }
}

pub fn dark_theme(settings: &mut Settings, id: &str) {
    let id = id.trim();
    if !id.is_empty() {
        settings.appearance.dark_theme = id.to_owned();
    }
}

pub fn language(settings: &mut Settings, language: crate::i18n::Language) {
    settings.appearance.language = language;
}

pub fn ui_font(settings: &mut Settings, font: &str) {
    settings.appearance.ui_font = optional_text(font);
}

pub fn ui_font_size(settings: &mut Settings, size: f64) {
    settings.appearance.ui_font_size = clamp(size, FONT_SIZE) as f32;
}

pub fn code_font(settings: &mut Settings, font: &str) {
    settings.appearance.code_font = optional_text(font);
}

pub fn code_font_size(settings: &mut Settings, size: f64) {
    settings.appearance.code_font_size = clamp(size, FONT_SIZE) as f32;
}

pub fn tab_width(settings: &mut Settings, width: f64) {
    settings.appearance.tab_width = clamp(width, TAB_WIDTH).round() as u8;
}

pub fn diff_layout(settings: &mut Settings, layout: DiffLayout) {
    settings.diff.layout = layout;
}

pub fn ignore_whitespace(settings: &mut Settings, ignore: bool) {
    settings.diff.ignore_whitespace = ignore;
}

pub fn context_lines(settings: &mut Settings, lines: f64) {
    settings.diff.context_lines = clamp(lines, CONTEXT_LINES).round() as u32;
}

pub fn wrap_lines(settings: &mut Settings, wrap: bool) {
    settings.diff.wrap_lines = wrap;
}

pub fn syntax_highlighting(settings: &mut Settings, highlight: bool) {
    settings.diff.syntax_highlighting = highlight;
}

pub fn show_whitespace(settings: &mut Settings, show: bool) {
    settings.diff.show_whitespace = show;
}

pub fn show_remote_branches(settings: &mut Settings, show: bool) {
    settings.history.show_remote_branches = show;
}

pub fn show_tags(settings: &mut Settings, show: bool) {
    settings.history.show_tags = show;
}

pub fn show_stashes(settings: &mut Settings, show: bool) {
    settings.history.show_stashes = show;
}

pub fn show_commit_signatures(settings: &mut Settings, show: bool) {
    settings.history.show_commit_signatures = show;
}

pub fn show_changed_file_count(settings: &mut Settings, show: bool) {
    settings.history.show_changed_file_count = show;
}

pub fn date_format(settings: &mut Settings, format: DateFormat) {
    settings.history.date_format = format;
}

/// The path as typed. An empty field means `git` from `PATH`.
pub fn git_executable(settings: &mut Settings, path: &str) {
    settings.behaviour.git_executable = optional_text(path).map(PathBuf::from);
}

/// The path picked in the file dialog.
pub fn git_executable_path(settings: &mut Settings, path: PathBuf) {
    settings.behaviour.git_executable = Some(path);
}

/// Only the offered intervals are stored, so a stale saved value cannot ask for
/// a fetch every 0 minutes.
pub fn auto_fetch_minutes(settings: &mut Settings, minutes: u32) {
    let nearest = AUTO_FETCH_MINUTES
        .into_iter()
        .min_by_key(|option| option.abs_diff(minutes))
        .unwrap_or(0);
    settings.behaviour.auto_fetch_minutes = nearest;
}

pub fn prune_on_fetch(settings: &mut Settings, prune: bool) {
    settings.behaviour.prune_on_fetch = prune;
}

pub fn summary_length_hint(settings: &mut Settings, length: f64) {
    settings.behaviour.summary_length_hint = clamp(length, SUMMARY_LENGTH).round() as u32;
}

pub fn confirm_destructive(settings: &mut Settings, confirm: bool) {
    settings.behaviour.confirm_destructive = confirm;
}

pub fn restore_last_repository(settings: &mut Settings, restore: bool) {
    settings.behaviour.restore_last_repository = restore;
}

pub fn editor(settings: &mut Settings, command: &str) {
    settings.tools.editor = optional_text(command);
}

pub fn terminal(settings: &mut Settings, command: &str) {
    settings.tools.terminal = optional_text(command);
}

#[cfg_attr(not(feature = "terminal"), allow(dead_code))]
pub fn embedded_shell(settings: &mut Settings, command: &str) {
    settings.tools.embedded_shell = optional_text(command);
}

pub fn ghq_enabled(settings: &mut Settings, enabled: bool) {
    settings.tools.ghq.enabled = enabled;
}

/// A blank field keeps using the root `ghq root` reports.
pub fn ghq_root(settings: &mut Settings, root: &str) {
    settings.tools.ghq.root = optional_text(root).map(PathBuf::from);
}

pub fn gh_enabled(settings: &mut Settings, enabled: bool) {
    settings.tools.gh.enabled = enabled;
}

pub fn external_avatars(settings: &mut Settings, load: bool) {
    settings.privacy.external_avatars = load;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips through the file, so a handler that writes something the
    /// store cannot read back fails here rather than at the next start.
    fn round_trip(settings: &Settings) -> Settings {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.json");
        settings.save(&path).expect("save");
        Settings::load(&path)
    }

    #[test]
    fn every_section_round_trips_through_the_file() {
        let mut settings = Settings::default();
        theme(&mut settings, ThemeMode::Dark);
        ui_font(&mut settings, "Inter");
        ui_font_size(&mut settings, 15.0);
        code_font(&mut settings, "JetBrains Mono");
        code_font_size(&mut settings, 14.0);
        tab_width(&mut settings, 2.0);
        diff_layout(&mut settings, DiffLayout::Split);
        wrap_lines(&mut settings, true);
        syntax_highlighting(&mut settings, false);
        show_whitespace(&mut settings, true);
        context_lines(&mut settings, 8.0);
        ignore_whitespace(&mut settings, true);
        show_remote_branches(&mut settings, false);
        show_tags(&mut settings, false);
        show_stashes(&mut settings, false);
        show_commit_signatures(&mut settings, false);
        show_changed_file_count(&mut settings, false);
        date_format(&mut settings, DateFormat::Absolute);
        git_executable(&mut settings, "/opt/git/bin/git");
        auto_fetch_minutes(&mut settings, 15);
        prune_on_fetch(&mut settings, false);
        summary_length_hint(&mut settings, 90.0);
        confirm_destructive(&mut settings, false);
        restore_last_repository(&mut settings, false);
        editor(&mut settings, "code --goto {path}:{line}");
        terminal(&mut settings, "alacritty --working-directory {path}");
        embedded_shell(&mut settings, "/bin/zsh -l");
        external_avatars(&mut settings, true);

        let loaded = round_trip(&settings);
        assert_eq!(loaded, settings);
        assert_eq!(loaded.appearance.theme, ThemeMode::Dark);
        assert_eq!(loaded.diff.layout, DiffLayout::Split);
        assert_eq!(loaded.behaviour.auto_fetch_minutes, 15);
        assert_eq!(
            loaded.behaviour.git_executable,
            Some(PathBuf::from("/opt/git/bin/git"))
        );
        assert_eq!(
            loaded.tools.editor.as_deref(),
            Some("code --goto {path}:{line}")
        );
        assert_eq!(loaded.tools.embedded_shell.as_deref(), Some("/bin/zsh -l"));
        assert!(loaded.privacy.external_avatars);
    }

    #[test]
    fn blank_text_fields_mean_the_platform_default() {
        let mut settings = Settings::default();
        ui_font(&mut settings, "   ");
        code_font(&mut settings, "");
        editor(&mut settings, "  ");
        terminal(&mut settings, "\t");
        embedded_shell(&mut settings, "  \t ");
        git_executable(&mut settings, "");
        assert_eq!(settings.appearance.ui_font, None);
        assert_eq!(settings.appearance.code_font, None);
        assert_eq!(settings.tools.editor, None);
        assert_eq!(settings.tools.terminal, None);
        assert_eq!(settings.tools.embedded_shell, None);
        assert_eq!(settings.behaviour.git_executable, None);

        // The padding around a real value is dropped, an inner space is kept.
        git_executable(&mut settings, "  /opt/My Git/bin/git ");
        assert_eq!(
            settings.behaviour.git_executable,
            Some(PathBuf::from("/opt/My Git/bin/git"))
        );
    }

    #[test]
    fn sizes_and_widths_are_clamped_to_what_the_panes_can_draw() {
        let mut settings = Settings::default();
        ui_font_size(&mut settings, 400.0);
        code_font_size(&mut settings, 1.0);
        tab_width(&mut settings, 40.0);
        context_lines(&mut settings, 99.0);
        summary_length_hint(&mut settings, 1.0);
        assert_eq!(settings.appearance.ui_font_size, 24.0);
        assert_eq!(settings.appearance.code_font_size, 9.0);
        assert_eq!(settings.appearance.tab_width, 8);
        assert_eq!(settings.diff.context_lines, 20);
        assert_eq!(settings.behaviour.summary_length_hint, 20);

        ui_font_size(&mut settings, f64::NAN);
        assert_eq!(settings.appearance.ui_font_size, 9.0);
    }

    #[test]
    fn auto_fetch_snaps_to_an_offered_interval() {
        let mut settings = Settings::default();
        for (asked, expected) in [
            (0, 0),
            (4, 5),
            (12, 10),
            (14, 15),
            (23, 30),
            (22, 15),
            (45, 30),
            (59, 60),
            (600, 60),
        ] {
            auto_fetch_minutes(&mut settings, asked);
            assert_eq!(
                settings.behaviour.auto_fetch_minutes, expected,
                "asked for {asked}"
            );
        }
    }

    #[test]
    fn resetting_a_section_leaves_the_others_alone() {
        let mut settings = Settings::default();
        diff_layout(&mut settings, DiffLayout::Split);
        show_whitespace(&mut settings, true);
        theme(&mut settings, ThemeMode::Light);
        auto_fetch_minutes(&mut settings, 30);

        reset_section(&mut settings, Section::Diff);
        assert_eq!(settings.diff, DiffSettings::default());
        assert_eq!(settings.appearance.theme, ThemeMode::Light);
        assert_eq!(settings.behaviour.auto_fetch_minutes, 30);

        for section in Section::ALL {
            reset_section(&mut settings, section);
        }
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn every_section_has_a_title() {
        assert_eq!(Section::ALL.len(), 8);
        for section in Section::ALL {
            assert!(!section.title().is_empty());
        }
    }

    #[test]
    fn the_git_executable_can_come_from_the_picker() {
        let mut settings = Settings::default();
        git_executable_path(&mut settings, PathBuf::from("/usr/local/bin/git"));
        assert_eq!(
            settings.behaviour.git_executable,
            Some(PathBuf::from("/usr/local/bin/git"))
        );
    }

    #[test]
    fn light_and_dark_theme_ids_update_and_reset() {
        let mut settings = Settings::default();
        light_theme(&mut settings, "catppuccin-latte");
        dark_theme(&mut settings, "catppuccin-mocha");
        assert_eq!(settings.appearance.light_theme, "catppuccin-latte");
        assert_eq!(settings.appearance.dark_theme, "catppuccin-mocha");

        // Blank values leave the active selection intact.
        light_theme(&mut settings, "   ");
        dark_theme(&mut settings, "");
        assert_eq!(settings.appearance.light_theme, "catppuccin-latte");
        assert_eq!(settings.appearance.dark_theme, "catppuccin-mocha");

        reset_section(&mut settings, Section::Appearance);
        assert_eq!(
            settings.appearance.light_theme,
            crate::style::theme::DEFAULT_LIGHT_THEME_ID
        );
        assert_eq!(
            settings.appearance.dark_theme,
            crate::style::theme::DEFAULT_DARK_THEME_ID
        );
    }
}
