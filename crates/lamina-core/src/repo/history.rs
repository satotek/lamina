//! History limited to a path.
//!
//! A commit changed `path` when its tree entry for `path` differs from the entry
//! in its first parent. Only the path's own tree entries are read: the lookup
//! descends both trees in lockstep along the path components, short-circuiting
//! as soon as any ancestor subtree matches, and caches resolved entries per
//! path level. When the commit-graph includes changed-path Bloom filters,
//! commits whose filters prove the path untouched are skipped without reading
//! any tree objects, and remaining batches are evaluated across worker threads
//! while preserving exact `--date-order` and rename semantics.
//!
//! Renames are followed backwards, the way `git log --follow` does: when the
//! tracked path appears in a commit but is absent from its first parent, the
//! parent tree is diffed with rename detection and the tracked path becomes the
//! source of the matching rewrite for all older commits. Each reported commit
//! carries the name the file had there.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use gix::bstr::ByteSlice as _;
use gix::object::tree::diff::ChangeDetached;
use gix::objs::{FindExt, TreeRefIter};

use super::Repo;
use super::bloom::{BloomKeys, CommitGraphBloom};
use super::log::{CancelCheck, PathCommit, PathWalk, Shallow};
use super::renames;
use crate::{CommitInfo, Error, Oid, Result};

/// Direct-mapped entry cache slots per worker/inline checker: power of two so
/// lookup is a single unaligned load and bitmask, with no clearing cliff.
const ROOT_CACHE_SLOTS: usize = 8_192;
const SUBTREE_CACHE_SLOTS: usize = 2_048;

/// 8 MiB of decoded objects for the primary reader (used for rename diffs and
/// inline checks); worker threads use smaller caches because their per-level
/// tree-entry caches already absorb most lookups.
const OBJECT_CACHE_BYTES: usize = 8 * 1024 * 1024;
const WORKER_OBJECT_CACHE_BYTES: usize = 4 * 1024 * 1024;

/// Check the first few hundred commits inline so paths touched near HEAD return
/// their first row immediately without thread-pool startup overhead.
const INLINE_WARMUP: usize = 256;
const INITIAL_BATCH_SIZE: usize = 4_096;
const MAX_BATCH_SIZE: usize = 16_384;
const WORKER_CHUNK_SIZE: usize = 512;
const MAX_WORKERS: usize = 8;

/// A tree entry as it decides whether a commit changed the path: id and mode.
/// The mode matters because a file turning executable, or a file becoming a
/// directory, is a change even when the blob id stays the same.
type Entry = (Oid, u16);

/// One row of a path-limited history: the commit and the path the file had in
/// it, which differs from the queried path for commits before a rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathHistoryEntry {
    pub commit: Oid,
    /// Repository-relative path of the file at `commit`.
    pub path: String,
}

#[derive(Clone, Copy)]
enum CommitChange {
    Unchanged,
    ChangedNoRename,
    /// Path exists in commit's tree with `dest_mode` and is absent in first parent.
    ChangedAdded(u16),
}

#[derive(Clone)]
struct CacheSlot {
    key: Oid,
    value: Option<Entry>,
    occupied: bool,
}

impl Default for CacheSlot {
    fn default() -> Self {
        Self {
            key: Oid::null(gix::hash::Kind::Sha1),
            value: None,
            occupied: false,
        }
    }
}

struct LevelCache {
    slots: Vec<CacheSlot>,
    mask: usize,
}

impl LevelCache {
    fn new(capacity: usize) -> Self {
        debug_assert!(capacity.is_power_of_two());
        Self {
            slots: vec![CacheSlot::default(); capacity],
            mask: capacity - 1,
        }
    }

    fn clear(&mut self) {
        for slot in &mut self.slots {
            slot.occupied = false;
        }
    }

    #[inline]
    fn index(&self, key: Oid) -> usize {
        let bytes = key.as_bytes();
        let raw = u64::from_ne_bytes(bytes[..8].try_into().expect("hash >= 8 bytes"));
        (raw as usize) & self.mask
    }

    #[inline]
    fn get(&self, key: Oid) -> Option<Option<Entry>> {
        let slot = &self.slots[self.index(key)];
        (slot.occupied && slot.key == key).then_some(slot.value)
    }

    #[inline]
    fn insert(&mut self, key: Oid, value: Option<Entry>) {
        let idx = self.index(key);
        self.slots[idx] = CacheSlot {
            key,
            value,
            occupied: true,
        };
    }
}

#[derive(Clone)]
struct PathQuery {
    components: Vec<Vec<u8>>,
    bloom_keys: Option<BloomKeys>,
}

impl PathQuery {
    fn from_components(components: Vec<Vec<u8>>) -> Self {
        let bloom_keys = if components.is_empty() {
            None
        } else {
            let total_len: usize =
                components.iter().map(Vec::len).sum::<usize>() + components.len() - 1;
            let mut joined = Vec::with_capacity(total_len);
            for (i, comp) in components.iter().enumerate() {
                if i > 0 {
                    joined.push(b'/');
                }
                joined.extend_from_slice(comp);
            }
            Some(BloomKeys::new(&joined))
        };
        Self {
            components,
            bloom_keys,
        }
    }
}

struct TreeChecker {
    repo: gix::Repository,
    odb_buf: Vec<u8>,
    caches: Vec<LevelCache>,
    cached_generation: u64,
}

impl TreeChecker {
    fn new(repo: gix::Repository) -> Self {
        Self {
            repo,
            odb_buf: Vec::new(),
            caches: Vec::new(),
            cached_generation: u64::MAX,
        }
    }

    fn sync_path(&mut self, path_generation: u64, num_components: usize) {
        if self.cached_generation == path_generation && self.caches.len() == num_components {
            return;
        }
        self.cached_generation = path_generation;
        for (idx, cache) in self.caches.iter_mut().enumerate() {
            if idx < num_components {
                cache.clear();
            }
        }
        while self.caches.len() < num_components {
            let cap = if self.caches.is_empty() {
                ROOT_CACHE_SLOTS
            } else {
                SUBTREE_CACHE_SLOTS
            };
            self.caches.push(LevelCache::new(cap));
        }
        self.caches.truncate(num_components);
    }

    fn read_commit_tree(&mut self, id: Oid) -> Result<Oid> {
        let commit = self.repo.find_commit(id).map_err(Error::gix)?;
        Ok(commit.tree_id().map_err(Error::gix)?.detach())
    }

    fn check_commit(
        &mut self,
        commit: &PathCommit,
        query: &PathQuery,
        bloom: Option<&CommitGraphBloom>,
        path_generation: u64,
    ) -> Result<CommitChange> {
        self.sync_path(path_generation, query.components.len());
        let tree = match commit.tree {
            Some(tree) => tree,
            None => self.read_commit_tree(commit.id)?,
        };
        let Some(parent) = commit.parent else {
            // A root commit changed the path when it exists there (or for the
            // empty repository-root path).
            let changed = query.components.is_empty()
                || self.lookup_from(0, tree, &query.components)?.is_some();
            return Ok(if changed {
                CommitChange::ChangedNoRename
            } else {
                CommitChange::Unchanged
            });
        };
        let parent_tree = match commit.parent_tree {
            Some(tree) => tree,
            None => match self.read_commit_tree(parent) {
                Ok(tree) => tree,
                // The first parent is missing only outside a shallow clone's
                // known boundary; report changed when the path exists here.
                Err(_) => {
                    let changed = !query.components.is_empty()
                        && self.lookup_from(0, tree, &query.components)?.is_some();
                    return Ok(if changed {
                        CommitChange::ChangedNoRename
                    } else {
                        CommitChange::Unchanged
                    });
                }
            },
        };
        if tree == parent_tree {
            return Ok(CommitChange::Unchanged);
        }
        if query.components.is_empty() {
            return Ok(CommitChange::ChangedNoRename);
        }
        if let (Some(bloom), Some(pos), Some(keys)) =
            (bloom, commit.graph_pos, query.bloom_keys.as_ref())
            && !bloom.may_contain(pos, keys)
        {
            return Ok(CommitChange::Unchanged);
        }
        self.trees_differ(tree, parent_tree, &query.components)
    }

    fn trees_differ(
        &mut self,
        mut tree_a: Oid,
        mut tree_b: Oid,
        components: &[Vec<u8>],
    ) -> Result<CommitChange> {
        let last = components.len() - 1;
        for level in 0..=last {
            if tree_a == tree_b {
                return Ok(CommitChange::Unchanged);
            }
            let name = components[level].as_slice();
            let entry_a = self.entry_at_level(level, tree_a, name)?;
            let entry_b = self.entry_at_level(level, tree_b, name)?;
            if entry_a == entry_b {
                return Ok(CommitChange::Unchanged);
            }
            if level == last {
                return Ok(match (entry_a, entry_b) {
                    (Some((_, mode)), None) => CommitChange::ChangedAdded(mode),
                    _ => CommitChange::ChangedNoRename,
                });
            }
            match (next_tree(entry_a), next_tree(entry_b)) {
                (Some(sub_a), Some(sub_b)) => {
                    tree_a = sub_a;
                    tree_b = sub_b;
                }
                (Some(sub_a), None) => {
                    return Ok(match self.lookup_from(level + 1, sub_a, components)? {
                        Some((_, mode)) => CommitChange::ChangedAdded(mode),
                        None => CommitChange::Unchanged,
                    });
                }
                (None, Some(sub_b)) => {
                    return Ok(
                        if self.lookup_from(level + 1, sub_b, components)?.is_some() {
                            CommitChange::ChangedNoRename
                        } else {
                            CommitChange::Unchanged
                        },
                    );
                }
                (None, None) => return Ok(CommitChange::Unchanged),
            }
        }
        Ok(CommitChange::Unchanged)
    }

    fn lookup_from(
        &mut self,
        start_level: usize,
        mut tree: Oid,
        components: &[Vec<u8>],
    ) -> Result<Option<Entry>> {
        let last = components.len();
        for (offset, component) in components[start_level..].iter().enumerate() {
            let level = start_level + offset;
            let Some(entry) = self.entry_at_level(level, tree, component)? else {
                return Ok(None);
            };
            if level + 1 == last {
                return Ok(Some(entry));
            }
            let Some(sub_tree) = next_tree(Some(entry)) else {
                return Ok(None);
            };
            tree = sub_tree;
        }
        Ok(None)
    }

    fn entry_at_level(
        &mut self,
        level: usize,
        tree: Oid,
        component: &[u8],
    ) -> Result<Option<Entry>> {
        if let Some(cached) = self.caches[level].get(tree) {
            return Ok(cached);
        }
        let found = self.scan_tree(tree, component)?;
        self.caches[level].insert(tree, found);
        Ok(found)
    }

    fn scan_tree(&mut self, tree: Oid, target: &[u8]) -> Result<Option<Entry>> {
        let data = self
            .repo
            .objects
            .find(&tree, &mut self.odb_buf)
            .map_err(Error::gix)?;
        if data.kind != gix::objs::Kind::Tree {
            return Ok(None);
        }
        let hash_len = data.object_hash.len_in_bytes();
        if let Ok(found) = scan_tree_entry(data.data, hash_len, target) {
            return Ok(found);
        }
        let found = TreeRefIter::from_bytes(data.data, data.object_hash)
            .filter_map(std::result::Result::ok)
            .find(|entry| entry.filename == target)
            .map(|entry| (entry.oid.to_owned(), entry.mode.value()));
        Ok(found)
    }
}

#[inline]
fn is_tree_mode(mode: u16) -> bool {
    (mode & 0o170000) == 0o040000
}

#[inline]
fn next_tree(entry: Option<Entry>) -> Option<Oid> {
    let (oid, mode) = entry?;
    is_tree_mode(mode).then_some(oid)
}

fn scan_tree_entry(
    data: &[u8],
    hash_len: usize,
    target: &[u8],
) -> std::result::Result<Option<Entry>, ()> {
    let mut pos = 0usize;
    while pos < data.len() {
        let rest = &data[pos..];
        let space_rel = rest.find_byte(b' ').ok_or(())?;
        if space_rel == 0 || space_rel > 6 {
            return Err(());
        }
        let after_space = space_rel + 1;
        let nul_rel = rest[after_space..].find_byte(0).ok_or(())?;
        let oid_start = after_space + nul_rel + 1;
        let oid_end = oid_start + hash_len;
        if oid_end > rest.len() {
            return Err(());
        }
        if nul_rel == target.len() && &rest[after_space..after_space + nul_rel] == target {
            let mut mode = 0u16;
            for &b in &rest[..space_rel] {
                if !(b'0'..=b'7').contains(&b) {
                    return Err(());
                }
                mode = mode
                    .checked_mul(8)
                    .and_then(|m| m.checked_add(u16::from(b - b'0')))
                    .ok_or(())?;
            }
            let oid = Oid::try_from(&rest[oid_start..oid_end]).map_err(|_| ())?;
            return Ok(Some((oid, mode)));
        }
        pos += oid_end;
    }
    Ok(None)
}

struct SharedPoolState {
    generation: u64,
    shutdown: bool,
    commits: Vec<PathCommit>,
    query: Arc<PathQuery>,
    path_generation: u64,
    next_chunk: AtomicUsize,
    chunks_total: usize,
    chunks_done: usize,
    chunk_results: Vec<Option<Vec<Result<CommitChange>>>>,
}

struct WorkerPool {
    shared: Arc<(Mutex<SharedPoolState>, Condvar, Condvar)>,
    handles: Vec<JoinHandle<()>>,
}

impl WorkerPool {
    fn new(
        num_workers: usize,
        sync_repo: gix::ThreadSafeRepository,
        bloom: Option<Arc<CommitGraphBloom>>,
        cancelled: Option<CancelCheck>,
    ) -> Self {
        let state = SharedPoolState {
            generation: 0,
            shutdown: false,
            commits: Vec::new(),
            query: Arc::new(PathQuery::from_components(Vec::new())),
            path_generation: 0,
            next_chunk: AtomicUsize::new(0),
            chunks_total: 0,
            chunks_done: 0,
            chunk_results: Vec::new(),
        };
        let shared = Arc::new((Mutex::new(state), Condvar::new(), Condvar::new()));
        let mut handles = Vec::with_capacity(num_workers);

        for _ in 0..num_workers {
            let shared = Arc::clone(&shared);
            let sync_repo = sync_repo.clone();
            let bloom = bloom.clone();
            let cancelled = cancelled.clone();
            handles.push(std::thread::spawn(move || {
                worker_loop(shared, sync_repo, bloom, cancelled);
            }));
        }

        Self { shared, handles }
    }

    fn eval(
        &mut self,
        commits: Vec<PathCommit>,
        query: &Arc<PathQuery>,
        path_generation: u64,
    ) -> (Vec<PathCommit>, Vec<Result<CommitChange>>) {
        let chunks_total = commits.len().div_ceil(WORKER_CHUNK_SIZE);
        let (lock, work_cv, done_cv) = &*self.shared;
        let mut guard = lock.lock().expect("worker pool mutex poisoned");
        guard.generation += 1;
        guard.commits = commits;
        guard.query = Arc::clone(query);
        guard.path_generation = path_generation;
        guard.next_chunk.store(0, Ordering::Relaxed);
        guard.chunks_total = chunks_total;
        guard.chunks_done = 0;
        guard.chunk_results.resize_with(chunks_total, || None);
        work_cv.notify_all();

        while guard.chunks_done < guard.chunks_total {
            guard = done_cv.wait(guard).expect("worker pool mutex poisoned");
        }

        let commits = std::mem::take(&mut guard.commits);
        let mut results = Vec::with_capacity(commits.len());
        for (chunk_idx, slot) in guard.chunk_results.drain(..).enumerate() {
            let chunk = slot.expect("all chunks completed");
            let expected_len =
                WORKER_CHUNK_SIZE.min(commits.len().saturating_sub(chunk_idx * WORKER_CHUNK_SIZE));
            let truncated = chunk.len() < expected_len;
            results.extend(chunk);
            if truncated {
                break;
            }
        }
        (commits, results)
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        {
            let (lock, work_cv, _) = &*self.shared;
            let mut guard = lock.lock().expect("worker pool mutex poisoned");
            guard.shutdown = true;
            work_cv.notify_all();
        }
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

fn worker_loop(
    shared: Arc<(Mutex<SharedPoolState>, Condvar, Condvar)>,
    sync_repo: gix::ThreadSafeRepository,
    bloom: Option<Arc<CommitGraphBloom>>,
    cancelled: Option<CancelCheck>,
) {
    let mut local = sync_repo.to_thread_local();
    local.object_cache_size_if_unset(WORKER_OBJECT_CACHE_BYTES);
    let mut checker = TreeChecker::new(local);
    let mut seen_generation = 0u64;
    let (lock, work_cv, done_cv) = &*shared;

    loop {
        let (chunk_idx, generation, query, path_gen, commits_ptr_len): (
            usize,
            u64,
            Arc<PathQuery>,
            u64,
            usize,
        );
        let chunk_start;
        let chunk_end;
        let result: Vec<Result<CommitChange>>;
        {
            let mut guard = lock.lock().expect("worker pool mutex poisoned");
            loop {
                if guard.shutdown {
                    return;
                }
                if guard.generation != seen_generation && guard.chunks_total > 0 {
                    break;
                }
                guard = work_cv.wait(guard).expect("worker pool mutex poisoned");
            }
            generation = guard.generation;
            let total = guard.chunks_total;
            let idx = guard.next_chunk.fetch_add(1, Ordering::Relaxed);
            if idx >= total {
                seen_generation = generation;
                continue;
            }
            chunk_idx = idx;
            commits_ptr_len = guard.commits.len();
            chunk_start = idx * WORKER_CHUNK_SIZE;
            chunk_end = (chunk_start + WORKER_CHUNK_SIZE).min(commits_ptr_len);
            query = Arc::clone(&guard.query);
            path_gen = guard.path_generation;

            let mut slice = Vec::with_capacity(chunk_end - chunk_start);
            slice.extend_from_slice(&guard.commits[chunk_start..chunk_end]);
            drop(guard);

            result = eval_chunk(
                &mut checker,
                &slice,
                &query,
                bloom.as_deref(),
                path_gen,
                &cancelled,
            );
        }

        let mut guard = lock.lock().expect("worker pool mutex poisoned");
        if guard.generation == generation {
            guard.chunk_results[chunk_idx] = Some(result);
            guard.chunks_done += 1;
            if guard.chunks_done == guard.chunks_total {
                done_cv.notify_one();
            }
        }
    }
}

fn eval_chunk(
    checker: &mut TreeChecker,
    commits: &[PathCommit],
    query: &PathQuery,
    bloom: Option<&CommitGraphBloom>,
    path_generation: u64,
    cancelled: &Option<CancelCheck>,
) -> Vec<Result<CommitChange>> {
    let mut out = Vec::with_capacity(commits.len());
    for (idx, commit) in commits.iter().enumerate() {
        if idx & 63 == 0
            && let Some(check) = cancelled
            && check()
        {
            out.resize_with(commits.len(), || Ok(CommitChange::Unchanged));
            return out;
        }
        match checker.check_commit(commit, query, bloom, path_generation) {
            Ok(change) => out.push(Ok(change)),
            Err(err) => {
                out.push(Err(err));
                return out;
            }
        }
    }
    out
}

/// Streams [`PathHistoryEntry`]s for commits that changed the path, in the same
/// order as [`Repo::walk_commits`], following the file across renames.
///
/// A commit is reported when its tree entry for the tracked path differs from
/// its first parent's. That matches `git log --follow --full-history
/// --date-order HEAD --branches --remotes --tags refs/stash -- <path>` (with
/// `refs/stash` only when a stash exists) except for merge commits that are
/// TREESAME to their first parent but differ from another parent: git's full
/// history lists those, this walk does not, because it never traverses the
/// other parents.
pub struct PathHistoryEntries {
    sync_repo: gix::ThreadSafeRepository,
    checker: TreeChecker,
    walk: PathWalk,
    shallow: Shallow,
    bloom: Option<Arc<CommitGraphBloom>>,
    query: Arc<PathQuery>,
    /// `query.components` joined back into a path for the caller.
    display_path: String,
    path_generation: u64,
    checked_inline: usize,
    batch_size: usize,
    batch_commits: Vec<PathCommit>,
    batch_results: Vec<Result<CommitChange>>,
    batch_cursor: usize,
    trailing_error: Option<Error>,
    pool: Option<WorkerPool>,
    failed: bool,
    /// `None` for the public, uncancellable walk.
    cancelled: Option<CancelCheck>,
}

impl Iterator for PathHistoryEntries {
    type Item = Result<PathHistoryEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        loop {
            if self.is_cancelled() {
                return None;
            }
            if self.checked_inline < INLINE_WARMUP && self.batch_cursor >= self.batch_commits.len()
            {
                let commit = match self.walk.next() {
                    Some(Ok(commit)) => commit,
                    Some(Err(error)) => {
                        self.failed = true;
                        return Some(Err(error));
                    }
                    None => return None,
                };
                self.checked_inline += 1;
                match self.checker.check_commit(
                    &commit,
                    &self.query,
                    self.bloom.as_deref(),
                    self.path_generation,
                ) {
                    Ok(CommitChange::Unchanged) => continue,
                    Ok(change) => {
                        let entry = PathHistoryEntry {
                            commit: commit.id,
                            path: self.display_path.clone(),
                        };
                        if let CommitChange::ChangedAdded(dest_mode) = change
                            && let Err(error) = self.follow_rename(&commit, dest_mode)
                        {
                            self.failed = true;
                            return Some(Err(error));
                        }
                        return Some(Ok(entry));
                    }
                    Err(error) => {
                        self.failed = true;
                        return Some(Err(error));
                    }
                }
            }

            if self.batch_cursor >= self.batch_commits.len() && !self.refill_batch() {
                if let Some(error) = self.trailing_error.take() {
                    self.failed = true;
                    return Some(Err(error));
                }
                return None;
            }

            let idx = self.batch_cursor;
            self.batch_cursor += 1;
            let result =
                std::mem::replace(&mut self.batch_results[idx], Ok(CommitChange::Unchanged));
            match result {
                Ok(CommitChange::Unchanged) => continue,
                Ok(change) => {
                    let commit_id = self.batch_commits[idx].id;
                    let entry = PathHistoryEntry {
                        commit: commit_id,
                        path: self.display_path.clone(),
                    };
                    if let CommitChange::ChangedAdded(dest_mode) = change {
                        let commit = self.batch_commits[idx];
                        match self.follow_rename(&commit, dest_mode) {
                            Ok(true) => {
                                // Path switched to the rename source: discard
                                // tail evaluations under the newer name and
                                // re-check remaining commits in the batch.
                                self.batch_commits.drain(..self.batch_cursor);
                                self.batch_cursor = 0;
                                self.evaluate_batch();
                            }
                            Ok(false) => {}
                            Err(error) => {
                                self.failed = true;
                                return Some(Err(error));
                            }
                        }
                    }
                    return Some(Ok(entry));
                }
                Err(error) => {
                    self.failed = true;
                    return Some(Err(error));
                }
            }
        }
    }
}

impl PathHistoryEntries {
    pub(crate) fn read_commit_info(
        &mut self,
        id: Oid,
        parents_buf: &mut Vec<Oid>,
    ) -> Result<CommitInfo> {
        super::log::commit_info(
            &self.checker.repo,
            id,
            &self.shallow,
            &mut self.checker.odb_buf,
            parents_buf,
        )
    }

    fn is_cancelled(&self) -> bool {
        match &self.cancelled {
            Some(cancelled) => cancelled(),
            None => false,
        }
    }

    fn refill_batch(&mut self) -> bool {
        self.batch_commits.clear();
        self.batch_results.clear();
        self.batch_cursor = 0;

        if let Some(error) = self.trailing_error.take() {
            self.trailing_error = Some(error);
            return false;
        }

        let target = self.batch_size;
        while self.batch_commits.len() < target {
            if self.is_cancelled() {
                self.batch_commits.clear();
                return false;
            }
            match self.walk.next() {
                Some(Ok(commit)) => self.batch_commits.push(commit),
                Some(Err(error)) => {
                    self.trailing_error = Some(error);
                    break;
                }
                None => break,
            }
        }
        if self.batch_commits.is_empty() {
            return false;
        }
        self.batch_size = (self.batch_size * 2).min(MAX_BATCH_SIZE);
        self.evaluate_batch();
        true
    }

    fn evaluate_batch(&mut self) {
        self.batch_results.clear();
        if self.batch_commits.is_empty() {
            return;
        }
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, MAX_WORKERS);
        if workers <= 1 || self.batch_commits.len() < WORKER_CHUNK_SIZE {
            self.batch_results.reserve(self.batch_commits.len());
            for commit in &self.batch_commits {
                if self.is_cancelled() {
                    self.batch_results
                        .resize_with(self.batch_commits.len(), || Ok(CommitChange::Unchanged));
                    return;
                }
                self.batch_results.push(self.checker.check_commit(
                    commit,
                    &self.query,
                    self.bloom.as_deref(),
                    self.path_generation,
                ));
            }
            return;
        }

        let pool = self.pool.get_or_insert_with(|| {
            WorkerPool::new(
                workers,
                self.sync_repo.clone(),
                self.bloom.clone(),
                self.cancelled.clone(),
            )
        });
        let commits = std::mem::take(&mut self.batch_commits);
        let (commits, results) = pool.eval(commits, &self.query, self.path_generation);
        self.batch_commits = commits;
        self.batch_results = results;
    }

    /// Move the tracked path to the old name when this commit renamed the file.
    /// Returns `true` when `display_path` changed.
    fn follow_rename(&mut self, commit: &PathCommit, dest_mode: u16) -> Result<bool> {
        let Some(parent_id) = commit.parent else {
            return Ok(false);
        };
        if self.query.components.is_empty() {
            return Ok(false);
        }
        let commit_tree = match commit.tree {
            Some(tree) => tree,
            None => self.checker.read_commit_tree(commit.id)?,
        };
        let parent_tree = match commit.parent_tree {
            Some(tree) => tree,
            None => match self.checker.read_commit_tree(parent_id) {
                Ok(tree) => tree,
                Err(_) => return Ok(false),
            },
        };
        if commit_tree == parent_tree {
            return Ok(false);
        }
        let Some(old_path) = self.rename_source(parent_tree, commit_tree, dest_mode)? else {
            return Ok(false);
        };
        self.set_path(&old_path);
        Ok(true)
    }

    /// The parent-side path of the rename whose destination is the tracked
    /// path, as `gix`'s rewrite tracker reports it.
    fn rename_source(
        &self,
        parent_tree: Oid,
        commit_tree: Oid,
        dest_mode: u16,
    ) -> Result<Option<Vec<u8>>> {
        let parent = self
            .checker
            .repo
            .find_tree(parent_tree)
            .map_err(Error::gix)?;
        let commit = self
            .checker
            .repo
            .find_tree(commit_tree)
            .map_err(Error::gix)?;

        // Directory renames in gix-diff only depend on exact-ID child matches
        // (`percentage: None`); skipping the similarity pass avoids quadratic
        // blob diffs when a directory moves alongside many other changes.
        if is_tree_mode(dest_mode) {
            let changes = self
                .checker
                .repo
                .diff_tree_to_tree(
                    Some(&parent),
                    Some(&commit),
                    Some(
                        gix::diff::Options::default().with_rewrites(Some(gix::diff::Rewrites {
                            percentage: None,
                            ..gix::diff::Rewrites::default()
                        })),
                    ),
                )
                .map_err(Error::gix)?;
            for change in changes {
                if let ChangeDetached::Rewrite {
                    location,
                    source_location,
                    copy: false,
                    ..
                } = change
                    && location.as_slice() == self.display_path.as_bytes()
                {
                    return Ok(Some(source_location.into()));
                }
            }
            return Ok(None);
        }

        // For non-tree targets, run the raw tree diff without rewrites and
        // resolve renames with the exact same matching order as gix-diff,
        // skipping blob diffs whose size ratio cannot reach the threshold and
        // stopping as soon as the tracked destination is resolved.
        let raw_changes = self
            .checker
            .repo
            .diff_tree_to_tree(
                Some(&parent),
                Some(&commit),
                Some(gix::diff::Options::default().with_rewrites(None)),
            )
            .map_err(Error::gix)?;
        renames::source_of(
            &raw_changes,
            &self.checker.repo,
            self.display_path.as_bytes(),
        )
    }

    /// Point the walk at another name; cached entries were resolved for the old
    /// one, so they cannot be reused.
    fn set_path(&mut self, path: &[u8]) {
        let components: Vec<Vec<u8>> = path
            .split(|byte| *byte == b'/')
            .filter(|component| {
                let component: &[u8] = component;
                !component.is_empty() && component != b"."
            })
            .map(<[u8]>::to_vec)
            .collect();
        self.display_path = components
            .iter()
            .map(|component| String::from_utf8_lossy(component))
            .collect::<Vec<_>>()
            .join("/");
        self.query = Arc::new(PathQuery::from_components(components));
        self.path_generation += 1;
    }
}

/// Streams ids of commits that changed `path`, for callers that do not need the
/// per-commit path.
pub struct PathHistory {
    entries: PathHistoryEntries,
}

impl Iterator for PathHistory {
    type Item = Result<Oid>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.entries.next()?.map(|entry| entry.commit))
    }
}

impl Repo {
    /// Like `git log --follow --full-history --date-order HEAD --branches
    /// --remotes --tags refs/stash -- <path>`.
    ///
    /// `path` may name a file or a directory; a directory matches changes to any
    /// entry below it because its tree id covers the whole subtree. An empty
    /// path names the repository root and reports every commit. Only files
    /// follow renames; a directory only follows its own path components, which
    /// is also how git treats a pathspec.
    pub fn path_history(&self, path: &str) -> Result<PathHistory> {
        self.path_history_with_cancel(path, None)
    }

    /// [`path_history`](Self::path_history) that also reports the name the file
    /// had in each commit, for an old-name column in the UI.
    pub fn path_history_entries(&self, path: &str) -> Result<PathHistoryEntries> {
        self.path_history_entries_with_cancel(path, None)
    }

    /// [`path_history`](Self::path_history) that stops once `cancelled` returns
    /// `true`, even between commits that do not match the path.
    pub(crate) fn path_history_cancellable(
        &self,
        path: &str,
        cancelled: CancelCheck,
    ) -> Result<PathHistory> {
        self.path_history_with_cancel(path, Some(cancelled))
    }

    /// [`path_history_entries`](Self::path_history_entries) that stops once
    /// `cancelled` returns `true`, even between commits that do not match the path.
    pub(crate) fn path_history_entries_cancellable(
        &self,
        path: &str,
        cancelled: CancelCheck,
    ) -> Result<PathHistoryEntries> {
        self.path_history_entries_with_cancel(path, Some(cancelled))
    }

    fn path_history_with_cancel(
        &self,
        path: &str,
        cancelled: Option<CancelCheck>,
    ) -> Result<PathHistory> {
        Ok(PathHistory {
            entries: self.path_history_entries_with_cancel(path, cancelled)?,
        })
    }

    fn path_history_entries_with_cancel(
        &self,
        path: &str,
        cancelled: Option<CancelCheck>,
    ) -> Result<PathHistoryEntries> {
        let mut repo = self.local();
        repo.object_cache_size_if_unset(OBJECT_CACHE_BYTES);
        let (walk, shallow, bloom) = self.path_walk_with(cancelled.clone())?;
        let mut entries = PathHistoryEntries {
            sync_repo: self.inner.clone(),
            checker: TreeChecker::new(repo),
            walk,
            shallow,
            bloom: bloom.map(Arc::new),
            query: Arc::new(PathQuery::from_components(Vec::new())),
            display_path: String::new(),
            path_generation: 0,
            checked_inline: 0,
            batch_size: INITIAL_BATCH_SIZE,
            batch_commits: Vec::new(),
            batch_results: Vec::new(),
            batch_cursor: 0,
            trailing_error: None,
            pool: None,
            failed: false,
            cancelled,
        };
        entries.set_path(path.as_bytes());
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::repo::{PathHistoryEntry, Repo};
    use crate::test_support::{AUTHOR_EMAIL, AUTHOR_NAME, TestRepo, shallow_clone_fixture};
    use gix::object::tree::diff::ChangeDetached;
    use gix::objs::{FindExt as _, TreeRefIter};

    /// Straightforward single-threaded reference path walk without caches,
    /// Bloom filters, parallel workers, or fast rename prefilters.
    fn reference_path_history(repo: &Repo, path: &str) -> Vec<PathHistoryEntry> {
        let local = repo.local();
        let mut components: Vec<Vec<u8>> = path
            .as_bytes()
            .split(|b| *b == b'/')
            .filter(|c| !c.is_empty() && *c != b".")
            .map(<[u8]>::to_vec)
            .collect();
        let mut display_path = components
            .iter()
            .map(|c| String::from_utf8_lossy(c))
            .collect::<Vec<_>>()
            .join("/");

        let lookup_entry =
            |tree_id: gix::ObjectId, comps: &[Vec<u8>]| -> Option<(gix::ObjectId, u16)> {
                let mut current = tree_id;
                for (idx, comp) in comps.iter().enumerate() {
                    let mut buf = Vec::new();
                    let obj = local.objects.find(&current, &mut buf).ok()?;
                    if obj.kind != gix::objs::Kind::Tree {
                        return None;
                    }
                    let entry = TreeRefIter::from_bytes(obj.data, obj.object_hash)
                        .filter_map(Result::ok)
                        .find(|e| e.filename == comp.as_slice())?;
                    if idx + 1 == comps.len() {
                        return Some((entry.oid.to_owned(), entry.mode.value()));
                    }
                    if !entry.mode.is_tree() {
                        return None;
                    }
                    current = entry.oid.to_owned();
                }
                None
            };

        let commits: Vec<_> = repo
            .walk_commits()
            .expect("walk commits")
            .map(|c| c.expect("commit"))
            .collect();
        let mut trees = HashMap::new();
        for c in &commits {
            let tree = local
                .find_commit(c.id)
                .expect("commit")
                .tree_id()
                .expect("tree")
                .detach();
            trees.insert(c.id, tree);
        }

        let mut out = Vec::new();
        for c in commits {
            let tree = trees[&c.id];
            let parent = c.parents.first().copied();
            let changed = match parent.and_then(|p| trees.get(&p).copied()) {
                None => components.is_empty() || lookup_entry(tree, &components).is_some(),
                Some(ptree) => {
                    tree != ptree
                        && (components.is_empty()
                            || lookup_entry(tree, &components) != lookup_entry(ptree, &components))
                }
            };
            if !changed {
                continue;
            }
            out.push(PathHistoryEntry {
                commit: c.id,
                path: display_path.clone(),
            });

            if let Some(ptree) = parent.and_then(|p| trees.get(&p).copied())
                && !components.is_empty()
                && tree != ptree
                && lookup_entry(tree, &components).is_some()
                && lookup_entry(ptree, &components).is_none()
            {
                let p_obj = local.find_tree(ptree).expect("ptree");
                let c_obj = local.find_tree(tree).expect("ctree");
                let changes = local
                    .diff_tree_to_tree(
                        Some(&p_obj),
                        Some(&c_obj),
                        Some(
                            gix::diff::Options::default()
                                .with_rewrites(Some(gix::diff::Rewrites::default())),
                        ),
                    )
                    .expect("diff");
                for change in changes {
                    if let ChangeDetached::Rewrite {
                        location,
                        source_location,
                        copy: false,
                        ..
                    } = change
                        && location.as_slice() == display_path.as_bytes()
                    {
                        components = source_location
                            .as_slice()
                            .split(|b| *b == b'/')
                            .filter(|c| !c.is_empty() && *c != b".")
                            .map(<[u8]>::to_vec)
                            .collect();
                        display_path = components
                            .iter()
                            .map(|c| String::from_utf8_lossy(c))
                            .collect::<Vec<_>>()
                            .join("/");
                        break;
                    }
                }
            }
        }
        out
    }

    /// Append `count` commits that each overwrite `unrelated.txt`, using one
    /// `fast-import` run instead of one `git add` + `git commit` pair per
    /// commit: the chain is plain and linear, so the import keeps its shape
    /// (same messages, contents, parents and timestamps) while removing the
    /// per-commit process startup that dominated the fixture.
    fn append_unrelated_commits(
        fixture: &TestRepo,
        message_prefix: &str,
        content_prefix: &str,
        count: usize,
    ) {
        use std::fmt::Write as _;

        // `fast-import` requires an explicit committer. Reusing the fixture's
        // own timestamp keeps these commits date-identical to the rest of the
        // history, so date-order ties resolve the same way.
        let time = fixture
            .git(&["log", "-1", "--format=%ct"])
            .trim()
            .to_owned();
        let mut stream = String::new();
        let mut previous = "refs/heads/main^0".to_owned();
        for index in 0..count {
            let mark = index + 1;
            let message = format!("{message_prefix} {index}");
            let contents = format!("{content_prefix} {index}\n");
            let _ = writeln!(stream, "commit refs/heads/main");
            let _ = writeln!(stream, "mark :{mark}");
            let _ = writeln!(
                stream,
                "committer {AUTHOR_NAME} <{AUTHOR_EMAIL}> {time} +0000"
            );
            let _ = writeln!(stream, "data {}", message.len());
            let _ = writeln!(stream, "{message}");
            let _ = writeln!(stream, "from {previous}");
            let _ = writeln!(stream, "M 100644 inline unrelated.txt");
            let _ = writeln!(stream, "data {}", contents.len());
            let _ = writeln!(stream, "{contents}");
            previous = format!(":{mark}");
        }
        let _ = writeln!(stream, "done");
        fixture.git_input(&["fast-import", "--quiet"], &stream);
        // fast-import moves refs only; later fixture commits stage the
        // worktree, so it has to match the imported HEAD again.
        fixture.git(&["reset", "-q", "--hard", "HEAD"]);
    }

    #[test]
    fn file_history_stops_at_the_shallow_boundary() {
        let (_source, clone) = shallow_clone_fixture(&["--depth", "2"]);
        let repo = clone.open();
        let head = clone.head();
        let boundary = clone.rev("HEAD^1");

        let mut entries = repo
            .path_history_entries("dir/old.txt")
            .expect("path history");
        let mut commits = Vec::new();
        let mut parents_buf = Vec::new();
        for entry in entries.by_ref() {
            let entry = entry.expect("history row");
            commits.push(entry.commit);
        }
        assert_eq!(
            commits,
            [head, boundary],
            "the walk stops at the boundary instead of failing on its parents"
        );

        let info = entries
            .read_commit_info(boundary, &mut parents_buf)
            .expect("boundary info");
        assert!(info.parents.is_empty(), "a boundary row is shown as a root");

        // Git treats a boundary as a root, so it adds every path its tree has.
        // The walk must stop there rather than read the missing parents.
        let history: Vec<_> = repo
            .path_history("keep.txt")
            .expect("path history")
            .map(|commit| commit.expect("history row"))
            .collect();
        assert_eq!(history, [boundary]);
    }

    #[test]
    fn optimized_walk_matches_reference_across_merges_renames_deletions_and_readditions() {
        let fixture = TestRepo::new();
        let body = "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\n";
        fixture.write("dir/a.txt", body);
        fixture.write("other/keep.txt", "keep\n");
        fixture.commit("initial");

        // Branch with edits + rename with modification.
        fixture.git(&["checkout", "-q", "-b", "feature"]);
        fixture.write("dir/a.txt", &format!("{body}iota\n"));
        fixture.commit("edit on feature");
        std::fs::create_dir_all(fixture.path().join("sub")).expect("create sub dir");
        fixture.git(&["mv", "dir/a.txt", "sub/b.txt"]);
        fixture.write("sub/b.txt", &format!("{body}iota\nkappa\n"));
        fixture.commit("rename with edit");

        // Main branch adds unrelated commits. The total stays above
        // INLINE_WARMUP (256), so the commits past the warmup still go through
        // the batching path; 150 here and 140 after the delete keep the
        // fixture near 300 commits, half the previous 620, without dropping a
        // scenario.
        fixture.git(&["checkout", "-q", "main"]);
        append_unrelated_commits(&fixture, "unrelated", "tick", 150);

        // Merge feature into main.
        fixture.git(&[
            "merge",
            "--no-ff",
            "--quiet",
            "feature",
            "-m",
            "merge feature",
        ]);

        // Delete sub/b.txt, do more commits, then re-add with completely new content,
        // then rename directory.
        fixture.git(&["rm", "--quiet", "sub/b.txt"]);
        fixture.commit("delete sub/b.txt");
        append_unrelated_commits(&fixture, "post-delete", "post-delete", 140);
        fixture.write("sub/b.txt", &format!("{body}readded\nlambda\nmu\n"));
        fixture.commit("re-add sub/b.txt");
        std::fs::create_dir_all(fixture.path().join("final")).expect("create final dir");
        fixture.git(&["mv", "sub/b.txt", "final/c.txt"]);
        fixture.commit("move to final/c.txt");

        // Also create a renamed directory with exact files.
        fixture.write("old_dir/f1.txt", "one\ntwo\n");
        fixture.write("old_dir/f2.txt", "three\nfour\n");
        fixture.commit("add old_dir");
        fixture.git(&["mv", "old_dir", "new_dir"]);
        fixture.commit("rename directory");

        for use_changed_paths_graph in [false, true] {
            if use_changed_paths_graph {
                fixture.git(&["commit-graph", "write", "--reachable", "--changed-paths"]);
            }
            let repo = fixture.open();
            // Three paths, one per scenario: the rename chain across the merge,
            // the deletion and the re-addition (`final/c.txt`), a renamed
            // directory pathspec (`new_dir`), and the repository root (`""`),
            // which reports every commit and is the only test covering that
            // branch. Longer per-file histories add rows but no new branch, and
            // an absent path is covered by `path_history_for_an_unknown_path_is_empty`.
            for query_path in ["final/c.txt", "new_dir", ""] {
                let actual: Vec<PathHistoryEntry> = repo
                    .path_history_entries(query_path)
                    .expect("optimized walk")
                    .map(|r| r.expect("row"))
                    .collect();
                let expected = reference_path_history(&repo, query_path);
                assert_eq!(
                    actual, expected,
                    "mismatch for path={query_path:?} (bloom={use_changed_paths_graph})"
                );
            }
        }
    }

    #[test]
    fn renames_at_varied_similarity_levels_binaries_and_huge_files_match_reference() {
        fn numbered(prefix: &str, count: usize, start: usize) -> String {
            use std::fmt::Write as _;
            let mut s = String::new();
            for i in start..(start + count) {
                let _ = writeln!(
                    s,
                    "{prefix} line {i:04} payload content for history rename test"
                );
            }
            s
        }

        fn write_bytes(repo: &TestRepo, rel: &str, bytes: &[u8]) {
            let p = repo.path().join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).expect("create parent");
            }
            std::fs::write(p, bytes).expect("write bytes");
        }

        let fixture = TestRepo::new();
        // Multiple files that will undergo renames at ~90%, ~60%, ~50% boundary, ~30% (below threshold).
        fixture.write("src/high_0.txt", &numbered("high", 20, 0));
        fixture.write("src/mid_0.txt", &numbered("mid", 20, 0));
        fixture.write("src/edge_0.txt", &numbered("edge", 20, 0));
        fixture.write("src/low_0.txt", &numbered("low", 20, 0));
        // Competing basename tie-breakers with similar content.
        fixture.write("竞争/alpha/dup.txt", &numbered("dup", 20, 0));
        fixture.write("other/dup_alt.txt", &numbered("dup", 20, 0));
        // Binary files: one exact rename, one edited binary.
        let bin_exact: Vec<u8> = (0..256)
            .map(|i| if i % 8 == 0 { 0 } else { i as u8 })
            .collect();
        let bin_edit_a: Vec<u8> = (0..256)
            .map(|i| if i % 6 == 0 { 0 } else { i as u8 })
            .collect();
        write_bytes(&fixture, "bin/exact_0.bin", &bin_exact);
        write_bytes(&fixture, "bin/edit_0.bin", &bin_edit_a);
        // Huge (> 4 MiB) text file renamed with a small edit.
        let mut huge = String::with_capacity(4 * 1024 * 1024 + 8192);
        let row = "huge_history_line_0123456789abcdef0123456789abcdef\n";
        let mut idx = 0usize;
        while huge.len() <= 4 * 1024 * 1024 + 4096 {
            huge.push_str(&format!("{idx:06}:{row}"));
            idx += 1;
        }
        fixture.write("big/huge_0.txt", &huge);
        fixture.commit("seed files");

        // Pre-rename edit for the ~90% tier, so its history has a version before
        // the rename. The huge file deliberately keeps one pre-rename version:
        // the >4 MiB guard skips the similarity pass either way, so a second
        // version would only cost another 4 MiB write and blob.
        fixture.write(
            "src/high_0.txt",
            &format!("{}prelude\n", numbered("high", 20, 0)),
        );
        fixture.commit("edit before rename");

        // Commit 1: rename all simultaneously in one commit to exercise Pass 2 destination ordering.
        for rel in [
            "src/high_0.txt",
            "src/mid_0.txt",
            "src/edge_0.txt",
            "src/low_0.txt",
            "竞争/alpha/dup.txt",
            "other/dup_alt.txt",
            "bin/exact_0.bin",
            "bin/edit_0.bin",
            "big/huge_0.txt",
        ] {
            std::fs::remove_file(fixture.path().join(rel)).expect("rm old");
        }
        fixture.write(
            "dst/high_1.txt",
            &format!("{}prelude\nchanged tail\n", numbered("high", 19, 0)),
        );
        fixture.write(
            "dst/mid_1.txt",
            &format!("{}{}", numbered("mid", 12, 0), numbered("newmid", 8, 0)),
        );
        fixture.write(
            "dst/edge_1.txt",
            &format!("{}{}", numbered("edge", 10, 0), numbered("enew", 10, 10)),
        );
        fixture.write(
            "dst/low_1.txt",
            &format!("{}{}", numbered("low", 6, 0), numbered("newlow", 14, 0)),
        );
        fixture.write("dst/dup.txt", &format!("{}extra\n", numbered("dup", 19, 0)));
        fixture.write(
            "dst/dup_other.txt",
            &format!("{}alt\n", numbered("dup", 19, 0)),
        );
        write_bytes(&fixture, "bin/exact_1.bin", &bin_exact);
        let bin_edit_b: Vec<u8> = (0..256)
            .map(|i| {
                if i % 6 == 0 {
                    0
                } else {
                    (i as u8).wrapping_mul(3)
                }
            })
            .collect();
        write_bytes(&fixture, "bin/edit_1.bin", &bin_edit_b);
        fixture.write("big/huge_1.txt", &format!("{huge}edited tail\n"));
        fixture.commit("simultaneous renames");

        let repo = fixture.open();
        // One query per scenario, each checked against the reference walk and
        // its expected follow/stop outcome from that same walk: the ~90% tier,
        // the 50% boundary that git still pairs, the ~30% tier that git stops
        // at, the duplicate basename, the exact binary rename, and the >4 MiB
        // file whose similarity pass is skipped by the size guard.
        let cases: &[(&str, Option<usize>)] = &[
            ("dst/high_1.txt", Some(3)),
            ("dst/edge_1.txt", Some(2)),
            ("dst/low_1.txt", Some(1)),
            ("dst/dup.txt", None),
            ("bin/exact_1.bin", Some(2)),
            ("big/huge_1.txt", Some(2)),
        ];
        for (query_path, expected_count) in cases {
            let actual: Vec<PathHistoryEntry> = repo
                .path_history_entries(query_path)
                .expect("path history")
                .map(|r| r.expect("row"))
                .collect();
            let expected = reference_path_history(&repo, query_path);
            assert_eq!(actual, expected, "history mismatch for {query_path}");
            if let Some(count) = expected_count {
                assert_eq!(actual.len(), *count, "history length for {query_path}");
            }
        }

        // The ~60% tier and the edited binary keep their follow/stop outcomes
        // without a second full-list walk: they resolve through the same code
        // path as the tiers above.
        assert_eq!(
            repo.path_history_entries("dst/mid_1.txt")
                .expect("ok")
                .count(),
            2
        );
        assert_eq!(
            repo.path_history_entries("bin/edit_1.bin")
                .expect("ok")
                .count(),
            1
        );
    }
}
