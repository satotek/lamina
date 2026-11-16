//! Bounded LRU cache and debounced visible-row worker for commit changed-file counts.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui::{Context, Task};
use lamina_core::Oid;

use super::RepoView;
use crate::settings::Settings;

/// Maximum number of commits retained in the changed-file count cache.
pub(super) const CHANGED_COUNT_CACHE_CAPACITY: usize = 20_000;

/// Short settle window so rapid scrolling does not spawn diffs for transient rows.
const VISIBLE_DEBOUNCE: Duration = Duration::from_millis(50);

/// Bounded O(1)-amortized LRU cache from commit object id to changed-file count.
#[derive(Debug)]
pub(super) struct BoundedCountCache {
    capacity: usize,
    clock: u64,
    entries: HashMap<Oid, (usize, u64)>,
    order: VecDeque<(Oid, u64)>,
}

impl Default for BoundedCountCache {
    fn default() -> Self {
        Self::with_capacity(CHANGED_COUNT_CACHE_CAPACITY)
    }
}

impl BoundedCountCache {
    pub(super) fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            clock: 0,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn contains(&self, id: &Oid) -> bool {
        self.entries.contains_key(id)
    }

    /// Reads a cached count without mutating LRU recency (safe during `&RepoView` paint).
    pub(super) fn peek(&self, id: &Oid) -> Option<usize> {
        self.entries.get(id).map(|(count, _)| *count)
    }

    /// Reads a cached count and marks it most-recently-used.
    #[cfg(test)]
    pub(super) fn get(&mut self, id: &Oid) -> Option<usize> {
        self.touch(id);
        self.peek(id)
    }

    pub(super) fn touch(&mut self, id: &Oid) {
        let Some(entry) = self.entries.get_mut(id) else {
            return;
        };
        self.clock += 1;
        entry.1 = self.clock;
        self.order.push_back((*id, self.clock));
        self.maybe_compact();
    }

    pub(super) fn insert(&mut self, id: Oid, count: usize) {
        if self.capacity == 0 {
            return;
        }
        self.clock += 1;
        let stamp = self.clock;
        self.entries.insert(id, (count, stamp));
        self.order.push_back((id, stamp));

        while self.entries.len() > self.capacity {
            let Some((oldest_id, oldest_stamp)) = self.order.pop_front() else {
                break;
            };
            if self
                .entries
                .get(&oldest_id)
                .is_some_and(|(_, current)| *current == oldest_stamp)
            {
                self.entries.remove(&oldest_id);
            }
        }
        self.maybe_compact();
    }

    fn maybe_compact(&mut self) {
        let limit = self.capacity.saturating_mul(2).max(64);
        if self.order.len() <= limit {
            return;
        }
        let entries = &self.entries;
        self.order.retain(|(id, stamp)| {
            entries
                .get(id)
                .is_some_and(|(_, current)| *current == *stamp)
        });
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ScheduleAction {
    /// Visible rows did not introduce uncached work; nothing pending changed.
    Unchanged,
    /// All visible rows are cached (or none are missing); cancel obsolete work.
    CancelObsolete,
    /// Schedule a debounced background computation for `missing`.
    Schedule { generation: u64, missing: Vec<Oid> },
}

/// Tracks visible commit rows, debounces scroll movement, and cancels obsolete
/// background count queries when rows scroll out of view.
#[derive(Default)]
pub(super) struct ChangedFileCountTracker {
    cache: BoundedCountCache,
    generation: u64,
    last_observed: Vec<Oid>,
    pending: Vec<Oid>,
    debounce: Option<Task<()>>,
    inflight_set: HashSet<Oid>,
    inflight_cancel: Option<Arc<AtomicBool>>,
    inflight_task: Option<Task<()>>,
}

impl ChangedFileCountTracker {
    pub(super) fn peek(&self, id: Oid) -> Option<usize> {
        self.cache.peek(&id)
    }

    /// Cancels any queued debounce timer or background batch and advances the
    /// generation so stale results are ignored.
    pub(super) fn cancel_inflight(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.debounce = None;
        self.pending.clear();
        self.inflight_set.clear();
        self.last_observed.clear();
        if let Some(cancel) = self.inflight_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.inflight_task = None;
    }

    fn plan_visible(&mut self, visible: Vec<Oid>) -> ScheduleAction {
        let mut deduped = Vec::with_capacity(visible.len());
        let mut seen = HashSet::with_capacity(visible.len());
        for id in visible {
            if seen.insert(id) {
                deduped.push(id);
            }
        }

        if deduped != self.last_observed {
            for &id in &deduped {
                self.cache.touch(&id);
            }
            self.last_observed = deduped.clone();
        }

        let missing: Vec<Oid> = deduped
            .into_iter()
            .filter(|id| !self.cache.contains(id))
            .collect();

        if missing.is_empty() {
            return if !self.pending.is_empty()
                || !self.inflight_set.is_empty()
                || self.debounce.is_some()
                || self.inflight_task.is_some()
            {
                self.cancel_pending_and_worker();
                ScheduleAction::CancelObsolete
            } else {
                ScheduleAction::Unchanged
            };
        }

        if !self.pending.is_empty() && self.pending == missing {
            return ScheduleAction::Unchanged;
        }
        if !self.inflight_set.is_empty()
            && self.pending.is_empty()
            && missing.iter().all(|id| self.inflight_set.contains(id))
        {
            return ScheduleAction::Unchanged;
        }

        self.cancel_pending_and_worker();
        self.pending = missing.clone();
        ScheduleAction::Schedule {
            generation: self.generation,
            missing,
        }
    }

    fn cancel_pending_and_worker(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.debounce = None;
        self.pending.clear();
        self.inflight_set.clear();
        if let Some(cancel) = self.inflight_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.inflight_task = None;
    }

    fn accept_batch(&mut self, generation: u64, results: &[(Oid, usize)]) -> bool {
        if generation != self.generation {
            return false;
        }
        self.inflight_set.clear();
        self.inflight_cancel = None;
        self.inflight_task = None;
        for &(id, count) in results {
            self.cache.insert(id, count);
        }
        !results.is_empty()
    }
}

impl RepoView {
    pub(super) fn commit_changed_file_count(&self, id: Oid) -> Option<usize> {
        self.changed_counts.peek(id)
    }

    pub(super) fn observe_visible_changed_file_counts(
        &mut self,
        visible: Vec<Oid>,
        cx: &mut Context<Self>,
    ) {
        if self.smoke
            || self.session.is_none()
            || !Settings::get(cx).history.show_changed_file_count
        {
            return;
        }

        let ScheduleAction::Schedule { generation, .. } = self.changed_counts.plan_visible(visible)
        else {
            return;
        };

        self.changed_counts.debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(VISIBLE_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                this.flush_visible_changed_file_counts(generation, cx);
            });
        }));
    }

    fn flush_visible_changed_file_counts(&mut self, generation: u64, cx: &mut Context<Self>) {
        if self.changed_counts.generation != generation {
            return;
        }
        self.changed_counts.debounce = None;

        if self.smoke || !Settings::get(cx).history.show_changed_file_count {
            self.changed_counts.pending.clear();
            return;
        }
        let Some(session) = self.session.as_ref() else {
            self.changed_counts.pending.clear();
            return;
        };

        let cache = &self.changed_counts.cache;
        let batch: Vec<Oid> = self
            .changed_counts
            .pending
            .drain(..)
            .filter(|id| !cache.contains(id))
            .collect();
        if batch.is_empty() {
            return;
        }

        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_worker = Arc::clone(&cancel);
        self.changed_counts.inflight_set = batch.iter().copied().collect();
        self.changed_counts.inflight_cancel = Some(cancel);

        let repo = session.reader();
        self.changed_counts.inflight_task = Some(cx.spawn(async move |this, cx| {
            let computed = cx
                .background_executor()
                .spawn(async move {
                    let mut results = Vec::with_capacity(batch.len());
                    for oid in batch {
                        if cancel_worker.load(Ordering::Relaxed) {
                            return None;
                        }
                        if let Ok(count) = repo.commit_changed_file_count(oid) {
                            results.push((oid, count));
                        }
                    }
                    if cancel_worker.load(Ordering::Relaxed) {
                        None
                    } else {
                        Some(results)
                    }
                })
                .await;

            let Some(results) = computed else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if this.changed_counts.generation == generation
                    && Settings::get(cx).history.show_changed_file_count
                    && this.changed_counts.accept_batch(generation, &results)
                {
                    this.notify_if_active(cx);
                }
            });
        }));
    }

    pub(super) fn sync_changed_file_count_setting(&mut self, cx: &mut Context<Self>) {
        if !Settings::get(cx).history.show_changed_file_count {
            self.changed_counts.cancel_inflight();
        }
        self.notify_if_active(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(n: u32) -> Oid {
        format!("{n:040x}").parse().expect("hex object id")
    }

    #[test]
    fn cache_evicts_least_recently_used_beyond_capacity() {
        let mut cache = BoundedCountCache::with_capacity(3);
        let a = oid(1);
        let b = oid(2);
        let c = oid(3);
        let d = oid(4);

        cache.insert(a, 10);
        cache.insert(b, 20);
        cache.insert(c, 30);
        assert_eq!(cache.len(), 3);

        // Touch `a`, making `b` the least recently used entry.
        assert_eq!(cache.get(&a), Some(10));

        cache.insert(d, 40);
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.peek(&a), Some(10));
        assert_eq!(cache.peek(&b), None);
        assert_eq!(cache.peek(&c), Some(30));
        assert_eq!(cache.peek(&d), Some(40));
    }

    #[test]
    fn cache_enforces_20k_bound_and_compacts_history() {
        let mut cache = BoundedCountCache::default();
        for i in 0..25_000u32 {
            cache.insert(oid(i), i as usize);
        }
        assert_eq!(cache.len(), CHANGED_COUNT_CACHE_CAPACITY);
        for old in 0..5_000u32 {
            assert_eq!(cache.peek(&oid(old)), None);
        }
        for recent in 5_000..25_000u32 {
            assert_eq!(cache.peek(&oid(recent)), Some(recent as usize));
        }
    }

    #[test]
    fn tracker_cancels_inflight_and_ignores_stale_generation_when_scrolled_away() {
        let mut tracker = ChangedFileCountTracker::default();
        let old_a = oid(1);
        let old_b = oid(2);
        let new_c = oid(3);

        let first_gen = match tracker.plan_visible(vec![old_a, old_b]) {
            ScheduleAction::Schedule {
                generation,
                missing,
            } => {
                assert_eq!(missing, vec![old_a, old_b]);
                generation
            }
            other => panic!("expected initial schedule, got {other:?}"),
        };

        // Calling again with identical visible rows keeps the pending debounce intact.
        assert_eq!(
            tracker.plan_visible(vec![old_a, old_b]),
            ScheduleAction::Unchanged
        );

        // Simulate flush moving the pending batch into an active in-flight worker.
        let cancel = Arc::new(AtomicBool::new(false));
        tracker.pending.clear();
        tracker.inflight_cancel = Some(Arc::clone(&cancel));
        tracker.inflight_set = [old_a, old_b].into_iter().collect();

        // Scrolling to a new commit cancels the previous worker and bumps generation.
        let second_gen = match tracker.plan_visible(vec![new_c]) {
            ScheduleAction::Schedule {
                generation,
                missing,
            } => {
                assert_eq!(missing, vec![new_c]);
                generation
            }
            other => panic!("expected reschedule on scroll, got {other:?}"),
        };
        assert_ne!(second_gen, first_gen);
        assert!(cancel.load(Ordering::Relaxed));

        // Late results from `first_gen` must be ignored.
        assert!(!tracker.accept_batch(first_gen, &[(old_a, 4), (old_b, 9)]));
        assert_eq!(tracker.peek(old_a), None);

        // Results for `second_gen` are accepted and cached.
        assert!(tracker.accept_batch(second_gen, &[(new_c, 7)]));
        assert_eq!(tracker.peek(new_c), Some(7));

        // Returning to only-cached commits cancels any leftover pending work.
        assert_eq!(
            tracker.plan_visible(vec![new_c]),
            ScheduleAction::CancelObsolete
        );
    }
}
