//! Status refreshes: index and worktree only, never a history walk, plus the
//! configuration-derived remotes and upstreams when `.git/config` changed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use crate::repo::MAX_INCREMENTAL_PATHS;
use crate::session::SessionEvent;
use crate::session::publish::Shared;
use crate::{ConflictEntry, RemoteInfo, RepoState, Result, UpstreamInfo, WorktreeStatus};

/// Run a full safety scan after this much active time since the last full scan,
/// so missed filesystem events or subtle external state cannot linger.
const SAFETY_FULL_SCAN_INTERVAL: Duration = Duration::from_secs(30);

/// One refresh request. An `ack` lets the operation worker wait until the
/// refreshed snapshot is visible before it clears `busy`. `config` asks for the
/// configuration-derived fields on top of the status refresh.
#[derive(Default)]
pub(crate) struct RefreshStatus {
    pub(crate) ack: Option<Sender<()>>,
    pub(crate) config: bool,
    /// Worktree paths reported by the watcher when no git internals or rescan
    /// flags were set. `None` forces a full scan.
    pub(crate) paths: Option<Vec<PathBuf>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanKind {
    Incremental,
    Full,
}

struct RefreshState {
    has_baseline: bool,
    last_full_scan: Instant,
}

impl Default for RefreshState {
    fn default() -> Self {
        Self {
            has_baseline: false,
            last_full_scan: Instant::now(),
        }
    }
}

pub(crate) fn run(shared: Arc<Shared>, rx: Receiver<RefreshStatus>) {
    let mut state = RefreshState::default();

    while let Ok(first) = rx.recv() {
        let mut acks: Vec<Sender<()>> = first.ack.into_iter().collect();
        let mut config = first.config;
        let mut paths = first.paths;

        while let Ok(next) = rx.try_recv() {
            if let Some(ack) = next.ack {
                acks.push(ack);
            }
            config |= next.config;
            paths = merge_paths(paths, next.paths);
        }

        let _ = refresh_once(
            &shared,
            &mut state,
            config,
            paths.as_deref(),
            Instant::now(),
        );

        // Acknowledge even when the session is shutting down: the operation
        // worker is waiting for this before it stops, and a lost ack would
        // wedge it.
        for ack in acks {
            let _ = ack.send(());
        }
    }
}

fn should_run_incremental(
    has_baseline: bool,
    elapsed_since_full_scan: Duration,
    paths: Option<&[PathBuf]>,
) -> bool {
    has_baseline && elapsed_since_full_scan < SAFETY_FULL_SCAN_INTERVAL && paths.is_some()
}

fn refresh_once(
    shared: &Shared,
    state: &mut RefreshState,
    config: bool,
    paths: Option<&[PathBuf]>,
    now: Instant,
) -> ScanKind {
    let elapsed = now.saturating_duration_since(state.last_full_scan);
    let ran_incremental = if should_run_incremental(state.has_baseline, elapsed, paths)
        && let Some(changed_paths) = paths
    {
        let prev = shared.snapshot();
        match shared.repo.status_incremental(&prev.status, changed_paths) {
            Ok(Some(status)) => {
                shared.publish_if_changed(|snapshot| {
                    if snapshot.status.as_ref() == &status {
                        return false;
                    }
                    snapshot.status = Arc::new(status);
                    true
                });
                true
            }
            Ok(None) | Err(_) => false,
        }
    } else {
        false
    };

    let kind = if ran_incremental {
        ScanKind::Incremental
    } else {
        match read(shared) {
            Ok((status, state_value, conflicts)) => {
                state.has_baseline = true;
                state.last_full_scan = now;
                shared.publish_if_changed(|snapshot| {
                    if snapshot.status.as_ref() == &status
                        && snapshot.state == state_value
                        && snapshot.conflicts.as_ref() == &conflicts
                    {
                        return false;
                    }
                    snapshot.status = Arc::new(status);
                    snapshot.state = state_value;
                    snapshot.conflicts = Arc::new(conflicts);
                    true
                });
            }
            Err(error) => {
                state.has_baseline = false;
                shared.emit(SessionEvent::LoadFailed {
                    error: error.to_string(),
                });
            }
        }
        ScanKind::Full
    };

    // Remotes and upstreams come from the configuration, so a config change
    // refreshes exactly those and leaves history and status untouched.
    if config {
        match read_config(shared) {
            Ok((remotes, upstreams)) => {
                shared.publish_if_changed(|snapshot| {
                    if snapshot.remotes == remotes && snapshot.upstreams.as_ref() == &upstreams {
                        return false;
                    }
                    snapshot.remotes = remotes;
                    snapshot.upstreams = Arc::new(upstreams);
                    true
                });
            }
            Err(error) => shared.emit(SessionEvent::LoadFailed {
                error: error.to_string(),
            }),
        }
    }

    kind
}

fn merge_paths(a: Option<Vec<PathBuf>>, b: Option<Vec<PathBuf>>) -> Option<Vec<PathBuf>> {
    match (a, b) {
        (Some(mut lhs), Some(rhs)) => {
            if lhs.len() + rhs.len() > MAX_INCREMENTAL_PATHS {
                None
            } else {
                lhs.extend(rhs);
                Some(lhs)
            }
        }
        _ => None,
    }
}

/// The state and its unmerged paths describe the same index as the status, so
/// they are read together and published as one snapshot.
fn read(shared: &Shared) -> Result<(WorktreeStatus, RepoState, Vec<ConflictEntry>)> {
    let status = shared.repo.status()?;
    let state = shared.repo.state()?;
    let conflicts = shared.repo.conflicts()?;
    Ok((status, state, conflicts))
}

fn read_config(shared: &Shared) -> Result<(Vec<RemoteInfo>, HashMap<String, UpstreamInfo>)> {
    let remotes = shared.repo.remotes()?;
    let upstreams = shared.repo.upstreams()?;
    Ok((remotes, upstreams))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::empty_snapshot;
    use crate::test_support::TestRepo;
    use crate::{ChangeKind, FileChange};

    fn test_shared(test: &TestRepo) -> Arc<Shared> {
        let repo = test.open();
        let git = test.git_handle();
        let events = Arc::new(|_| {});
        Arc::new(Shared::new(repo, git, events, empty_snapshot()))
    }

    #[test]
    fn incremental_and_fallback_decisions() {
        let test = TestRepo::new();
        test.write("a.txt", "one\n");
        test.write("b.txt", "one\n");
        test.commit("init");
        let shared = test_shared(&test);
        let mut state = RefreshState::default();
        let t0 = Instant::now();

        // First scan always runs a full scan to establish a baseline.
        let path_a = test.path().join("a.txt");
        assert_eq!(
            refresh_once(
                &shared,
                &mut state,
                false,
                Some(std::slice::from_ref(&path_a)),
                t0
            ),
            ScanKind::Full
        );
        assert!(state.has_baseline);

        // Editing a tracked file uses incremental refresh.
        test.write("a.txt", "two\n");
        assert_eq!(
            refresh_once(
                &shared,
                &mut state,
                false,
                Some(std::slice::from_ref(&path_a)),
                t0 + Duration::from_secs(1),
            ),
            ScanKind::Incremental
        );
        assert_eq!(
            shared.snapshot().status.unstaged,
            vec![FileChange {
                path: "a.txt".into(),
                old_path: None,
                kind: ChangeKind::Modified,
            }]
        );
        shared
            .repo
            .assert_incremental_matches_full(&shared.snapshot().status)
            .expect("matches full scan");

        // Changing `.gitignore` falls back to full scan.
        let gitignore = test.path().join(".gitignore");
        test.write(".gitignore", "*.ignored\n");
        assert_eq!(
            refresh_once(
                &shared,
                &mut state,
                false,
                Some(&[gitignore]),
                t0 + Duration::from_secs(2),
            ),
            ScanKind::Full
        );
        assert_eq!(state.last_full_scan, t0 + Duration::from_secs(2));

        // Passing `None` (e.g. `.git/index` / `HEAD` / `refs` / rescan) falls back.
        test.git(&["add", ".gitignore"]);
        assert_eq!(
            refresh_once(
                &shared,
                &mut state,
                false,
                None,
                t0 + Duration::from_secs(3)
            ),
            ScanKind::Full
        );
        assert_eq!(state.last_full_scan, t0 + Duration::from_secs(3));

        // Periodic safety full scan triggers after 30 s of activity.
        test.write("b.txt", "two\n");
        let path_b = test.path().join("b.txt");
        assert_eq!(
            refresh_once(
                &shared,
                &mut state,
                false,
                Some(std::slice::from_ref(&path_b)),
                t0 + Duration::from_secs(3) + SAFETY_FULL_SCAN_INTERVAL,
            ),
            ScanKind::Full
        );
    }

    #[test]
    fn merge_paths_falls_back_above_limit() {
        let a: Vec<PathBuf> = (0..600).map(|i| PathBuf::from(format!("a/{i}"))).collect();
        let b: Vec<PathBuf> = (0..500).map(|i| PathBuf::from(format!("b/{i}"))).collect();
        assert!(merge_paths(Some(a.clone()), Some(b)).is_none());
        assert!(merge_paths(None, Some(a.clone())).is_none());
        assert!(merge_paths(Some(a), None).is_none());
    }
}
