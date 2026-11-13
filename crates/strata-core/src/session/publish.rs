//! The snapshot store and the state every session thread shares.

use std::collections::HashSet;
use std::mem;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};

use crate::git_cli::Git;
use crate::repo::Repo;
use crate::session::{DiffTicket, HistoryScope, OperationId, RepoSnapshot, SessionEvent};
use crate::watch::Changes;

pub(crate) type EventSink = Arc<dyn Fn(SessionEvent) + Send + Sync>;

/// Watcher refreshes held back while an operation was running.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct PendingRefresh {
    pub(crate) refs: bool,
    pub(crate) head: bool,
    pub(crate) index: bool,
    pub(crate) worktree: bool,
}

impl PendingRefresh {
    pub(crate) fn merge(&mut self, changes: &Changes) {
        self.refs |= changes.refs;
        self.head |= changes.head;
        self.index |= changes.index;
        self.worktree |= changes.worktree;
    }

    /// A ref or HEAD move invalidates the whole view, not just status.
    pub(crate) fn refs_changed(&self) -> bool {
        self.refs || self.head
    }

    pub(crate) fn status_changed(&self) -> bool {
        self.index || self.worktree
    }
}

/// Everything the workers need. The channel ends live on `Session` so that
/// dropping it closes them; `Shared` is held by the workers themselves.
pub(crate) struct Shared {
    pub(crate) repo: Repo,
    pub(crate) git: Git,
    events: EventSink,
    snapshot: RwLock<Arc<RepoSnapshot>>,
    /// Serializes publishing with generation bumps. A walk that was superseded
    /// therefore cannot write its snapshot after the newer one started.
    publish_lock: Mutex<()>,
    reload_generation: AtomicU64,
    shutdown: AtomicBool,
    next_operation: AtomicU64,
    next_ticket: AtomicU64,
    /// Starting points of the next history walk. Read by the loader when it
    /// starts, so a watch-triggered reload keeps the scope the UI chose.
    history_scope: RwLock<HistoryScope>,
    cancelled_diffs: Mutex<HashSet<DiffTicket>>,
    operation_active: AtomicBool,
    pending_refresh: Mutex<PendingRefresh>,
}

impl Shared {
    pub(crate) fn new(repo: Repo, git: Git, events: EventSink, snapshot: RepoSnapshot) -> Self {
        Self {
            repo,
            git,
            events,
            snapshot: RwLock::new(Arc::new(snapshot)),
            publish_lock: Mutex::new(()),
            reload_generation: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            next_operation: AtomicU64::new(0),
            next_ticket: AtomicU64::new(0),
            history_scope: RwLock::new(HistoryScope::All),
            cancelled_diffs: Mutex::new(HashSet::new()),
            operation_active: AtomicBool::new(false),
            pending_refresh: Mutex::new(PendingRefresh::default()),
        }
    }

    pub(crate) fn snapshot(&self) -> Arc<RepoSnapshot> {
        let guard = self.snapshot.read().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(&guard)
    }

    pub(crate) fn emit(&self, event: SessionEvent) {
        (self.events)(event);
    }

    pub(crate) fn reload_generation(&self) -> u64 {
        self.reload_generation.load(Ordering::SeqCst)
    }

    /// Starts a new walk generation. Bumping under the publish lock is what
    /// makes the generation check in `publish_for_generation` airtight.
    pub(crate) fn begin_reload(&self) -> u64 {
        let _guard = lock(&self.publish_lock);
        let generation = self.reload_generation.load(Ordering::SeqCst) + 1;
        self.reload_generation.store(generation, Ordering::SeqCst);
        generation
    }

    /// Whether a walk started for `generation` may still publish.
    pub(crate) fn is_current(&self, generation: u64) -> bool {
        !self.is_shutdown() && self.reload_generation() == generation
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    pub(crate) fn begin_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    pub(crate) fn next_operation_id(&self) -> OperationId {
        OperationId(self.next_operation.fetch_add(1, Ordering::SeqCst) + 1)
    }

    pub(crate) fn next_ticket(&self) -> DiffTicket {
        DiffTicket(self.next_ticket.fetch_add(1, Ordering::SeqCst) + 1)
    }

    pub(crate) fn cancel_diff(&self, ticket: DiffTicket) {
        lock(&self.cancelled_diffs).insert(ticket);
    }

    pub(crate) fn is_cancelled(&self, ticket: DiffTicket) -> bool {
        lock(&self.cancelled_diffs).contains(&ticket)
    }

    /// Drops the ticket's cancellation, so the set only grows with diffs the
    /// user cancelled while their job was still alive.
    pub(crate) fn forget_cancellation(&self, ticket: DiffTicket) {
        lock(&self.cancelled_diffs).remove(&ticket);
    }

    pub(crate) fn operation_running(&self) -> bool {
        self.operation_active.load(Ordering::SeqCst)
    }

    pub(crate) fn history_scope(&self) -> HistoryScope {
        self.history_scope
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set_history_scope(&self, scope: HistoryScope) {
        *self
            .history_scope
            .write()
            .unwrap_or_else(PoisonError::into_inner) = scope;
    }

    pub(crate) fn set_operation_running(&self, running: bool) {
        self.operation_active.store(running, Ordering::SeqCst);
    }

    pub(crate) fn hold_refresh(&self, changes: &Changes) {
        lock(&self.pending_refresh).merge(changes);
    }

    pub(crate) fn take_pending(&self) -> PendingRefresh {
        mem::take(&mut *lock(&self.pending_refresh))
    }

    /// Publishes a snapshot built from the current one.
    pub(crate) fn publish(&self, mutate: impl FnOnce(&mut RepoSnapshot)) {
        self.write(None, |snapshot| {
            mutate(snapshot);
            true
        });
    }

    /// Publishes only when `mutate` reports an actual change, so a watcher that
    /// fires for an unrelated path does not churn the UI.
    pub(crate) fn publish_if_changed(
        &self,
        mutate: impl FnOnce(&mut RepoSnapshot) -> bool,
    ) -> bool {
        self.write(None, mutate).is_some()
    }

    /// Publishes a walk result, unless a newer walk replaced this generation.
    pub(crate) fn publish_for_generation(
        &self,
        generation: u64,
        mutate: impl FnOnce(&mut RepoSnapshot),
    ) -> bool {
        self.write(Some(generation), |snapshot| {
            mutate(snapshot);
            true
        })
        .is_some()
    }

    fn write(
        &self,
        required_generation: Option<u64>,
        mutate: impl FnOnce(&mut RepoSnapshot) -> bool,
    ) -> Option<u64> {
        let generation = {
            let _publish = lock(&self.publish_lock);
            if self.is_shutdown() {
                return None;
            }
            if let Some(required) = required_generation
                && self.reload_generation.load(Ordering::SeqCst) != required
            {
                return None;
            }

            let mut slot = self
                .snapshot
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            let mut next = (**slot).clone();
            next.generation = slot.generation.wrapping_add(1);
            if !mutate(&mut next) {
                return None;
            }
            let generation = next.generation;
            *slot = Arc::new(next);
            generation
        };

        // Outside every lock: a callback may call `snapshot()` or `reload()`.
        self.emit(SessionEvent::SnapshotUpdated { generation });
        Some(generation)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
