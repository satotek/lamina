//! Three-way comparison of repository trees.
//!
//! Given a base commit and two descendants, the entries are the union of the
//! base→left and base→right tree diffs. Each path is classified by which side
//! changed it, and the same status vocabulary a folder comparison uses says
//! whether the path exists on one side only or differs between the sides.
//! Renames are treated as a delete plus an add, so a renamed path appears under
//! both names rather than being paired.
//!
//! A side can be a commit, the index or the working tree. Every side is first
//! listed as [`RepoCompareFile`]s carrying a content identity, so the same
//! comparison works for any mix of the three and the decision is plain data.
//!
//! When every side is a commit and git is new enough, `git merge-tree
//! --write-tree` marks the paths that would really conflict in a merge of the
//! two sides; it writes objects but never touches a ref, the index or the
//! working tree.

use std::collections::BTreeSet;
use std::fs::File;
use std::path::Path;

use gix::bstr::ByteSlice;
use gix::index::entry::Stage;
use gix::object::tree::EntryKind;
use gix::objs::TreeRefIter;

use super::Repo;
use crate::compare::{
    FolderEntryKind, RepoCompareFile, RepoCompareSide, ThreeWayTreeChange, ThreeWayTreeCompare,
    compare_three_repo_sides,
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

impl Repo {
    /// Compares the trees of `left`, `base` and `right`.
    ///
    /// The entry list covers every path at least one side changed relative to
    /// `base`; untouched files are left out, like a comparison of two commits
    /// leaves out the files they agree on.
    pub fn three_way_tree(&self, left: Oid, base: Oid, right: Oid) -> Result<ThreeWayTreeCompare> {
        let mut compare = self.three_way_side_tree([
            RepoCompareSide::Commit(left),
            RepoCompareSide::Commit(base),
            RepoCompareSide::Commit(right),
        ])?;

        if compare
            .entries
            .iter()
            .any(|entry| entry.change == ThreeWayTreeChange::BothDisagree)
        {
            // An old git, a bare repository or a failed probe leaves the
            // tree-diff classification in place without the real-conflict
            // marking; a comparison must not fail because of it.
            if let Ok(Some(conflicts)) = self.merge_tree_conflicts(left, right) {
                compare.merge_tree_checked = true;
                for entry in &mut compare.entries {
                    entry.conflicting = conflicts.contains(&entry.relative_path);
                }
            }
        }

        Ok(compare)
    }

    /// Compares any mix of commits, the index and the working tree.
    pub fn three_way_side_tree(&self, sides: [RepoCompareSide; 3]) -> Result<ThreeWayTreeCompare> {
        let [left, middle, right] = sides;
        let left_files = self.compare_side_files(left)?;
        let middle_files = self.compare_side_files(middle)?;
        let right_files = self.compare_side_files(right)?;
        Ok(compare_three_repo_sides(
            left,
            &left_files,
            middle,
            &middle_files,
            right,
            &right_files,
        ))
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

    /// The same file from three sides, each side reading it under its own
    /// path when a history walk followed renames.
    pub fn three_way_side_blobs(
        &self,
        sides: [RepoCompareSide; 3],
        paths: [&str; 3],
    ) -> Result<[Option<Vec<u8>>; 3]> {
        Ok([
            self.compare_side_blob(sides[0], paths[0])?,
            self.compare_side_blob(sides[1], paths[1])?,
            self.compare_side_blob(sides[2], paths[2])?,
        ])
    }

    /// One file of a comparison side.
    pub fn compare_side_blob(&self, side: RepoCompareSide, path: &str) -> Result<Option<Vec<u8>>> {
        match side {
            RepoCompareSide::Commit(commit) => self.file_at_commit(commit, path),
            RepoCompareSide::Index => self.file_in_index(path),
            RepoCompareSide::Worktree => self.file_in_worktree(path),
        }
    }

    /// Every file one side holds, with a content identity.
    ///
    /// The working-tree side is the tracked files on disk plus the untracked
    /// files git does not ignore; files deleted from the working tree are
    /// absent from it.
    pub fn compare_side_files(&self, side: RepoCompareSide) -> Result<Vec<RepoCompareFile>> {
        match side {
            RepoCompareSide::Commit(commit) => self.commit_side_files(commit),
            RepoCompareSide::Index => self.index_side_files(),
            RepoCompareSide::Worktree => self.worktree_side_files(),
        }
    }

    fn commit_side_files(&self, commit: Oid) -> Result<Vec<RepoCompareFile>> {
        let repo = self.local();
        let tree = repo
            .find_commit(commit)
            .map_err(Error::gix)?
            .tree()
            .map_err(Error::gix)?;
        let mut files = Vec::new();
        collect_tree_files(&repo, &tree, "", &mut files)?;
        Ok(files)
    }

    fn index_side_files(&self) -> Result<Vec<RepoCompareFile>> {
        let repo = self.local();
        let index = repo.index_or_empty().map_err(Error::gix)?;
        let mut files = Vec::new();
        for entry in index.entries() {
            if entry.stage() != Stage::Unconflicted
                || entry.mode.is_submodule()
                || entry.mode.is_sparse()
            {
                continue;
            }
            let path = entry.path(&index).to_str_lossy().into_owned();
            let kind = match entry.mode.to_tree_entry_mode().map(|mode| mode.kind()) {
                Some(EntryKind::Link) => FolderEntryKind::Symlink,
                Some(EntryKind::Tree | EntryKind::Commit) | None => continue,
                Some(EntryKind::Blob | EntryKind::BlobExecutable) => FolderEntryKind::File,
            };
            files.push(RepoCompareFile {
                path,
                kind,
                size: blob_size(&repo, entry.id),
                identity: entry.id.to_string(),
            });
        }
        Ok(files)
    }

    fn worktree_side_files(&self) -> Result<Vec<RepoCompareFile>> {
        let Some(workdir) = self.workdir() else {
            return Ok(Vec::new());
        };
        let repo = self.local();
        let index = repo.index_or_empty().map_err(Error::gix)?;
        let mut paths: BTreeSet<String> = index
            .entries()
            .iter()
            .filter(|entry| {
                entry.stage() == Stage::Unconflicted
                    && !entry.mode.is_submodule()
                    && !entry.mode.is_sparse()
                    && matches!(
                        entry.mode.to_tree_entry_mode().map(|mode| mode.kind()),
                        Some(EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link)
                    )
            })
            .map(|entry| entry.path(&index).to_str_lossy().into_owned())
            .collect();

        // Untracked files belong to the working-tree side alone. `status` is
        // the one walk that knows the ignore rules, and it also sees files
        // staged for deletion that are still on disk.
        if let Ok(status) = self.status() {
            for change in status.unstaged.iter().chain(status.staged.iter()) {
                paths.insert(change.path.clone());
            }
        }

        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            let absolute = workdir.join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
            let Ok(metadata) = std::fs::symlink_metadata(&absolute) else {
                // Deleted in the working tree, so this side does not hold it.
                continue;
            };
            if metadata.file_type().is_symlink() {
                let target = std::fs::read_link(&absolute)?;
                let bytes = target.to_string_lossy().into_owned().into_bytes();
                files.push(RepoCompareFile {
                    kind: FolderEntryKind::Symlink,
                    size: Some(bytes.len() as u64),
                    identity: blob_identity(&repo, &bytes)?,
                    path,
                });
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let size = metadata.len();
            files.push(RepoCompareFile {
                kind: FolderEntryKind::File,
                size: Some(size),
                identity: worktree_identity(&repo, &absolute, size)?,
                path,
            });
        }
        Ok(files)
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

/// Walks a commit tree recursively, collecting every blob and symlink.
fn collect_tree_files(
    repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    prefix: &str,
    out: &mut Vec<RepoCompareFile>,
) -> Result<()> {
    for entry in TreeRefIter::from_bytes(&tree.data, tree.id.kind()) {
        let entry = entry.map_err(Error::gix)?;
        let name = entry.filename.to_str_lossy();
        let path = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        let mode = entry.mode;
        match mode.kind() {
            EntryKind::Tree => {
                let child = repo.find_tree(entry.oid).map_err(Error::gix)?;
                collect_tree_files(repo, &child, &path, out)?;
            }
            EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link => {
                let kind = if mode.kind() == EntryKind::Link {
                    FolderEntryKind::Symlink
                } else {
                    FolderEntryKind::File
                };
                out.push(RepoCompareFile {
                    path,
                    kind,
                    size: blob_size(repo, entry.oid.to_owned()),
                    identity: entry.oid.to_string(),
                });
            }
            // A submodule is a commit pointer, not file content.
            EntryKind::Commit => {}
        }
    }
    Ok(())
}

/// Blob size from the object header, which avoids inflating the content.
fn blob_size(repo: &gix::Repository, id: Oid) -> Option<u64> {
    match repo.try_find_header(id) {
        Ok(Some(header)) if header.kind() == gix::objs::Kind::Blob => Some(header.size()),
        _ => None,
    }
}

/// The object id of an in-memory blob, for a symlink target or other small
/// content whose bytes are already known.
fn blob_identity(repo: &gix::Repository, bytes: &[u8]) -> Result<String> {
    let id = gix::objs::compute_hash(repo.object_hash(), gix::objs::Kind::Blob, bytes)
        .map_err(Error::gix)?;
    Ok(id.to_string())
}

/// A working-tree file's identity in the same vocabulary a commit and the
/// index use: the object id git would give the blob.
///
/// Hashing the file is what makes a raw worktree byte equal to the blob it
/// matches, without keeping every compared file in memory.
fn worktree_identity(repo: &gix::Repository, path: &Path, size: u64) -> Result<String> {
    let mut file = File::open(path)?;
    let interrupt = std::sync::atomic::AtomicBool::new(false);
    let id = gix::objs::compute_stream_hash(
        repo.object_hash(),
        gix::objs::Kind::Blob,
        &mut file,
        size,
        &mut gix::progress::Discard,
        &interrupt,
    )
    .map_err(Error::gix)?;
    Ok(id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::{
        FolderEntryStatus, RepoCompareSide, ThreeWayDiffKind, ThreeWayPeerStatus,
        ThreeWayTreeChange, ThreeWayTreeEntry, compare_three_sides,
    };

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
    fn peer_status_is_computed_for_every_tree_entry() {
        let (repo, left, base, right) = diverged_fixture();
        let compare = repo
            .open()
            .three_way_tree(left, base, right)
            .expect("three-way tree");

        assert_eq!(
            entry(&compare, "left-only.txt").peer,
            ThreeWayPeerStatus::LeftMiddleDiffer
        );
        assert_eq!(
            entry(&compare, "right-only.txt").peer,
            ThreeWayPeerStatus::MiddleRightDiffer
        );
        assert_eq!(
            entry(&compare, "both-same.txt").peer,
            ThreeWayPeerStatus::AllDifferent,
            "the middle differs from both outer sides, which agree with each other"
        );
        assert_eq!(
            entry(&compare, "conflict.txt").peer,
            ThreeWayPeerStatus::AllDifferent
        );
        assert_eq!(
            entry(&compare, "left-added.txt").peer,
            ThreeWayPeerStatus::MissingSomeSide
        );
        assert_eq!(
            entry(&compare, "removed-both.txt").peer,
            ThreeWayPeerStatus::MissingSomeSide
        );
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
            vec![ThreeWayDiffKind::BothDisagree]
        );
    }

    #[test]
    fn the_index_and_working_tree_are_comparison_sides() {
        let repo = TestRepo::new();
        repo.write("modified.txt", "base\n");
        repo.write("staged.txt", "base\n");
        repo.write("committed.txt", "base\n");
        let base = repo.commit("base");

        repo.write("modified.txt", "changed on disk\n");
        repo.write("staged.txt", "changed in the index\n");
        repo.git(&["add", "staged.txt"]);
        repo.write("untracked.txt", "new file\n");

        let opened = repo.open();
        let index = opened
            .compare_side_files(RepoCompareSide::Index)
            .expect("index files");
        let worktree = opened
            .compare_side_files(RepoCompareSide::Worktree)
            .expect("worktree files");
        let index_paths: Vec<&str> = index.iter().map(|file| file.path.as_str()).collect();
        let worktree_paths: Vec<&str> = worktree.iter().map(|file| file.path.as_str()).collect();
        assert!(index_paths.contains(&"staged.txt"));
        assert!(
            !index_paths.contains(&"untracked.txt"),
            "an untracked file is not in the index"
        );
        assert!(
            worktree_paths.contains(&"untracked.txt"),
            "the working tree holds the untracked file"
        );

        let compare = opened
            .three_way_side_tree([
                RepoCompareSide::Commit(base),
                RepoCompareSide::Index,
                RepoCompareSide::Worktree,
            ])
            .expect("three-way side tree");

        assert_eq!(
            entry(&compare, "staged.txt").peer,
            ThreeWayPeerStatus::LeftMiddleDiffer,
            "the index staged the change, so commit and working tree differ from it"
        );
        assert_eq!(
            entry(&compare, "modified.txt").peer,
            ThreeWayPeerStatus::MiddleRightDiffer,
            "only the working tree changed since the index"
        );
        assert_eq!(
            entry(&compare, "untracked.txt").status,
            FolderEntryStatus::OnlyRight
        );
        assert_eq!(
            entry(&compare, "untracked.txt").peer,
            ThreeWayPeerStatus::MissingSomeSide
        );

        let blob = opened
            .compare_side_blob(RepoCompareSide::Index, "staged.txt")
            .expect("index blob");
        assert_eq!(blob.as_deref(), Some(b"changed in the index\n".as_slice()));
        let blob = opened
            .compare_side_blob(RepoCompareSide::Worktree, "modified.txt")
            .expect("worktree blob");
        assert_eq!(blob.as_deref(), Some(b"changed on disk\n".as_slice()));
    }

    #[test]
    fn two_repo_sides_classify_each_file() {
        let (repo, left, _base, right) = diverged_fixture();
        let opened = repo.open();
        let left_files = opened
            .compare_side_files(RepoCompareSide::Commit(left))
            .expect("left files");
        let right_files = opened
            .compare_side_files(RepoCompareSide::Commit(right))
            .expect("right files");
        let compare = crate::compare::compare_two_repo_sides(
            RepoCompareSide::Commit(left),
            &left_files,
            RepoCompareSide::Commit(right),
            &right_files,
        );
        let status = |path: &str| {
            compare
                .entries
                .iter()
                .find(|entry| entry.relative_path == path)
                .map(|entry| entry.status)
        };
        assert_eq!(status("left-only.txt"), Some(FolderEntryStatus::Different));
        assert_eq!(status("left-added.txt"), Some(FolderEntryStatus::OnlyLeft));
        assert_eq!(
            status("right-added.txt"),
            Some(FolderEntryStatus::OnlyRight)
        );
        assert_eq!(
            status("removed-left.txt"),
            Some(FolderEntryStatus::OnlyRight),
            "the left deleted it, so only the right holds it"
        );
        assert!(
            status("removed-both.txt").is_none(),
            "both deleted it, so no side holds it"
        );
    }

    #[test]
    fn file_history_paths_feed_three_versions_across_a_rename() {
        let repo = TestRepo::new();
        repo.write("old.txt", "one\n");
        let first = repo.commit("Add old.txt");
        repo.git(&["mv", "old.txt", "new.txt"]);
        let second = repo.commit("Rename to new.txt");
        repo.write("new.txt", "three\n");
        let third = repo.commit("Edit new.txt");

        let opened = repo.open();
        let entries: Vec<(Oid, String)> = opened
            .path_history_entries("new.txt")
            .expect("path history")
            .map(|entry| {
                let entry = entry.expect("walk entry");
                (entry.commit, entry.path)
            })
            .collect();
        assert_eq!(
            entries,
            vec![
                (third, "new.txt".to_owned()),
                (second, "new.txt".to_owned()),
                (first, "old.txt".to_owned()),
            ],
            "the walk follows the rename and reports the old name"
        );

        let sides = [
            RepoCompareSide::Commit(first),
            RepoCompareSide::Commit(second),
            RepoCompareSide::Commit(third),
        ];
        let paths = ["old.txt", "new.txt", "new.txt"];
        let blobs = opened.three_way_side_blobs(sides, paths).expect("blobs");
        assert_eq!(blobs[0].as_deref(), Some(b"one\n".as_slice()));
        assert_eq!(blobs[1].as_deref(), Some(b"one\n".as_slice()));
        assert_eq!(blobs[2].as_deref(), Some(b"three\n".as_slice()));
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
