//! State that belongs to the development and test entry points.
//!
//! `--fake`, `LAMINA_SMOKE`, the frame-stats overlay and `--debug-launch` all
//! exist for one run whose behaviour is measured or scripted. None of them is
//! part of a repository the user opened to work in, so they live together here
//! rather than beside that repository's real state.

use std::rc::Rc;
use std::sync::Arc;

use gpui::{Context, Window};
use lamina_core::Oid;
use lamina_core::session::{RepoReader, RepoSnapshot};

use super::RepoView;
use crate::debug_launch::{self, DebugLaunch, SnapshotRevMatch};
use crate::diff_view::Row as DiffRow;
use crate::fake::FakeRepo;
use crate::metrics::FrameMetrics;

/// The dev-only switches and the state their runs accumulate.
pub(super) struct DevHarness {
    /// `--fake [N]`: synthetic history in place of a session.
    pub(super) fake: Option<Rc<FakeRepo>>,
    /// Pre-flattened synthetic diffs so `--fake` does not rebuild one per frame.
    pub(super) fake_diffs: Rc<Vec<Rc<Vec<DiffRow>>>>,
    /// The `LAMINA_SMOKE` run, which scrolls the history as fast as it can.
    pub(super) smoke: bool,
    /// The frame-stats overlay, toggled by the `ToggleFrameStats` action.
    pub(super) overlay: bool,
    /// Set once the smoke run has reported; the app quits after that.
    pub(super) finished: bool,
    /// Frame timings the overlay and the smoke summary read.
    pub(super) metrics: FrameMetrics,
    /// The display's refresh interval, the overlay's target frame time.
    pub(super) target_frame_ms: f32,
    /// `--debug-launch` work waiting for the first snapshot.
    pub(super) debug_launch: Option<DebugLaunchRunner>,
}

impl DevHarness {
    /// `smoke` opens the overlay: the run exists to print its numbers, so they
    /// should be on screen while it scrolls.
    pub(super) fn new(smoke: bool) -> Self {
        Self {
            fake: None,
            fake_diffs: Rc::new(Vec::new()),
            smoke,
            overlay: smoke,
            finished: false,
            metrics: FrameMetrics::default(),
            target_frame_ms: 1000.0 / 60.0,
            debug_launch: None,
        }
    }
}

/// One slot of a `--debug-launch` revision still being resolved.
#[derive(Clone, Copy)]
pub(super) enum RevSlot {
    Select(usize),
    Tree,
    Blame,
}

pub(super) struct DebugLaunchRunner {
    pub(super) launch: DebugLaunch,
    /// One slot per comma-separated `--select` revision; a multi-selection
    /// needs every revision in the loaded history before it can be applied.
    pub(super) select_oids: Vec<Option<Result<Oid, String>>>,
    pub(super) select_resolving: bool,
    pub(super) tree_oid: Option<Result<Oid, String>>,
    pub(super) tree_resolving: bool,
    pub(super) blame_oid: Option<Result<Oid, String>>,
    pub(super) blame_resolving: bool,
}

impl DebugLaunchRunner {
    pub(super) fn new(launch: DebugLaunch) -> Self {
        let select_oids = launch
            .select
            .as_deref()
            .map(debug_launch::select_revs)
            .map(|revs| vec![None; revs.len()])
            .unwrap_or_default();
        Self {
            launch,
            select_oids,
            select_resolving: false,
            tree_oid: None,
            tree_resolving: false,
            blame_oid: None,
            blame_resolving: false,
        }
    }

    /// Returns `true` when this revision is settled (either resolved and present
    /// in `snapshot.commit_index`, or resolved to a final error / missing after
    /// `snapshot.commits.complete`).
    pub(super) fn resolve_rev(
        &mut self,
        rev: &str,
        which: RevSlot,
        snapshot: &RepoSnapshot,
        repo: Arc<dyn RepoReader>,
        window: &mut Window,
        cx: &mut Context<RepoView>,
    ) -> bool {
        let (slot, resolving) = match which {
            RevSlot::Select(index) => (&mut self.select_oids[index], &mut self.select_resolving),
            RevSlot::Tree => (&mut self.tree_oid, &mut self.tree_resolving),
            RevSlot::Blame => (&mut self.blame_oid, &mut self.blame_resolving),
        };

        if slot.is_none() {
            if *resolving {
                return false;
            }
            match debug_launch::resolve_in_snapshot(snapshot, rev) {
                SnapshotRevMatch::Found(oid) => *slot = Some(Ok(oid)),
                SnapshotRevMatch::Ambiguous(count) => {
                    *slot = Some(Err(format!(
                        "ambiguous revision '{rev}' ({count} matching commits)"
                    )));
                }
                SnapshotRevMatch::NotFound => {
                    *resolving = true;
                    let window_handle = window.window_handle();
                    let rev_owned = rev.to_owned();
                    cx.spawn(async move |this, cx| {
                        let resolved = cx
                            .background_executor()
                            .spawn(async move {
                                match repo.resolve_commit(&rev_owned) {
                                    Ok(Some(id)) => Ok(id),
                                    Ok(None) => Err(format!(
                                        "revision '{rev_owned}' did not resolve to a commit"
                                    )),
                                    Err(err) => Err(err.to_string()),
                                }
                            })
                            .await;
                        let _ = window_handle.update(cx, |_, window, cx| {
                            let _ = this.update(cx, |this, cx| {
                                this.finish_debug_rev_lookup(which, resolved, window, cx);
                            });
                        });
                    })
                    .detach();
                    return false;
                }
            }
        }

        match slot {
            Some(Ok(oid)) => snapshot.commit_index.contains_key(oid) || snapshot.commits.complete,
            Some(Err(_)) => true,
            None => false,
        }
    }
}
