//! Per-commit line statistics.
//!
//! A commit is compared with its first parent, or with the empty tree for a
//! root commit. The result matches `git diff-tree -r --numstat --no-renames`:
//! a rename is a deletion plus an addition, and a binary file is flagged with
//! no line counts.
//!
//! Counting never materialises hunks or line text: only the edit script's
//! ranges are read. The first-parent tree walk is the same one the commit's
//! file list needs, so it is computed once per commit and shared: whichever
//! request arrives first runs it and the other reuses it. Rename detection is a
//! separate pass only the file list needs, so a line count never waits for it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};

use gix::bstr::{BString, ByteSlice};
use gix::diff::blob::{Diff, InternedInput};
use gix::object::tree::EntryMode;
use gix::object::tree::diff::ChangeDetached;
use gix::objs::{FindExt as _, TreeRefIter};

use super::Repo;
use super::log::Shallow;
use crate::{ChangeKind, CommitStat, Error, FileStat, Oid, Result};

/// A text diff above this size is skipped: counting the lines of a huge blob
/// costs more than the whole commit view is worth, so the file is reported the
/// way the UI shows "no line counts" (like a binary) instead.
pub(crate) const MAX_STAT_BYTES: u64 = 4 * 1024 * 1024;

/// How many commit diffs to keep. The UI looks at one commit at a time; a
/// selected commit and the one next to it are enough to make moving through the
/// log cheap without holding a large merge's file list forever.
const DIFF_CACHE_COMMITS: usize = 2;

/// Minimum number of changed files before spinning up scoped worker threads.
const MIN_PARALLEL_STAT_FILES: usize = 64;
const STAT_CHUNK_SIZE: usize = 32;
const MAX_STAT_WORKERS: usize = 8;
const BLOB_CACHE_SHARDS: usize = 16;

impl Repo {
    /// Added and removed lines of `commit` against its first parent, with the
    /// counts per file.
    ///
    /// Binary files are flagged and contribute nothing to the totals, as are
    /// files too large to count that `git` would have diffed.
    pub fn commit_stat(&self, commit: Oid) -> Result<CommitStat> {
        // The shared walk has no rewrite detection: a rename is still the
        // deletion and the addition that `--no-renames` counts, which is what a
        // stat needs. Rename detection is the file list's job.
        let diff = self.commit_diff(commit)?;
        let files: Vec<ChangedFile> = diff.changes.iter().filter_map(changed_file).collect();
        if files.is_empty() {
            return Ok(CommitStat::default());
        }

        let workers = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, MAX_STAT_WORKERS);

        let mut stat = CommitStat::default();
        stat.files.reserve(files.len());

        if workers <= 1 || files.len() < MIN_PARALLEL_STAT_FILES {
            let mut scratch = StatScratch::new(self.local(), Some(&diff.blobs));
            for file in &files {
                let entry = file_stat_with(&mut scratch, file)?;
                stat.added += entry.added;
                stat.removed += entry.removed;
                stat.files.push(entry);
            }
        } else {
            let chunks: Vec<&[ChangedFile]> = files.chunks(STAT_CHUNK_SIZE).collect();
            let results: Vec<OnceLock<Result<Vec<FileStat>>>> =
                (0..chunks.len()).map(|_| OnceLock::new()).collect();
            let next_chunk = AtomicUsize::new(0);
            let worker_count = workers.min(chunks.len());

            std::thread::scope(|scope| {
                for _ in 0..worker_count {
                    let chunks = &chunks;
                    let results = &results;
                    let next_chunk = &next_chunk;
                    let blobs = &diff.blobs;
                    scope.spawn(move || {
                        let mut scratch = StatScratch::new(self.local(), Some(blobs));
                        loop {
                            let idx = next_chunk.fetch_add(1, Ordering::Relaxed);
                            if idx >= chunks.len() {
                                break;
                            }
                            let chunk = chunks[idx];
                            let mut out = Vec::with_capacity(chunk.len());
                            let mut err = None;
                            for file in chunk {
                                match file_stat_with(&mut scratch, file) {
                                    Ok(entry) => out.push(entry),
                                    Err(e) => {
                                        err = Some(e);
                                        break;
                                    }
                                }
                            }
                            let value = match err {
                                Some(e) => Err(e),
                                None => Ok(out),
                            };
                            let _ = results[idx].set(value);
                        }
                    });
                }
            });

            for slot in results {
                let chunk_files = slot.into_inner().expect("all stat chunks completed")?;
                for entry in chunk_files {
                    stat.added += entry.added;
                    stat.removed += entry.removed;
                    stat.files.push(entry);
                }
            }
        }

        stat.files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(stat)
    }

    /// The first-parent tree walk of `commit`, without rewrite detection.
    ///
    /// `commit_stat` counts these entries and `commit_changes` pairs renames in
    /// them. The UI asks for both when a commit is selected, so the walk happens
    /// once: a second caller finds it cached, or waits for the walk already
    /// running instead of walking the same trees again. A stat never waits for
    /// rename detection, which is the file list's own, separate pass.
    pub(crate) fn commit_diff(&self, commit: Oid) -> Result<Arc<CommitDiff>> {
        enum Claim {
            Cached(Arc<CommitDiff>),
            Waiting(Arc<Pending>),
            Leader(Arc<Pending>),
        }

        let claim = {
            let mut cache = self.commit_diffs();
            if let Some(diff) = cache.get(commit) {
                Claim::Cached(diff)
            } else if let Some(pending) = cache.pending.get(&commit) {
                Claim::Waiting(Arc::clone(pending))
            } else {
                let pending = Arc::new(Pending::default());
                cache.pending.insert(commit, Arc::clone(&pending));
                Claim::Leader(pending)
            }
        };

        match claim {
            Claim::Cached(diff) => Ok(diff),
            Claim::Waiting(pending) => pending.wait(),
            Claim::Leader(pending) => {
                let mut leader = DiffLeader {
                    cache: Arc::clone(&self.commit_diffs),
                    commit,
                    pending,
                    finished: false,
                };
                let result = self
                    .diff_first_parent(commit)
                    .map(|changes| CommitDiff {
                        changes,
                        blobs: BlobCache::default(),
                    })
                    .map(Arc::new);
                leader.finish(&result);
                result
            }
        }
    }

    /// The changed files of `commit`, with renames and copies applied.
    ///
    /// Rename detection is the expensive half of the file list, so its result is
    /// cached with the walk. It runs over the walk's added and deleted entries,
    /// never over the trees again.
    pub(crate) fn commit_files(&self, commit: Oid) -> Result<Arc<Vec<ChangedFile>>> {
        if let Some(files) = self.commit_diffs().get_rewritten(commit) {
            return Ok(files);
        }

        let diff = self.commit_diff(commit)?;
        #[cfg(test)]
        self.commit_diffs().record_rewrite_pass();
        let repo = self.local();
        let files = Arc::new(super::renames::apply(
            &diff.changes,
            &repo,
            Some(&diff.blobs),
        )?);
        self.commit_diffs().cache_rewritten(commit, &files);
        Ok(files)
    }

    fn diff_first_parent(&self, commit: Oid) -> Result<Vec<ChangeDetached>> {
        #[cfg(test)]
        self.commit_diffs().record_tree_diff();

        let repo = self.local();
        // A shallow boundary is a root here even though its commit object still
        // names parents that this clone does not have.
        let boundary = Shallow::load(&repo)?.is_boundary(commit);
        let commit = repo.find_commit(commit).map_err(Error::gix)?;
        let tree = commit.tree().map_err(Error::gix)?;
        let parent_tree = match commit.parent_ids().next() {
            Some(parent) if !boundary => Some(
                repo.find_commit(parent.detach())
                    .map_err(Error::gix)?
                    .tree()
                    .map_err(Error::gix)?,
            ),
            // A root commit added everything, so the empty tree is the other side.
            _ => None,
        };
        let empty = repo.empty_tree();
        let parent_tree = parent_tree.as_ref().unwrap_or(&empty);

        tree_changes(&repo, parent_tree, &tree)
    }

    fn commit_diffs(&self) -> MutexGuard<'_, CommitDiffCache> {
        lock(&self.commit_diffs)
    }

    /// How many first-parent tree walks have run. Tests use it to prove that
    /// the stat and the file list share one.
    #[cfg(test)]
    pub(crate) fn tree_diff_count(&self) -> u64 {
        self.commit_diffs().tree_diffs
    }

    /// How many rename passes have run. Tests use it to prove that a stat never
    /// waits for one and that the file list runs it once.
    #[cfg(test)]
    pub(crate) fn rewrite_pass_count(&self) -> u64 {
        self.commit_diffs().rewrite_passes
    }

    /// How many commits the diff cache currently holds.
    #[cfg(test)]
    pub(crate) fn cached_commit_diffs(&self) -> usize {
        self.commit_diffs().entries.len()
    }
}

/// Sharded cache of decoded Added/Deleted blobs shared between `commit_stat`
/// and `commit_files` (rename detection) on a [`CommitDiff`].
#[derive(Default)]
pub(crate) struct BlobCache {
    shards: [Mutex<HashMap<Oid, CachedBlob>>; BLOB_CACHE_SHARDS],
}

#[derive(Clone)]
pub(crate) enum CachedBlob {
    /// Exceeded `MAX_STAT_BYTES`; `commit_stat` skips counting without inflating.
    TooLarge,
    Loaded(Arc<CachedBlobData>),
}

pub(crate) struct CachedBlobData {
    pub(crate) data: Arc<[u8]>,
    pub(crate) binary: bool,
    pub(crate) lines: OnceLock<u64>,
}

impl BlobCache {
    fn shard_index(oid: Oid) -> usize {
        usize::from(oid.as_bytes()[0]) & (BLOB_CACHE_SHARDS - 1)
    }

    pub(crate) fn get(&self, oid: Oid) -> Option<CachedBlob> {
        let map = lock(&self.shards[Self::shard_index(oid)]);
        map.get(&oid).cloned()
    }

    pub(crate) fn insert_loaded(
        &self,
        oid: Oid,
        data: Arc<[u8]>,
        binary: bool,
        lines: Option<u64>,
    ) {
        let line_cell = OnceLock::new();
        if let Some(count) = lines {
            let _ = line_cell.set(count);
        }
        let entry = CachedBlob::Loaded(Arc::new(CachedBlobData {
            data,
            binary,
            lines: line_cell,
        }));
        let mut map = lock(&self.shards[Self::shard_index(oid)]);
        map.entry(oid).or_insert(entry);
    }

    pub(crate) fn insert_too_large(&self, oid: Oid) {
        let mut map = lock(&self.shards[Self::shard_index(oid)]);
        map.entry(oid).or_insert(CachedBlob::TooLarge);
    }
}

/// One first-parent tree walk, without rewrite detection.
pub(crate) struct CommitDiff {
    /// The tree changes as the walk emitted them. The stat skips directory
    /// entries; rename detection pairs the added and deleted ones.
    changes: Vec<ChangeDetached>,
    /// Shared cache of Added/Deleted blobs so stat and rename detection do not
    /// inflate or NUL-scan the same blob twice.
    blobs: BlobCache,
}

/// One cached commit: the shared walk, and the file list built from it once
/// rename detection has run.
struct CacheEntry {
    commit: Oid,
    diff: Arc<CommitDiff>,
    /// `None` until the file list asks for it; a stat never fills it.
    rewritten: Option<Arc<Vec<ChangedFile>>>,
}

/// Recent commit diffs and the ones being computed.
#[derive(Default)]
pub(crate) struct CommitDiffCache {
    /// Least recently used first, at most one entry per commit.
    entries: Vec<CacheEntry>,
    /// Walks in flight, so a second request waits for the same walk.
    pending: HashMap<Oid, Arc<Pending>>,
    #[cfg(test)]
    tree_diffs: u64,
    #[cfg(test)]
    rewrite_passes: u64,
}

impl CommitDiffCache {
    fn get(&mut self, commit: Oid) -> Option<Arc<CommitDiff>> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.commit == commit)?;
        // A hit becomes the most recently used entry.
        let entry = self.entries.remove(index);
        let diff = Arc::clone(&entry.diff);
        self.entries.push(entry);
        Some(diff)
    }

    fn get_rewritten(&mut self, commit: Oid) -> Option<Arc<Vec<ChangedFile>>> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.commit == commit)?;
        let entry = self.entries.remove(index);
        let files = entry.rewritten.clone();
        self.entries.push(entry);
        files
    }

    fn cache_rewritten(&mut self, commit: Oid, files: &Arc<Vec<ChangedFile>>) {
        // Only an entry that is still cached may be upgraded; a commit evicted
        // while its rename pass ran must not resurrect itself.
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.commit == commit) {
            entry.rewritten = Some(Arc::clone(files));
        }
    }

    fn insert(&mut self, commit: Oid, diff: &Arc<CommitDiff>) {
        self.entries.retain(|entry| entry.commit != commit);
        self.entries.push(CacheEntry {
            commit,
            diff: Arc::clone(diff),
            rewritten: None,
        });
        if self.entries.len() > DIFF_CACHE_COMMITS {
            self.entries.remove(0);
        }
    }

    #[cfg(test)]
    fn record_tree_diff(&mut self) {
        self.tree_diffs += 1;
    }

    #[cfg(test)]
    fn record_rewrite_pass(&mut self) {
        self.rewrite_passes += 1;
    }
}

/// A diff one thread is computing. Others wait here rather than walking the
/// same trees: on a large merge the difference is hundreds of milliseconds.
#[derive(Default)]
struct Pending {
    outcome: Mutex<Option<Result<Arc<CommitDiff>, String>>>,
    ready: Condvar,
}

impl Pending {
    fn wait(&self) -> Result<Arc<CommitDiff>> {
        let mut outcome = lock(&self.outcome);
        while outcome.is_none() {
            outcome = self
                .ready
                .wait(outcome)
                .unwrap_or_else(PoisonError::into_inner);
        }
        match outcome.as_ref().expect("filled in by the leader") {
            Ok(diff) => Ok(Arc::clone(diff)),
            Err(message) => Err(Error::other(message.clone())),
        }
    }

    fn finish(&self, result: &Result<Arc<CommitDiff>>) {
        let mut outcome = lock(&self.outcome);
        *outcome = Some(match result {
            Ok(diff) => Ok(Arc::clone(diff)),
            Err(error) => Err(error.to_string()),
        });
        drop(outcome);
        self.ready.notify_all();
    }
}

/// Publishes a leader's result to its waiters and the cache.
///
/// The drop path covers a panic in the walk: waiters get an error instead of
/// blocking forever on a thread that will never finish.
struct DiffLeader {
    cache: Arc<Mutex<CommitDiffCache>>,
    commit: Oid,
    pending: Arc<Pending>,
    finished: bool,
}

impl DiffLeader {
    fn finish(&mut self, result: &Result<Arc<CommitDiff>>) {
        let mut cache = lock(&self.cache);
        cache.pending.remove(&self.commit);
        if let Ok(diff) = result {
            cache.insert(self.commit, diff);
        }
        self.pending.finish(result);
        self.finished = true;
    }
}

impl Drop for DiffLeader {
    fn drop(&mut self) {
        if !self.finished {
            let mut cache = lock(&self.cache);
            cache.pending.remove(&self.commit);
            self.pending
                .finish(&Err(Error::other("the first-parent tree diff panicked")));
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
/// The changes between two trees, without rewrite detection.
///
/// The walk needs no blobs, attributes or rename configuration, so it runs
/// without the resource cache a rewrite-aware diff prepares. A first-parent
/// commit diff and a comparison of two arbitrary commits both start here.
pub(crate) fn tree_changes(
    repo: &gix::Repository,
    old: &gix::Tree<'_>,
    new: &gix::Tree<'_>,
) -> Result<Vec<ChangeDetached>> {
    let mut recorder = gix::diff::tree::Recorder::default();
    let mut state = gix::diff::tree::State::default();
    gix::diff::tree(
        TreeRefIter::from_bytes(&old.data, old.id.kind()),
        TreeRefIter::from_bytes(&new.data, new.id.kind()),
        &mut state,
        &repo.objects,
        &mut recorder,
    )
    .map_err(Error::gix)?;

    Ok(recorder.records.into_iter().map(detached_change).collect())
}

/// One file of a first-parent tree diff, before its lines are counted.
///
/// Shared with `commit_changes`, which reports the same entries as git's
/// `--name-status` does.
#[derive(Clone)]
pub(crate) struct ChangedFile {
    pub(crate) path: String,
    pub(crate) old_path: Option<String>,
    pub(crate) old_id: Option<Oid>,
    pub(crate) new_id: Option<Oid>,
    pub(crate) old_mode: Option<EntryMode>,
    pub(crate) new_mode: Option<EntryMode>,
    pub(crate) kind: ChangeKind,
}

impl ChangedFile {
    /// A submodule has no blob content to count lines in.
    fn uncountable(&self) -> bool {
        !countable(self.old_mode) || !countable(self.new_mode)
    }
}

/// A change from the walk, with the location the recorder tracked for it.
fn detached_change(change: gix::diff::tree::recorder::Change) -> ChangeDetached {
    use gix::diff::tree::recorder::Change;

    match change {
        Change::Addition {
            entry_mode,
            oid,
            path,
            relation,
        } => ChangeDetached::Addition {
            location: path,
            relation,
            entry_mode,
            id: oid,
        },
        Change::Deletion {
            entry_mode,
            oid,
            path,
            relation,
        } => ChangeDetached::Deletion {
            location: path,
            relation,
            entry_mode,
            id: oid,
        },
        Change::Modification {
            previous_entry_mode,
            previous_oid,
            entry_mode,
            oid,
            path,
        } => ChangeDetached::Modification {
            location: path,
            previous_entry_mode,
            previous_id: previous_oid,
            entry_mode,
            id: oid,
        },
    }
}

/// The parsed tree change of one entry; `None` for a directory entry, whose
/// children carry the changes.
pub(crate) fn changed_file(
    change: &gix::object::tree::diff::ChangeDetached,
) -> Option<ChangedFile> {
    use gix::object::tree::diff::ChangeDetached;

    let (path, old_path, old_id, new_id, old_mode, new_mode, kind) = match change {
        ChangeDetached::Addition {
            location,
            entry_mode,
            id,
            ..
        } => {
            // Directories are reported through their children.
            if entry_mode.is_tree() {
                return None;
            }
            (
                location,
                None,
                None,
                Some(*id),
                None,
                Some(*entry_mode),
                ChangeKind::Added,
            )
        }
        ChangeDetached::Deletion {
            location,
            entry_mode,
            id,
            ..
        } => {
            if entry_mode.is_tree() {
                return None;
            }
            (
                location,
                None,
                Some(*id),
                None,
                Some(*entry_mode),
                None,
                ChangeKind::Deleted,
            )
        }
        ChangeDetached::Modification {
            location,
            previous_entry_mode,
            previous_id,
            entry_mode,
            id,
        } => {
            if entry_mode.is_tree() && previous_entry_mode.is_tree() {
                // A changed directory is reported through its children.
                return None;
            } else if entry_mode.is_tree() {
                // A file replaced by a directory is a deletion of the file.
                (
                    location,
                    None,
                    Some(*previous_id),
                    None,
                    Some(*previous_entry_mode),
                    None,
                    ChangeKind::Deleted,
                )
            } else if previous_entry_mode.is_tree() {
                // A directory replaced by a file is an addition of the file.
                (
                    location,
                    None,
                    None,
                    Some(*id),
                    None,
                    Some(*entry_mode),
                    ChangeKind::Added,
                )
            } else {
                let kind = if kind_changed(*previous_entry_mode, *entry_mode) {
                    ChangeKind::TypeChanged
                } else {
                    ChangeKind::Modified
                };
                (
                    location,
                    None,
                    Some(*previous_id),
                    Some(*id),
                    Some(*previous_entry_mode),
                    Some(*entry_mode),
                    kind,
                )
            }
        }
        ChangeDetached::Rewrite {
            location,
            source_location,
            source_entry_mode,
            source_id,
            entry_mode,
            id,
            copy,
            ..
        } => {
            if entry_mode.is_tree() {
                return None;
            }
            (
                location,
                Some(source_location),
                Some(*source_id),
                Some(*id),
                Some(*source_entry_mode),
                Some(*entry_mode),
                if *copy {
                    ChangeKind::Copied
                } else {
                    ChangeKind::Renamed
                },
            )
        }
    };

    Some(ChangedFile {
        path: to_path(path),
        old_path: old_path.map(to_path),
        old_id,
        new_id,
        old_mode,
        new_mode,
        kind,
    })
}

pub(crate) struct StatScratch<'a> {
    repo: gix::Repository,
    empty_blob: Oid,
    blobs: Option<&'a BlobCache>,
    old_buf: Vec<u8>,
    new_buf: Vec<u8>,
    diff: Diff,
}

impl<'a> StatScratch<'a> {
    pub(crate) fn new(repo: gix::Repository, blobs: Option<&'a BlobCache>) -> Self {
        let empty_blob = Oid::empty_blob(repo.object_hash());
        Self {
            repo,
            empty_blob,
            blobs,
            old_buf: Vec::new(),
            new_buf: Vec::new(),
            diff: Diff::default(),
        }
    }
}

enum LoadedSide<'b> {
    Skip,
    Cached(Arc<CachedBlobData>),
    Borrowed(&'b [u8]),
}

impl<'b> LoadedSide<'b> {
    fn as_slice(&self) -> &[u8] {
        match self {
            LoadedSide::Skip => &[],
            LoadedSide::Cached(data) => &data.data,
            LoadedSide::Borrowed(slice) => slice,
        }
    }
}

pub(crate) fn file_stat_with(
    scratch: &mut StatScratch<'_>,
    change: &ChangedFile,
) -> Result<FileStat> {
    if change.uncountable() {
        return Ok(binary_stat(change));
    }

    // A missing side or empty blob needs no two-way diff: count lines on the
    // present side directly. Identical blobs only changed mode, which has no
    // line changes.
    let (added, removed) = match (change.old_id, change.new_id) {
        (None, None) => return Ok(binary_stat(change)),
        (old, new) if old == new => (0, 0),
        (None, Some(new_id)) => match single_side_lines(scratch, new_id, true)? {
            Some(lines) => (lines, 0),
            None => return Ok(binary_stat(change)),
        },
        (Some(old_id), None) => match single_side_lines(scratch, old_id, true)? {
            Some(lines) => (0, lines),
            None => return Ok(binary_stat(change)),
        },
        (Some(old_id), Some(new_id)) if old_id == scratch.empty_blob => {
            match single_side_lines(scratch, new_id, false)? {
                Some(lines) => (lines, 0),
                None => return Ok(binary_stat(change)),
            }
        }
        (Some(old_id), Some(new_id)) if new_id == scratch.empty_blob => {
            match single_side_lines(scratch, old_id, false)? {
                Some(lines) => (0, lines),
                None => return Ok(binary_stat(change)),
            }
        }
        (Some(old_id), Some(new_id)) => {
            // Inspect `old` completely before inflating `new` so binary or
            // oversized left-hand files bail out early.
            let old = load_blob_side(
                &scratch.repo,
                scratch.empty_blob,
                scratch.blobs,
                old_id,
                false,
                &mut scratch.old_buf,
            )?;
            if matches!(old, LoadedSide::Skip) {
                return Ok(binary_stat(change));
            }
            let new = load_blob_side(
                &scratch.repo,
                scratch.empty_blob,
                scratch.blobs,
                new_id,
                false,
                &mut scratch.new_buf,
            )?;
            if matches!(new, LoadedSide::Skip) {
                return Ok(binary_stat(change));
            }
            count_changed_lines_with(old.as_slice(), new.as_slice(), &mut scratch.diff)
        }
    };

    Ok(FileStat {
        path: change.path.clone(),
        old_path: change.old_path.clone(),
        added,
        removed,
        binary: false,
    })
}

fn single_side_lines(
    scratch: &mut StatScratch<'_>,
    id: Oid,
    cache_for_renames: bool,
) -> Result<Option<u64>> {
    if id == scratch.empty_blob {
        return Ok(Some(0));
    }
    let loaded = load_blob_side(
        &scratch.repo,
        scratch.empty_blob,
        scratch.blobs,
        id,
        cache_for_renames,
        &mut scratch.new_buf,
    )?;
    match loaded {
        LoadedSide::Skip => Ok(None),
        LoadedSide::Cached(entry) => {
            Ok(Some(*entry.lines.get_or_init(|| count_lines(&entry.data))))
        }
        LoadedSide::Borrowed(slice) => Ok(Some(count_lines(slice))),
    }
}

fn load_blob_side<'b>(
    repo: &gix::Repository,
    empty_blob: Oid,
    blobs: Option<&BlobCache>,
    id: Oid,
    cache_insert: bool,
    buf: &'b mut Vec<u8>,
) -> Result<LoadedSide<'b>> {
    if id == empty_blob {
        return Ok(LoadedSide::Borrowed(&[]));
    }
    if let Some(cache) = blobs
        && let Some(cached) = cache.get(id)
    {
        return Ok(match cached {
            CachedBlob::TooLarge => LoadedSide::Skip,
            CachedBlob::Loaded(entry) if entry.binary => LoadedSide::Skip,
            CachedBlob::Loaded(entry) => LoadedSide::Cached(entry),
        });
    }
    if is_large_id(repo, id) {
        if cache_insert && let Some(cache) = blobs {
            cache.insert_too_large(id);
        }
        return Ok(LoadedSide::Skip);
    }
    let blob = repo.objects.find_blob(&id, buf).map_err(Error::gix)?;
    let binary = is_binary(blob.data);
    if cache_insert && let Some(cache) = blobs {
        let lines = (!binary).then(|| count_lines(blob.data));
        let data: Arc<[u8]> = Arc::from(blob.data);
        cache.insert_loaded(id, data, binary, lines);
    }
    if binary {
        return Ok(LoadedSide::Skip);
    }
    Ok(LoadedSide::Borrowed(blob.data))
}

/// A binary or uncountable file: flagged, with no line counts.
fn binary_stat(change: &ChangedFile) -> FileStat {
    FileStat {
        path: change.path.clone(),
        old_path: change.old_path.clone(),
        added: 0,
        removed: 0,
        binary: true,
    }
}

/// The lines a blob adds or removes as a whole file.
pub(crate) fn count_lines(data: &[u8]) -> u64 {
    let lines = data.find_iter(b"\n").count();
    let lines = if data.is_empty() || data.ends_with(b"\n") {
        lines
    } else {
        // A final line without a terminator still counts.
        lines + 1
    };
    lines as u64
}

/// Strip leading and trailing byte-identical line tokens before invoking
/// `imara_diff`. Because `Diff::compute` strips the same common prefix and
/// suffix tokens before running histogram diffing, trimming at exact line
/// boundaries produces identical edit-script counts while skipping tokenization
/// and hashing of unchanged lines.
pub(crate) fn strip_common_lines<'a>(old: &'a [u8], new: &'a [u8]) -> (&'a [u8], &'a [u8]) {
    let common_prefix = common_prefix_len(old, new);
    let prefix = if common_prefix == old.len() && common_prefix == new.len() {
        common_prefix
    } else {
        old[..common_prefix]
            .rfind_byte(b'\n')
            .map_or(0, |idx| idx + 1)
    };

    let old_mid = &old[prefix..];
    let new_mid = &new[prefix..];
    let common_suffix = common_suffix_len(old_mid, new_mid);
    let suffix = if common_suffix == 0 {
        0
    } else {
        let old_start = old_mid.len() - common_suffix;
        let new_start = new_mid.len() - common_suffix;
        let old_aligned = old_start == 0 || old_mid[old_start - 1] == b'\n';
        let new_aligned = new_start == 0 || new_mid[new_start - 1] == b'\n';
        if old_aligned && new_aligned {
            common_suffix
        } else {
            let s = &old_mid[old_start..];
            match s.find_byte(b'\n') {
                Some(nl) => common_suffix - (nl + 1),
                None => 0,
            }
        }
    };

    (
        &old_mid[..old_mid.len() - suffix],
        &new_mid[..new_mid.len() - suffix],
    )
}

fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let len = a.len().min(b.len());
    let mut i = 0;
    while i + 8 <= len {
        if a[i..i + 8] != b[i..i + 8] {
            break;
        }
        i += 8;
    }
    while i < len && a[i] == b[i] {
        i += 1;
    }
    i
}

fn common_suffix_len(a: &[u8], b: &[u8]) -> usize {
    let len = a.len().min(b.len());
    let mut s = 0;
    while s + 8 <= len {
        let ai = a.len() - s - 8;
        let bi = b.len() - s - 8;
        if a[ai..ai + 8] != b[bi..bi + 8] {
            break;
        }
        s += 8;
    }
    while s < len && a[a.len() - 1 - s] == b[b.len() - 1 - s] {
        s += 1;
    }
    s
}

#[inline]
pub(crate) fn is_single_line(data: &[u8]) -> bool {
    !data.is_empty() && data[..data.len() - 1].find_byte(b'\n').is_none()
}

/// Added and removed lines of two text blobs.
///
/// Only the edit script's ranges are read: building hunks, line text and
/// emphasis would mean allocating a copy of every changed line, which is what
/// made counting a large merge as slow as rendering it.
fn count_changed_lines_with(old: &[u8], new: &[u8], diff: &mut Diff) -> (u64, u64) {
    let (old_mid, new_mid) = strip_common_lines(old, new);
    if old_mid.is_empty() {
        return (count_lines(new_mid), 0);
    }
    if new_mid.is_empty() {
        return (0, count_lines(old_mid));
    }
    if is_single_line(old_mid) && is_single_line(new_mid) {
        return (1, 1);
    }

    let input = InternedInput::new(old_mid, new_mid);
    diff.compute_with(
        gix::diff::blob::Algorithm::Histogram,
        &input.before,
        &input.after,
        input.interner.num_tokens(),
    );
    (diff.count_additions() as u64, diff.count_removals() as u64)
}

#[cfg(test)]
fn count_changed_lines(old: &[u8], new: &[u8]) -> (u64, u64) {
    let mut diff = Diff::default();
    count_changed_lines_with(old, new, &mut diff)
}

/// Whether a blob looks binary: a NUL byte near its start, as git decides.
///
/// Only the head is inspected, which keeps the check cheap for a large file.
pub(crate) fn is_binary(data: &[u8]) -> bool {
    const PROBE: usize = 8_000;
    data[..data.len().min(PROBE)].contains(&0)
}

/// `None` is the absent side of an addition or deletion: empty content, which
/// is countable. A submodule entry is not a blob.
fn countable(mode: Option<EntryMode>) -> bool {
    match mode {
        None => true,
        Some(mode) => mode.is_blob_or_symlink(),
    }
}

/// Whether a blob is too large to count, read from its header so the content is
/// never inflated for it.
fn is_large_id(repo: &gix::Repository, id: Oid) -> bool {
    match repo.try_find_header(id) {
        Ok(Some(header)) => header.size() > MAX_STAT_BYTES,
        _ => false,
    }
}

fn kind_changed(before: EntryMode, after: EntryMode) -> bool {
    fn family(mode: EntryMode) -> u8 {
        match mode.kind() {
            gix::object::tree::EntryKind::Blob | gix::object::tree::EntryKind::BlobExecutable => 0,
            gix::object::tree::EntryKind::Link => 1,
            gix::object::tree::EntryKind::Commit => 2,
            gix::object::tree::EntryKind::Tree => 3,
        }
    }
    family(before) != family(after)
}

fn to_path(path: &BString) -> String {
    path.to_str_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;
    use crate::FileChange;
    use crate::diff::{DiffOptions, LineKind, diff_text};
    use crate::test_support::{TestRepo, shallow_clone_fixture};

    /// The counts of the full text diff, which used to back the stat.
    fn text_counts(old: &[u8], new: &[u8]) -> (u64, u64) {
        let diff = diff_text(old, new, &DiffOptions::default());
        let mut added = 0;
        let mut removed = 0;
        for line in diff.hunks.iter().flat_map(|hunk| &hunk.lines) {
            match line.kind {
                LineKind::Added => added += 1,
                LineKind::Removed => removed += 1,
                LineKind::Context => {}
            }
        }
        (added, removed)
    }

    #[test]
    fn counting_matches_the_full_text_diff() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"", b""),
            (b"", b"one\n"),
            (b"one\n", b""),
            (b"one\ntwo\n", b"one\nthree\n"),
            // Single-line and incomplete final newline edge cases around prefix/suffix trimming.
            (b"one", b"two"),
            (b"abc", b"abc\ndef"),
            (b"abc\ndef", b"abc"),
            (b"x\nabc", b"x\nabc\n"),
            (b"ax\ny\n", b"bx\ny\n"),
            (b"a1\nend", b"b1\nend"),
            (b"prefix\nfoo\nbar\nsuffix\n", b"prefix\nsuffix\n"),
            (b"prefix\nsuffix\n", b"prefix\nfoo\nbar\nsuffix\n"),
            // A move that the slider heuristic can place differently.
            (b"a\nb\na\nb\n", b"b\na\nb\na\n"),
            // A dropped terminator and a CRLF switch are changes.
            (b"a\nb", b"a\nb\n"),
            (b"a\r\nb\r\n", b"a\nb\n"),
            (b"x\ny\nx\ny\n", b"x\nx\ny\n"),
            (b"one\ntwo\n", b"one\ntwo\nthree\n"),
        ];
        for (old, new) in cases {
            assert_eq!(
                count_changed_lines(old, new),
                text_counts(old, new),
                "old={old:?} new={new:?}"
            );
        }
    }

    #[test]
    fn counting_a_whole_file_keeps_a_final_line_without_terminator() {
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"\n"), 1);
        assert_eq!(count_lines(b"one\n"), 1);
        assert_eq!(count_lines(b"one\ntwo"), 2);
        assert_eq!(count_lines(b"one\n\n"), 2);
    }

    /// A root commit and one commit that renames and edits a file: the file
    /// list detects the rename while the stat still counts both sides of it.
    fn rename_repo() -> (TestRepo, Oid) {
        let repo = TestRepo::new();
        repo.write("old.txt", "one\ntwo\nthree\n");
        repo.commit("root");
        repo.write("new.txt", "one\ntwo\nthree\nfour\n");
        std::fs::remove_file(repo.path().join("old.txt")).expect("remove old file");
        let commit = repo.commit("rename and edit");
        (repo, commit)
    }

    fn assert_rename_sides(stat: &CommitStat, changes: &[FileChange]) {
        assert!(
            changes
                .iter()
                .any(|change| change.kind == ChangeKind::Renamed),
            "the file list should detect the rename: {changes:?}"
        );
        let paths: Vec<&str> = stat.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            paths,
            ["new.txt", "old.txt"],
            "the stat counts the rename as a deletion and an addition"
        );
    }

    #[test]
    fn a_stat_then_the_file_list_walks_the_tree_once() {
        let (repo, commit) = rename_repo();
        let handle = repo.open();

        let stat = handle.commit_stat(commit).expect("commit stat");
        assert_eq!(handle.tree_diff_count(), 1);
        assert_eq!(
            handle.rewrite_pass_count(),
            0,
            "a line count must not pay for rename detection"
        );
        let changes = handle.commit_changes(commit).expect("commit changes");
        assert_eq!(
            handle.tree_diff_count(),
            1,
            "the file list must reuse the stat's tree walk"
        );
        assert_eq!(handle.rewrite_pass_count(), 1);
        assert_rename_sides(&stat, &changes);

        // The cached file list is not rebuilt on the next request.
        assert_eq!(
            handle.commit_changes(commit).expect("commit changes"),
            changes
        );
        assert_eq!(handle.rewrite_pass_count(), 1);

        let fresh = repo.open();
        assert_eq!(fresh.commit_stat(commit).expect("commit stat"), stat);
        assert_eq!(
            fresh.commit_changes(commit).expect("commit changes"),
            changes
        );
    }

    #[test]
    fn a_file_list_then_the_stat_walks_the_tree_once() {
        let (repo, commit) = rename_repo();
        let handle = repo.open();

        let changes = handle.commit_changes(commit).expect("commit changes");
        assert_eq!(handle.tree_diff_count(), 1);
        assert_eq!(handle.rewrite_pass_count(), 1);
        let stat = handle.commit_stat(commit).expect("commit stat");
        assert_eq!(
            handle.tree_diff_count(),
            1,
            "the stat must reuse the file list's tree walk"
        );
        assert_eq!(handle.rewrite_pass_count(), 1);
        assert_rename_sides(&stat, &changes);

        let fresh = repo.open();
        assert_eq!(
            fresh.commit_changes(commit).expect("commit changes"),
            changes
        );
        assert_eq!(fresh.commit_stat(commit).expect("commit stat"), stat);
    }

    #[test]
    fn concurrent_requests_wait_for_one_tree_diff() {
        let (repo, commit) = rename_repo();
        let handle = repo.open();
        let barrier = Arc::new(Barrier::new(2));

        let stat_thread = {
            let repo = handle.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                repo.commit_stat(commit).expect("commit stat")
            })
        };
        let changes_thread = {
            let repo = handle.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                repo.commit_changes(commit).expect("commit changes")
            })
        };
        let stat = stat_thread.join().expect("stat thread");
        let changes = changes_thread.join().expect("changes thread");

        assert_eq!(
            handle.tree_diff_count(),
            1,
            "a second request must wait for the walk already running"
        );
        assert_eq!(handle.rewrite_pass_count(), 1);
        assert_rename_sides(&stat, &changes);
    }

    #[test]
    fn a_shallow_boundary_is_diffed_against_the_empty_tree() {
        let (_source, clone) = shallow_clone_fixture(&["--depth", "2"]);
        let handle = clone.open();
        let boundary = clone.rev("HEAD^1");

        let stat = handle.commit_stat(boundary).expect("boundary stat");
        let paths: Vec<&str> = stat.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            paths,
            ["base.txt", "dir/old.txt", "keep.txt", "topic.txt"],
            "the missing parent must not hide the whole commit"
        );
        assert_eq!(
            stat.added, 7,
            "every line of the boundary tree is an addition"
        );
        assert_eq!(stat.removed, 0);

        let changes = handle.commit_changes(boundary).expect("boundary changes");
        assert!(
            changes
                .iter()
                .all(|change| change.kind == ChangeKind::Added),
            "a boundary is a root, so its file list adds everything: {changes:?}"
        );
        assert_eq!(changes.len(), stat.files.len());
    }

    #[test]
    fn a_renamed_directory_lists_every_file() {
        let repo = TestRepo::new();
        repo.write("dir/a.txt", "one\ntwo\nthree\n");
        repo.write("dir/b.txt", "unchanged\n");
        repo.commit("root");

        repo.git(&["mv", "dir", "moved"]);
        repo.write("moved/a.txt", "one\ntwo\nthree\nfour\n");
        let commit = repo.commit("move and edit");

        let changes = repo.open().commit_changes(commit).expect("commit changes");
        let renamed: Vec<(&str, &str)> = changes
            .iter()
            .filter(|change| change.kind == ChangeKind::Renamed)
            .map(|change| {
                (
                    change.path.as_str(),
                    change.old_path.as_deref().expect("old path"),
                )
            })
            .collect();
        assert_eq!(
            renamed,
            [("moved/a.txt", "dir/a.txt"), ("moved/b.txt", "dir/b.txt")]
        );
    }

    #[test]
    fn identical_files_are_paired_by_name() {
        let repo = TestRepo::new();
        repo.write("dir/a.txt", "same\n");
        repo.write("dir/b.txt", "same\n");
        repo.commit("root");

        repo.git(&["mv", "dir", "moved"]);
        let commit = repo.commit("move");

        let changes = repo.open().commit_changes(commit).expect("commit changes");
        let renamed: Vec<(&str, &str)> = changes
            .iter()
            .filter(|change| change.kind == ChangeKind::Renamed)
            .map(|change| {
                (
                    change.path.as_str(),
                    change.old_path.as_deref().expect("old path"),
                )
            })
            .collect();
        assert_eq!(
            renamed,
            [("moved/a.txt", "dir/a.txt"), ("moved/b.txt", "dir/b.txt")],
            "identical contents must pair by file name"
        );
    }

    #[test]
    fn the_diff_cache_keeps_only_the_last_commits() {
        let repo = TestRepo::new();
        let mut commits = Vec::new();
        for index in 0..3 {
            repo.write("file.txt", &format!("line {index}\n"));
            commits.push(repo.commit(&format!("commit {index}")));
        }

        let handle = repo.open();
        for commit in &commits {
            handle.commit_changes(*commit).expect("commit changes");
        }
        assert_eq!(handle.rewrite_pass_count(), 3);
        assert!(
            handle.cached_commit_diffs() <= 2,
            "the cache must stay bounded"
        );

        // The oldest commit was evicted and has to be computed again; the newest
        // one is still cached.
        handle.commit_changes(commits[0]).expect("commit changes");
        assert_eq!(handle.rewrite_pass_count(), 4);
        handle.commit_changes(commits[2]).expect("commit changes");
        assert_eq!(handle.rewrite_pass_count(), 4);
    }
}
