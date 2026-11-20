//! Rename detection over a first-parent tree change list.
//!
//! A commit's file list and its line counts share one tree walk, and only the
//! file list needs to know about renames. Replaying the walk through gix's
//! rewrite tracker would pair every added and deleted file, which on a large
//! merge means tens of thousands of blob diffs.
//!
//! This pass pairs the same entries with the same rules: identical blob ids
//! first, then the best similarity above the same threshold gix uses. The score
//! is gix's, but prepared blobs are cached per item, candidates are bounded by
//! both size ratio and a line-hash multiset intersection before any diff runs,
//! sorted so the first provably maximal match can stop the search early, and
//! identical leading/trailing lines are stripped before `imara-diff`.

use std::cmp::Ordering;
use std::sync::Arc;

use gix::bstr::{BStr, BString, ByteSlice};
use gix::diff::blob::platform::prepare_diff::Operation;
use gix::diff::blob::{Algorithm, Diff, InternedInput, ResourceKind};
use gix::object::tree::diff::ChangeDetached;
use gix::object::tree::{EntryKind, EntryMode};

use super::stat::{
    BlobCache, CachedBlob, ChangedFile, MAX_STAT_BYTES, changed_file, is_binary, is_single_line,
    strip_common_lines,
};
use crate::{ChangeKind, Error, Oid, Result};

/// One added or deleted file from the walk.
struct Item {
    path: BString,
    id: Oid,
    mode: EntryMode,
    added: bool,
    emitted: bool,
}

/// The files of `changes`, with renames applied.
pub(crate) fn apply(
    changes: &[ChangeDetached],
    repo: &gix::Repository,
    blobs: Option<&BlobCache>,
) -> Result<Vec<ChangedFile>> {
    let mut items = Vec::new();
    let mut direct = Vec::new();
    for (index, change) in changes.iter().enumerate() {
        match change {
            ChangeDetached::Addition {
                location,
                entry_mode,
                id,
                ..
            } if !entry_mode.is_tree() => items.push(Item {
                path: location.clone(),
                id: *id,
                mode: *entry_mode,
                added: true,
                emitted: false,
            }),
            ChangeDetached::Deletion {
                location,
                entry_mode,
                id,
                ..
            } if !entry_mode.is_tree() => items.push(Item {
                path: location.clone(),
                id: *id,
                mode: *entry_mode,
                added: false,
                emitted: false,
            }),
            // Modifications, and the directory entries a file list never shows.
            _ => direct.push(index),
        }
    }

    // The tracker's order makes matching deterministic and independent of the
    // order in which the walk emitted the changes.
    items.sort_by(|a, b| {
        a.id.cmp(&b.id)
            .then_with(|| a.path.as_bstr().cmp(b.path.as_bstr()))
            .then_with(|| a.added.cmp(&b.added))
            .then_with(|| a.mode.value().cmp(&b.mode.value()))
    });

    let rewrites = gix::diff::Rewrites::default();
    debug_assert!(
        rewrites.copies.is_none(),
        "this pass detects renames, not copies"
    );
    let percentage = rewrites.percentage.unwrap_or(1.0);
    let track_empty = rewrites.track_empty;

    let mut pairs = Vec::new();
    let cap = max_checks(items.len(), rewrites.limit);
    match_by_identity(&mut items, track_empty, cap, &mut pairs);

    let sources = items
        .iter()
        .filter(|item| !item.added && !item.emitted)
        .count();
    let destinations = items
        .iter()
        .filter(|item| item.added && !item.emitted)
        .count();
    let limited = rewrites.limit != 0
        && (sources as u128) * (destinations as u128)
            > (rewrites.limit as u128) * (rewrites.limit as u128);
    if !limited && sources > 0 && destinations > 0 && percentage < 1.0 {
        let mut platform = repo
            .diff_resource_cache_for_tree_diff()
            .map_err(Error::gix)?;
        match_by_similarity(
            &mut items,
            track_empty,
            percentage,
            cap,
            repo,
            blobs,
            &mut platform,
            &mut pairs,
        )?;
    }

    let mut files = Vec::with_capacity(changes.len());
    for index in direct {
        if let Some(file) = changed_file(&changes[index]) {
            files.push(file);
        }
    }
    for &(dest, source) in &pairs {
        let (dest, source) = (&items[dest], &items[source]);
        files.push(ChangedFile {
            path: dest.path.to_str_lossy().into_owned(),
            old_path: Some(source.path.to_str_lossy().into_owned()),
            old_id: Some(source.id),
            new_id: Some(dest.id),
            old_mode: Some(source.mode),
            new_mode: Some(dest.mode),
            kind: ChangeKind::Renamed,
        });
    }
    for item in items.iter().filter(|item| !item.emitted) {
        files.push(ChangedFile {
            path: item.path.to_str_lossy().into_owned(),
            old_path: None,
            old_id: (!item.added).then_some(item.id),
            new_id: item.added.then_some(item.id),
            old_mode: (!item.added).then_some(item.mode),
            new_mode: item.added.then_some(item.mode),
            kind: if item.added {
                ChangeKind::Added
            } else {
                ChangeKind::Deleted
            },
        });
    }
    Ok(files)
}

/// The most similarity checks gix allows before it stops pairing a walk with
/// this many entries.
fn max_checks(items_len: usize, limit: usize) -> usize {
    if items_len < 100_000 {
        0
    } else {
        limit.saturating_pow(2)
    }
}

/// Pairs added files with the deleted file that has the same contents.
fn match_by_identity(
    items: &mut [Item],
    track_empty: bool,
    max_checks: usize,
    pairs: &mut Vec<(usize, usize)>,
) {
    let mut checks = 0usize;
    for dest in 0..items.len() {
        if !wants_destination(items, dest, track_empty) {
            continue;
        }
        let name = filename(&items[dest].path).to_owned();
        // Items are sorted by id, so the same-id candidates are contiguous.
        let start = items.partition_point(|item| item.id < items[dest].id);
        let end = items.partition_point(|item| item.id <= items[dest].id);
        let mut fallback = None;
        for source in start..end {
            if !is_source(items, source) || !compatible(items[source].mode, items[dest].mode) {
                continue;
            }
            checks += 1;
            if filename(&items[source].path) == name.as_slice() {
                fallback = Some(source);
                break;
            }
            fallback.get_or_insert(source);
        }
        if let Some(source) = fallback {
            items[dest].emitted = true;
            items[source].emitted = true;
            pairs.push((dest, source));
        }
        if max_checks != 0 && checks > max_checks {
            return;
        }
    }
}

/// Pairs the remaining added files with their most similar deleted file.
#[allow(clippy::too_many_arguments)]
fn match_by_similarity(
    items: &mut [Item],
    track_empty: bool,
    percentage: f32,
    max_checks: usize,
    repo: &gix::Repository,
    blobs: Option<&BlobCache>,
    platform: &mut gix::diff::blob::Platform,
    pairs: &mut Vec<(usize, usize)>,
) -> Result<()> {
    let source_candidates: Vec<usize> = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| (!item.added && !item.emitted && is_blob(item.mode)).then_some(i))
        .collect();
    if source_candidates.is_empty() {
        return Ok(());
    }

    let simple_platform = platform.filter.drivers().is_empty();
    let algorithm = platform.options.algorithm.unwrap_or(Algorithm::Histogram);
    let objects = CachedObjects::new(repo, blobs);
    let mut preps = vec![PreparedBlob::Unloaded; items.len()];
    let mut diff = Diff::default();
    let mut candidates = Vec::new();
    let mut checks = 0usize;

    for dest in 0..items.len() {
        if !wants_destination(items, dest, track_empty) || !is_blob(items[dest].mode) {
            continue;
        }
        if simple_platform {
            let PreparedBlob::Text { data, .. } = preps[dest].ensure(
                platform,
                &objects,
                items[dest].id,
                items[dest].mode.kind(),
                items[dest].path.as_bstr(),
                ResourceKind::NewOrDestination,
            )?
            else {
                continue;
            };
            let dst_len = data.len();
            if dst_len == 0 {
                continue;
            }

            let dest_name = filename(&items[dest].path).to_vec();
            candidates.clear();

            for (src_pos, &source) in source_candidates.iter().enumerate() {
                if items[source].emitted || !compatible(items[source].mode, items[dest].mode) {
                    continue;
                }
                checks += 1;
                let src_len = match preps[source].ensure_len(
                    platform,
                    &objects,
                    items[source].id,
                    items[source].mode.kind(),
                    items[source].path.as_bstr(),
                    ResourceKind::OldOrSource,
                )? {
                    Some(len) => len,
                    None => continue,
                };
                if !size_bound_passes(src_len, dst_len, percentage) {
                    continue;
                }

                // Lazily compute line profiles once for items that pass size bounds.
                let (dst_profile, src_profile) = ensure_pair_profiles(&mut preps, dest, source);
                let upper_sim = upper_similarity(src_len, dst_len, src_profile, dst_profile);
                if upper_sim < percentage {
                    continue;
                }
                let same_name = filename(&items[source].path) == dest_name.as_slice();
                candidates.push(SimilarityCandidate {
                    src_idx: source,
                    src_pos,
                    upper_sim,
                    same_name,
                });
            }

            if max_checks != 0 && checks > max_checks {
                return Ok(());
            }
            if candidates.is_empty() {
                continue;
            }

            sort_candidates(&mut candidates);
            let dst_data = preps[dest].data().expect("text dest");
            let mut best: Option<(usize, usize, f32, bool)> = None;

            for &cand in &candidates {
                if !candidate_can_beat(best, cand) {
                    break;
                }
                let src_data = preps[cand.src_idx].data().expect("text src");
                let sim = similarity_bytes(src_data, dst_data, algorithm, &mut diff);
                if sim < percentage {
                    continue;
                }
                if is_better_match(best, cand.src_pos, sim, cand.same_name) {
                    best = Some((cand.src_idx, cand.src_pos, sim, cand.same_name));
                }
            }

            if let Some((source, _, _, _)) = best {
                items[dest].emitted = true;
                items[source].emitted = true;
                pairs.push((dest, source));
            }
        } else {
            let dest_name = filename(&items[dest].path).to_vec();
            let mut best: Option<(usize, f32, bool)> = None;
            for &source in &source_candidates {
                if items[source].emitted || !compatible(items[source].mode, items[dest].mode) {
                    continue;
                }
                checks += 1;
                let Some(sim) = similarity_via_platform(
                    platform,
                    &objects,
                    items[dest].id,
                    items[dest].mode,
                    items[dest].path.as_bstr(),
                    items[source].id,
                    items[source].mode,
                    items[source].path.as_bstr(),
                    Some(percentage),
                    &mut diff,
                )?
                else {
                    continue;
                };
                if sim < percentage {
                    continue;
                }
                let same_name = filename(&items[source].path) == dest_name.as_slice();
                let better = best.is_none_or(|(_, best_similarity, best_same_name)| {
                    sim > best_similarity
                        || (sim == best_similarity && same_name && !best_same_name)
                });
                if better {
                    best = Some((source, sim, same_name));
                }
            }
            if let Some((source, _, _)) = best {
                items[dest].emitted = true;
                items[source].emitted = true;
                pairs.push((dest, source));
            }
            if max_checks != 0 && checks > max_checks {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Object lookup wrapper that serves decoded Added/Deleted blobs directly from
/// `BlobCache` when present and records newly read blobs so `commit_stat` or a
/// subsequent pass does not re-inflate or re-scan them.
pub(crate) struct CachedObjects<'a> {
    repo: &'a gix::Repository,
    blobs: Option<&'a BlobCache>,
    empty_blob: Oid,
}

impl<'a> CachedObjects<'a> {
    pub(crate) fn new(repo: &'a gix::Repository, blobs: Option<&'a BlobCache>) -> Self {
        Self {
            repo,
            blobs,
            empty_blob: Oid::empty_blob(repo.object_hash()),
        }
    }

    pub(crate) fn cached_data(&self, id: Oid) -> Option<CachedBlob> {
        self.blobs.and_then(|cache| cache.get(id))
    }
}

impl gix::objs::Find for CachedObjects<'_> {
    fn try_find<'a>(
        &self,
        id: &gix::oid,
        buffer: &'a mut Vec<u8>,
    ) -> std::result::Result<Option<gix::objs::Data<'a>>, gix::error::Error> {
        let owned = id.to_owned();
        if let Some(cache) = self.blobs
            && let Some(CachedBlob::Loaded(entry)) = cache.get(owned)
        {
            buffer.clear();
            buffer.extend_from_slice(&entry.data);
            return Ok(Some(gix::objs::Data::new(
                buffer,
                gix::objs::Kind::Blob,
                id.kind(),
            )));
        }
        let result = self.repo.objects.try_find(id, buffer)?;
        if let Some(ref data) = result
            && data.kind == gix::objs::Kind::Blob
            && owned != self.empty_blob
            && let Some(cache) = self.blobs
        {
            if (data.data.len() as u64) > MAX_STAT_BYTES {
                cache.insert_too_large(owned);
            } else {
                cache.insert_loaded(owned, Arc::from(data.data), is_binary(data.data), None);
            }
        }
        Ok(result)
    }
}

impl gix::objs::FindHeader for CachedObjects<'_> {
    fn try_header(
        &self,
        id: &gix::oid,
    ) -> std::result::Result<Option<gix::objs::Header>, gix::error::Error> {
        if let Some(cache) = self.blobs
            && let Some(CachedBlob::Loaded(entry)) = cache.get(id.to_owned())
        {
            return Ok(Some(gix::objs::Header {
                kind: gix::objs::Kind::Blob,
                size: entry.data.len() as u64,
            }));
        }
        self.repo.objects.try_header(id)
    }
}

#[derive(Clone)]
pub(crate) enum PreparedBlob {
    Unloaded,
    Skip,
    Text {
        data: Arc<[u8]>,
        profile: Option<LineProfile>,
    },
}

impl PreparedBlob {
    pub(crate) fn ensure(
        &mut self,
        platform: &mut gix::diff::blob::Platform,
        objects: &CachedObjects<'_>,
        id: Oid,
        mode: EntryKind,
        path: &BStr,
        kind: ResourceKind,
    ) -> Result<&Self> {
        if !matches!(self, Self::Unloaded) {
            return Ok(self);
        }
        let cached = if id == objects.empty_blob {
            None
        } else {
            objects.cached_data(id)
        };
        if let Some(CachedBlob::Loaded(entry)) = &cached
            && entry.binary
            && platform.filter.drivers().is_empty()
        {
            *self = Self::Skip;
            return Ok(self);
        }

        platform
            .set_resource(id, mode, path, kind, objects)
            .map_err(|err| Error::other(err.to_string()))?;
        let next = match platform.resource(kind).and_then(|r| r.data.as_slice()) {
            Some(buf) => {
                let data = match cached {
                    Some(CachedBlob::Loaded(entry)) if entry.data.as_ref() == buf => {
                        Arc::clone(&entry.data)
                    }
                    _ => Arc::from(buf),
                };
                Self::Text {
                    data,
                    profile: None,
                }
            }
            None => Self::Skip,
        };
        *self = next;
        Ok(self)
    }

    pub(crate) fn ensure_len(
        &mut self,
        platform: &mut gix::diff::blob::Platform,
        objects: &CachedObjects<'_>,
        id: Oid,
        mode: EntryKind,
        path: &BStr,
        kind: ResourceKind,
    ) -> Result<Option<usize>> {
        self.ensure(platform, objects, id, mode, path, kind)?;
        Ok(match self {
            Self::Text { data, .. } => Some(data.len()),
            Self::Unloaded | Self::Skip => None,
        })
    }

    pub(crate) fn data(&self) -> Option<&[u8]> {
        match self {
            Self::Text { data, .. } => Some(data),
            Self::Unloaded | Self::Skip => None,
        }
    }

    pub(crate) fn ensure_profile(&mut self) -> Option<&LineProfile> {
        match self {
            Self::Text { data, profile } => {
                Some(profile.get_or_insert_with(|| LineProfile::new(data)))
            }
            Self::Unloaded | Self::Skip => None,
        }
    }
}

pub(crate) fn ensure_pair_profiles(
    preps: &mut [PreparedBlob],
    dest_idx: usize,
    src_idx: usize,
) -> (&LineProfile, &LineProfile) {
    debug_assert_ne!(dest_idx, src_idx);
    if dest_idx < src_idx {
        let (left, right) = preps.split_at_mut(src_idx);
        let dst = left[dest_idx].ensure_profile().expect("text dest");
        let src = right[0].ensure_profile().expect("text src");
        (dst, src)
    } else {
        let (left, right) = preps.split_at_mut(dest_idx);
        let src = left[src_idx].ensure_profile().expect("text src");
        let dst = right[0].ensure_profile().expect("text dest");
        (dst, src)
    }
}

#[derive(Clone, Copy)]
struct LineCount {
    hash: u64,
    len: u32,
    count: u32,
}

/// Sorted multiset of `(hash, byte_len) -> count` for every line token in a
/// blob (matching `imara_diff`'s `\n`-terminated byte-line tokenization).
///
/// Any hash collision only merges distinct lines of equal length into a single
/// bucket with a larger count, so the multiset intersection sum can only
/// overestimate (never underestimate) the bytes that an edit script may match.
#[derive(Clone)]
pub(crate) struct LineProfile {
    entries: Vec<LineCount>,
}

impl LineProfile {
    pub(crate) fn new(data: &[u8]) -> Self {
        if data.is_empty() {
            return Self {
                entries: Vec::new(),
            };
        }
        let mut lines = Vec::with_capacity((data.len() / 32).max(8));
        let mut start = 0usize;
        while start < data.len() {
            let end = match data[start..].find_byte(b'\n') {
                Some(rel) => start + rel + 1,
                None => data.len(),
            };
            let line = &data[start..end];
            lines.push((hash_line(line), line.len() as u32));
            start = end;
        }
        lines.sort_unstable();

        let mut entries: Vec<LineCount> = Vec::with_capacity(lines.len());
        for (hash, len) in lines {
            if let Some(last) = entries.last_mut()
                && last.hash == hash
                && last.len == len
            {
                last.count += 1;
            } else {
                entries.push(LineCount {
                    hash,
                    len,
                    count: 1,
                });
            }
        }
        Self { entries }
    }

    pub(crate) fn matched_bytes_upper_bound(&self, other: &Self) -> usize {
        let a = &self.entries;
        let b = &other.entries;
        let mut i = 0usize;
        let mut j = 0usize;
        let mut matched = 0usize;
        while i < a.len() && j < b.len() {
            let ea = &a[i];
            let eb = &b[j];
            match (ea.hash, ea.len).cmp(&(eb.hash, eb.len)) {
                Ordering::Equal => {
                    matched += (ea.count.min(eb.count) as usize) * (ea.len as usize);
                    i += 1;
                    j += 1;
                }
                Ordering::Less => i += 1,
                Ordering::Greater => j += 1,
            }
        }
        matched
    }
}

#[inline]
fn hash_line(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0x517c_c1b7_2722_0a95 ^ (bytes.len() as u64);
    let (chunks, rem) = bytes.as_chunks::<8>();
    for chunk in chunks {
        let word = u64::from_le_bytes(*chunk);
        h = (h.rotate_left(5) ^ word).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    if !rem.is_empty() {
        let mut tail = 0u64;
        for (i, &b) in rem.iter().enumerate() {
            tail |= u64::from(b) << (i * 8);
        }
        h = (h.rotate_left(5) ^ tail).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    h ^ (h >> 32)
}

#[inline]
pub(crate) fn size_bound_passes(old_len: usize, new_len: usize, percentage: f32) -> bool {
    let longest = old_len.max(new_len);
    if longest == 0 {
        return false;
    }
    (old_len.min(new_len) as f32) / (longest as f32) >= percentage
}

#[inline]
pub(crate) fn upper_similarity(
    old_len: usize,
    new_len: usize,
    old_profile: &LineProfile,
    new_profile: &LineProfile,
) -> f32 {
    let longest = old_len.max(new_len);
    if longest == 0 {
        return 0.0;
    }
    let max_matched = old_profile
        .matched_bytes_upper_bound(new_profile)
        .min(old_len)
        .min(new_len);
    max_matched as f32 / longest as f32
}

#[derive(Clone, Copy)]
pub(crate) struct SimilarityCandidate {
    pub(crate) src_idx: usize,
    pub(crate) src_pos: usize,
    pub(crate) upper_sim: f32,
    pub(crate) same_name: bool,
}

pub(crate) fn sort_candidates(candidates: &mut [SimilarityCandidate]) {
    candidates.sort_unstable_by(|a, b| {
        b.upper_sim
            .total_cmp(&a.upper_sim)
            .then_with(|| b.same_name.cmp(&a.same_name))
            .then_with(|| a.src_pos.cmp(&b.src_pos))
    });
}

#[inline]
pub(crate) fn candidate_can_beat(
    best: Option<(usize, usize, f32, bool)>,
    cand: SimilarityCandidate,
) -> bool {
    let Some((_, best_pos, best_sim, best_same_name)) = best else {
        return true;
    };
    cand.upper_sim
        .total_cmp(&best_sim)
        .then_with(|| cand.same_name.cmp(&best_same_name))
        .then_with(|| best_pos.cmp(&cand.src_pos))
        .is_gt()
}

#[inline]
pub(crate) fn is_better_match(
    best: Option<(usize, usize, f32, bool)>,
    src_pos: usize,
    sim: f32,
    same_name: bool,
) -> bool {
    let Some((_, best_pos, best_sim, best_same_name)) = best else {
        return true;
    };
    sim.total_cmp(&best_sim)
        .then_with(|| same_name.cmp(&best_same_name))
        .then_with(|| best_pos.cmp(&src_pos))
        .is_gt()
}

/// Exact gix similarity score over two text blobs, reusing `diff` and skipping
/// identical leading/trailing lines.
pub(crate) fn similarity_bytes(
    old: &[u8],
    new: &[u8],
    algorithm: Algorithm,
    diff: &mut Diff,
) -> f32 {
    let longest = old.len().max(new.len());
    if longest == 0 {
        return 0.0;
    }
    if old == new {
        return old.len() as f32 / longest as f32;
    }
    let (old_mid, new_mid) = strip_common_lines(old, new);
    let removed_bytes = if old_mid.is_empty() {
        0
    } else if new_mid.is_empty() || (is_single_line(old_mid) && is_single_line(new_mid)) {
        old_mid.len()
    } else {
        let input = InternedInput::new(old_mid, new_mid);
        diff.compute_with(
            algorithm,
            &input.before,
            &input.after,
            input.interner.num_tokens(),
        );
        diff.hunks()
            .map(|hunk| {
                input.before[hunk.before.start as usize..hunk.before.end as usize]
                    .iter()
                    .map(|token| input.interner[*token].len())
                    .sum::<usize>()
            })
            .sum()
    };
    old.len().saturating_sub(removed_bytes) as f32 / longest as f32
}

/// Fallback similarity computation using full `gix::diff::blob::Platform` when
/// custom diff drivers or filters are configured.
#[allow(clippy::too_many_arguments)]
pub(crate) fn similarity_via_platform(
    platform: &mut gix::diff::blob::Platform,
    objects: &CachedObjects<'_>,
    dest_id: Oid,
    dest_mode: EntryMode,
    dest_path: &BStr,
    src_id: Oid,
    src_mode: EntryMode,
    src_path: &BStr,
    min_percentage: Option<f32>,
    diff: &mut Diff,
) -> Result<Option<f32>> {
    platform
        .set_resource(
            dest_id,
            dest_mode.kind(),
            dest_path,
            ResourceKind::NewOrDestination,
            objects,
        )
        .map_err(|error| Error::other(error.to_string()))?;
    platform
        .set_resource(
            src_id,
            src_mode.kind(),
            src_path,
            ResourceKind::OldOrSource,
            objects,
        )
        .map_err(|error| Error::other(error.to_string()))?;
    let prep = platform
        .prepare_diff()
        .map_err(|error| Error::other(error.to_string()))?;
    let Operation::InternalDiff { algorithm } = prep.operation else {
        return Ok(None);
    };

    let old = prep.old.data.as_slice().unwrap_or_default();
    let new = prep.new.data.as_slice().unwrap_or_default();
    if let Some(pct) = min_percentage
        && !size_bound_passes(old.len(), new.len(), pct)
    {
        return Ok(None);
    }
    Ok(Some(similarity_bytes(old, new, algorithm, diff)))
}

fn wants_destination(items: &[Item], index: usize, track_empty: bool) -> bool {
    let item = &items[index];
    item.added && !item.emitted && (track_empty || item.id != Oid::empty_blob(item.id.kind()))
}

fn is_source(items: &[Item], index: usize) -> bool {
    !items[index].added && !items[index].emitted
}

/// The entry kinds rewrite detection may pair, as gix decides it.
fn compatible(a: EntryMode, b: EntryMode) -> bool {
    use gix::object::tree::EntryKind::*;
    matches!(
        (a.kind(), b.kind()),
        (Blob | BlobExecutable, Blob | BlobExecutable) | (Link, Link) | (Commit, Commit)
    )
}

fn is_blob(mode: EntryMode) -> bool {
    mode.is_blob()
}

fn filename(path: &BString) -> &[u8] {
    path.rfind_byte(b'/')
        .map_or(path, |index| &path[index + 1..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRepo;

    fn numbered_lines(prefix: &str, count: usize, start: usize) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for i in start..(start + count) {
            let _ = writeln!(out, "{prefix} line {i:04} with deterministic payload text");
        }
        out
    }

    fn write_bytes(repo: &TestRepo, rel: &str, data: &[u8]) {
        let full = repo.path().join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(full, data).expect("write bytes");
    }

    fn reference_commit_changes(
        repo: &gix::Repository,
        commit: Oid,
    ) -> Vec<(String, Option<String>, String)> {
        let commit_obj = repo.find_commit(commit).expect("commit");
        let tree = commit_obj.tree().expect("tree");
        let parent_tree = commit_obj
            .parent_ids()
            .next()
            .map(|p| {
                repo.find_commit(p.detach())
                    .expect("parent")
                    .tree()
                    .expect("ptree")
            })
            .unwrap_or_else(|| repo.empty_tree());
        let changes = repo
            .diff_tree_to_tree(
                Some(&parent_tree),
                Some(&tree),
                Some(
                    gix::diff::Options::default()
                        .with_rewrites(Some(gix::diff::Rewrites::default())),
                ),
            )
            .expect("reference tree diff");
        let mut out: Vec<_> = changes
            .iter()
            .filter_map(changed_file)
            .map(|f| (f.path, f.old_path, format!("{:?}", f.kind)))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn line_profile_upper_bound_never_underestimates_true_similarity() {
        let pairs: &[(&[u8], &[u8])] = &[
            (b"a\nb\nc\n", b"a\nb\nd\n"),
            (b"a\na\nb\n", b"a\nb\nb\n"),
            (b"x\ny", b"x\ny\n"),
            (
                b"alpha\nbeta\ngamma\ndelta\n",
                b"delta\ngamma\nbeta\nalpha\n",
            ),
            (b"same\nsame\nsame\n", b"same\nsame\n"),
            (b"", b"a\n"),
        ];
        let mut diff = Diff::default();
        for &(old, new) in pairs {
            if old.is_empty() || new.is_empty() {
                continue;
            }
            let p_old = LineProfile::new(old);
            let p_new = LineProfile::new(new);
            let upper = upper_similarity(old.len(), new.len(), &p_old, &p_new);
            let exact = similarity_bytes(old, new, Algorithm::Histogram, &mut diff);
            assert!(
                upper >= exact,
                "upper bound {upper} < exact {exact} for old={old:?} new={new:?}"
            );
        }
    }

    #[test]
    fn renames_apply_matches_reference_across_similarities_binaries_and_huge_files() {
        let repo = TestRepo::new();

        // ~90% similarity rename.
        repo.write("orig_high.txt", &numbered_lines("high", 20, 0));
        // ~60% similarity rename.
        repo.write("orig_mid.txt", &numbered_lines("mid", 20, 0));
        // Exact 50% similarity boundary (10 shared lines out of 20).
        repo.write("orig_edge50.txt", &numbered_lines("edge", 20, 0));
        // Below 50% threshold (~30%): must stay Deleted + Added.
        repo.write("orig_low.txt", &numbered_lines("low", 20, 0));
        // Same-basename tie-breaker vs another identical/similar deleted file.
        let shared = numbered_lines("shared", 20, 0);
        repo.write("a/item.txt", &shared);
        repo.write("b/other.txt", &shared);
        // Binary exact rename + binary modified (must NOT match modified binary as rename).
        let bin_same: Vec<u8> = (0..128)
            .map(|i| if i % 7 == 0 { 0 } else { i as u8 })
            .collect();
        let bin_diff1: Vec<u8> = (0..128)
            .map(|i| if i % 5 == 0 { 0 } else { i as u8 })
            .collect();
        write_bytes(&repo, "bin_same.dat", &bin_same);
        write_bytes(&repo, "bin_mod.dat", &bin_diff1);
        // Huge > 4 MiB text file (over MAX_STAT_BYTES) that is renamed with minor edits.
        let mut huge = String::with_capacity(4 * 1024 * 1024 + 8192);
        let chunk = "huge_rename_payload_0123456789abcdefghijklmnopqrstuvwxyz\n";
        let mut idx = 0usize;
        while huge.len() <= 4 * 1024 * 1024 + 4096 {
            huge.push_str(&format!("{idx:06}:{chunk}"));
            idx += 1;
        }
        repo.write("huge_old.txt", &huge);
        repo.commit("base");

        // Remove all originals and add edited destinations.
        for rel in [
            "orig_high.txt",
            "orig_mid.txt",
            "orig_edge50.txt",
            "orig_low.txt",
            "a/item.txt",
            "b/other.txt",
            "bin_same.dat",
            "bin_mod.dat",
            "huge_old.txt",
        ] {
            std::fs::remove_file(repo.path().join(rel)).expect("remove source");
        }

        // High (~90%): replace last 2 lines of 20.
        repo.write(
            "dest_high.txt",
            &format!(
                "{}{}",
                numbered_lines("high", 18, 0),
                numbered_lines("HIGH_NEW", 2, 0)
            ),
        );
        // Mid (~60%): keep 12 of 20 lines, replace 8.
        repo.write(
            "dest_mid.txt",
            &format!(
                "{}{}",
                numbered_lines("mid", 12, 0),
                numbered_lines("MID_NEW", 8, 0)
            ),
        );
        // 50% boundary: keep first 10 of 20 lines, replace 10 (each line has equal byte len).
        repo.write(
            "dest_edge50.txt",
            &format!(
                "{}{}",
                numbered_lines("edge", 10, 0),
                numbered_lines("EDGE", 10, 10)
            ),
        );
        // Low (~30%): keep 6 of 20 lines, replace 14.
        repo.write(
            "dest_low.txt",
            &format!(
                "{}{}",
                numbered_lines("low", 6, 0),
                numbered_lines("LOW_NEW", 14, 0)
            ),
        );
        // Tie-breaker destination sharing basename with `a/item.txt`.
        repo.write("c/item.txt", &shared);
        // Competing non-exact destination for `b/other.txt` with minor edit.
        repo.write(
            "c/competing.txt",
            &format!(
                "{}{}",
                numbered_lines("shared", 19, 0),
                "shared tail change\n"
            ),
        );
        // Exact binary move vs modified binary.
        write_bytes(&repo, "bin_moved.dat", &bin_same);
        let bin_diff2: Vec<u8> = (0..128)
            .map(|i| {
                if i % 5 == 0 {
                    0
                } else {
                    (i as u8).wrapping_add(1)
                }
            })
            .collect();
        write_bytes(&repo, "bin_mod_moved.dat", &bin_diff2);
        // Huge renamed with a few added lines.
        repo.write("huge_new.txt", &format!("{huge}added tail line\n"));

        let commit = repo.commit("renames at multiple similarities");

        let handle = repo.open();
        let gix_repo = handle.local();
        let expected = reference_commit_changes(&gix_repo, commit);

        // Check both cold `commit_changes` and after `commit_stat` (shared BlobCache, including TooLarge).
        let stat = handle.commit_stat(commit).expect("commit stat");
        let huge_stat = stat
            .files
            .iter()
            .find(|f| f.path == "huge_new.txt")
            .expect("huge_new in stat");
        assert!(huge_stat.binary, "huge file exceeds MAX_STAT_BYTES");

        let changes = handle.commit_changes(commit).expect("commit changes");
        let mut actual: Vec<_> = changes
            .iter()
            .map(|f| (f.path.clone(), f.old_path.clone(), format!("{:?}", f.kind)))
            .collect();
        actual.sort();
        assert_eq!(actual, expected);

        // Fresh handle with `commit_changes` running BEFORE `commit_stat` must match too.
        let fresh = repo.open();
        let changes_first = fresh.commit_changes(commit).expect("commit changes first");
        assert_eq!(changes_first, changes);
        let stat_second = fresh.commit_stat(commit).expect("commit stat second");
        assert_eq!(stat_second, stat);

        // Explicitly verify expected semantic classifications.
        let find = |p: &str| changes.iter().find(|c| c.path == p).expect(p);
        assert_eq!(
            find("dest_high.txt").old_path.as_deref(),
            Some("orig_high.txt")
        );
        assert_eq!(
            find("dest_mid.txt").old_path.as_deref(),
            Some("orig_mid.txt")
        );
        assert_eq!(
            find("dest_edge50.txt").old_path.as_deref(),
            Some("orig_edge50.txt")
        );
        assert_eq!(find("dest_low.txt").kind, ChangeKind::Added);
        assert_eq!(find("c/item.txt").old_path.as_deref(), Some("a/item.txt"));
        assert_eq!(
            find("bin_moved.dat").old_path.as_deref(),
            Some("bin_same.dat")
        );
        assert_eq!(find("bin_mod_moved.dat").kind, ChangeKind::Added);
        assert_eq!(
            find("huge_new.txt").old_path.as_deref(),
            Some("huge_old.txt")
        );
    }
}
