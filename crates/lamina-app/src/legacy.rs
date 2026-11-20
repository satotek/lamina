//! Data saved under the app's former name, Strata.
//!
//! Releases up to 0.1.10 kept their settings, the proxy password, theme ids
//! and keymap action names under "Strata". Each is carried over or accepted
//! here so an update keeps the user's setup instead of starting fresh.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// The credential-store service the proxy password was saved under.
pub const KEYRING_SERVICE: &str = "strata";

/// Where the former name kept its configuration directory, mirroring
/// [`crate::settings::config_dir`].
fn config_dir() -> Option<PathBuf> {
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

/// Copies the former configuration directory to the new one on the first
/// launch under the new name.
///
/// A copy rather than a move, so going back to an older release still finds
/// its settings. Skipped when the new directory already exists (it is the
/// user's now) or when `LAMINA_CONFIG_DIR` points somewhere explicit.
pub fn migrate_config_dir() {
    if std::env::var_os("LAMINA_CONFIG_DIR").is_some() {
        return;
    }
    let (Some(old), Some(new)) = (config_dir(), crate::settings::config_dir()) else {
        return;
    };
    match copy_config_dir(&old, &new) {
        Ok(true) => log::info!(
            "copied settings from {} to {}",
            old.display(),
            new.display()
        ),
        Ok(false) => {}
        Err(error) => log::warn!(
            "could not copy settings from {} to {}: {error}",
            old.display(),
            new.display()
        ),
    }
}

/// Copies `old` to `new` when only `old` exists. Returns whether it copied.
fn copy_config_dir(old: &Path, new: &Path) -> std::io::Result<bool> {
    if new.exists() || !old.is_dir() {
        return Ok(false);
    }
    // Copy beside the target and rename it into place, so an interrupted copy
    // never leaves a half-filled directory that would block the next attempt.
    let staging = new.with_extension("migrating");
    let _ = std::fs::remove_dir_all(&staging);
    copy_tree(old, &staging)?;
    std::fs::rename(&staging, new)?;
    Ok(true)
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// The current id of a bundled theme saved under its former id.
pub fn theme_id(id: &str) -> &str {
    match id {
        "strata-dark" => "lamina-dark",
        "strata-light" => "lamina-light",
        other => other,
    }
}

/// A keymap action name with a former namespace (`strata::`, `strata_*::`)
/// moved to the current one, so a user's `keymap.json` keeps working.
pub fn action_name(name: &str) -> Cow<'_, str> {
    match name.strip_prefix("strata") {
        Some(rest) if rest.starts_with("::") || rest.starts_with('_') => {
            Cow::Owned(format!("lamina{rest}"))
        }
        _ => Cow::Borrowed(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_former_directory_is_copied_once_and_left_in_place() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("Strata");
        let new = root.path().join("Lamina");
        std::fs::create_dir_all(old.join("themes")).unwrap();
        std::fs::write(old.join("settings.json"), "{}").unwrap();
        std::fs::write(old.join("themes/mine.toml"), "id = 'mine'").unwrap();

        assert!(copy_config_dir(&old, &new).unwrap());
        assert_eq!(
            std::fs::read_to_string(new.join("settings.json")).unwrap(),
            "{}"
        );
        assert!(new.join("themes/mine.toml").is_file());
        assert!(old.join("settings.json").is_file(), "the old copy stays");

        std::fs::write(new.join("settings.json"), "{\"changed\":1}").unwrap();
        assert!(
            !copy_config_dir(&old, &new).unwrap(),
            "an existing directory wins"
        );
        assert_eq!(
            std::fs::read_to_string(new.join("settings.json")).unwrap(),
            "{\"changed\":1}"
        );
    }

    #[test]
    fn nothing_is_copied_without_a_former_directory() {
        let root = tempfile::tempdir().unwrap();
        let new = root.path().join("Lamina");
        assert!(!copy_config_dir(&root.path().join("Strata"), &new).unwrap());
        assert!(!new.exists());
    }

    #[test]
    fn former_theme_ids_and_action_names_map_to_current_ones() {
        assert_eq!(theme_id("strata-dark"), "lamina-dark");
        assert_eq!(theme_id("strata-light"), "lamina-light");
        assert_eq!(theme_id("catppuccin-mocha"), "catppuccin-mocha");

        assert_eq!(action_name("strata::OpenSettings"), "lamina::OpenSettings");
        assert_eq!(
            action_name("strata_conflicts::Next"),
            "lamina_conflicts::Next"
        );
        assert_eq!(action_name("SwitchBranch"), "SwitchBranch");
        assert_eq!(action_name("stratagem::X"), "stratagem::X");
    }
}
