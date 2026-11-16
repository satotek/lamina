//! Three-way comparison of repository trees.
//!
//! Given a base commit and two descendants, the entries are the union of the
//! base→left and base→right tree diffs. Each path is classified by which side
//! changed it, and the same status vocabulary a folder comparison uses says
//! whether the path exists on one side only or differs between the sides.
//! Renames are treated as a delete plus an add, so a renamed path appears under
//! both names rather than being paired.
//!
//! When git is new enough, `git merge-tree --write-tree` marks the paths that
//! would really conflict in a merge of the two sides; it writes objects but
//! never touches a ref, the index or the working tree.

use std::collections::BTreeSet;

use gix::object::tree::EntryKind;

use super::Repo;
use super::stat::tree_changes;
use crate::compare::{
    FolderCompareCounts, FolderEntryKind, FolderEntryStatus, FolderSideInfo, ThreeWayTreeChange,
    ThreeWayTreeCompare, ThreeWayTreeEntry,
};
use crate::git_cli::Git;
use crate::{Error, Oid, Result};

/// The roles a three-commit comparison starts with: the base in the middle,
/// and the two remaining commits on the sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreeWayRoles {
    pub left: Oid,
    pub base: Oid,
    pub right: Oid,
}

/// One tree entry as the three sides hold it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SideState {
    mode: u16,
    oid: Oid,
    kind: FolderEntryKind,
}

impl Repo {
    /// Compares the trees of `left`, `base` and `right`.
    ///
    /// The entry list covers every path at least one side changed relative to
    /// `base`; untouched files are left out, like a comparison of two commits
    /// leaves out the files they agree on.
    pub fn three_way_tree(&self, left: Oid, base: Oid, right: Oid) -> Result<ThreeWayTreeCompare> {
        let repo = self.local();
        let left_tree = commit_tree(&repo, left)?;
        let base_tree = commit_tree(&repo, base)?;
        let right_tree = commit_tree(&repo, right)?;

        let mut paths = BTreeSet::new();
        for change in tree_changes(&repo, &base_tree, &left_tree)? {
            if let Some(file) = super::stat::changed_file(&change) {
                paths.insert(file.path);
            }
        }
        for change in tree_changes(&repo, &base_tree, &right_tree)? {
            if let Some(file) = super::stat::changed_file(&change) {
                paths.insert(file.path);
            }
        }

        let mut entries = Vec::with_capacity(paths.len());
        for path in paths {
            let left_state = entry_state(&left_tree, &path)?;
            let base_state = entry_state(&base_tree, &path)?;
            let right_state = entry_state(&right_tree, &path)?;

            let left_differs = left_state != base_state;
            let right_differs = right_state != base_state;
            let change = match (left_differs, right_differs) {
                (true, false) => ThreeWayTreeChange::LeftChanged,
                (false, true) => ThreeWayTreeChange::RightChanged,
                (true, true) if left_state == right_state => ThreeWayTreeChange::BothAgree,
                (true, true) => ThreeWayTreeChange::BothDisagree,
                // A path in the union always differs on at least one side.
                (false, false) => continue,
            };

            let states = [left_state, base_state, right_state];
            let present: Vec<usize> = states
                .iter()
                .enumerate()
                .filter_map(|(index, state)| state.is_some().then_some(index))
                .collect();
            let primary = states
                .iter()
                .flatten()
                .next()
                .map(|state| state.kind)
                .unwrap_or(FolderEntryKind::Other);
            let kinds_match = states.iter().flatten().all(|state| state.kind == primary);

            let status = if !kinds_match {
                FolderEntryStatus::TypeMismatch
            } else if present.len() == 1 {
                match present[0] {
                    0 => FolderEntryStatus::OnlyLeft,
                    1 => FolderEntryStatus::OnlyMiddle,
                    _ => FolderEntryStatus::OnlyRight,
                }
            } else if states.windows(2).all(|pair| pair[0] == pair[1]) {
                FolderEntryStatus::Identical
            } else {
                FolderEntryStatus::Different
            };

            let sides = states
                .iter()
                .map(|state| {
                    state.map(|state| FolderSideInfo {
                        kind: state.kind,
                        size: blob_size(&repo, state),
                    })
                })
                .collect();

            entries.push(ThreeWayTreeEntry {
                relative_path: path,
                kind: primary,
                status,
                change,
                conflicting: false,
                sides,
            });
        }

        let mut merge_tree_checked = false;
        if entries
            .iter()
            .any(|entry| entry.change == ThreeWayTreeChange::BothDisagree)
        {
            // An old git, a bare repository or a failed probe leaves the
            // tree-diff classification in place without the real-conflict
            // marking; a comparison must not fail because of it.
            if let Ok(Some(conflicts)) = self.merge_tree_conflicts(left, right) {
                merge_tree_checked = true;
                for entry in &mut entries {
                    entry.conflicting = conflicts.contains(&entry.relative_path);
                }
            }
        }

        let mut counts = FolderCompareCounts::default();
        for entry in &entries {
            counts.record(entry.status);
        }

        Ok(ThreeWayTreeCompare {
            left,
            base,
            right,
            entries,
            counts,
            merge_tree_checked,
        })
    }

    /// The blob contents of `path` in the three commits, with `None` where the
    /// path is absent or not a blob.
    pub fn three_way_blobs(
        &self,
        left: Oid,
        base: Oid,
        right: Oid,
        path: &str,
    ) -> Result<[Option<Vec<u8>>; 3]> {
        Ok([
            self.file_at_commit(left, path)?,
            self.file_at_commit(base, path)?,
            self.file_at_commit(right, path)?,
        ])
    }

    /// The paths `git merge-tree --write-tree` reports as conflicts for a
    /// merge of `ours` and `theirs`, or `None` when git is too old or there is
    /// no working tree to run it in.
    pub fn merge_tree_conflicts(&self, ours: Oid, theirs: Oid) -> Result<Option<Vec<String>>> {
        let Some(workdir) = self.workdir() else {
            return Ok(None);
        };
        Git::new(workdir.to_path_buf()).merge_tree_conflicts(ours, theirs)
    }

    /// Picks the base and the two sides for a three-commit comparison.
    ///
    /// A commit both others descend from is the base. Without such a commit,
    /// the oldest commit date is, and the remaining two keep the older one on
    /// the left so the reading order matches history order.
    pub fn three_way_roles(&self, commits: [Oid; 3]) -> Result<ThreeWayRoles> {
        let [a, b, c] = commits;
        let times = [
            self.commit_time(a)?,
            self.commit_time(b)?,
            self.commit_time(c)?,
        ];

        let mut base = None;
        for (candidate, descendants) in [(a, [b, c]), (b, [a, c]), (c, [a, b])] {
            if self.is_ancestor(candidate, descendants[0])?
                && self.is_ancestor(candidate, descendants[1])?
            {
                base = Some(candidate);
                break;
            }
        }
        let base = match base {
            Some(base) => base,
            None => {
                let mut oldest = (times[0], a);
                for index in [1usize, 2] {
                    let candidate = (times[index], commits[index]);
                    if candidate < oldest {
                        oldest = candidate;
                    }
                }
                oldest.1
            }
        };

        let mut rest: Vec<(i64, Oid)> = commits
            .into_iter()
            .zip(times)
            .filter(|(oid, _)| *oid != base)
            .map(|(oid, time)| (time, oid))
            .collect();
        rest.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        let [(_, left), (_, right), ..] = rest.as_slice() else {
            return Err(Error::other(
                "a three-way comparison needs three distinct commits",
            ));
        };
        Ok(ThreeWayRoles {
            left: *left,
            base,
            right: *right,
        })
    }
}

fn commit_tree<'repo>(repo: &'repo gix::Repository, commit: Oid) -> Result<gix::Tree<'repo>> {
    repo.find_commit(commit)
        .map_err(Error::gix)?
        .tree()
        .map_err(Error::gix)
}

fn entry_state(tree: &gix::Tree<'_>, path: &str) -> Result<Option<SideState>> {
    let Some(entry) = tree.lookup_entry_by_path(path).map_err(Error::gix)? else {
        return Ok(None);
    };
    let mode = entry.mode();
    Ok(Some(SideState {
        mode: mode.value(),
        oid: entry.object_id(),
        kind: entry_kind(mode.kind()),
    }))
}

fn entry_kind(kind: EntryKind) -> FolderEntryKind {
    match kind {
        EntryKind::Tree => FolderEntryKind::Directory,
        EntryKind::Link => FolderEntryKind::Symlink,
        EntryKind::Blob | EntryKind::BlobExecutable => FolderEntryKind::File,
        // A submodule is a commit pointer, which has no file content to
        // compare; the folder vocabulary has no submodule kind.
        EntryKind::Commit => FolderEntryKind::Other,
    }
}

fn blob_size(repo: &gix::Repository, state: SideState) -> Option<u64> {
    if state.kind == FolderEntryKind::Directory {
        return None;
    }
    match repo.try_find_header(state.oid) {
        Ok(Some(header)) if header.kind() == gix::objs::Kind::Blob => Some(header.size()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::{FolderEntryStatus, ThreeWayTreeChange, compare_three_sides};
    use crate::test_support::TestRepo;

    /// A base with one file per classification, plus files each side adds and
    /// removes on its own.
    fn diverged_fixture() -> (TestRepo, Oid, Oid, Oid) {
        let repo = TestRepo::new();
        repo.write("left-only.txt", "base\n");
        repo.write("right-only.txt", "base\n");
        repo.write("both-same.txt", "base\n");
        repo.write("conflict.txt", "base\n");
        repo.write("removed-both.txt", "gone\n");
        repo.write("removed-left.txt", "kept right\n");
        let base = repo.commit("base");

        repo.git(&["checkout", "-q", "-b", "left"]);
        repo.write("left-only.txt", "left\n");
        repo.write("both-same.txt", "changed the same\n");
        repo.write("conflict.txt", "left\n");
        repo.write("left-added.txt", "new\n");
        repo.git(&["rm", "-q", "removed-both.txt", "removed-left.txt"]);
        let left = repo.commit("left changes");

        repo.git(&["checkout", "-q", "main"]);
        repo.git(&["checkout", "-q", "-b", "right"]);
        repo.write("right-only.txt", "right\n");
        repo.write("both-same.txt", "changed the same\n");
        repo.write("conflict.txt", "right\n");
        repo.write("right-added.txt", "new\n");
        repo.git(&["rm", "-q", "removed-both.txt"]);
        let right = repo.commit("right changes");

        (repo, left, base, right)
    }

    fn entry<'a>(compare: &'a ThreeWayTreeCompare, path: &str) -> &'a ThreeWayTreeEntry {
        compare
            .entries
            .iter()
            .find(|entry| entry.relative_path == path)
            .unwrap_or_else(|| panic!("{path} is in the comparison: {compare:?}"))
    }

    #[test]
    fn classifies_each_file_relative_to_the_base() {
        let (repo, left, base, right) = diverged_fixture();
        let compare = repo
            .open()
            .three_way_tree(left, base, right)
            .expect("three-way tree");

        assert_eq!(
            entry(&compare, "left-only.txt").change,
            ThreeWayTreeChange::LeftChanged
        );
        assert_eq!(
            entry(&compare, "right-only.txt").change,
            ThreeWayTreeChange::RightChanged
        );
        assert_eq!(
            entry(&compare, "both-same.txt").change,
            ThreeWayTreeChange::BothAgree
        );
        assert_eq!(
            entry(&compare, "conflict.txt").change,
            ThreeWayTreeChange::BothDisagree
        );

        let added = entry(&compare, "left-added.txt");
        assert_eq!(added.change, ThreeWayTreeChange::LeftChanged);
        assert_eq!(added.status, FolderEntryStatus::OnlyLeft);
        assert_eq!(added.kind, FolderEntryKind::File);
        assert!(added.sides[0].is_some());
        assert!(added.sides[1].is_none() && added.sides[2].is_none());
        assert_eq!(
            entry(&compare, "right-added.txt").status,
            FolderEntryStatus::OnlyRight
        );

        assert_eq!(
            entry(&compare, "removed-left.txt").change,
            ThreeWayTreeChange::LeftChanged,
            "deleting on the left only is a left-side change"
        );
        assert_eq!(
            entry(&compare, "removed-both.txt").status,
            FolderEntryStatus::OnlyMiddle
        );
        assert_eq!(
            entry(&compare, "removed-both.txt").change,
            ThreeWayTreeChange::BothAgree,
            "both sides deleted the same path"
        );
        assert_eq!(compare.counts.total, compare.entries.len());
    }

    #[test]
    fn merge_tree_marks_real_conflicts_when_git_supports_it() {
        let (repo, left, base, right) = diverged_fixture();
        let compare = repo
            .open()
            .three_way_tree(left, base, right)
            .expect("three-way tree");
        let version = repo.git_handle().version().expect("git version");
        if version < (2, 38, 0) {
            assert!(
                !compare.merge_tree_checked,
                "an old git falls back to the tree-diff classification"
            );
            return;
        }
        assert!(compare.merge_tree_checked, "merge-tree answered");
        assert!(
            entry(&compare, "conflict.txt").conflicting,
            "both sides changed conflict.txt and disagree"
        );
        assert!(
            !entry(&compare, "both-same.txt").conflicting,
            "both sides made the same change, which merges cleanly"
        );
        assert!(
            !entry(&compare, "left-only.txt").conflicting,
            "a one-sided change is not a merge conflict"
        );
    }

    #[test]
    fn three_way_blobs_feed_the_in_memory_file_compare() {
        let (repo, left, base, right) = diverged_fixture();
        let opened = repo.open();
        let [left_bytes, base_bytes, right_bytes] = opened
            .three_way_blobs(left, base, right, "conflict.txt")
            .expect("blobs");
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let compare = compare_three_sides(
            crate::compare::ThreeWaySide::new("left", Some("conflict.txt".into()), left_bytes),
            crate::compare::ThreeWaySide::new("base", Some("conflict.txt".into()), base_bytes),
            crate::compare::ThreeWaySide::new("right", Some("conflict.txt".into()), right_bytes),
            &cancel,
        )
        .expect("not cancelled");
        assert_eq!(
            compare
                .change_regions()
                .map(|region| region.kind)
                .collect::<Vec<_>>(),
            vec![crate::compare::ThreeWayDiffKind::BothDisagree]
        );
    }

    #[test]
    fn the_ancestor_commit_is_the_base() {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\n");
        let root = repo.commit("root");
        repo.git(&["checkout", "-q", "-b", "left"]);
        repo.write("left.txt", "left\n");
        let left = commit_at(&repo, "left", 1_700_000_100);
        repo.git(&["checkout", "-q", "main"]);
        repo.git(&["checkout", "-q", "-b", "right"]);
        repo.write("right.txt", "right\n");
        let right = commit_at(&repo, "right", 1_700_000_200);

        let roles = repo
            .open()
            .three_way_roles([right, root, left])
            .expect("roles");
        assert_eq!(roles.base, root, "the common ancestor is the base");
        assert_eq!(roles.left, left, "the older side is on the left");
        assert_eq!(roles.right, right);
    }

    #[test]
    fn unrelated_commits_use_the_oldest_as_the_base() {
        let repo = TestRepo::new();
        repo.write("one.txt", "one\n");
        let oldest = commit_at(&repo, "oldest", 1_700_000_100);
        repo.git(&["checkout", "-q", "--orphan", "middle"]);
        repo.git(&["rm", "-q", "-rf", "."]);
        repo.write("two.txt", "two\n");
        repo.git(&["add", "-A"]);
        let middle = commit_at(&repo, "middle", 1_700_000_200);
        repo.git(&["checkout", "-q", "--orphan", "newest"]);
        repo.git(&["rm", "-q", "-rf", "."]);
        repo.write("three.txt", "three\n");
        repo.git(&["add", "-A"]);
        let newest = commit_at(&repo, "newest", 1_700_000_300);

        let roles = repo
            .open()
            .three_way_roles([newest, oldest, middle])
            .expect("roles");
        assert_eq!(roles.base, oldest, "no ancestry means the oldest date wins");
        assert_eq!(roles.left, middle);
        assert_eq!(roles.right, newest);
    }

    fn commit_at(repo: &TestRepo, message: &str, seconds: u64) -> Oid {
        repo.git(&["add", "-A"]);
        let date = format!("{seconds} +0000");
        let output = repo
            .command("git")
            .args(["commit", "-q", "-m", message])
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .expect("run git commit");
        assert!(
            output.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        repo.head()
    }
}
