//! Comparing two commits: the changed files between their trees, with line
//! counts, and how far apart the two histories are.
//!
//! The tree walk and the rename pass are the ones a commit's file list already
//! runs ([`tree_changes`](super::stat::tree_changes) and
//! [`renames`](super::renames)), so a rename shows up the same way in a
//! comparison of two arbitrary commits as in a commit view. The ahead/behind
//! counts reuse the shallow-aware walk the upstream tracking column uses.

use gix::revision::plumbing::merge_base::Flags;
use gix::revwalk::graph::Commit;

use super::Repo;
use super::log::Shallow;
use super::renames;
use super::stat::{
    BlobCache, StatScratch, changed_file, count_changed_lines, count_lines, file_stat_with,
    is_binary, tree_changes,
};
use super::upstream::{count_exclusive, count_exclusive_shallow, exclusive_counts_shallow};
use crate::{ChangeKind, Error, FileStat, Oid, Result};

/// Which trees [`Repo::comparison`] diffs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CompareMode {
    /// `A..B`: the trees of the two commits themselves.
    #[default]
    TwoDot,
    /// `A...B`: the merge base of the two commits against `B`, which is what a
    /// branch "adds" since it diverged.
    ThreeDot,
}

/// One changed file of a [`Comparison`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareFile {
    /// Path and line counts, as a commit's file list reports them.
    pub stat: FileStat,
    pub kind: ChangeKind,
}

/// Two commits compared: the files between their trees and their divergence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    /// The left-hand tree; `None` diffs against the empty tree, which is what
    /// an unrelated three-dot range compares against.
    pub base: Option<Oid>,
    /// The right-hand commit. Its tree is always the right side.
    pub head: Oid,
    /// The best merge base of the two commits, when they share history.
    pub merge_base: Option<Oid>,
    /// Commits reachable from `head` but not from the other side.
    pub ahead: u32,
    /// Commits reachable from the other side but not from `head`.
    pub behind: u32,
    /// Changed files, sorted by path.
    pub files: Vec<CompareFile>,
    /// Totals over `files`; binary files add nothing.
    pub added: u64,
    pub removed: u64,
}

impl Repo {
    /// The files changed between `a` and `b`, with their line counts, and the
    /// number of commits each side has that the other does not.
    ///
    /// `b` is always the right-hand tree. In [`CompareMode::ThreeDot`] the
    /// left-hand tree is the merge base, so unrelated histories compare
    /// against the empty tree, like `git diff A...B` does.
    pub fn comparison(&self, a: Oid, b: Oid, mode: CompareMode) -> Result<Comparison> {
        let merge_base = self.merge_base(a, b)?;
        let base = match mode {
            CompareMode::TwoDot => Some(a),
            CompareMode::ThreeDot => merge_base,
        };
        self.comparison_of(base, b, merge_base, Some(a))
    }

    /// The files changed between `base`'s tree and `head`'s tree.
    ///
    /// `base` is `None` when the left side is the empty tree, which is how a
    /// range that starts at a root commit compares. With a commit on the left,
    /// `merge_base` and the ahead/behind counts describe the two commits;
    /// against the empty tree there is nothing to count.
    pub fn comparison_from(&self, base: Option<Oid>, head: Oid) -> Result<Comparison> {
        let merge_base = match base {
            Some(base) => self.merge_base(base, head)?,
            None => None,
        };
        self.comparison_of(base, head, merge_base, base)
    }

    /// The files changed between `commit` and the working tree, with their line
    /// counts.
    ///
    /// The right side is the files on disk, not HEAD: an uncommitted edit shows
    /// up even when HEAD already moved past the commit. The candidate paths are
    /// the ones the commit's tree versus HEAD and the index/worktree status
    /// name, which covers every path where the working tree can differ from the
    /// commit; a candidate whose bytes match is dropped.
    pub fn comparison_with_worktree(&self, commit: Oid) -> Result<Comparison> {
        let repo = self.local();
        let head = match self.head()? {
            crate::HeadInfo::Branch {
                target: Some(target),
                ..
            }
            | crate::HeadInfo::Detached { target } => target,
            crate::HeadInfo::Branch { target: None, .. } => commit,
        };
        let commit_tree = repo
            .find_commit(commit)
            .map_err(Error::gix)?
            .tree()
            .map_err(Error::gix)?;
        let head_tree = repo
            .find_commit(head)
            .map_err(Error::gix)?
            .tree()
            .map_err(Error::gix)?;

        let mut paths = std::collections::BTreeSet::new();
        for change in tree_changes(&repo, &commit_tree, &head_tree)? {
            if let Some(file) = changed_file(&change) {
                paths.insert(file.path);
            }
        }
        let status = self.status()?;
        for change in status.staged.iter().chain(status.unstaged.iter()) {
            paths.insert(change.path.clone());
        }

        let mut files = Vec::new();
        for path in paths {
            let old = self.file_at_commit(commit, &path)?;
            let new = self.file_in_worktree_as_git(&path)?;
            if old == new {
                continue;
            }
            let kind = match (&old, &new) {
                (None, Some(_)) => ChangeKind::Added,
                (Some(_), None) => ChangeKind::Deleted,
                _ => ChangeKind::Modified,
            };
            let (added, removed, binary) = match (old.as_deref(), new.as_deref()) {
                (Some(old), Some(new)) if is_binary(old) || is_binary(new) => (0, 0, true),
                (Some(old), Some(new)) => {
                    let (added, removed) = count_changed_lines(old, new);
                    (added, removed, false)
                }
                (Some(old), None) => (0, count_lines(old), false),
                (None, Some(new)) => (count_lines(new), 0, false),
                (None, None) => continue,
            };
            files.push(CompareFile {
                stat: FileStat {
                    path,
                    old_path: None,
                    added,
                    removed,
                    binary,
                },
                kind,
            });
        }
        let added = files.iter().map(|file| file.stat.added).sum();
        let removed = files.iter().map(|file| file.stat.removed).sum();
        Ok(Comparison {
            base: Some(commit),
            head: commit,
            merge_base: Some(commit),
            ahead: 0,
            behind: 0,
            files,
            added,
            removed,
        })
    }

    /// The best merge base of `a` and `b`, or `None` for unrelated histories.
    pub fn merge_base(&self, a: Oid, b: Oid) -> Result<Option<Oid>> {
        if a == b {
            return Ok(Some(a));
        }
        let repo = self.local();
        let cache = repo.commit_graph_if_enabled().ok().flatten();
        let mut graph = repo.revision_graph::<Commit<Flags>>(cache.as_ref());
        Ok(repo
            .merge_base_with_graph(a, b, &mut graph)
            .map_err(Error::gix)?
            .map(|id| id.detach()))
    }

    /// The comparison for an already resolved left tree. `counter` is the
    /// commit on the other side of the ahead/behind counts, or `None` when the
    /// left side is the empty tree.
    fn comparison_of(
        &self,
        base: Option<Oid>,
        head: Oid,
        merge_base: Option<Oid>,
        counter: Option<Oid>,
    ) -> Result<Comparison> {
        let repo = self.local();
        let shallow = Shallow::load(&repo)?;

        let (ahead, behind) = match counter {
            Some(left) if left != head => {
                // One graph serves the counting walks; a missing commit-graph
                // only makes them slower.
                let cache = repo.commit_graph_if_enabled().ok().flatten();
                let mut graph = repo.revision_graph::<Commit<Flags>>(cache.as_ref());
                if shallow.is_empty() {
                    let bases: Vec<Oid> = merge_base.into_iter().collect();
                    (
                        count_exclusive(&mut graph, head, &bases)?,
                        count_exclusive(&mut graph, left, &bases)?,
                    )
                } else {
                    // Below the boundary there is no trustworthy merge base, so
                    // the walk compares the two histories up to it instead. A
                    // comparison has no unknown state, so an untrustworthy
                    // count still shows the fetched range rather than zero.
                    match count_exclusive_shallow(&repo, &shallow, head, left)? {
                        Some(counts) => counts,
                        None => exclusive_counts_shallow(&repo, &shallow, head, left)?,
                    }
                }
            }
            _ => (0, 0),
        };

        let head_tree = repo
            .find_commit(head)
            .map_err(Error::gix)?
            .tree()
            .map_err(Error::gix)?;
        let empty = repo.empty_tree();
        let base_tree = match base {
            Some(base) => repo
                .find_commit(base)
                .map_err(Error::gix)?
                .tree()
                .map_err(Error::gix)?,
            // No base means the left side is empty, so everything is added.
            None => empty,
        };

        let changes = tree_changes(&repo, &base_tree, &head_tree)?;
        let blobs = BlobCache::default();
        let mut files = Vec::new();
        let applied = renames::apply(&changes, &repo, Some(&blobs))?;
        let mut scratch = StatScratch::new(repo.clone(), Some(&blobs));
        for file in applied {
            let stat = file_stat_with(&mut scratch, &file)?;
            files.push(CompareFile {
                stat,
                kind: file.kind,
            });
        }
        files.sort_by(|a, b| a.stat.path.cmp(&b.stat.path));
        let added = files.iter().map(|file| file.stat.added).sum();
        let removed = files.iter().map(|file| file.stat.removed).sum();

        Ok(Comparison {
            base,
            head,
            merge_base,
            ahead,
            behind,
            files,
            added,
            removed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRepo;

    /// `main` with a rename, a commit on `topic` that changes a file `main`
    /// keeps, and one commit on `main` after the branches forked.
    fn diverged_fixture() -> (TestRepo, Oid, Oid) {
        let repo = TestRepo::new();
        repo.write("keep.txt", "one\ntwo\nthree\n");
        repo.write("old.txt", "alpha\nbeta\n");
        repo.write("gone.txt", "gone\n");
        repo.commit("base");

        repo.git(&["checkout", "-q", "-b", "topic"]);
        repo.write("topic.txt", "topic\n");
        repo.commit("topic adds a file");
        repo.write("old.txt", "alpha\nBETA\n");
        repo.commit("topic edits old.txt");
        let topic = repo.head();

        repo.git(&["checkout", "-q", "main"]);
        repo.git(&["mv", "old.txt", "new.txt"]);
        repo.write("new.txt", "alpha\nbeta\ngamma\n");
        repo.commit("main renames old.txt");
        repo.write("keep.txt", "one\nTWO\nthree\n");
        repo.git(&["rm", "-q", "gone.txt"]);
        repo.commit("main edits keep and drops gone");
        let main = repo.head();

        (repo, topic, main)
    }

    fn file<'a>(comparison: &'a Comparison, path: &str) -> &'a CompareFile {
        comparison
            .files
            .iter()
            .find(|file| file.stat.path == path)
            .unwrap_or_else(|| panic!("{path} is in the comparison: {comparison:?}"))
    }

    #[test]
    fn two_dot_lists_the_files_each_side_has() {
        let (repo, topic, main) = diverged_fixture();
        let comparison = repo
            .open()
            .comparison(topic, main, CompareMode::TwoDot)
            .expect("compare");

        let paths: Vec<&str> = comparison
            .files
            .iter()
            .map(|file| file.stat.path.as_str())
            .collect();
        assert_eq!(
            paths,
            ["gone.txt", "keep.txt", "new.txt", "old.txt", "topic.txt"],
            "the trees differ in every file the branches changed, on either side"
        );
        assert_eq!(comparison.base, Some(topic), "two-dot starts at A's tree");
        assert_eq!(comparison.head, main);

        assert_eq!(file(&comparison, "old.txt").kind, ChangeKind::Deleted);
        assert_eq!(file(&comparison, "new.txt").kind, ChangeKind::Added);
        assert_eq!(
            file(&comparison, "new.txt").stat.old_path,
            None,
            "the rename is not paired here: the trees hold different content"
        );
        assert_eq!(file(&comparison, "keep.txt").stat.added, 1);
        assert_eq!(file(&comparison, "keep.txt").stat.removed, 1);
        assert_eq!(
            comparison.added,
            comparison.files.iter().map(|f| f.stat.added).sum::<u64>()
        );
        assert_eq!(
            comparison.removed,
            comparison.files.iter().map(|f| f.stat.removed).sum::<u64>()
        );
    }

    #[test]
    fn three_dot_starts_at_the_merge_base() {
        let (repo, topic, main) = diverged_fixture();
        let fork = repo.rev("main~2");
        let comparison = repo
            .open()
            .comparison(topic, main, CompareMode::ThreeDot)
            .expect("compare");

        assert_eq!(comparison.base, Some(fork), "three-dot starts at the fork");
        assert_eq!(comparison.merge_base, Some(fork));
        let paths: Vec<&str> = comparison
            .files
            .iter()
            .map(|file| file.stat.path.as_str())
            .collect();
        assert_eq!(
            paths,
            ["gone.txt", "keep.txt", "new.txt"],
            "only what B changed since the fork is shown"
        );
        assert!(
            comparison
                .files
                .iter()
                .all(|file| file.stat.path != "old.txt"),
            "the renamed-away path is paired, not listed on its own"
        );
        assert_eq!(file(&comparison, "new.txt").kind, ChangeKind::Renamed);
        assert_eq!(
            file(&comparison, "new.txt").stat.old_path.as_deref(),
            Some("old.txt"),
            "the rename is paired against the merge base"
        );
        assert_eq!(file(&comparison, "new.txt").stat.added, 1);
        assert_eq!(file(&comparison, "new.txt").stat.removed, 0);
    }

    #[test]
    fn a_worktree_comparison_lists_uncommitted_edits() {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\ntwo\n");
        repo.write("keep.txt", "same\n");
        let commit = repo.commit("base");
        repo.write("a.txt", "one\nTWO\n");
        repo.write("new.txt", "new\n");

        let comparison = repo
            .open()
            .comparison_with_worktree(commit)
            .expect("compare");
        let paths: Vec<&str> = comparison
            .files
            .iter()
            .map(|file| file.stat.path.as_str())
            .collect();
        assert_eq!(paths, ["a.txt", "new.txt"]);
        assert_eq!(file(&comparison, "a.txt").kind, ChangeKind::Modified);
        assert_eq!(file(&comparison, "a.txt").stat.added, 1);
        assert_eq!(file(&comparison, "a.txt").stat.removed, 1);
        assert_eq!(file(&comparison, "new.txt").kind, ChangeKind::Added);
        assert_eq!(file(&comparison, "new.txt").stat.added, 1);

        // A file the user edited back to the commit's content is not a change.
        repo.write("a.txt", "one\ntwo\n");
        repo.write_bytes("new.txt", b"");
        let comparison = repo
            .open()
            .comparison_with_worktree(commit)
            .expect("compare");
        assert!(
            !comparison
                .files
                .iter()
                .any(|file| file.stat.path == "a.txt"),
            "a file that matches the commit is not listed"
        );
    }

    #[test]
    fn a_rename_across_the_range_is_paired() {
        let repo = TestRepo::new();
        repo.write(
            "src/before.rs",
            "fn main() {\n    println!(\"before\");\n}\n",
        );
        repo.commit("add before.rs");
        repo.git(&["mv", "src/before.rs", "src/after.rs"]);
        repo.commit("rename after.rs");
        let root = repo.rev("HEAD~1");
        let head = repo.head();

        let comparison = repo
            .open()
            .comparison(root, head, CompareMode::TwoDot)
            .expect("compare");
        assert_eq!(
            comparison.files.len(),
            1,
            "one rename, not a delete and add: {comparison:?}"
        );
        let file = file(&comparison, "src/after.rs");
        assert_eq!(file.kind, ChangeKind::Renamed);
        assert_eq!(file.stat.old_path.as_deref(), Some("src/before.rs"));
        assert_eq!((file.stat.added, file.stat.removed), (0, 0));
    }

    #[test]
    fn ahead_and_behind_count_what_each_side_added() {
        let (repo, topic, main) = diverged_fixture();
        let opened = repo.open();

        let comparison = opened
            .comparison(topic, main, CompareMode::ThreeDot)
            .expect("compare");
        let expected = repo.git(&["rev-list", "--left-right", "--count", "topic...main"]);
        let mut fields = expected
            .split_whitespace()
            .map(|field| field.parse::<u32>().expect("count"));
        let (behind, ahead) = (
            fields.next().expect("topic side"),
            fields.next().expect("main side"),
        );
        assert_eq!(
            (comparison.ahead, comparison.behind),
            (ahead, behind),
            "ahead counts B's commits, behind counts A's"
        );
        assert_eq!((comparison.ahead, comparison.behind), (2, 2));

        // The counts describe the histories, so both modes report them.
        let two_dot = opened
            .comparison(topic, main, CompareMode::TwoDot)
            .expect("compare");
        assert_eq!(
            (two_dot.ahead, two_dot.behind),
            (comparison.ahead, comparison.behind)
        );

        // The same commit on both sides has nothing to compare.
        let same = opened
            .comparison(main, main, CompareMode::TwoDot)
            .expect("compare");
        assert_eq!((same.ahead, same.behind), (0, 0));
        assert!(same.files.is_empty());
        assert_eq!(same.base, Some(main));
    }

    #[test]
    fn unrelated_histories_compare_against_the_empty_tree() {
        let repo = TestRepo::new();
        repo.write("mine.txt", "mine\n");
        repo.commit("mine");
        repo.git(&["checkout", "-q", "--orphan", "other"]);
        repo.git(&["rm", "-q", "-rf", "."]);
        repo.write("theirs.txt", "theirs\n");
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-q", "-m", "theirs"]);
        let theirs = repo.head();
        let mine = repo.rev("main");

        let comparison = repo
            .open()
            .comparison(mine, theirs, CompareMode::ThreeDot)
            .expect("compare");
        assert_eq!(comparison.merge_base, None);
        assert_eq!(comparison.base, None, "no fork point means the empty tree");
        let paths: Vec<&str> = comparison
            .files
            .iter()
            .map(|file| file.stat.path.as_str())
            .collect();
        assert_eq!(paths, ["theirs.txt"]);
        assert_eq!(comparison.behind, 1);
        assert_eq!(comparison.ahead, 1);
    }

    #[test]
    fn a_range_against_the_empty_tree_lists_every_tracked_file() {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\n");
        repo.commit("add a");
        repo.write("b.txt", "two\n");
        let head = repo.commit("add b");
        let opened = repo.open();

        let all = opened.comparison_from(None, head).expect("compare");
        assert_eq!(all.base, None, "a root range starts at the empty tree");
        assert_eq!(all.merge_base, None);
        assert_eq!((all.ahead, all.behind), (0, 0));
        let paths: Vec<&str> = all
            .files
            .iter()
            .map(|file| file.stat.path.as_str())
            .collect();
        assert_eq!(paths, ["a.txt", "b.txt"]);
        assert_eq!(all.added, 2);

        // Comparing from a parent commit is the same as the two-dot
        // comparison of that pair, and counts the one commit between them.
        let parent = repo.rev("HEAD~1");
        let from_parent = opened.comparison_from(Some(parent), head).expect("compare");
        let two_dot = opened
            .comparison(parent, head, CompareMode::TwoDot)
            .expect("compare");
        assert_eq!(from_parent.files, two_dot.files);
        assert_eq!(from_parent.base, Some(parent));
        assert_eq!((from_parent.ahead, from_parent.behind), (1, 0));
    }
}
