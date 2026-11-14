//! The app's asset source.
//!
//! `gpui_kit_assets::Assets` embeds only the component library's own icons. Any
//! other `IconName` the app draws renders as nothing and logs "could not find
//! asset" at runtime, so every such icon is listed here and embedded on top.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

gpui_kit_assets::icon_assets!(ExtraIcons, [ClockArrowDown]);

/// The component icons plus the extras above.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let mut paths = gpui_kit_assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lucide's file name for an `IconName` variant: `ClockArrowDown` is
    /// `clock-arrow-down`, `Settings2` is `settings-2`.
    fn file_name(variant: &str) -> String {
        let mut name = String::new();
        let mut previous: Option<char> = None;
        for c in variant.chars() {
            let boundary = match previous {
                Some(p) => c.is_ascii_uppercase() || (c.is_ascii_digit() && !p.is_ascii_digit()),
                None => false,
            };
            if boundary {
                name.push('-');
            }
            name.push(c.to_ascii_lowercase());
            previous = Some(c);
        }
        name
    }

    /// Every icon the app names must load, or it silently draws nothing and
    /// only a runtime log says so. The names come from this crate's sources.
    #[test]
    fn every_icon_the_app_draws_is_embedded() {
        let sources = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut names = std::collections::BTreeSet::new();
        let mut stack = vec![sources];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("source directory") {
                let path = entry.expect("directory entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let text = std::fs::read_to_string(&path).expect("source file");
                    for (index, _) in text.match_indices("IconName::") {
                        let name: String = text[index + "IconName::".len()..]
                            .chars()
                            .take_while(char::is_ascii_alphanumeric)
                            .collect();
                        if name.starts_with(|c: char| c.is_ascii_uppercase()) {
                            names.insert(name);
                        }
                    }
                }
            }
        }
        assert!(names.contains("ClockArrowDown"), "the scan found no icons");
        let missing: Vec<_> = names
            .iter()
            .filter(|name| {
                let path = format!("icons/{}.svg", file_name(name));
                AppAssets.load(&path).ok().flatten().is_none()
            })
            .collect();
        assert!(missing.is_empty(), "icons not embedded: {missing:?}");
    }
}
