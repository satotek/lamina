//! Tab collection logic and per-window session persistence.
//!
//! Kept free of GPUI state so every tab operation (opening, closing, closing
//! the last tab, reordering, cycling, jumping, and restoring while skipping
//! missing repositories) can be tested directly.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "session.json";

/// Result of closing a tab from a [`TabDeck`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseOutcome<T> {
    /// The index did not refer to any tab.
    OutOfBounds,
    /// The last remaining tab was closed; the host window should close.
    ClosedLast(T),
    /// One tab was removed and at least one remains open.
    Closed { removed: T, new_active: usize },
}

/// Ordered tabs for one window, tracking which tab is active.
///
/// Invariant: when `items` is non-empty, `active < items.len()`. When `items`
/// is empty, `active == 0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabDeck<T> {
    items: Vec<T>,
    active: usize,
}

impl<T> Default for TabDeck<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> TabDeck<T> {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            active: 0,
        }
    }

    pub fn from_items(items: Vec<T>, active: usize) -> Self {
        let clamped = if items.is_empty() {
            0
        } else {
            active.min(items.len() - 1)
        };
        Self {
            items,
            active: clamped,
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active(&self) -> Option<&T> {
        self.items.get(self.active)
    }

    pub fn active_mut(&mut self) -> Option<&mut T> {
        self.items.get_mut(self.active)
    }

    pub fn items(&self) -> &[T] {
        &self.items
    }

    #[cfg(feature = "terminal")]
    pub fn items_mut(&mut self) -> &mut [T] {
        &mut self.items
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        self.items.get(index)
    }

    /// Appends `item` as a new tab and activates it, returning its index.
    pub fn open(&mut self, item: T) -> usize {
        self.items.push(item);
        self.active = self.items.len() - 1;
        self.active
    }

    /// Inserts `item` at `index` (clamped to the end) and activates it,
    /// returning the index it landed on.
    pub fn insert(&mut self, index: usize, item: T) -> usize {
        let index = index.min(self.items.len());
        self.items.insert(index, item);
        self.active = index;
        index
    }

    /// Switches to `index` if it is in bounds and differs from the active tab.
    pub fn select(&mut self, index: usize) -> bool {
        if index >= self.items.len() || index == self.active {
            return false;
        }
        self.active = index;
        true
    }

    /// Jumps to a 1-based shortcut slot (`1..=9`). Out-of-range slots do
    /// nothing and return `None`.
    pub fn jump(&mut self, one_based: usize) -> Option<usize> {
        let index = one_based.checked_sub(1)?;
        self.select(index).then_some(index)
    }

    /// Moves forward (`delta > 0`) or backward (`delta < 0`) with wraparound.
    /// Returns the new active index when the active tab changed.
    pub fn cycle(&mut self, delta: isize) -> Option<usize> {
        let len = self.items.len();
        if len <= 1 || delta == 0 {
            return None;
        }
        let next = (self.active as isize + delta).rem_euclid(len as isize) as usize;
        if next == self.active {
            return None;
        }
        self.active = next;
        Some(next)
    }

    /// Closes the tab at `index`, updating `active` so the active view stays
    /// stable (or shifts to the neighbor when the active tab itself closes).
    pub fn close(&mut self, index: usize) -> CloseOutcome<T> {
        if index >= self.items.len() {
            return CloseOutcome::OutOfBounds;
        }
        let removed = self.items.remove(index);
        if self.items.is_empty() {
            self.active = 0;
            return CloseOutcome::ClosedLast(removed);
        }
        if index < self.active {
            self.active -= 1;
        } else if index == self.active {
            self.active = self.active.min(self.items.len() - 1);
        }
        CloseOutcome::Closed {
            removed,
            new_active: self.active,
        }
    }

    /// Closes the active tab.
    #[cfg(test)]
    pub fn close_active(&mut self) -> CloseOutcome<T> {
        self.close(self.active)
    }

    /// Moves the tab at `from` to `to`, keeping whichever tab is active selected.
    pub fn reorder(&mut self, from: usize, to: usize) -> bool {
        let len = self.items.len();
        if from == to || from >= len || to >= len {
            return false;
        }
        let moved = self.items.remove(from);
        self.items.insert(to, moved);

        if self.active == from {
            self.active = to;
        } else if from < self.active && self.active <= to {
            self.active -= 1;
        } else if to <= self.active && self.active < from {
            self.active += 1;
        }
        true
    }
}

/// How many closed tabs one window remembers for "Reopen closed tab".
pub const MAX_CLOSED_TABS: usize = 10;

/// A repository tab that was closed, with the position it had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedTab {
    pub path: PathBuf,
    /// Index the tab had in the deck when it was closed.
    pub index: usize,
}

/// The most recently closed tabs of one window, oldest first.
///
/// Kept per window and not persisted: a tab closed in a session that has ended
/// is not what reopening means.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClosedTabStack {
    entries: Vec<ClosedTab>,
}

impl ClosedTabStack {
    /// Records a closed tab, dropping the oldest once the stack is full.
    pub fn push(&mut self, tab: ClosedTab) {
        self.entries.push(tab);
        while self.entries.len() > MAX_CLOSED_TABS {
            self.entries.remove(0);
        }
    }

    /// The most recent entry `is_valid` accepts, dropping the entries skipped
    /// on the way. A repository deleted since it was closed is skipped so the
    /// user gets the next older tab instead of nothing.
    pub fn pop_existing(&mut self, mut is_valid: impl FnMut(&Path) -> bool) -> Option<ClosedTab> {
        while let Some(tab) = self.entries.pop() {
            if is_valid(&tab.path) {
                return Some(tab);
            }
        }
        None
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Persisted open tabs and active tab index for one window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedWindow {
    pub tabs: Vec<PathBuf>,
    pub active: usize,
}

impl SavedWindow {
    /// Drops paths that no longer exist (or fail `is_valid`) and duplicates,
    /// shifting `active` to point at the same surviving repository (or its
    /// nearest neighbor if the previously active path disappeared).
    pub fn resolve_existing<F>(&self, mut is_valid: F) -> Option<SavedWindow>
    where
        F: FnMut(&Path) -> bool,
    {
        let mut kept: Vec<PathBuf> = Vec::with_capacity(self.tabs.len());
        let mut new_active: Option<usize> = None;
        let mut fallback_active: Option<usize> = None;

        for (index, path) in self.tabs.iter().enumerate() {
            if !is_valid(path) {
                continue;
            }
            // Collapse duplicates without disturbing whichever copy was active.
            if let Some(existing) = kept.iter().position(|seen| seen == path) {
                if index == self.active && new_active.is_none() {
                    new_active = Some(existing);
                }
                continue;
            }
            let next_pos = kept.len();
            kept.push(path.clone());
            if index == self.active {
                new_active = Some(next_pos);
            } else if index > self.active && new_active.is_none() && fallback_active.is_none() {
                fallback_active = Some(next_pos);
            }
        }

        if kept.is_empty() {
            return None;
        }

        let active = new_active
            .or(fallback_active)
            .unwrap_or_else(|| kept.len() - 1);
        Some(SavedWindow { tabs: kept, active })
    }
}

/// Persisted open windows and their tabs across the application.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedSession {
    pub windows: Vec<SavedWindow>,
}

impl SavedSession {
    /// Reads `path`. Missing or malformed files resolve to an empty session
    /// rather than failing startup.
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

    pub fn load_default() -> Self {
        session_path().map_or_else(Self::default, |path| Self::load(&path))
    }

    /// Writes atomically via a temp file + rename so crashes never leave a
    /// truncated `session.json`.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, json)?;
        std::fs::rename(&temp, path)
    }

    pub fn save_default(&self) -> io::Result<()> {
        let path = session_path()
            .ok_or_else(|| io::Error::other("no config directory for window session state"))?;
        self.save(&path)
    }

    /// Resolves every saved window, dropping windows with no surviving tabs.
    pub fn resolve_windows<F>(&self, mut is_valid: F) -> Vec<SavedWindow>
    where
        F: FnMut(&Path) -> bool,
    {
        self.windows
            .iter()
            .filter_map(|window| window.resolve_existing(&mut is_valid))
            .collect()
    }
}

/// The file open windows and tabs are stored in.
pub fn session_path() -> Option<PathBuf> {
    std::env::var_os("LAMINA_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(crate::settings::config_dir)
        .map(|dir| dir.join(FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_select_cycle_and_jump_track_active_index() {
        let mut deck = TabDeck::new();
        assert!(deck.is_empty());
        assert_eq!(deck.cycle(1), None);

        assert_eq!(deck.open("a"), 0);
        assert_eq!(deck.active(), Some(&"a"));
        assert_eq!(deck.cycle(1), None, "single tab cannot cycle");

        assert_eq!(deck.open("b"), 1);
        assert_eq!(deck.open("c"), 2);
        assert_eq!(deck.active_index(), 2);

        // Cycle wraps forward and backward.
        assert_eq!(deck.cycle(1), Some(0));
        assert_eq!(deck.cycle(-1), Some(2));
        assert_eq!(deck.cycle(-1), Some(1));
        assert_eq!(deck.active(), Some(&"b"));

        // 1-based jump respects bounds and rejects 0 or out-of-range.
        assert_eq!(deck.jump(0), None);
        assert_eq!(deck.jump(4), None);
        assert_eq!(deck.jump(1), Some(0));
        assert_eq!(deck.jump(1), None, "already on tab 1");
        assert_eq!(deck.jump(3), Some(2));
        assert_eq!(deck.active(), Some(&"c"));
    }

    #[test]
    fn closing_tabs_updates_active_and_reports_last_tab() {
        let mut deck = TabDeck::from_items(vec!["a", "b", "c", "d"], 2);
        assert_eq!(deck.close(99), CloseOutcome::OutOfBounds);

        // Closing a tab before active shifts active left to keep "c" selected.
        assert_eq!(
            deck.close(0),
            CloseOutcome::Closed {
                removed: "a",
                new_active: 1,
            }
        );
        assert_eq!(deck.active(), Some(&"c"));

        // Closing after active keeps active index unchanged.
        assert_eq!(
            deck.close(2),
            CloseOutcome::Closed {
                removed: "d",
                new_active: 1,
            }
        );
        assert_eq!(deck.items(), &["b", "c"]);
        assert_eq!(deck.active(), Some(&"c"));

        // Closing the active rightmost tab selects its left neighbor.
        assert_eq!(
            deck.close_active(),
            CloseOutcome::Closed {
                removed: "c",
                new_active: 0,
            }
        );
        assert_eq!(deck.active(), Some(&"b"));

        // Closing the final tab reports ClosedLast.
        assert_eq!(deck.close_active(), CloseOutcome::ClosedLast("b"));
        assert!(deck.is_empty());
        assert_eq!(deck.active_index(), 0);
    }

    #[test]
    fn insert_puts_a_tab_at_its_old_position_and_activates_it() {
        let mut deck = TabDeck::from_items(vec!["a", "b", "c"], 2);
        assert_eq!(deck.insert(1, "b2"), 1);
        assert_eq!(deck.items(), &["a", "b2", "b", "c"]);
        assert_eq!(deck.active(), Some(&"b2"));

        // A position past the end lands at the end, like opening a tab.
        assert_eq!(deck.insert(99, "d"), 4);
        assert_eq!(deck.items(), &["a", "b2", "b", "c", "d"]);
        assert_eq!(deck.active(), Some(&"d"));
    }

    #[test]
    fn the_closed_tab_stack_keeps_the_most_recent_ten() {
        let mut stack = ClosedTabStack::default();
        for index in 0..MAX_CLOSED_TABS + 2 {
            stack.push(ClosedTab {
                path: PathBuf::from(format!("/repo/{index}")),
                index,
            });
        }
        assert_eq!(stack.len(), MAX_CLOSED_TABS);

        let newest = stack.pop_existing(|_| true).expect("newest entry");
        assert_eq!(
            newest.path,
            PathBuf::from(format!("/repo/{}", MAX_CLOSED_TABS + 1))
        );
        assert_eq!(newest.index, MAX_CLOSED_TABS + 1);
    }

    #[test]
    fn reopening_skips_repositories_that_no_longer_exist() {
        let mut stack = ClosedTabStack::default();
        let keep = PathBuf::from("/repo/keep");
        stack.push(ClosedTab {
            path: keep.clone(),
            index: 0,
        });
        stack.push(ClosedTab {
            path: PathBuf::from("/repo/gone"),
            index: 1,
        });

        let reopened = stack.pop_existing(|path| path == keep).expect("kept entry");
        assert_eq!(
            reopened,
            ClosedTab {
                path: keep,
                index: 0
            }
        );
        assert!(stack.is_empty(), "the missing entry must be dropped too");
        assert_eq!(stack.pop_existing(|_| true), None);
    }

    #[test]
    fn reorder_moves_tab_and_preserves_active_identity() {
        let mut deck = TabDeck::from_items(vec!["a", "b", "c", "d"], 1);
        assert!(!deck.reorder(1, 1));
        assert!(!deck.reorder(0, 9));

        // Move active tab "b" from 1 -> 3.
        assert!(deck.reorder(1, 3));
        assert_eq!(deck.items(), &["a", "c", "d", "b"]);
        assert_eq!(deck.active_index(), 3);
        assert_eq!(deck.active(), Some(&"b"));

        // Move "a" from 0 -> 2 (stays left of 3, so active "b" remains at 3).
        assert!(deck.reorder(0, 2));
        assert_eq!(deck.items(), &["c", "d", "a", "b"]);
        assert_eq!(deck.active_index(), 3);
        assert_eq!(deck.active(), Some(&"b"));

        // Move "c" from 0 -> 3 (crosses active at 3 -> shifts active left to 2).
        assert!(deck.reorder(0, 3));
        assert_eq!(deck.items(), &["d", "a", "b", "c"]);
        assert_eq!(deck.active_index(), 2);
        assert_eq!(deck.active(), Some(&"b"));

        // Move "c" back from 3 -> 0 (crosses active at 2 -> shifts active right to 3).
        assert!(deck.reorder(3, 0));
        assert_eq!(deck.items(), &["c", "d", "a", "b"]);
        assert_eq!(deck.active_index(), 3);
        assert_eq!(deck.active(), Some(&"b"));
    }

    #[test]
    fn restore_skips_missing_paths_and_adjusts_active_index() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("repo-a");
        let b = temp.path().join("missing-b");
        let c = temp.path().join("repo-c");
        let d = temp.path().join("missing-d");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&c).unwrap();

        // Missing tab before active -> active shifts to point at repo-c.
        let win1 = SavedWindow {
            tabs: vec![a.clone(), b.clone(), c.clone()],
            active: 2,
        };
        let resolved1 = win1.resolve_existing(|p| p.is_dir()).unwrap();
        assert_eq!(resolved1.tabs, vec![a.clone(), c.clone()]);
        assert_eq!(resolved1.active, 1);

        // Active tab itself is missing -> falls back to next surviving tab (repo-c).
        let win2 = SavedWindow {
            tabs: vec![a.clone(), b.clone(), c.clone()],
            active: 1,
        };
        let resolved2 = win2.resolve_existing(|p| p.is_dir()).unwrap();
        assert_eq!(resolved2.tabs, vec![a.clone(), c.clone()]);
        assert_eq!(resolved2.active, 1);

        // Last tab was missing and active -> clamps to last surviving tab.
        let win3 = SavedWindow {
            tabs: vec![a.clone(), c.clone(), d.clone()],
            active: 2,
        };
        let resolved3 = win3.resolve_existing(|p| p.is_dir()).unwrap();
        assert_eq!(resolved3.tabs, vec![a.clone(), c.clone()]);
        assert_eq!(resolved3.active, 1);

        // Window where every tab is missing is dropped.
        let win_empty = SavedWindow {
            tabs: vec![b.clone(), d.clone()],
            active: 0,
        };
        assert_eq!(win_empty.resolve_existing(|p| p.is_dir()), None);

        let session = SavedSession {
            windows: vec![win_empty, win1],
        };
        let resolved_session = session.resolve_windows(|p| p.is_dir());
        assert_eq!(resolved_session, vec![resolved1]);
    }

    #[test]
    fn saved_session_roundtrips_and_tolerates_broken_files() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("nested/session.json");
        assert_eq!(SavedSession::load(&file), SavedSession::default());

        let session = SavedSession {
            windows: vec![SavedWindow {
                tabs: vec![PathBuf::from("/tmp/one"), PathBuf::from("/tmp/two")],
                active: 1,
            }],
        };
        session.save(&file).unwrap();
        assert_eq!(SavedSession::load(&file), session);
        assert!(!file.with_extension("json.tmp").exists());

        std::fs::write(&file, b"{ not json").unwrap();
        assert_eq!(SavedSession::load(&file), SavedSession::default());
    }

    #[test]
    fn an_old_session_file_still_loads_and_a_remote_tab_needs_no_directory() {
        let old = r#"{"windows":[{"tabs":["/tmp/repo"],"active":0}]}"#;
        let session: SavedSession = serde_json::from_str(old).unwrap();
        assert_eq!(
            session,
            SavedSession {
                windows: vec![SavedWindow {
                    tabs: vec![PathBuf::from("/tmp/repo")],
                    active: 0,
                }],
            }
        );

        let remote = PathBuf::from("ssh://git.example.com:2222/home/repo");
        let missing = PathBuf::from("/missing");
        let window = SavedWindow {
            tabs: vec![remote.clone(), missing, remote.clone()],
            active: 0,
        };
        // The same predicate startup uses: an ssh URI is kept, a path that is
        // not a directory is dropped, and a repeated URI stays one tab.
        let resolved = window
            .resolve_existing(|path| path.to_string_lossy().starts_with("ssh://") || path.is_dir())
            .unwrap();
        assert_eq!(resolved.tabs, vec![remote]);
        assert_eq!(resolved.active, 0);
    }
}
