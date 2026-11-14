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
use super::stat::{file_stat, tree_changes};
use super::upstream::{count_exclusive, count_exclusive_shallow};
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
        let repo = self.local();
        // One graph serves both the merge-base search and the two counting
        // walks; a missing commit-graph only makes them slower.
        let cache = repo.commit_graph_if_enabled().ok().flatten();
        let mut graph = repo.revision_graph::<Commit<Flags>>(cache.as_ref());
        let shallow = Shallow::load(&repo)?;

        let merge_base = if a == b {
            Some(a)
        } else {
            repo.merge_base_with_graph(a, b, &mut graph)
                .map_err(Error::gix)?
                .map(|id| id.detach())
        };
        let base = match mode {
            CompareMode::TwoDot => Some(a),
            CompareMode::ThreeDot => merge_base,
        };

        let head_tree = repo
            .find_commit(b)
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
            // No merge base means nothing is shared, so everything in `b` is.
            None => empty,
        };

        let changes = tree_changes(&repo, &base_tree, &head_tree)?;
        let mut files = Vec::new();
        for file in renames::apply(&changes, &repo)? {
            let stat = file_stat(&repo, &file)?;
            files.push(CompareFile {
                stat,
                kind: file.kind,
            });
        }
        files.sort_by(|a, b| a.stat.path.cmp(&b.stat.path));
        let added = files.iter().map(|file| file.stat.added).sum();
        let removed = files.iter().map(|file| file.stat.removed).sum();

        let (ahead, behind) = if a == b {
            (0, 0)
        } else if shallow.is_empty() {
            let bases: Vec<Oid> = merge_base.into_iter().collect();
            (
                count_exclusive(&mut graph, b, &bases)?,
                count_exclusive(&mut graph, a, &bases)?,
            )
        } else {
            // Below the boundary there is no trustworthy merge base, so the
            // walk compares the two histories up to it instead.
            count_exclusive_shallow(&repo, &shallow, b, a)?
        };

        Ok(Comparison {
            base,
            head: b,
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
            comparison.files.iter().map(|f| f.stat.added).sum()
        );
        assert_eq!(
            comparison.removed,
            comparison.files.iter().map(|f| f.stat.removed).sum()
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
}
