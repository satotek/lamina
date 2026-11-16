//! The fonts installed on this machine, and the families the app draws with.
//!
//! GPUI panics when a line is laid out in a family the text system does not
//! have, so a family stored in the settings is only used once the text system
//! reports it. A stored family that is no longer installed falls back to the
//! platform default at render time instead of crashing the app.
//!
//! Enumerating fonts costs around a hundred milliseconds on macOS, so the
//! answer is resolved once and reused: the installed set does not change while
//! the process runs.

use std::sync::{Arc, OnceLock, RwLock};

use gpui::{App, SharedString, font};
use gpui_component::Theme;

use crate::settings::Settings;

/// The virtual family GPUI maps to the platform's UI font on every platform.
pub const SYSTEM_UI_FONT: &str = ".SystemUIFont";

/// Families reported by the text system, sorted case-insensitively and
/// de-duplicated.
///
/// The cache stays empty until a text system answers with at least one family:
/// on the web the text system starts with no fonts at all, so an empty answer
/// is not the final one there.
static FAMILIES: OnceLock<RwLock<Option<Arc<Vec<String>>>>> = OnceLock::new();

/// The installed families, resolved once and then handed out as a cheap clone.
pub fn families(cx: &App) -> Arc<Vec<String>> {
    let cache = FAMILIES.get_or_init(|| RwLock::new(None));
    if let Ok(guard) = cache.read()
        && let Some(families) = guard.as_ref()
    {
        return families.clone();
    }
    let resolved = Arc::new(sorted_unique(cx.text_system().all_font_names()));
    if !resolved.is_empty()
        && let Ok(mut guard) = cache.write()
    {
        *guard = Some(resolved.clone());
    }
    resolved
}

/// GPUI's virtual families start with a dot and resolve inside the platform
/// text system, so they are always accepted.
fn installed_family_in(installed: &[String], family: &str) -> Option<SharedString> {
    if family.starts_with('.') {
        return Some(SharedString::from(family));
    }
    installed
        .iter()
        .find(|name| name.eq_ignore_ascii_case(family))
        .map(|name| SharedString::from(name.clone()))
}

/// The UI family the app draws with: the user's choice when installed, the
/// platform default otherwise.
pub fn ui_font_family(settings: &Settings, cx: &App) -> SharedString {
    let installed = families(cx);
    settings
        .appearance
        .ui_font
        .as_deref()
        .and_then(|family| installed_family_in(&installed, family))
        .unwrap_or_else(|| platform_ui_font(cx, &installed))
}

/// The family `.SystemUIFont` resolves to, named directly.
///
/// gpui-component names the resolved family on the theme itself; resolving it
/// here keeps the value stable across repeated applications, so applying the
/// settings does not force the library to resolve it again every time.
fn platform_ui_font(cx: &App, installed: &[String]) -> SharedString {
    let text_system = cx.text_system();
    let resolved = text_system
        .get_font_for_id(text_system.resolve_font(&font(SYSTEM_UI_FONT)))
        .map(|font| font.family);
    match resolved {
        Some(family)
            if family.as_ref() != SYSTEM_UI_FONT
                && installed
                    .iter()
                    .any(|name| name.as_str() == family.as_ref()) =>
        {
            family
        }
        _ => SharedString::from(SYSTEM_UI_FONT),
    }
}

/// The monospace family the app draws with: the user's choice when installed,
/// the bundled face otherwise.
pub fn code_font_family(settings: &Settings, cx: &App) -> SharedString {
    let installed = families(cx);
    settings
        .appearance
        .code_font
        .as_deref()
        .and_then(|family| installed_family_in(&installed, family))
        .unwrap_or_else(|| platform_code_font(&installed))
}

/// Words in a family name that mark a monospace face.
///
/// The text system lists families but not their spacing, so a face is guessed
/// from its name; anything not recognised stays in the list with the rest.
const MONOSPACE_WORDS: &[&str] = &[
    "code",
    "courier",
    "consolas",
    "menlo",
    "monaco",
    "cascadia",
    "iosevka",
    "inconsolata",
    "hack",
    "typewriter",
    "terminal",
];

/// Whether a family name looks like a monospace face.
fn is_monospace_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("mono")
        || lower
            .split(|c: char| !c.is_alphanumeric())
            .any(|word| MONOSPACE_WORDS.contains(&word))
}

/// The bundled monospace face, or the closest installed stand-in.
///
/// The bundled face is not installed everywhere and GPUI panics on a missing
/// family, so the first installed name that looks monospace is used instead.
fn platform_code_font(installed: &[String]) -> SharedString {
    let bundled = bundled_mono_font();
    if installed_family_in(installed, &bundled).is_some() {
        return bundled;
    }
    installed
        .iter()
        .find(|name| is_monospace_name(name))
        .map_or_else(
            || SharedString::from(SYSTEM_UI_FONT),
            |name| SharedString::from(name.clone()),
        )
}

fn bundled_mono_font() -> SharedString {
    static MONO: OnceLock<SharedString> = OnceLock::new();
    MONO.get_or_init(|| Theme::default().mono_font_family.clone())
        .clone()
}

/// Sorts names case-insensitively and keeps one spelling per family.
///
/// The first spelling the text system reports for a family is the one kept, so
/// a duplicate that differs only in case cannot replace the system's own name.
fn sorted_unique(names: Vec<String>) -> Vec<String> {
    let mut unique = Vec::with_capacity(names.len());
    let mut seen = std::collections::HashSet::with_capacity(names.len());
    for name in names {
        if name.trim().is_empty() {
            continue;
        }
        if seen.insert(name.to_lowercase()) {
            unique.push(name);
        }
    }
    unique.sort_by(|left, right| {
        left.to_lowercase()
            .cmp(&right.to_lowercase())
            .then_with(|| left.cmp(right))
    });
    unique
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn families_sort_without_case_and_keep_one_spelling_each() {
        let sorted = sorted_unique(names(&[
            "menlo",
            "Inter",
            "DejaVu Sans Mono",
            "INTER",
            "  ",
            "Inter",
        ]));
        assert_eq!(
            sorted,
            names(&["DejaVu Sans Mono", "Inter", "menlo"]),
            "sorted case-insensitively, de-duplicated, blanks dropped"
        );
    }

    #[test]
    fn a_missing_bundled_face_falls_back_to_an_installed_monospace_family() {
        assert_eq!(
            platform_code_font(&names(&["Inter", "Fira Code", "Helvetica"])),
            SharedString::from("Fira Code"),
            "a machine without the bundled face still draws code in a fixed-width font"
        );
        assert_eq!(
            platform_code_font(&[]),
            SharedString::from(SYSTEM_UI_FONT),
            "with no fonts at all the only safe family is GPUI's own"
        );
    }
}
