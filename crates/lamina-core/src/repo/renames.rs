//! Rename detection over a tree change list.
//!
//! Two callers need the same pairing: a commit's file list ([`apply`]), and
//! path-limited history (`source_of`), which follows one file across the rename
//! that produced it. A commit's file list and its line counts share one tree
//! walk, and only the file list needs to know about renames. Replaying the walk
//! through gix's rewrite tracker would pair every added and deleted file, which
//! on a large merge means tens of thousands of blob diffs.
//!
//! This module pairs the same entries with the same rules: identical blob ids
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
use gix::diff::tree::visit::Relation;
use gix::object::tree::diff::ChangeDetached;
use gix::object::tree::{EntryKind, EntryMode};

use super::stat::{
    BlobCache, CachedBlob, ChangedFile, MAX_STAT_BYTES, changed_file, is_binary, is_single_line,
    strip_common_lines,
};
use crate::{ChangeKind, Error, Oid, Result};

/// How a walk's changes become rename items.
#[derive(Clone, Copy)]
enum Walk {
    /// A commit's file list only shows files; a rewritten directory is not a
    /// row of its own, and its entries carry no relation to anything.
    Files,
    /// Path history needs directory entries too: gix pairs renamed directories
    /// first and then propagates the pair to their children.
    Tree,
}

/// One added or deleted entry from the walk.
struct Item {
    path: BString,
    id: Oid,
    mode: EntryMode,
    added: bool,
    /// Set only for entries of a rewritten directory, which is what lets a
    /// directory rename carry its children along.
    relation: Option<Relation>,
    emitted: bool,
}

impl Item {
    /// The item `change` contributes, unless the walk has no use for it.
    fn from_change(change: &ChangeDetached, walk: Walk) -> Option<Self> {
        let (path, relation, mode, id, added) = match change {
            ChangeDetached::Addition {
                location,
                relation,
                entry_mode,
                id,
                ..
            } => (location, relation, entry_mode, id, true),
            ChangeDetached::Deletion {
                location,
                relation,
                entry_mode,
                id,
                ..
            } => (location, relation, entry_mode, id, false),
            ChangeDetached::Modification { .. } | ChangeDetached::Rewrite { .. } => return None,
        };
        let keep = match walk {
            Walk::Files => !mode.is_tree(),
            // A tree without a relation is an ordinary added or removed
            // directory, not part of a rewrite; gix's tracker skips it too.
            Walk::Tree => !mode.is_tree() || relation.is_some(),
        };
        if !keep {
            return None;
        }
        let relation = match walk {
            Walk::Files => None,
            Walk::Tree => *relation,
        };
        Some(Self {
            path: path.clone(),
            id: *id,
            mode: *mode,
            added,
            relation,
            emitted: false,
        })
    }
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
        match Item::from_change(change, Walk::Files) {
            Some(item) => items.push(item),
            // Modifications, and the directory entries a file list never shows.
            None => direct.push(index),
        }
    }

    // The tracker's order makes matching deterministic and independent of the
    // order in which the walk emitted the changes.
    sort_items(&mut items);

    let rewrites = gix::diff::Rewrites::default();
    debug_assert!(
        rewrites.copies.is_none(),
        "this pass detects renames, not copies"
    );
    let track_empty = rewrites.track_empty;
    let cap = max_checks(items.len(), rewrites.limit);

    let mut pairs = Vec::new();
    match_by_identity(&mut items, None, track_empty, false, cap, &mut pairs);

    let sources = items
        .iter()
        .filter(|item| !item.added && !item.emitted)
        .count();
    let destinations = items
        .iter()
        .filter(|item| item.added && !item.emitted)
        .count();
    let percentage = rewrites.percentage.unwrap_or(1.0);
    let limited = rewrites.limit != 0
        && (sources as u128) * (destinations as u128)
            > (rewrites.limit as u128) * (rewrites.limit as u128);
    if !limited && sources > 0 && destinations > 0 && percentage < 1.0 {
        match_by_similarity(
            &mut items,
            repo,
            blobs,
            track_empty,
            percentage,
            cap,
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

/// The parent-side path of the rename that produced `target_path`, if any.
///
/// Path history follows one file backwards, so the walk stops as soon as its
/// own destination is resolved instead of pairing the whole change list.
pub(crate) fn source_of(
    changes: &[ChangeDetached],
    repo: &gix::Repository,
    target_path: &[u8],
) -> Result<Option<Vec<u8>>> {
    let mut items = Vec::new();
    let mut has_deletion = false;
    let mut has_target = false;
    for change in changes {
        let Some(item) = Item::from_change(change, Walk::Tree) else {
            continue;
        };
        if item.added && item.path.as_slice() == target_path {
            has_target = true;
        }
        has_deletion |= !item.added;
        items.push(item);
    }
    if !has_target || !has_deletion {
        return Ok(None);
    }

    sort_items(&mut items);

    let Some(target) = items
        .iter()
        .position(|item| item.added && item.path.as_slice() == target_path)
    else {
        return Ok(None);
    };

    let rewrites = gix::diff::Rewrites::default();
    let cap = max_checks(items.len(), rewrites.limit);
    let track_empty = rewrites.track_empty;
    let mut pairs = Vec::new();

    // Pass 1a: parent directories matched by identity (and their children).
    if match_by_identity(&mut items, Some(target), track_empty, true, cap, &mut pairs) {
        return Ok(Some(paired_source(&items, &pairs, target)));
    }
    if items[target].emitted {
        return Ok(None);
    }

    // Pass 1b: all additions vs deletions matched by exact object id.
    if match_by_identity(
        &mut items,
        Some(target),
        track_empty,
        false,
        cap,
        &mut pairs,
    ) {
        return Ok(Some(paired_source(&items, &pairs, target)));
    }
    if items[target].emitted {
        return Ok(None);
    }

    let Some(percentage) = rewrites.percentage.filter(|&percentage| percentage < 1.0) else {
        return Ok(None);
    };
    let Some(source) = match_similarity_target(
        &mut items,
        repo,
        track_empty,
        percentage,
        cap,
        rewrites.limit,
        target,
    )?
    else {
        return Ok(None);
    };
    Ok(Some(items[source].path.clone().into()))
}

/// The source path of `target`, which the identity pass just paired.
fn paired_source(items: &[Item], pairs: &[(usize, usize)], target: usize) -> Vec<u8> {
    let &(_, source) = pairs
        .iter()
        .rev()
        .find(|(dest, _)| *dest == target)
        .expect("target was paired in this pass");
    items[source].path.clone().into()
}

/// The tracker's order: id first, so same-id candidates are contiguous, then
/// every observable field so matching does not depend on the walk's order.
fn sort_items(items: &mut [Item]) {
    items.sort_by(|a, b| {
        a.id.cmp(&b.id)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.added.cmp(&b.added))
            .then_with(|| a.relation.cmp(&b.relation))
            .then_with(|| a.mode.cmp(&b.mode))
    });
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

/// Pairs each eligible destination with the same-id source gix would choose.
///
/// `parents_only` restricts the pass to directories, which is how gix pairs a
/// renamed directory before carrying its children along. When `target` is set,
/// the pass returns as soon as that destination is paired.
fn match_by_identity(
    items: &mut [Item],
    target: Option<usize>,
    track_empty: bool,
    parents_only: bool,
    max_checks: usize,
    pairs: &mut Vec<(usize, usize)>,
) -> bool {
    // Items only stop being sources while matching, so this list is a superset
    // of the sources any later destination can see.
    let sources: Vec<usize> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| (!item.added && !item.emitted).then_some(index))
        .collect();
    if sources.is_empty() {
        return false;
    }

    let mut checks = 0usize;
    for dest in 0..items.len() {
        if !is_identity_destination(&items[dest], track_empty, parents_only) {
            continue;
        }
        let dest_id = items[dest].id;
        let start = sources.partition_point(|&index| items[index].id < dest_id);
        let end = sources.partition_point(|&index| items[index].id <= dest_id);
        let mut matched = None;
        if start < end {
            let dest_name = filename(&items[dest].path);
            for &source in &sources[start..end] {
                checks += 1;
                if source == dest
                    || items[source].emitted
                    || !compatible(items[source].mode, items[dest].mode)
                {
                    continue;
                }
                if filename(&items[source].path) == dest_name {
                    matched = Some(source);
                    break;
                }
                matched.get_or_insert(source);
            }
        }
        if max_checks != 0 && checks > max_checks {
            return false;
        }
        let Some(source) = matched else {
            continue;
        };

        let relations = items[source].relation.zip(items[dest].relation);
        items[dest].emitted = true;
        items[source].emitted = true;
        pairs.push((dest, source));
        if Some(dest) == target {
            return true;
        }

        // A renamed directory carries identical children with it; gix treats
        // those children as renamed without another identity lookup.
        if let Some((Relation::Parent(src_parent), Relation::Parent(dst_parent))) = relations {
            let children = |parent: u32| -> Vec<usize> {
                items
                    .iter()
                    .enumerate()
                    .filter_map(|(index, item)| {
                        (!item.emitted
                            && matches!(item.relation, Some(Relation::ChildOfParent(id)) if id == parent))
                            .then_some(index)
                    })
                    .collect()
            };
            let src_children = children(src_parent);
            let dst_children = children(dst_parent);
            for (child_src, child_dest) in src_children.into_iter().zip(dst_children) {
                if items[child_src].id == items[child_dest].id
                    && filename(&items[child_src].path) == filename(&items[child_dest].path)
                {
                    items[child_src].emitted = true;
                    items[child_dest].emitted = true;
                    pairs.push((child_dest, child_src));
                    if Some(child_dest) == target {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Pairs the remaining added files with their most similar deleted file.
#[allow(clippy::too_many_arguments)]
fn match_by_similarity(
    items: &mut [Item],
    repo: &gix::Repository,
    blobs: Option<&BlobCache>,
    track_empty: bool,
    percentage: f32,
    max_checks: usize,
    pairs: &mut Vec<(usize, usize)>,
) -> Result<()> {
    let mut matcher = SimilarityMatcher::new(items, repo, blobs, percentage, max_checks)?;
    if matcher.sources.is_empty() {
        return Ok(());
    }

    if matcher.simple {
        let mut candidates = Vec::new();
        for dest in 0..items.len() {
            if !is_similarity_destination(&items[dest], track_empty) {
                continue;
            }
            let Some(dst_len) = matcher.prepare_destination(items, dest)? else {
                continue;
            };
            matcher.fill_candidates(items, dest, dst_len, &mut candidates)?;
            if matcher.budget_spent() {
                return Ok(());
            }
            if candidates.is_empty() {
                continue;
            }
            if let Some((source, _, _, _)) = matcher.best_match(items, dest, &mut candidates, &[]) {
                items[dest].emitted = true;
                items[source].emitted = true;
                pairs.push((dest, source));
            }
        }
        return Ok(());
    }

    for dest in 0..items.len() {
        if !is_similarity_destination(&items[dest], track_empty) {
            continue;
        }
        let best = matcher.best_via_platform(items, dest, Some(percentage))?;
        if matcher.budget_spent() {
            return Ok(());
        }
        if let Some((source, _, _)) = best {
            items[dest].emitted = true;
            items[source].emitted = true;
            pairs.push((dest, source));
        }
    }
    Ok(())
}

/// Similarity pass for one tracked destination.
///
/// The destination is checked first: if no source can reach the threshold, the
/// pass returns without touching the destinations before it. Otherwise the
/// pass runs in gix's destination order up to `target`, so the sources earlier
/// destinations consume are the same ones the tracker would have consumed.
#[allow(clippy::too_many_arguments)]
fn match_similarity_target(
    items: &mut [Item],
    repo: &gix::Repository,
    track_empty: bool,
    percentage: f32,
    max_checks: usize,
    limit: usize,
    target: usize,
) -> Result<Option<usize>> {
    if !is_similarity_destination(&items[target], track_empty) {
        return Ok(None);
    }
    let source_count = items
        .iter()
        .filter(|item| !item.added && !item.emitted)
        .count();
    if source_count == 0 {
        return Ok(None);
    }
    let destinations = items
        .iter()
        .filter(|item| item.added && !item.emitted)
        .count();
    if limit != 0
        && (source_count as u128) * (destinations as u128) > (limit as u128).saturating_pow(2)
    {
        return Ok(None);
    }

    let mut matcher = SimilarityMatcher::new(items, repo, None, percentage, max_checks)?;

    if matcher.simple {
        let Some(target_len) = matcher.prepare_destination(items, target)? else {
            return Ok(None);
        };
        let mut target_candidates = Vec::new();
        matcher.fill_candidates(items, target, target_len, &mut target_candidates)?;
        if target_candidates.is_empty() {
            return Ok(None);
        }
        let mut cached = Vec::new();
        if !matcher.target_reachable(target, &mut target_candidates, &mut cached) {
            return Ok(None);
        }

        let mut candidates = Vec::new();
        for dest in 0..=target {
            if !is_similarity_destination(&items[dest], track_empty) {
                continue;
            }
            if dest == target {
                matcher.checks += matcher.compatible_source_count(items, dest);
                if matcher.budget_spent() {
                    return Ok(None);
                }
                return Ok(matcher
                    .best_match(items, dest, &mut target_candidates, &cached)
                    .map(|(source, _, _, _)| source));
            }
            let Some(dst_len) = matcher.prepare_destination(items, dest)? else {
                continue;
            };
            matcher.fill_candidates(items, dest, dst_len, &mut candidates)?;
            if matcher.budget_spent() {
                return Ok(None);
            }
            if candidates.is_empty() {
                continue;
            }
            if let Some((source, _, _, _)) = matcher.best_match(items, dest, &mut candidates, &[]) {
                items[dest].emitted = true;
                items[source].emitted = true;
            }
        }
        return Ok(None);
    }

    if !matcher.any_source_reaches(items, target)? {
        return Ok(None);
    }
    for dest in 0..=target {
        if !is_similarity_destination(&items[dest], track_empty) {
            continue;
        }
        let best = matcher.best_via_platform(items, dest, Some(percentage))?;
        if matcher.budget_spent() {
            return Ok(None);
        }
        if let Some((source, _, _)) = best {
            items[dest].emitted = true;
            items[source].emitted = true;
            if dest == target {
                return Ok(Some(source));
            }
        }
    }
    Ok(None)
}

/// State the similarity passes share: the diff platform, cached object reads,
/// prepared blobs, and the check budget gix uses to bound pathological walks.
struct SimilarityMatcher<'repo> {
    platform: gix::diff::blob::Platform,
    objects: CachedObjects<'repo>,
    preps: Vec<PreparedBlob>,
    diff: Diff,
    algorithm: Algorithm,
    percentage: f32,
    max_checks: usize,
    checks: usize,
    sources: Vec<usize>,
    simple: bool,
}

impl<'repo> SimilarityMatcher<'repo> {
    fn new(
        items: &[Item],
        repo: &'repo gix::Repository,
        blobs: Option<&'repo BlobCache>,
        percentage: f32,
        max_checks: usize,
    ) -> Result<Self> {
        let mut platform = repo
            .diff_resource_cache_for_tree_diff()
            .map_err(Error::gix)?;
        // Similarity only compares what git itself would diff; leaving this on
        // would turn an external diff driver into an unscoreable candidate.
        platform
            .options
            .skip_internal_diff_if_external_is_configured = false;
        let simple = platform.filter.drivers().is_empty();
        let algorithm = platform.options.algorithm.unwrap_or(Algorithm::Histogram);
        let sources = items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| (!item.added && !item.emitted).then_some(index))
            .collect();
        Ok(Self {
            platform,
            objects: CachedObjects::new(repo, blobs),
            preps: vec![PreparedBlob::Unloaded; items.len()],
            diff: Diff::default(),
            algorithm,
            percentage,
            max_checks,
            checks: 0,
            sources,
            simple,
        })
    }

    fn budget_spent(&self) -> bool {
        self.max_checks != 0 && self.checks > self.max_checks
    }

    /// The destination's text length, or `None` when it cannot take part in a
    /// similarity diff.
    fn prepare_destination(&mut self, items: &[Item], dest: usize) -> Result<Option<usize>> {
        Ok(self.preps[dest]
            .ensure_len(
                &mut self.platform,
                &self.objects,
                items[dest].id,
                items[dest].mode.kind(),
                items[dest].path.as_bstr(),
                ResourceKind::NewOrDestination,
            )?
            .filter(|len| *len > 0))
    }

    /// Fill `out` with the sources that can still reach the threshold, in the
    /// tracker's source order.
    fn fill_candidates(
        &mut self,
        items: &[Item],
        dest: usize,
        dst_len: usize,
        out: &mut Vec<SimilarityCandidate>,
    ) -> Result<()> {
        out.clear();
        let dest_name = filename(&items[dest].path).to_vec();
        for (src_pos, &source) in self.sources.iter().enumerate() {
            if items[source].emitted || !compatible(items[source].mode, items[dest].mode) {
                continue;
            }
            self.checks += 1;
            let Some(src_len) = self.preps[source].ensure_len(
                &mut self.platform,
                &self.objects,
                items[source].id,
                items[source].mode.kind(),
                items[source].path.as_bstr(),
                ResourceKind::OldOrSource,
            )?
            else {
                continue;
            };
            if !size_bound_passes(src_len, dst_len, self.percentage) {
                continue;
            }

            // Lazily compute line profiles once for items that pass size bounds.
            let (dst_profile, src_profile) = ensure_pair_profiles(&mut self.preps, dest, source);
            let upper_sim = upper_similarity(src_len, dst_len, src_profile, dst_profile);
            if upper_sim < self.percentage {
                continue;
            }
            out.push(SimilarityCandidate {
                src_idx: source,
                src_pos,
                upper_sim,
                same_name: filename(&items[source].path) == dest_name.as_slice(),
            });
        }
        Ok(())
    }

    /// The best candidate that reaches the threshold. `cache` holds scores the
    /// tracked destination already computed, so they are not recomputed here.
    fn best_match(
        &mut self,
        items: &[Item],
        dest: usize,
        candidates: &mut [SimilarityCandidate],
        cache: &[(usize, f32)],
    ) -> Option<(usize, usize, f32, bool)> {
        sort_candidates(candidates);
        let dst_data = self.preps[dest].data().expect("text dest");
        let mut best: Option<(usize, usize, f32, bool)> = None;

        for &cand in candidates.iter() {
            if items[cand.src_idx].emitted {
                continue;
            }
            if !candidate_can_beat(best, cand) {
                break;
            }
            let sim = match cache.iter().find(|(source, _)| *source == cand.src_idx) {
                Some(&(_, sim)) => sim,
                None => {
                    let src_data = self.preps[cand.src_idx].data().expect("text src");
                    similarity_bytes(src_data, dst_data, self.algorithm, &mut self.diff)
                }
            };
            if sim < self.percentage {
                continue;
            }
            if is_better_match(best, cand.src_pos, sim, cand.same_name) {
                best = Some((cand.src_idx, cand.src_pos, sim, cand.same_name));
            }
        }
        best
    }

    /// Whether the tracked destination has any source reaching the threshold,
    /// recording the scores it computed so the full pass can reuse them.
    fn target_reachable(
        &mut self,
        target: usize,
        candidates: &mut [SimilarityCandidate],
        cache: &mut Vec<(usize, f32)>,
    ) -> bool {
        sort_candidates(candidates);
        let target_data = self.preps[target].data().expect("text target");
        for &cand in candidates.iter() {
            let src_data = self.preps[cand.src_idx].data().expect("text src");
            let sim = similarity_bytes(src_data, target_data, self.algorithm, &mut self.diff);
            cache.push((cand.src_idx, sim));
            if sim >= self.percentage {
                return true;
            }
        }
        false
    }

    fn compatible_source_count(&self, items: &[Item], dest: usize) -> usize {
        self.sources
            .iter()
            .filter(|&&source| {
                !items[source].emitted && compatible(items[source].mode, items[dest].mode)
            })
            .count()
    }

    fn any_source_reaches(&mut self, items: &[Item], dest: usize) -> Result<bool> {
        for &source in &self.sources {
            if !compatible(items[source].mode, items[dest].mode) {
                continue;
            }
            if let Some(sim) = similarity_via_platform(
                &mut self.platform,
                &self.objects,
                items[dest].id,
                items[dest].mode,
                items[dest].path.as_bstr(),
                items[source].id,
                items[source].mode,
                items[source].path.as_bstr(),
                Some(self.percentage),
                &mut self.diff,
            )? && sim >= self.percentage
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The fallback when custom diff drivers or filters are configured: each
    /// pair goes through the platform, which may refuse to diff it at all.
    fn best_via_platform(
        &mut self,
        items: &[Item],
        dest: usize,
        min_percentage: Option<f32>,
    ) -> Result<Option<(usize, f32, bool)>> {
        let dest_name = filename(&items[dest].path).to_vec();
        let mut best: Option<(usize, f32, bool)> = None;
        for &source in &self.sources {
            if source == dest
                || items[source].emitted
                || !compatible(items[source].mode, items[dest].mode)
            {
                continue;
            }
            self.checks += 1;
            let Some(sim) = similarity_via_platform(
                &mut self.platform,
                &self.objects,
                items[dest].id,
                items[dest].mode,
                items[dest].path.as_bstr(),
                items[source].id,
                items[source].mode,
                items[source].path.as_bstr(),
                Some(best.map_or_else(
                    || min_percentage.unwrap_or(self.percentage),
                    |(_, best, _)| best,
                )),
                &mut self.diff,
            )?
            else {
                continue;
            };
            if sim < self.percentage {
                continue;
            }
            let same_name = filename(&items[source].path) == dest_name.as_slice();
            let better = best.is_none_or(|(_, best_similarity, best_same_name)| {
                sim > best_similarity || (sim == best_similarity && same_name && !best_same_name)
            });
            if better {
                best = Some((source, sim, same_name));
            }
        }
        Ok(best)
    }
}

/// Object lookup wrapper that serves decoded Added/Deleted blobs directly from
/// `BlobCache` when present and records newly read blobs so `commit_stat` or a
/// subsequent pass does not re-inflate or re-scan them.
struct CachedObjects<'a> {
    repo: &'a gix::Repository,
    blobs: Option<&'a BlobCache>,
    empty_blob: Oid,
}

impl<'a> CachedObjects<'a> {
    fn new(repo: &'a gix::Repository, blobs: Option<&'a BlobCache>) -> Self {
        Self {
            repo,
            blobs,
            empty_blob: Oid::empty_blob(repo.object_hash()),
        }
    }

    fn cached_data(&self, id: Oid) -> Option<CachedBlob> {
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
enum PreparedBlob {
    Unloaded,
    Skip,
    Text {
        data: Arc<[u8]>,
        profile: Option<LineProfile>,
    },
}

impl PreparedBlob {
    fn ensure(
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

    fn ensure_len(
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

    fn data(&self) -> Option<&[u8]> {
        match self {
            Self::Text { data, .. } => Some(data),
            Self::Unloaded | Self::Skip => None,
        }
    }

    fn ensure_profile(&mut self) -> Option<&LineProfile> {
        match self {
            Self::Text { data, profile } => {
                Some(profile.get_or_insert_with(|| LineProfile::new(data)))
            }
            Self::Unloaded | Self::Skip => None,
        }
    }
}

fn ensure_pair_profiles(
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
struct LineProfile {
    entries: Vec<LineCount>,
}

impl LineProfile {
    fn new(data: &[u8]) -> Self {
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

    fn matched_bytes_upper_bound(&self, other: &Self) -> usize {
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
fn size_bound_passes(old_len: usize, new_len: usize, percentage: f32) -> bool {
    let longest = old_len.max(new_len);
    if longest == 0 {
        return false;
    }
    (old_len.min(new_len) as f32) / (longest as f32) >= percentage
}

#[inline]
fn upper_similarity(
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
struct SimilarityCandidate {
    src_idx: usize,
    src_pos: usize,
    upper_sim: f32,
    same_name: bool,
}

fn sort_candidates(candidates: &mut [SimilarityCandidate]) {
    candidates.sort_unstable_by(|a, b| {
        b.upper_sim
            .total_cmp(&a.upper_sim)
            .then_with(|| b.same_name.cmp(&a.same_name))
            .then_with(|| a.src_pos.cmp(&b.src_pos))
    });
}

#[inline]
fn candidate_can_beat(best: Option<(usize, usize, f32, bool)>, cand: SimilarityCandidate) -> bool {
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
fn is_better_match(
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
fn similarity_bytes(old: &[u8], new: &[u8], algorithm: Algorithm, diff: &mut Diff) -> f32 {
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
fn similarity_via_platform(
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

/// Destinations the exact-id pass may pair. The parent pass only looks at
/// directories; the general pass also pairs the empty files of a rewritten
/// directory, which gix keeps even when empty.
fn is_identity_destination(item: &Item, track_empty: bool, parents_only: bool) -> bool {
    if !item.added || item.emitted {
        return false;
    }
    if parents_only {
        return matches!(item.relation, Some(Relation::Parent(_)));
    }
    track_empty
        || matches!(item.relation, Some(Relation::ChildOfParent(_)))
        || item.id != Oid::empty_blob(item.id.kind())
}

/// Destinations the similarity pass may pair: an added, unpaired file that the
/// tracker still considers, including empty files inside a rewritten directory.
fn is_similarity_destination(item: &Item, track_empty: bool) -> bool {
    item.added
        && !item.emitted
        && item.mode.is_blob()
        && (track_empty
            || matches!(item.relation, Some(Relation::ChildOfParent(_)))
            || item.id != Oid::empty_blob(item.id.kind()))
}

/// The entry kinds rewrite detection may pair, as gix decides it.
fn compatible(a: EntryMode, b: EntryMode) -> bool {
    use gix::object::tree::EntryKind::*;
    matches!(
        (a.kind(), b.kind()),
        (Blob | BlobExecutable, Blob | BlobExecutable)
            | (Link, Link)
            | (Tree, Tree)
            | (Commit, Commit)
    )
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
