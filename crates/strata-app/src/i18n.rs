//! The interface language.
//!
//! Every user-visible string in the app comes from the embedded catalogue in
//! `locales/` through `rust_i18n`'s `t!` macro. This module owns the locale
//! choice: which locale the operating system asks for, how the saved setting
//! maps onto a locale, and how a change reaches the windows that are open.
//!
//! Only the app's own text is translated. Git's output, object ids, file
//! names, config keys and command lines stay as they are, and `strata-core`
//! has no catalogue at all.

use std::borrow::Cow;

use gpui::App;
use serde::{Deserialize, Serialize};

use crate::settings::Settings;

/// The language the interface is drawn in.
///
/// `System` is the default, so an existing settings file (which has no
/// `language` key) keeps following the operating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    #[default]
    System,
    English,
    Japanese,
}

impl Language {
    /// The order the settings dropdown lists them in.
    pub const ALL: [Language; 3] = [Language::System, Language::English, Language::Japanese];

    /// The stable id used in the saved file and in the dropdown.
    pub fn id(self) -> &'static str {
        match self {
            Language::System => "system",
            Language::English => "english",
            Language::Japanese => "japanese",
        }
    }

    pub fn from_id(id: &str) -> Language {
        match id {
            "english" => Language::English,
            "japanese" => Language::Japanese,
            _ => Language::System,
        }
    }

    /// The locale `rust-i18n` is switched to. `System` resolves the operating
    /// system's own language every time it is asked, so a change is picked up
    /// the next time the app reads it.
    pub fn locale(self) -> &'static str {
        match self {
            Language::System => system_language().fixed_locale(),
            Language::English => "en",
            Language::Japanese => "ja",
        }
    }

    /// A fixed locale for the two explicit choices, and for the result of
    /// [`system_language`].
    fn fixed_locale(self) -> &'static str {
        match self {
            Language::Japanese => "ja",
            _ => "en",
        }
    }

    /// The dropdown label. The two language names are their own names.
    pub fn label(self) -> String {
        match self {
            Language::System => t!("settings.appearance.language_system").into_owned(),
            Language::English => t!("settings.appearance.language_english").into_owned(),
            Language::Japanese => t!("settings.appearance.language_japanese").into_owned(),
        }
    }
}

/// The language a locale tag names. Anything that does not start with `ja`
/// falls back to English, which is also the catalogue's fallback.
pub fn from_locale_tag(tag: &str) -> Language {
    let tag = tag.trim().to_ascii_lowercase();
    // Locale tags come as `ja_JP.UTF-8`, `ja-JP` or `ja`; the encoding after
    // the dot and the territory after the separator do not change the language.
    let language = tag
        .split('.')
        .next()
        .unwrap_or_default()
        .split(['_', '-'])
        .next()
        .unwrap_or_default();
    if language == "ja" {
        Language::Japanese
    } else {
        Language::English
    }
}

/// The operating system's language: `AppleLanguages` or `LANG` on macOS,
/// the user's UI language on Windows, and `LC_ALL`/`LC_MESSAGES`/`LANG`
/// elsewhere.
pub fn system_language() -> Language {
    first_language(system_locale_candidates())
}

/// The first non-empty candidate decides, which is what the POSIX variables
/// already encode: `LC_ALL` overrides the more specific ones.
fn first_language(candidates: impl IntoIterator<Item = Option<String>>) -> Language {
    candidates
        .into_iter()
        .flatten()
        .map(|value| value.trim().to_owned())
        .find(|value| !value.is_empty())
        .map_or(Language::English, |value| from_locale_tag(&value))
}

fn environment(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[cfg(target_os = "macos")]
fn system_locale_candidates() -> Vec<Option<String>> {
    vec![
        macos_preferred_language(),
        environment("LANG"),
        environment("LC_ALL"),
    ]
}

/// The first entry of the user's `AppleLanguages` preference, which is what
/// the Finder and the rest of the system follow.
#[cfg(target_os = "macos")]
fn macos_preferred_language() -> Option<String> {
    let output = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLanguages"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    text.split('"').nth(1).map(str::to_owned)
}

#[cfg(target_os = "windows")]
fn system_locale_candidates() -> Vec<Option<String>> {
    vec![windows_ui_language(), environment("LANG")]
}

/// `GetUserDefaultUILanguage` names the language of the installed Windows UI.
/// No crate is added for it: the two calls are declared here.
#[cfg(target_os = "windows")]
fn windows_ui_language() -> Option<String> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
        fn LCIDToLocaleName(locale: u32, name: *mut u16, name_len: i32, flags: u32) -> i32;
    }

    let langid = unsafe { GetUserDefaultUILanguage() as u32 };
    let mut buffer = [0u16; 85];
    let written = unsafe { LCIDToLocaleName(langid, buffer.as_mut_ptr(), buffer.len() as i32, 0) };
    if written <= 1 {
        return None;
    }
    String::from_utf16(&buffer[..written as usize - 1]).ok()
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn system_locale_candidates() -> Vec<Option<String>> {
    vec![
        environment("LC_ALL"),
        environment("LC_MESSAGES"),
        environment("LANG"),
    ]
}

/// The translation of `key` in `locale`, without the fallback chain, so a
/// missing Japanese key is visible to the tests instead of reading as English.
pub fn lookup(locale: &str, key: &str) -> Option<String> {
    crate::_rust_i18n_backend()
        .translate(locale, key)
        .map(Cow::into_owned)
}

/// The palette title of an action. The action's identifier stays in English:
/// only the text the user reads goes through the catalogue.
pub fn action_title(full_name: &str, english: &str) -> String {
    let short = full_name.rsplit("::").next().unwrap_or(full_name);
    let key = format!("action.{}", snake_case(short));
    lookup(&rust_i18n::locale(), &key).unwrap_or_else(|| english.to_owned())
}

/// `SwitchBranch` -> `switch_branch`, `SelectTab1` -> `select_tab1`.
pub(crate) fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (index, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Applies the saved language. Called once at startup, before any window
/// opens, and again whenever the setting changes.
pub fn apply(cx: &mut App) {
    // The component library has its own catalogue, which has no Japanese. Its
    // `Settings.search_placeholder` and anything else under `gpui_component.`
    // in our files take precedence once extended.
    static EXTEND: std::sync::Once = std::sync::Once::new();
    EXTEND.call_once(|| rust_i18n::extend!(gpui_component));

    rust_i18n::set_locale(Settings::get(cx).appearance.language.locale());
    // The menu bar is text too: its titles and items are rebuilt from the new
    // locale, not left in the language it was built with.
    crate::menus::install(cx);
    cx.refresh_windows();
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    /// Every `t!` key in this crate's sources. The scan is deliberately
    /// textual, like the icon test in `assets.rs`: a key that is only known at
    /// runtime would be invisible here, which is why every call site uses a
    /// literal key.
    fn source_keys() -> BTreeSet<String> {
        let sources = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut keys = BTreeSet::new();
        let mut stack = vec![sources];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("source directory") {
                let path = entry.expect("directory entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let text = std::fs::read_to_string(&path).expect("source file");
                    for (index, _) in text.match_indices("t!(\"") {
                        // A `format!` call ends in the same four bytes; only a
                        // `t!` that is not the tail of another macro counts.
                        let is_macro = index == 0
                            || !matches!(
                                text.as_bytes()[index - 1],
                                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'!'
                            );
                        if !is_macro {
                            continue;
                        }
                        let start = index + "t!(\"".len();
                        if let Some(end) = text[start..].find('"') {
                            keys.insert(text[start..start + end].to_owned());
                        }
                    }
                }
            }
        }
        assert!(!keys.is_empty(), "the scan found no keys");
        keys
    }

    fn messages(locale: &str) -> BTreeMap<String, String> {
        crate::_rust_i18n_backend()
            .messages_for_locale(locale)
            .expect("a locale with a catalogue")
            .into_iter()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }

    fn placeholders(text: &str) -> BTreeSet<&str> {
        let mut names = BTreeSet::new();
        let mut rest = text;
        while let Some(start) = rest.find("%{") {
            rest = &rest[start + 2..];
            if let Some(end) = rest.find('}') {
                names.insert(&rest[..end]);
                rest = &rest[end + 1..];
            } else {
                break;
            }
        }
        names
    }

    #[test]
    fn every_key_used_in_the_sources_is_translated_in_both_locales() {
        let keys = source_keys();
        for key in &keys {
            for locale in ["en", "ja"] {
                assert!(lookup(locale, key).is_some(), "{locale} is missing `{key}`");
            }
        }
    }

    #[test]
    fn the_two_catalogues_have_the_same_keys() {
        let en = messages("en");
        let ja = messages("ja");
        let missing_in_ja: Vec<&String> = en.keys().filter(|key| !ja.contains_key(*key)).collect();
        let missing_in_en: Vec<&String> = ja.keys().filter(|key| !en.contains_key(*key)).collect();
        assert!(missing_in_ja.is_empty(), "ja is missing {missing_in_ja:?}");
        assert!(missing_in_en.is_empty(), "en is missing {missing_in_en:?}");
    }

    #[test]
    fn interpolation_placeholders_match_between_locales() {
        let en = messages("en");
        let ja = messages("ja");
        for (key, english) in &en {
            let Some(japanese) = ja.get(key) else {
                continue;
            };
            assert_eq!(
                placeholders(english),
                placeholders(japanese),
                "`{key}` uses different placeholders in en and ja"
            );
        }
    }

    #[test]
    fn every_action_has_a_title_in_both_locales() {
        for spec in crate::keymap::ACTIONS {
            let short = spec.name.rsplit("::").next().unwrap_or(spec.name);
            let key = format!("action.{}", snake_case(short));
            for locale in ["en", "ja"] {
                assert!(
                    lookup(locale, &key).is_some(),
                    "{locale} has no title for {} (`{key}`)",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn a_japanese_locale_tag_maps_to_japanese_and_everything_else_to_english() {
        for tag in ["ja", "ja_JP", "ja-JP", "ja_JP.UTF-8", "ja-JP-u-ca-japanese"] {
            assert_eq!(from_locale_tag(tag), Language::Japanese, "{tag}");
        }
        for tag in ["en", "en_US.UTF-8", "C", "POSIX", "fr_FR", "zh_CN", ""] {
            assert_eq!(from_locale_tag(tag), Language::English, "{tag}");
        }
    }

    #[test]
    fn the_first_non_empty_candidate_decides() {
        let candidates = [
            Some(String::new()),
            Some("ja_JP.UTF-8".to_owned()),
            Some("en_US.UTF-8".to_owned()),
        ];
        assert_eq!(first_language(candidates), Language::Japanese);

        let candidates = [Some("C".to_owned()), Some("ja_JP.UTF-8".to_owned())];
        assert_eq!(
            first_language(candidates),
            Language::English,
            "LC_ALL=C must win over LANG, as POSIX says"
        );

        assert_eq!(first_language([None, None]), Language::English);
    }

    #[test]
    fn the_system_choice_follows_the_environment() {
        // The test process controls these variables, so this checks the same
        // path startup takes without depending on the host's language.
        let previous = std::env::var("LC_ALL").ok();
        unsafe { std::env::set_var("LC_ALL", "ja_JP.UTF-8") };
        if cfg!(not(any(target_os = "macos", target_os = "windows"))) {
            assert_eq!(system_language(), Language::Japanese);
        }
        match previous {
            Some(value) => unsafe { std::env::set_var("LC_ALL", value) },
            None => unsafe { std::env::remove_var("LC_ALL") },
        }
    }
}
