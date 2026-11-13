//! Rename detection over a first-parent tree change list.
//!
//! A commit's file list and its line counts share one tree walk, and only the
//! file list needs to know about renames. Replaying the walk through gix's
//! rewrite tracker would pair every added and deleted file, which on a large
//! merge means tens of thousands of blob diffs.
//!
//! This pass pairs the same entries with the same rules: identical blob ids
//! first, then the best similarity above the same threshold gix uses. The score
//! is gix's, but a pair whose sizes cannot reach the threshold is skipped before
//! its contents are diffed, which removes most of the work without changing
//! which pairs match.

use gix::bstr::{BString, ByteSlice};
use gix::diff::blob::platform::prepare_diff::Operation;
use gix::diff::blob::{Diff, InternedInput, ResourceKind};
use gix::object::tree::EntryMode;
use gix::object::tree::diff::ChangeDetached;

use super::stat::{ChangedFile, changed_file};
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
    if !limited {
        let mut platform = repo
            .diff_resource_cache_for_tree_diff()
            .map_err(Error::gix)?;
        match_by_similarity(
            &mut items,
            track_empty,
            percentage,
            cap,
            repo,
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
fn match_by_similarity(
    items: &mut [Item],
    track_empty: bool,
    percentage: f32,
    max_checks: usize,
    repo: &gix::Repository,
    platform: &mut gix::diff::blob::Platform,
    pairs: &mut Vec<(usize, usize)>,
) -> Result<()> {
    let mut checks = 0usize;
    for dest in 0..items.len() {
        if !wants_destination(items, dest, track_empty) {
            continue;
        }
        let dest_name = filename(&items[dest].path).to_owned();
        let mut best: Option<(usize, f32, bool)> = None;
        for source in 0..items.len() {
            if !is_source(items, source) || !compatible(items[source].mode, items[dest].mode) {
                continue;
            }
            if !is_blob(items[dest].mode) || !is_blob(items[source].mode) {
                continue;
            }
            checks += 1;
            let Some(similarity) =
                similarity(platform, repo, &items[dest], &items[source], percentage)?
            else {
                continue;
            };
            if similarity < percentage {
                continue;
            }
            let same_name = filename(&items[source].path) == dest_name.as_slice();
            let better = best.is_none_or(|(_, best_similarity, best_same_name)| {
                similarity > best_similarity
                    || (similarity == best_similarity && same_name && !best_same_name)
            });
            if better {
                best = Some((source, similarity, same_name));
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
    Ok(())
}

/// How similar two files are, or `None` when their sizes rule it out or one of
/// them is binary.
fn similarity(
    platform: &mut gix::diff::blob::Platform,
    repo: &gix::Repository,
    dest: &Item,
    source: &Item,
    percentage: f32,
) -> Result<Option<f32>> {
    platform
        .set_resource(
            dest.id,
            dest.mode.kind(),
            dest.path.as_bstr(),
            ResourceKind::NewOrDestination,
            &repo.objects,
        )
        .map_err(|error| Error::other(error.to_string()))?;
    platform
        .set_resource(
            source.id,
            source.mode.kind(),
            source.path.as_bstr(),
            ResourceKind::OldOrSource,
            &repo.objects,
        )
        .map_err(|error| Error::other(error.to_string()))?;
    let prep = platform
        .prepare_diff()
        .map_err(|error| Error::other(error.to_string()))?;
    let Operation::InternalDiff { algorithm } = prep.operation else {
        return Ok(None);
    };

    let old_len = prep.old.data.as_slice().unwrap_or_default().len();
    let new_len = prep.new.data.as_slice().unwrap_or_default().len();
    let longest = old_len.max(new_len);
    if longest == 0 {
        return Ok(None);
    }
    // Matched bytes can never exceed the shorter side, so a pair whose shorter
    // side is below the threshold cannot reach it: skip the diff entirely.
    if (old_len.min(new_len) as f64) / (longest as f64) < f64::from(percentage) {
        return Ok(None);
    }

    let input = InternedInput::new(prep.old.intern_source(), prep.new.intern_source());
    let diff = Diff::compute(algorithm, &input);
    let removed: usize = diff
        .hunks()
        .map(|hunk| {
            input.before[hunk.before.start as usize..hunk.before.end as usize]
                .iter()
                .map(|token| input.interner[*token].len())
                .sum::<usize>()
        })
        .sum();
    Ok(Some(
        old_len.saturating_sub(removed) as f32 / longest as f32,
    ))
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
