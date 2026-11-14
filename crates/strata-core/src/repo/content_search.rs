//! History content search (`git log -S` and `git log -G`), with optional path
//! limiting.
//!
//! Walks history in `--date-order` using [`PathWalk`], skips merge commits
//! (matching git's default `-S`/`-G` behaviour without `-m`), and evaluates
//! first-parent tree diffs in parallel across worker threads while streaming
//! matches and scan progress in exact walk order.
//!
//! Optimizations:
//! - When a path limit is given, commit-graph changed-path Bloom filters skip
//!   untouched commits without reading trees, and tree lookups descend in
//!   lockstep with per-level entry caches (like [`super::history`]).
//! - Without a path limit (or inside a path-limited directory subtree), tree
//!   diffs inspect modified blobs inline and short-circuit on the first match.
//! - Binary blobs (NUL in the first 8,000 bytes) and blobs larger than
//!   `MAX_STAT_BYTES` are skipped without inflating oversized blobs.
//! - Per-worker OID result caches reuse `-S` occurrence counts and `-G`
//!   line-match presence across parent/child commits, avoiding redundant blob
//!   reads and line diffs (`-G` also avoids `imara-diff` whenever at most one
//!   side has a matching line).
//! - Rename detection runs lazily via [`super::renames::apply`] only when no
//!   modification matched and both unpaired additions and deletions remain
//!   with at least one candidate match.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use gix::bstr::{BStr, BString, ByteSlice as _, ByteVec as _};
use gix::diff::tree::visit::{Action, Change as VisitChange, Relation};
use gix::object::tree::diff::ChangeDetached;
use gix::object::tree::{EntryKind, EntryMode};
use gix::objs::{FindExt as _, TreeRefIter};
use imara_diff::{Algorithm, Diff, InternedInput};
use memchr::memmem::Finder;
use regex::bytes::{Regex, RegexBuilder};

use super::Repo;
use super::bloom::{BloomKeys, CommitGraphBloom};
use super::log::{CancelCheck, PathCommit, PathWalk};
use super::renames;
use super::stat::{MAX_STAT_BYTES, is_binary, is_single_line, strip_common_lines};
use crate::{ChangeKind, Error, Oid, Result};

const ROOT_CACHE_SLOTS: usize = 8_192;
const SUBTREE_CACHE_SLOTS: usize = 2_048;
const BLOB_RESULT_CACHE_SLOTS: usize = 4_096;

const OBJECT_CACHE_BYTES: usize = 8 * 1024 * 1024;
const WORKER_OBJECT_CACHE_BYTES: usize = 4 * 1024 * 1024;

const INLINE_WARMUP: usize = 256;
const INITIAL_BATCH_SIZE: usize = 4_096;
const MAX_BATCH_SIZE: usize = 8_192;
const WORKER_CHUNK_SIZE: usize = 512;
const MAX_WORKERS: usize = 8;

type TreeEntry = (Oid, u16);

/// Which content search semantics to apply to each commit's first-parent diff.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ContentSearchMode {
    /// `git log -S<string>`: commits that change the number of non-overlapping
    /// occurrences of the substring in a changed file.
    Substring(String),
    /// `git log -G<regex>`: commits whose diff adds or removes a line matching
    /// the regular expression.
    Regex(String),
}

/// Parameters for [`Repo::content_search`] and
/// `Session::content_search_streaming`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContentSearchQuery {
    pub mode: ContentSearchMode,
    /// Optional repository-relative file or directory path limit.
    pub path: Option<String>,
}

/// One matching commit from a history content search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentSearchEntry {
    pub commit: Oid,
    /// First matching repository-relative path in this commit, when known.
    pub matched_path: Option<String>,
}

/// Scan progress of an in-flight or completed content search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentSearchProgress {
    pub scanned: usize,
    pub total: usize,
}

enum CompiledMatcher {
    Substring {
        // Boxed: the searcher's SIMD state makes this variant several times
        // the size of the regex one on x86_64.
        finder: Box<Finder<'static>>,
        has_newline: bool,
    },
    Regex {
        line: Regex,
        prefilter: Option<Regex>,
    },
}

struct CompiledQuery {
    matcher: CompiledMatcher,
    path_components: Vec<Vec<u8>>,
    display_path: String,
    bloom_keys: Option<BloomKeys>,
}

impl CompiledQuery {
    fn new(query: &ContentSearchQuery) -> Result<Self> {
        let matcher = match &query.mode {
            ContentSearchMode::Substring(needle) => {
                if needle.is_empty() {
                    return Err(Error::other("content search query must not be empty"));
                }
                CompiledMatcher::Substring {
                    finder: Box::new(Finder::new(needle.as_bytes()).into_owned()),
                    has_newline: needle.as_bytes().contains(&b'\n'),
                }
            }
            ContentSearchMode::Regex(pattern) => {
                if pattern.is_empty() {
                    return Err(Error::other("content search regex must not be empty"));
                }
                let line = RegexBuilder::new(pattern)
                    .dot_matches_new_line(false)
                    .build()
                    .map_err(|err| Error::other(format!("invalid regular expression: {err}")))?;
                // Whole-slice prefiltering uses multi_line `^`/`$`; disable it
                // if the pattern anchors to start/end of input or uses inline flags.
                let prefilter = if pattern.contains("\\A")
                    || pattern.contains("\\z")
                    || pattern.contains("\\Z")
                    || pattern.contains("(?")
                {
                    None
                } else {
                    RegexBuilder::new(pattern)
                        .multi_line(true)
                        .dot_matches_new_line(false)
                        .build()
                        .ok()
                };
                CompiledMatcher::Regex { line, prefilter }
            }
        };

        let path_components: Vec<Vec<u8>> = query
            .path
            .as_deref()
            .unwrap_or("")
            .as_bytes()
            .split(|byte| *byte == b'/')
            .filter(|component| !component.is_empty() && *component != b".")
            .map(<[u8]>::to_vec)
            .collect();

        let display_path = path_components
            .iter()
            .map(|component| String::from_utf8_lossy(component))
            .collect::<Vec<_>>()
            .join("/");

        let bloom_keys =
            (!path_components.is_empty()).then(|| BloomKeys::new(display_path.as_bytes()));

        Ok(Self {
            matcher,
            path_components,
            display_path,
            bloom_keys,
        })
    }
}

#[derive(Clone)]
struct CacheSlot {
    key: Oid,
    value: Option<TreeEntry>,
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

    #[inline]
    fn index(&self, key: Oid) -> usize {
        let bytes = key.as_bytes();
        let raw = u64::from_ne_bytes(bytes[..8].try_into().expect("hash >= 8 bytes"));
        (raw as usize) & self.mask
    }

    #[inline]
    fn get(&self, key: Oid) -> Option<Option<TreeEntry>> {
        let slot = &self.slots[self.index(key)];
        (slot.occupied && slot.key == key).then_some(slot.value)
    }

    #[inline]
    fn insert(&mut self, key: Oid, value: Option<TreeEntry>) {
        let idx = self.index(key);
        self.slots[idx] = CacheSlot {
            key,
            value,
            occupied: true,
        };
    }
}

/// Compact per-blob summary cached across commits on a single worker.
///
/// For `-S`: `0..=usize::MAX - 1` is the occurrence count (`-1` sentinel is `Skip`).
/// For `-G`: `0` = NoMatch, `1` = HasMatch, `u64::MAX` = Skip.
#[derive(Clone, Copy)]
struct BlobSummarySlot {
    key: Oid,
    value: u64,
    occupied: bool,
}

impl Default for BlobSummarySlot {
    fn default() -> Self {
        Self {
            key: Oid::null(gix::hash::Kind::Sha1),
            value: 0,
            occupied: false,
        }
    }
}

struct BlobSummaryCache {
    slots: Vec<BlobSummarySlot>,
    mask: usize,
}

impl BlobSummaryCache {
    fn new(capacity: usize) -> Self {
        debug_assert!(capacity.is_power_of_two());
        Self {
            slots: vec![BlobSummarySlot::default(); capacity],
            mask: capacity - 1,
        }
    }

    #[inline]
    fn index(&self, key: Oid) -> usize {
        let bytes = key.as_bytes();
        let raw = u64::from_ne_bytes(bytes[..8].try_into().expect("hash >= 8 bytes"));
        (raw as usize) & self.mask
    }

    #[inline]
    fn get(&self, key: Oid) -> Option<u64> {
        let slot = &self.slots[self.index(key)];
        (slot.occupied && slot.key == key).then_some(slot.value)
    }

    #[inline]
    fn insert(&mut self, key: Oid, value: u64) {
        let idx = self.index(key);
        self.slots[idx] = BlobSummarySlot {
            key,
            value,
            occupied: true,
        };
    }
}

const SKIP_SENTINEL: u64 = u64::MAX;
const REGEX_NO_MATCH: u64 = 0;
const REGEX_HAS_MATCH: u64 = 1;

#[derive(Clone, Copy)]
enum BlobEval {
    Skip,
    Count(usize),
    Regex { has_match: bool },
}

struct SideRecord {
    oid: Oid,
    mode: EntryMode,
    relation: Option<Relation>,
    path_start: usize,
    path_end: usize,
}

struct BlobMatcher {
    empty_blob: Oid,
    odb_buf: Vec<u8>,
    old_blob_buf: Vec<u8>,
    new_blob_buf: Vec<u8>,
    diff: Diff,
    blob_cache: BlobSummaryCache,
}

impl BlobMatcher {
    fn new(empty_blob: Oid) -> Self {
        Self {
            empty_blob,
            odb_buf: Vec::new(),
            old_blob_buf: Vec::new(),
            new_blob_buf: Vec::new(),
            diff: Diff::default(),
            blob_cache: BlobSummaryCache::new(BLOB_RESULT_CACHE_SLOTS),
        }
    }
}

struct SearchEvaluator {
    repo: gix::Repository,
    old_tree_buf: Vec<u8>,
    new_tree_buf: Vec<u8>,
    diff_state: gix::diff::tree::State,
    caches: Vec<LevelCache>,
    blobs: BlobMatcher,
}

impl SearchEvaluator {
    fn new(repo: gix::Repository, num_path_components: usize) -> Self {
        let empty_blob = Oid::empty_blob(repo.object_hash());
        let mut caches = Vec::with_capacity(num_path_components);
        for level in 0..num_path_components {
            let cap = if level == 0 {
                ROOT_CACHE_SLOTS
            } else {
                SUBTREE_CACHE_SLOTS
            };
            caches.push(LevelCache::new(cap));
        }
        Self {
            repo,
            old_tree_buf: Vec::new(),
            new_tree_buf: Vec::new(),
            diff_state: gix::diff::tree::State::default(),
            caches,
            blobs: BlobMatcher::new(empty_blob),
        }
    }

    fn read_commit_tree(&mut self, id: Oid) -> Result<Oid> {
        let commit = self.repo.find_commit(id).map_err(Error::gix)?;
        Ok(commit.tree_id().map_err(Error::gix)?.detach())
    }

    fn check_commit(
        &mut self,
        commit: &PathCommit,
        query: &CompiledQuery,
        bloom: Option<&CommitGraphBloom>,
    ) -> Result<Option<String>> {
        // Git's default `-S`/`-G` skips merge diffs unless `-m` is requested.
        if commit.is_merge {
            return Ok(None);
        }

        let tree = match commit.tree {
            Some(tree) => tree,
            None => self.read_commit_tree(commit.id)?,
        };
        let parent_tree = match commit.parent {
            None => None,
            Some(parent) => match commit.parent_tree {
                Some(tree) => Some(tree),
                None => self.read_commit_tree(parent).ok(),
            },
        };

        if Some(tree) == parent_tree {
            return Ok(None);
        }

        if !query.path_components.is_empty() {
            if let (Some(bloom), Some(pos), Some(keys)) =
                (bloom, commit.graph_pos, query.bloom_keys.as_ref())
                && !bloom.may_contain(pos, keys)
            {
                return Ok(None);
            }
            return self.check_path_limited(parent_tree, tree, query);
        }

        self.diff_trees_for_search(parent_tree, Some(tree), "", query)
    }

    fn check_path_limited(
        &mut self,
        parent_tree: Option<Oid>,
        tree: Oid,
        query: &CompiledQuery,
    ) -> Result<Option<String>> {
        let components = &query.path_components;
        let last = components.len() - 1;

        let (entry_old, entry_new) = match parent_tree {
            None => (None, self.lookup_from(0, tree, components)?),
            Some(mut tree_old) => {
                let mut tree_new = tree;
                let mut resolved = None;
                for level in 0..=last {
                    if tree_old == tree_new {
                        return Ok(None);
                    }
                    let name = components[level].as_slice();
                    let old_entry = self.entry_at_level(level, tree_old, name)?;
                    let new_entry = self.entry_at_level(level, tree_new, name)?;
                    if old_entry == new_entry {
                        return Ok(None);
                    }
                    if level == last {
                        resolved = Some((old_entry, new_entry));
                        break;
                    }
                    match (next_tree(old_entry), next_tree(new_entry)) {
                        (Some(sub_old), Some(sub_new)) => {
                            tree_old = sub_old;
                            tree_new = sub_new;
                        }
                        (Some(sub_old), None) => {
                            let old = self.lookup_from(level + 1, sub_old, components)?;
                            resolved = Some((old, None));
                            break;
                        }
                        (None, Some(sub_new)) => {
                            let new = self.lookup_from(level + 1, sub_new, components)?;
                            resolved = Some((None, new));
                            break;
                        }
                        (None, None) => return Ok(None),
                    }
                }
                match resolved {
                    Some(pair) => pair,
                    None => return Ok(None),
                }
            }
        };

        let old_is_tree = entry_old.is_some_and(|(_, m)| is_tree_mode(m));
        let new_is_tree = entry_new.is_some_and(|(_, m)| is_tree_mode(m));

        match (old_is_tree, new_is_tree) {
            (false, false) => {
                let old_blob = entry_old
                    .filter(|(_, m)| is_countable_mode(*m))
                    .map(|(id, _)| id);
                let new_blob = entry_new
                    .filter(|(_, m)| is_countable_mode(*m))
                    .map(|(id, _)| id);
                let matched = match (old_blob, new_blob) {
                    (Some(old_id), Some(new_id)) => {
                        self.blobs
                            .match_pair(&self.repo, old_id, new_id, &query.matcher)?
                    }
                    (None, Some(new_id)) => {
                        self.blobs
                            .match_single(&self.repo, new_id, &query.matcher)?
                    }
                    (Some(old_id), None) => {
                        self.blobs
                            .match_single(&self.repo, old_id, &query.matcher)?
                    }
                    (None, None) => false,
                };
                Ok(matched.then(|| query.display_path.clone()))
            }
            (true, true) => self.diff_trees_for_search(
                entry_old.map(|(id, _)| id),
                entry_new.map(|(id, _)| id),
                &query.display_path,
                query,
            ),
            (true, false) => {
                if let Some((new_id, new_mode)) = entry_new
                    && is_countable_mode(new_mode)
                    && self
                        .blobs
                        .match_single(&self.repo, new_id, &query.matcher)?
                {
                    return Ok(Some(query.display_path.clone()));
                }
                self.diff_trees_for_search(
                    entry_old.map(|(id, _)| id),
                    None,
                    &query.display_path,
                    query,
                )
            }
            (false, true) => {
                if let Some((old_id, old_mode)) = entry_old
                    && is_countable_mode(old_mode)
                    && self
                        .blobs
                        .match_single(&self.repo, old_id, &query.matcher)?
                {
                    return Ok(Some(query.display_path.clone()));
                }
                self.diff_trees_for_search(
                    None,
                    entry_new.map(|(id, _)| id),
                    &query.display_path,
                    query,
                )
            }
        }
    }

    fn diff_trees_for_search(
        &mut self,
        old_tree: Option<Oid>,
        new_tree: Option<Oid>,
        prefix: &str,
        query: &CompiledQuery,
    ) -> Result<Option<String>> {
        if old_tree == new_tree {
            return Ok(None);
        }

        let hash_kind = self.repo.object_hash();
        let old_bytes = read_optional_tree(&self.repo, old_tree, &mut self.old_tree_buf)?;
        let new_bytes = read_optional_tree(&self.repo, new_tree, &mut self.new_tree_buf)?;

        let mut visitor = SearchTreeVisitor::new(&self.repo, &mut self.blobs, &query.matcher);
        let diff_res = gix::diff::tree(
            TreeRefIter::from_bytes(old_bytes, hash_kind),
            TreeRefIter::from_bytes(new_bytes, hash_kind),
            &mut self.diff_state,
            &self.repo.objects,
            &mut visitor,
        );

        let matched_rel = visitor.matched_path;
        let error = visitor.error;
        let additions = visitor.additions;
        let deletions = visitor.deletions;
        let path_arena = visitor.path_arena;

        if let Some(err) = error {
            return Err(err);
        }
        if let Some(rel) = matched_rel {
            return Ok(Some(join_prefix(prefix, &rel)));
        }
        if let Err(err) = diff_res {
            match err {
                gix::diff::tree::Error::Cancelled => {}
                gix::diff::tree::Error::Failure(e) => return Err(Error::gix(e)),
            }
        }

        if additions.is_empty() && deletions.is_empty() {
            return Ok(None);
        }

        if additions.is_empty() {
            for del in &deletions {
                if self
                    .blobs
                    .match_single(&self.repo, del.oid, &query.matcher)?
                {
                    let rel = String::from_utf8_lossy(&path_arena[del.path_start..del.path_end]);
                    return Ok(Some(join_prefix(prefix, &rel)));
                }
            }
            return Ok(None);
        }

        if deletions.is_empty() {
            for add in &additions {
                if self
                    .blobs
                    .match_single(&self.repo, add.oid, &query.matcher)?
                {
                    let rel = String::from_utf8_lossy(&path_arena[add.path_start..add.path_end]);
                    return Ok(Some(join_prefix(prefix, &rel)));
                }
            }
            return Ok(None);
        }

        // Both additions and deletions exist, so renames may pair them.
        // First eliminate exact-OID renames in memory (zero blob I/O) on typical
        // commits; only invoke full rename detection if unpaired candidates
        // remain on both sides AND at least one candidate matches.
        if additions.len() + deletions.len() < 100_000 {
            let mut add_paired = vec![false; additions.len()];
            let mut del_paired = vec![false; deletions.len()];
            eliminate_identity_pairs(
                &additions,
                &deletions,
                &path_arena,
                self.blobs.empty_blob,
                &mut add_paired,
                &mut del_paired,
            );

            let mut matched_add: Option<usize> = None;
            let mut matched_del: Option<usize> = None;
            let mut unpaired_adds = 0usize;
            let mut unpaired_dels = 0usize;

            for (i, add) in additions.iter().enumerate() {
                if add_paired[i] {
                    continue;
                }
                unpaired_adds += 1;
                if self
                    .blobs
                    .match_single(&self.repo, add.oid, &query.matcher)?
                    && matched_add.is_none()
                {
                    matched_add = Some(i);
                }
            }
            for (i, del) in deletions.iter().enumerate() {
                if del_paired[i] {
                    continue;
                }
                unpaired_dels += 1;
                if self
                    .blobs
                    .match_single(&self.repo, del.oid, &query.matcher)?
                    && matched_del.is_none()
                {
                    matched_del = Some(i);
                }
            }

            if matched_add.is_none() && matched_del.is_none() {
                return Ok(None);
            }
            if unpaired_adds == 0 || unpaired_dels == 0 {
                if let Some(i) = matched_add {
                    let add = &additions[i];
                    let rel = String::from_utf8_lossy(&path_arena[add.path_start..add.path_end]);
                    return Ok(Some(join_prefix(prefix, &rel)));
                }
                if let Some(i) = matched_del {
                    let del = &deletions[i];
                    let rel = String::from_utf8_lossy(&path_arena[del.path_start..del.path_end]);
                    return Ok(Some(join_prefix(prefix, &rel)));
                }
            }
        }

        let mut raw_changes = Vec::with_capacity(additions.len() + deletions.len());
        for add in &additions {
            raw_changes.push(ChangeDetached::Addition {
                location: BString::from(&path_arena[add.path_start..add.path_end]),
                relation: add.relation,
                entry_mode: add.mode,
                id: add.oid,
            });
        }
        for del in &deletions {
            raw_changes.push(ChangeDetached::Deletion {
                location: BString::from(&path_arena[del.path_start..del.path_end]),
                relation: del.relation,
                entry_mode: del.mode,
                id: del.oid,
            });
        }
        let rewritten = renames::apply(&raw_changes, &self.repo, None)?;
        for file in rewritten {
            let matched = match file.kind {
                ChangeKind::Renamed
                | ChangeKind::Copied
                | ChangeKind::Modified
                | ChangeKind::TypeChanged => match (file.old_id, file.new_id) {
                    (Some(old_id), Some(new_id)) => {
                        self.blobs
                            .match_pair(&self.repo, old_id, new_id, &query.matcher)?
                    }
                    (None, Some(new_id)) => {
                        self.blobs
                            .match_single(&self.repo, new_id, &query.matcher)?
                    }
                    (Some(old_id), None) => {
                        self.blobs
                            .match_single(&self.repo, old_id, &query.matcher)?
                    }
                    (None, None) => false,
                },
                ChangeKind::Added => match file.new_id {
                    Some(id) => self.blobs.match_single(&self.repo, id, &query.matcher)?,
                    None => false,
                },
                ChangeKind::Deleted => match file.old_id {
                    Some(id) => self.blobs.match_single(&self.repo, id, &query.matcher)?,
                    None => false,
                },
                ChangeKind::Untracked | ChangeKind::Conflicted => false,
            };
            if matched {
                return Ok(Some(join_prefix(prefix, &file.path)));
            }
        }

        Ok(None)
    }
}

impl BlobMatcher {
    fn eval_blob_cached(
        &mut self,
        repo: &gix::Repository,
        id: Oid,
        matcher: &CompiledMatcher,
    ) -> Result<BlobEval> {
        if id == self.empty_blob {
            return Ok(match matcher {
                CompiledMatcher::Substring { .. } => BlobEval::Count(0),
                CompiledMatcher::Regex { .. } => BlobEval::Regex { has_match: false },
            });
        }

        if let Some(cached) = self.blob_cache.get(id) {
            if cached == SKIP_SENTINEL {
                return Ok(BlobEval::Skip);
            }
            return Ok(match matcher {
                CompiledMatcher::Substring { .. } => BlobEval::Count(cached as usize),
                CompiledMatcher::Regex { .. } => BlobEval::Regex {
                    has_match: cached != REGEX_NO_MATCH,
                },
            });
        }

        if is_large_id(repo, id) {
            self.blob_cache.insert(id, SKIP_SENTINEL);
            return Ok(BlobEval::Skip);
        }

        let blob = repo
            .objects
            .find_blob(&id, &mut self.odb_buf)
            .map_err(Error::gix)?;
        if is_binary(blob.data) {
            self.blob_cache.insert(id, SKIP_SENTINEL);
            return Ok(BlobEval::Skip);
        }

        match matcher {
            CompiledMatcher::Substring { finder, .. } => {
                let count = finder.find_iter(blob.data).count();
                self.blob_cache.insert(id, count as u64);
                Ok(BlobEval::Count(count))
            }
            CompiledMatcher::Regex { line, prefilter } => {
                let has_match = any_line_matches(blob.data, line, prefilter.as_ref());
                self.blob_cache.insert(
                    id,
                    if has_match {
                        REGEX_HAS_MATCH
                    } else {
                        REGEX_NO_MATCH
                    },
                );
                Ok(BlobEval::Regex { has_match })
            }
        }
    }

    fn match_single(
        &mut self,
        repo: &gix::Repository,
        id: Oid,
        matcher: &CompiledMatcher,
    ) -> Result<bool> {
        Ok(match self.eval_blob_cached(repo, id, matcher)? {
            BlobEval::Skip => false,
            BlobEval::Count(n) => n > 0,
            BlobEval::Regex { has_match } => has_match,
        })
    }

    fn match_pair(
        &mut self,
        repo: &gix::Repository,
        old_id: Oid,
        new_id: Oid,
        matcher: &CompiledMatcher,
    ) -> Result<bool> {
        if old_id == new_id {
            return Ok(false);
        }
        if old_id == self.empty_blob {
            return self.match_single(repo, new_id, matcher);
        }
        if new_id == self.empty_blob {
            return self.match_single(repo, old_id, matcher);
        }

        match matcher {
            CompiledMatcher::Substring {
                finder,
                has_newline,
            } => {
                let old_cached = self.blob_cache.get(old_id);
                let new_cached = self.blob_cache.get(new_id);

                if let (Some(old_val), Some(new_val)) = (old_cached, new_cached) {
                    if old_val == SKIP_SENTINEL || new_val == SKIP_SENTINEL {
                        return Ok(false);
                    }
                    return Ok(old_val != new_val);
                }

                // If neither is cached and the needle cannot cross a line
                // boundary, stripping identical prefix/suffix lines avoids
                // scanning unchanged portions of large modified files.
                if old_cached.is_none() && new_cached.is_none() && !*has_newline {
                    if is_large_id(repo, old_id) || is_large_id(repo, new_id) {
                        return Ok(false);
                    }
                    let old_data = repo
                        .objects
                        .find_blob(&old_id, &mut self.old_blob_buf)
                        .map_err(Error::gix)?
                        .data;
                    if is_binary(old_data) {
                        self.blob_cache.insert(old_id, SKIP_SENTINEL);
                        return Ok(false);
                    }
                    let new_data = repo
                        .objects
                        .find_blob(&new_id, &mut self.new_blob_buf)
                        .map_err(Error::gix)?
                        .data;
                    if is_binary(new_data) {
                        self.blob_cache.insert(new_id, SKIP_SENTINEL);
                        return Ok(false);
                    }
                    let (old_mid, new_mid) = strip_common_lines(old_data, new_data);
                    let old_count = finder.find_iter(old_mid).count();
                    let new_count = finder.find_iter(new_mid).count();
                    return Ok(old_count != new_count);
                }

                let old_eval = self.eval_blob_cached(repo, old_id, matcher)?;
                let BlobEval::Count(old_count) = old_eval else {
                    return Ok(false);
                };
                let new_eval = self.eval_blob_cached(repo, new_id, matcher)?;
                let BlobEval::Count(new_count) = new_eval else {
                    return Ok(false);
                };
                Ok(old_count != new_count)
            }
            CompiledMatcher::Regex { line, prefilter } => {
                let old_eval = self.eval_blob_cached(repo, old_id, matcher)?;
                let BlobEval::Regex { has_match: old_has } = old_eval else {
                    return Ok(false);
                };
                let new_eval = self.eval_blob_cached(repo, new_id, matcher)?;
                let BlobEval::Regex { has_match: new_has } = new_eval else {
                    return Ok(false);
                };

                match (old_has, new_has) {
                    (false, false) => Ok(false),
                    // Exactly one side has matching lines while the other has
                    // none, so at least one matching line must be added/removed.
                    (true, false) | (false, true) => Ok(true),
                    (true, true) => {
                        let old_data = repo
                            .objects
                            .find_blob(&old_id, &mut self.old_blob_buf)
                            .map_err(Error::gix)?
                            .data;
                        let new_data = repo
                            .objects
                            .find_blob(&new_id, &mut self.new_blob_buf)
                            .map_err(Error::gix)?
                            .data;
                        Ok(diff_has_matching_line(
                            old_data,
                            new_data,
                            line,
                            prefilter.as_ref(),
                            &mut self.diff,
                        ))
                    }
                }
            }
        }
    }
}

impl SearchEvaluator {
    fn lookup_from(
        &mut self,
        start_level: usize,
        mut tree: Oid,
        components: &[Vec<u8>],
    ) -> Result<Option<TreeEntry>> {
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
    ) -> Result<Option<TreeEntry>> {
        if let Some(cached) = self.caches[level].get(tree) {
            return Ok(cached);
        }
        let found = self.scan_tree(tree, component)?;
        self.caches[level].insert(tree, found);
        Ok(found)
    }

    fn scan_tree(&mut self, tree: Oid, target: &[u8]) -> Result<Option<TreeEntry>> {
        let data = self
            .repo
            .objects
            .find(&tree, &mut self.blobs.odb_buf)
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

struct SearchTreeVisitor<'a> {
    repo: &'a gix::Repository,
    blobs: &'a mut BlobMatcher,
    matcher: &'a CompiledMatcher,
    path_deque: VecDeque<BString>,
    path: BString,
    path_arena: Vec<u8>,
    additions: Vec<SideRecord>,
    deletions: Vec<SideRecord>,
    matched_path: Option<String>,
    error: Option<Error>,
}

impl<'a> SearchTreeVisitor<'a> {
    fn new(
        repo: &'a gix::Repository,
        blobs: &'a mut BlobMatcher,
        matcher: &'a CompiledMatcher,
    ) -> Self {
        Self {
            repo,
            blobs,
            matcher,
            path_deque: VecDeque::new(),
            path: BString::default(),
            path_arena: Vec::new(),
            additions: Vec::new(),
            deletions: Vec::new(),
            matched_path: None,
            error: None,
        }
    }

    fn push_element(&mut self, name: &BStr) {
        if name.is_empty() {
            return;
        }
        if !self.path.is_empty() {
            self.path.push(b'/');
        }
        self.path.push_str(name);
    }

    fn pop_element(&mut self) {
        if let Some(pos) = self.path.rfind_byte(b'/') {
            self.path.resize(pos, 0);
        } else {
            self.path.clear();
        }
    }

    fn push_side(&mut self, is_add: bool, oid: Oid, mode: EntryMode, relation: Option<Relation>) {
        let start = self.path_arena.len();
        self.path_arena.extend_from_slice(&self.path);
        let end = self.path_arena.len();
        let rec = SideRecord {
            oid,
            mode,
            relation,
            path_start: start,
            path_end: end,
        };
        if is_add {
            self.additions.push(rec);
        } else {
            self.deletions.push(rec);
        }
    }
}

impl gix::diff::tree::Visit for SearchTreeVisitor<'_> {
    fn pop_front_tracked_path_and_set_current(&mut self) {
        self.path = self
            .path_deque
            .pop_front()
            .expect("every parent is set only once");
    }

    fn push_back_tracked_path_component(&mut self, component: &BStr) {
        self.push_element(component);
        self.path_deque.push_back(self.path.clone());
    }

    fn push_path_component(&mut self, component: &BStr) {
        self.push_element(component);
    }

    fn pop_path_component(&mut self) {
        self.pop_element();
    }

    fn visit(&mut self, change: VisitChange) -> Action {
        match change {
            VisitChange::Addition {
                entry_mode,
                oid,
                relation,
            } => {
                if !entry_mode.is_tree() && entry_mode.is_blob_or_symlink() {
                    self.push_side(true, oid, entry_mode, relation);
                }
            }
            VisitChange::Deletion {
                entry_mode,
                oid,
                relation,
            } => {
                if !entry_mode.is_tree() && entry_mode.is_blob_or_symlink() {
                    self.push_side(false, oid, entry_mode, relation);
                }
            }
            VisitChange::Modification {
                previous_entry_mode,
                previous_oid,
                entry_mode,
                oid,
            } => {
                if previous_entry_mode.is_tree() && entry_mode.is_tree() {
                    return Action::Continue(());
                }
                if entry_mode.is_tree() {
                    if previous_entry_mode.is_blob_or_symlink() {
                        self.push_side(false, previous_oid, previous_entry_mode, None);
                    }
                    return Action::Continue(());
                }
                if previous_entry_mode.is_tree() {
                    if entry_mode.is_blob_or_symlink() {
                        self.push_side(true, oid, entry_mode, None);
                    }
                    return Action::Continue(());
                }
                if !previous_entry_mode.is_blob_or_symlink() || !entry_mode.is_blob_or_symlink() {
                    return Action::Continue(());
                }
                if previous_oid == oid {
                    return Action::Continue(());
                }
                match self
                    .blobs
                    .match_pair(self.repo, previous_oid, oid, self.matcher)
                {
                    Ok(true) => {
                        self.matched_path = Some(self.path.to_str_lossy().into_owned());
                        return Action::Break(());
                    }
                    Ok(false) => {}
                    Err(err) => {
                        self.error = Some(err);
                        return Action::Break(());
                    }
                }
            }
        }
        Action::Continue(())
    }
}

fn read_optional_tree<'a>(
    repo: &gix::Repository,
    tree: Option<Oid>,
    buf: &'a mut Vec<u8>,
) -> Result<&'a [u8]> {
    let Some(tree) = tree else {
        return Ok(&[]);
    };
    let obj = repo.objects.find(&tree, buf).map_err(Error::gix)?;
    if obj.kind != gix::objs::Kind::Tree {
        return Err(Error::other(format!("{tree} is not a tree")));
    }
    Ok(obj.data)
}

fn join_prefix(prefix: &str, rel: &str) -> String {
    if prefix.is_empty() {
        rel.to_owned()
    } else if rel.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}/{rel}")
    }
}

fn any_line_matches(data: &[u8], line_regex: &Regex, prefilter: Option<&Regex>) -> bool {
    if data.is_empty() {
        return false;
    }
    if let Some(pre) = prefilter
        && !pre.is_match(data)
    {
        return false;
    }
    for_each_line_until(data, |line| line_regex.is_match(line))
}

fn diff_has_matching_line(
    old: &[u8],
    new: &[u8],
    line_regex: &Regex,
    prefilter: Option<&Regex>,
    diff: &mut Diff,
) -> bool {
    let (old_mid, new_mid) = strip_common_lines(old, new);
    if old_mid.is_empty() && new_mid.is_empty() {
        return false;
    }
    if old_mid.is_empty() {
        return any_line_matches(new_mid, line_regex, prefilter);
    }
    if new_mid.is_empty() {
        return any_line_matches(old_mid, line_regex, prefilter);
    }
    if let Some(pre) = prefilter
        && !pre.is_match(old_mid)
        && !pre.is_match(new_mid)
    {
        return false;
    }
    if is_single_line(old_mid) && is_single_line(new_mid) {
        let old_line = old_mid.strip_suffix(b"\n").unwrap_or(old_mid);
        let new_line = new_mid.strip_suffix(b"\n").unwrap_or(new_mid);
        return line_regex.is_match(old_line) || line_regex.is_match(new_line);
    }

    let input = InternedInput::new(old_mid, new_mid);
    diff.compute_with(
        Algorithm::Myers,
        &input.before,
        &input.after,
        input.interner.num_tokens(),
    );
    diff.postprocess_lines(&input);

    for hunk in diff.hunks() {
        for &token in &input.before[hunk.before.start as usize..hunk.before.end as usize] {
            let line = input.interner[token];
            let line = line.strip_suffix(b"\n").unwrap_or(line);
            if line_regex.is_match(line) {
                return true;
            }
        }
        for &token in &input.after[hunk.after.start as usize..hunk.after.end as usize] {
            let line = input.interner[token];
            let line = line.strip_suffix(b"\n").unwrap_or(line);
            if line_regex.is_match(line) {
                return true;
            }
        }
    }
    false
}

fn for_each_line_until(data: &[u8], mut pred: impl FnMut(&[u8]) -> bool) -> bool {
    let mut start = 0usize;
    while start < data.len() {
        let rest = &data[start..];
        if let Some(nl) = rest.find_byte(b'\n') {
            if pred(&rest[..nl]) {
                return true;
            }
            start += nl + 1;
        } else {
            return pred(rest);
        }
    }
    false
}

fn eliminate_identity_pairs(
    additions: &[SideRecord],
    deletions: &[SideRecord],
    path_arena: &[u8],
    empty_blob: Oid,
    add_paired: &mut [bool],
    del_paired: &mut [bool],
) {
    let mut del_indices: Vec<usize> = (0..deletions.len()).collect();
    del_indices.sort_by(|&a, &b| deletions[a].oid.cmp(&deletions[b].oid));

    for (ai, add) in additions.iter().enumerate() {
        if add.oid == empty_blob && !matches!(add.relation, Some(Relation::ChildOfParent(_))) {
            continue;
        }
        let start = del_indices.partition_point(|&di| deletions[di].oid < add.oid);
        let end = del_indices.partition_point(|&di| deletions[di].oid <= add.oid);
        if start == end {
            continue;
        }
        let add_base = basename(&path_arena[add.path_start..add.path_end]);
        let mut fallback = None;
        for &di in &del_indices[start..end] {
            if del_paired[di] || !modes_compatible(deletions[di].mode, add.mode) {
                continue;
            }
            let del = &deletions[di];
            if basename(&path_arena[del.path_start..del.path_end]) == add_base {
                fallback = Some(di);
                break;
            }
            fallback.get_or_insert(di);
        }
        if let Some(di) = fallback {
            add_paired[ai] = true;
            del_paired[di] = true;
        }
    }
}

fn basename(path: &[u8]) -> &[u8] {
    path.rfind_byte(b'/').map_or(path, |idx| &path[idx + 1..])
}

fn modes_compatible(a: EntryMode, b: EntryMode) -> bool {
    matches!(
        (a.kind(), b.kind()),
        (
            EntryKind::Blob | EntryKind::BlobExecutable,
            EntryKind::Blob | EntryKind::BlobExecutable
        ) | (EntryKind::Link, EntryKind::Link)
            | (EntryKind::Tree, EntryKind::Tree)
            | (EntryKind::Commit, EntryKind::Commit)
    )
}

#[inline]
fn is_tree_mode(mode: u16) -> bool {
    (mode & 0o170000) == 0o040000
}

#[inline]
fn is_countable_mode(mode: u16) -> bool {
    EntryMode::try_from(u32::from(mode)).is_ok_and(|entry_mode| entry_mode.is_blob_or_symlink())
}

#[inline]
fn next_tree(entry: Option<TreeEntry>) -> Option<Oid> {
    let (oid, mode) = entry?;
    is_tree_mode(mode).then_some(oid)
}

fn scan_tree_entry(
    data: &[u8],
    hash_len: usize,
    target: &[u8],
) -> std::result::Result<Option<TreeEntry>, ()> {
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

fn is_large_id(repo: &gix::Repository, id: Oid) -> bool {
    match repo.try_find_header(id) {
        Ok(Some(header)) => header.size() > MAX_STAT_BYTES,
        _ => false,
    }
}

struct SharedPoolState {
    generation: u64,
    shutdown: bool,
    commits: Vec<PathCommit>,
    next_chunk: AtomicUsize,
    chunks_total: usize,
    chunks_done: usize,
    chunk_results: Vec<Option<Vec<Result<Option<String>>>>>,
}

struct WorkerPool {
    shared: Arc<(Mutex<SharedPoolState>, Condvar, Condvar)>,
    handles: Vec<JoinHandle<()>>,
}

impl WorkerPool {
    fn new(
        num_workers: usize,
        sync_repo: gix::ThreadSafeRepository,
        query: Arc<CompiledQuery>,
        bloom: Option<Arc<CommitGraphBloom>>,
        cancelled: Option<CancelCheck>,
    ) -> Self {
        let state = SharedPoolState {
            generation: 0,
            shutdown: false,
            commits: Vec::new(),
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
            let query = Arc::clone(&query);
            let bloom = bloom.clone();
            let cancelled = cancelled.clone();
            handles.push(std::thread::spawn(move || {
                worker_loop(shared, sync_repo, query, bloom, cancelled);
            }));
        }

        Self { shared, handles }
    }

    fn eval(&mut self, commits: Vec<PathCommit>) -> (Vec<PathCommit>, Vec<Result<Option<String>>>) {
        let chunks_total = commits.len().div_ceil(WORKER_CHUNK_SIZE);
        let (lock, work_cv, done_cv) = &*self.shared;
        let mut guard = lock.lock().expect("worker pool mutex poisoned");
        guard.generation += 1;
        guard.commits = commits;
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
    query: Arc<CompiledQuery>,
    bloom: Option<Arc<CommitGraphBloom>>,
    cancelled: Option<CancelCheck>,
) {
    let mut local = sync_repo.to_thread_local();
    local.object_cache_size_if_unset(WORKER_OBJECT_CACHE_BYTES);
    let mut evaluator = SearchEvaluator::new(local, query.path_components.len());
    let mut seen_generation = 0u64;
    let (lock, work_cv, done_cv) = &*shared;

    loop {
        let (chunk_idx, generation, slice): (usize, u64, Vec<PathCommit>);
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
            let chunk_start = idx * WORKER_CHUNK_SIZE;
            let chunk_end = (chunk_start + WORKER_CHUNK_SIZE).min(guard.commits.len());
            slice = guard.commits[chunk_start..chunk_end].to_vec();
        }

        let result = eval_chunk(&mut evaluator, &slice, &query, bloom.as_deref(), &cancelled);

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
    evaluator: &mut SearchEvaluator,
    commits: &[PathCommit],
    query: &CompiledQuery,
    bloom: Option<&CommitGraphBloom>,
    cancelled: &Option<CancelCheck>,
) -> Vec<Result<Option<String>>> {
    let mut out = Vec::with_capacity(commits.len());
    for (idx, commit) in commits.iter().enumerate() {
        if idx & 15 == 0
            && let Some(check) = cancelled
            && check()
        {
            out.resize_with(commits.len(), || Ok(None));
            return out;
        }
        match evaluator.check_commit(commit, query, bloom) {
            Ok(matched) => out.push(Ok(matched)),
            Err(err) => {
                out.push(Err(err));
                return out;
            }
        }
    }
    out
}

const REFILL_WALK_CAP: usize = 32_768;

/// Streams matching [`ContentSearchEntry`] rows in `--date-order`, exposing
/// scan progress (`scanned` / `total`) along the way.
pub struct ContentSearchEntries {
    sync_repo: gix::ThreadSafeRepository,
    evaluator: SearchEvaluator,
    walk: PathWalk,
    bloom: Option<Arc<CommitGraphBloom>>,
    query: Arc<CompiledQuery>,
    total: usize,
    scanned: usize,
    checked_inline: usize,
    batch_size: usize,
    batch_commits: Vec<PathCommit>,
    batch_skipped: Vec<usize>,
    trailing_skipped: usize,
    batch_results: Vec<Result<Option<String>>>,
    batch_cursor: usize,
    trailing_error: Option<Error>,
    pool: Option<WorkerPool>,
    failed: bool,
    cancelled: Option<CancelCheck>,
}

impl ContentSearchEntries {
    pub fn progress(&self) -> ContentSearchProgress {
        ContentSearchProgress {
            scanned: self.scanned,
            total: self.total,
        }
    }

    /// Advances the search, yielding `Some(Ok(Some(entry)))` for a match,
    /// `Some(Ok(None))` when a batch finishes scanning without yielding another
    /// match (so streaming callers can emit `"N of M commits scanned"`), or
    /// `None` when the walk completes or is cancelled.
    pub(crate) fn advance(&mut self) -> Option<Result<Option<ContentSearchEntry>>> {
        if self.failed {
            return None;
        }
        loop {
            if self.is_cancelled() {
                return None;
            }
            if self.checked_inline < INLINE_WARMUP {
                let commit = match self.walk.next() {
                    Some(Ok(commit)) => commit,
                    Some(Err(error)) => {
                        self.failed = true;
                        return Some(Err(error));
                    }
                    None => return None,
                };
                self.checked_inline += 1;
                self.scanned += 1;
                match self
                    .evaluator
                    .check_commit(&commit, &self.query, self.bloom.as_deref())
                {
                    Ok(Some(matched_path)) => {
                        return Some(Ok(Some(ContentSearchEntry {
                            commit: commit.id,
                            matched_path: Some(matched_path),
                        })));
                    }
                    Ok(None) => {
                        if self.checked_inline == INLINE_WARMUP {
                            return Some(Ok(None));
                        }
                        continue;
                    }
                    Err(error) => {
                        self.failed = true;
                        return Some(Err(error));
                    }
                }
            }

            if self.batch_cursor >= self.batch_commits.len() {
                self.scanned += std::mem::take(&mut self.trailing_skipped);
                if !self.refill_batch() {
                    if let Some(error) = self.trailing_error.take() {
                        self.failed = true;
                        return Some(Err(error));
                    }
                    return None;
                }
                if self.batch_commits.is_empty() {
                    self.scanned += std::mem::take(&mut self.trailing_skipped);
                    return Some(Ok(None));
                }
            }

            while self.batch_cursor < self.batch_commits.len() {
                if self.is_cancelled() {
                    return None;
                }
                let idx = self.batch_cursor;
                self.batch_cursor += 1;
                self.scanned += self.batch_skipped[idx] + 1;
                let result = std::mem::replace(&mut self.batch_results[idx], Ok(None));
                match result {
                    Ok(None) => continue,
                    Ok(Some(matched_path)) => {
                        return Some(Ok(Some(ContentSearchEntry {
                            commit: self.batch_commits[idx].id,
                            matched_path: Some(matched_path),
                        })));
                    }
                    Err(error) => {
                        self.failed = true;
                        return Some(Err(error));
                    }
                }
            }
            self.scanned += std::mem::take(&mut self.trailing_skipped);
            return Some(Ok(None));
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.as_ref().is_some_and(|check| check())
    }

    fn refill_batch(&mut self) -> bool {
        self.batch_commits.clear();
        self.batch_skipped.clear();
        self.batch_results.clear();
        self.batch_cursor = 0;
        self.trailing_skipped = 0;

        if let Some(error) = self.trailing_error.take() {
            self.trailing_error = Some(error);
            return false;
        }

        let target = self.batch_size;
        let mut walked = 0usize;
        let mut skipped_since_prev = 0usize;
        while self.batch_commits.len() < target && walked < REFILL_WALK_CAP {
            if self.is_cancelled() {
                self.batch_commits.clear();
                self.batch_skipped.clear();
                return false;
            }
            match self.walk.next() {
                Some(Ok(commit)) => {
                    walked += 1;
                    if commit.is_merge {
                        skipped_since_prev += 1;
                        continue;
                    }
                    if let (Some(bloom), Some(pos), Some(keys)) = (
                        self.bloom.as_deref(),
                        commit.graph_pos,
                        self.query.bloom_keys.as_ref(),
                    ) && !bloom.may_contain(pos, keys)
                    {
                        skipped_since_prev += 1;
                        continue;
                    }
                    self.batch_commits.push(commit);
                    self.batch_skipped.push(skipped_since_prev);
                    skipped_since_prev = 0;
                }
                Some(Err(error)) => {
                    self.trailing_error = Some(error);
                    break;
                }
                None => break,
            }
        }
        self.trailing_skipped = skipped_since_prev;
        if walked == 0 && self.batch_commits.is_empty() {
            return false;
        }
        if self.batch_commits.len() >= target {
            self.batch_size = (self.batch_size * 2).min(MAX_BATCH_SIZE);
        }
        self.evaluate_batch();
        true
    }

    fn evaluate_batch(&mut self) {
        self.batch_results.clear();
        if self.batch_commits.is_empty() {
            return;
        }
        let workers = std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .clamp(1, MAX_WORKERS);
        if workers <= 1 || self.batch_commits.len() < WORKER_CHUNK_SIZE {
            self.batch_results.reserve(self.batch_commits.len());
            for commit in &self.batch_commits {
                if self.is_cancelled() {
                    self.batch_results
                        .resize_with(self.batch_commits.len(), || Ok(None));
                    return;
                }
                self.batch_results.push(self.evaluator.check_commit(
                    commit,
                    &self.query,
                    self.bloom.as_deref(),
                ));
            }
            return;
        }

        let pool = self.pool.get_or_insert_with(|| {
            WorkerPool::new(
                workers,
                self.sync_repo.clone(),
                Arc::clone(&self.query),
                self.bloom.clone(),
                self.cancelled.clone(),
            )
        });
        let commits = std::mem::take(&mut self.batch_commits);
        let (commits, results) = pool.eval(commits);
        self.batch_commits = commits;
        self.batch_results = results;
    }
}

impl Iterator for ContentSearchEntries {
    type Item = Result<ContentSearchEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.advance()? {
                Ok(Some(entry)) => return Some(Ok(entry)),
                Ok(None) => continue,
                Err(err) => return Some(Err(err)),
            }
        }
    }
}

impl Repo {
    /// Streams commits whose first-parent diff matches `query` (`git log -S` or
    /// `-G`, with optional path limit), in `--date-order`.
    pub fn content_search(&self, query: &ContentSearchQuery) -> Result<ContentSearchEntries> {
        self.content_search_internal(query, None, None)
    }

    /// Like [`content_search`](Self::content_search), starting only from
    /// `specs` (refs or revisions).
    pub fn content_search_from_refs(
        &self,
        query: &ContentSearchQuery,
        specs: &[String],
    ) -> Result<ContentSearchEntries> {
        self.content_search_internal(query, Some(specs), None)
    }

    pub(crate) fn content_search_cancellable(
        &self,
        query: &ContentSearchQuery,
        specs: Option<&[String]>,
        cancelled: CancelCheck,
    ) -> Result<ContentSearchEntries> {
        self.content_search_internal(query, specs, Some(cancelled))
    }

    fn content_search_internal(
        &self,
        query: &ContentSearchQuery,
        specs: Option<&[String]>,
        cancelled: Option<CancelCheck>,
    ) -> Result<ContentSearchEntries> {
        let compiled = Arc::new(CompiledQuery::new(query)?);
        let mut repo = self.local();
        repo.object_cache_size_if_unset(OBJECT_CACHE_BYTES);
        let (walk, _shallow, bloom) = match specs {
            Some(specs) => self.path_walk_from_refs_with(specs, cancelled.clone())?,
            None => self.path_walk_with(cancelled.clone())?,
        };
        let total = walk.total_commits();
        let evaluator = SearchEvaluator::new(repo, compiled.path_components.len());
        Ok(ContentSearchEntries {
            sync_repo: self.inner.clone(),
            evaluator,
            walk,
            bloom: bloom.map(Arc::new),
            query: compiled,
            total,
            scanned: 0,
            checked_inline: 0,
            batch_size: INITIAL_BATCH_SIZE,
            batch_commits: Vec::new(),
            batch_skipped: Vec::new(),
            trailing_skipped: 0,
            batch_results: Vec::new(),
            batch_cursor: 0,
            trailing_error: None,
            pool: None,
            failed: false,
            cancelled,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::test_support::TestRepo;

    #[test]
    fn cancellation_stops_content_search_promptly() {
        let fx = TestRepo::new();
        for i in 0..40 {
            let content = "match_token\n".repeat(i + 1);
            fx.write("log.txt", &content);
            fx.commit(&format!("commit {i}"));
        }

        let repo = Repo::open(fx.path()).expect("open");
        let flag = Arc::new(AtomicBool::new(false));
        let check: CancelCheck = {
            let flag = Arc::clone(&flag);
            Arc::new(move || flag.load(Ordering::Relaxed))
        };
        let q = ContentSearchQuery {
            mode: ContentSearchMode::Substring("match_token".into()),
            path: None,
        };
        let mut entries = repo
            .content_search_cancellable(&q, None, check)
            .expect("search");
        assert_eq!(entries.progress().total, 40);
        assert_eq!(entries.progress().scanned, 0);

        let first = entries.next().expect("first match").expect("ok");
        assert_eq!(first.matched_path.as_deref(), Some("log.txt"));
        assert_eq!(entries.progress().scanned, 1);

        flag.store(true, Ordering::Relaxed);
        assert!(
            entries.next().is_none(),
            "cancelled iterator must terminate"
        );
    }
}
