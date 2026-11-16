//! Repositories that were opened before.
//!
//! The list lives in `recent.json` in the platform config directory (the same
//! place as `settings.json`, and `STRATA_CONFIG_DIR` overrides both), so the
//! welcome screen and the repository dialog can offer it on the next launch.
//! Entries are de-duplicated by canonical path and kept newest first.
//!
//! Writes are atomic (temp file + rename) and every read tolerates a missing or
//! broken file: losing the list is never a reason to fail opening Strata.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How many repositories are remembered; the oldest fall off the end.
pub const MAX_RECENT: usize = 20;

const FILE_NAME: &str = "recent.json";

/// One remembered repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentRepository {
    pub path: PathBuf,
    /// Seconds since the Unix epoch.
    pub opened_at: u64,
}

impl RecentRepository {
    /// The directory's own name, or the whole path when it has none.
    pub fn name(&self) -> String {
        repository_name(&self.path)
    }
}

/// The remembered repositories, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecentRepositories {
    entries: Vec<RecentRepository>,
}

impl RecentRepositories {
    /// Reads `path`. A missing file is an empty list; an unreadable or broken
    /// one is logged and also becomes an empty list, leaving the file in place
    /// so nothing is overwritten until the next successful open.
    pub fn load(path: &Path) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Self::default(),
            Err(error) => {
                log::warn!("cannot read {}: {error}", path.display());
                return Self::default();
            }
        };
        serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            log::warn!("ignoring {}: {error}", path.display());
            Self::default()
        })
    }

    /// Reads the platform's list; an empty list when there is no config
    /// directory at all.
    pub fn load_default() -> Self {
        recent_path().map_or_else(Self::default, |path| Self::load(&path))
    }

    /// Writes atomically through a temp file, so a crash mid-write cannot leave
    /// a truncated list behind.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, json)?;
        std::fs::rename(&temp, path)
    }

    /// Saves to the platform's list. Fails only when no config directory can be
    /// resolved or the write fails.
    pub fn save_default(&self) -> io::Result<()> {
        let path = recent_path().ok_or_else(|| {
            io::Error::other("no config directory for the recent-repository list")
        })?;
        self.save(&path)
    }

    pub fn entries(&self) -> &[RecentRepository] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&RecentRepository> {
        self.entries.get(index)
    }

    /// The repository opened most recently, which is what a restore reopens.
    pub fn most_recent(&self) -> Option<&Path> {
        self.entries.first().map(|entry| entry.path.as_path())
    }

    /// Moves `path` to the front with `opened_at`, comparing canonical paths so
    /// the same repository reached through a symlink or `..` is one entry.
    pub fn record(&mut self, path: impl AsRef<Path>, opened_at: u64) {
        let path = normalize(path.as_ref());
        // Existing entries are compared canonically too, so a spelling recorded
        // while the directory was elsewhere cannot survive next to the same
        // repository's current path.
        self.entries
            .retain(|entry| entry.path != path && normalize(&entry.path) != path);
        self.entries.insert(0, RecentRepository { path, opened_at });
        self.entries.truncate(MAX_RECENT);
    }

    /// [`Self::record`] with the current time.
    pub fn record_now(&mut self, path: impl AsRef<Path>) {
        self.record(path, now_seconds());
    }

    /// Forgets the entry whose path is `path` (canonically compared), which is
    /// what a "forget this repository" button removes.
    pub fn remove(&mut self, path: &Path) -> bool {
        self.remove_index(
            self.entries
                .iter()
                .position(|entry| entry.path == normalize(path)),
        )
        .is_some()
    }

    pub fn remove_index(&mut self, index: Option<usize>) -> Option<RecentRepository> {
        let index = index?;
        (index < self.entries.len()).then(|| self.entries.remove(index))
    }
}

/// The file the list is stored in.
pub fn recent_path() -> Option<PathBuf> {
    std::env::var_os("STRATA_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(crate::settings::config_dir)
        .map(|dir| dir.join(FILE_NAME))
}

/// Records an opened repository in the platform list and saves it.
///
/// The file is a few hundred bytes, so this runs synchronously; callers are on
/// the UI thread once per opening, not per frame.
pub fn record(path: &Path) {
    let mut recents = RecentRepositories::load_default();
    recents.record_now(path);
    if let Err(error) = recents.save_default() {
        log::warn!("cannot save the recent-repository list: {error}");
    }
}

pub fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Canonical when the path exists, so `a/../b` and `b` are one entry; the
/// spelled path otherwise, because a removed directory still deserves a row the
/// user can see and delete.
fn normalize(path: &Path) -> PathBuf {
    strata_core::fs_path::canonicalize(path)
}

/// The directory name shown in the list, with the full path as a fallback.
pub fn repository_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(recents: &RecentRepositories) -> Vec<String> {
        recents
            .entries()
            .iter()
            .map(|entry| entry.path.display().to_string())
            .collect()
    }

    #[test]
    fn entries_are_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut recents = RecentRepositories::default();
        recents.record(dir.path().join("one"), 10);
        recents.record(dir.path().join("two"), 20);
        recents.record(dir.path().join("three"), 30);

        assert_eq!(recents.len(), 3);
        assert_eq!(
            paths(&recents),
            ["three", "two", "one"].map(|name| dir.path().join(name).display().to_string())
        );
        assert_eq!(recents.get(0).map(|entry| entry.opened_at), Some(30));
        assert_eq!(
            recents.most_recent(),
            Some(dir.path().join("three").as_path())
        );
    }

    #[test]
    fn reopening_an_entry_moves_it_to_the_front_without_duplicating_it() {
        let dir = tempfile::tempdir().unwrap();
        let one = dir.path().join("one");
        let two = dir.path().join("two");
        let mut recents = RecentRepositories::default();
        recents.record(&one, 10);
        recents.record(&two, 20);
        recents.record(&one, 30);

        assert_eq!(recents.len(), 2);
        assert_eq!(
            paths(&recents),
            [one, two].map(|path| path.display().to_string())
        );
        assert_eq!(recents.get(0).map(|entry| entry.opened_at), Some(30));
    }

    #[test]
    fn the_same_directory_through_a_symlink_or_dotdot_is_one_entry() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let indirect = repo.join("..").join("repo");

        let mut recents = RecentRepositories::default();
        recents.record(&repo, 10);
        recents.record(&indirect, 20);

        assert_eq!(recents.len(), 1, "both spellings are the same repository");
        assert_eq!(
            recents.get(0).map(|entry| entry.path.clone()),
            // `normalize`, not `canonicalize`: the stored path drops Windows'
            // `\\?\` prefix.
            Some(normalize(&repo))
        );
        assert!(recents.remove(&indirect));
        assert!(recents.is_empty());
    }

    #[test]
    fn only_the_most_recent_entries_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut recents = RecentRepositories::default();
        for index in 0..(MAX_RECENT + 5) {
            recents.record(dir.path().join(format!("repo-{index}")), index as u64);
        }

        assert_eq!(recents.len(), MAX_RECENT);
        assert_eq!(
            recents.get(0).map(|entry| entry.path.clone()),
            Some(dir.path().join(format!("repo-{}", MAX_RECENT + 4)))
        );
        assert!(
            !paths(&recents).iter().any(|path| path.ends_with("repo-0")),
            "the oldest entry falls off the end"
        );
        // Keeping the limit after a re-record too: the moved entry must not
        // push the list past its cap.
        recents.record(dir.path().join("repo-1"), 99);
        assert_eq!(recents.len(), MAX_RECENT);
    }

    #[test]
    fn a_broken_or_missing_file_reads_as_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recent.json");

        assert_eq!(
            RecentRepositories::load(&path),
            RecentRepositories::default()
        );

        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(
            RecentRepositories::load(&path),
            RecentRepositories::default()
        );
        assert!(path.exists(), "the broken file is left for inspection");

        std::fs::write(&path, br#"{"entries":[{"path":"/tmp/repo"}]}"#).unwrap();
        assert_eq!(
            RecentRepositories::load(&path),
            RecentRepositories::default(),
            "an entry without a timestamp is not usable"
        );
    }

    #[test]
    fn a_saved_list_loads_back_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/recent.json");
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();

        let mut recents = RecentRepositories::default();
        recents.record(&repo, 1_700_000_000);
        recents.save(&path).unwrap();

        assert_eq!(RecentRepositories::load(&path), recents);
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the temp file is renamed away"
        );
    }

    #[test]
    fn names_fall_back_to_the_path() {
        assert_eq!(repository_name(Path::new("/tmp/strata")), "strata");
        assert_eq!(
            repository_name(Path::new("/")),
            "/",
            "a root path has no file name of its own"
        );
    }
}
