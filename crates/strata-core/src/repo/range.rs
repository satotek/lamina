//! Planning the diff of a multi-selection in History.
//!
//! The details pane compares the ends of a selection along first-parent
//! history: the oldest selection's first parent (or the empty tree for a root
//! commit) against the newest commit. This module decides which pair that is,
//! whether unselected commits sit inside it, and whether the two ends are even
//! on one line of history.
//!
//! [`plan_selection_range`] is pure: it asks for a first parent and a merge
//! base instead of reading the repository, which keeps every rule testable
//! without a fixture. [`Repo::selection_range`] supplies both from gix; it runs
//! off the UI thread because a wide selection can walk a lot of history.

use gix::revision::plumbing::merge_base::Flags;
use gix::revwalk::graph::Commit;

use super::Repo;
use crate::{Error, Oid, Result};

/// How the diff of a multi-selection is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionRangeMode {
    /// The oldest selection's first parent against the newest selection: the
    /// net change of the range. A root commit starts at the empty tree.
    #[default]
    Range,
    /// The two endpoints themselves, without their parents.
    BetweenEndpoints,
}

/// What the selected commits look like along first-parent history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionRangeKind {
    /// The selection is the whole first-parent run between its ends, so no
    /// commit inside the range is left out.
    Contiguous,
    /// The ends are on one line, but this many commits between them were not
    /// selected and are still part of the diff.
    Gapped { skipped: usize },
    /// Exactly two commits, not adjacent on one line: both the range and the
    /// plain two-endpoint comparison are meaningful.
    TwoEndpoints,
    /// Neither end is an ancestor of the other, so no range spans them; the
    /// two trees are compared directly.
    Diverged,
}

/// The commit pair a multi-selection compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionRange {
    /// The newest selected commit: always the right-hand tree.
    pub head: Oid,
    /// The oldest selected commit, which is also the left end of the
    /// two-endpoint comparison.
    pub oldest: Oid,
    /// The left end of the range: the oldest's first parent, or `None` when it
    /// is a root commit and the diff starts at the empty tree.
    pub range_base: Option<Oid>,
    pub kind: SelectionRangeKind,
}

impl SelectionRange {
    /// The left-hand tree for `mode`.
    ///
    /// A diverged selection has no range to take, so both modes name its older
    /// endpoint; only the two-endpoint comparison means anything there.
    pub fn base(self, mode: SelectionRangeMode) -> Option<Oid> {
        match (self.kind, mode) {
            (SelectionRangeKind::Diverged, _) | (_, SelectionRangeMode::BetweenEndpoints) => {
                Some(self.oldest)
            }
            (_, SelectionRangeMode::Range) => self.range_base,
        }
    }
}

/// Plans the comparison of `selected`, given in display order (newest first).
///
/// `first_parent` returns a commit's first parent; `merge_base` returns the
/// best common ancestor of two commits. `None` means fewer than two distinct
/// commits were selected.
pub fn plan_selection_range(
    selected: &[Oid],
    mut first_parent: impl FnMut(Oid) -> Result<Option<Oid>>,
    mut merge_base: impl FnMut(Oid, Oid) -> Result<Option<Oid>>,
) -> Result<Option<SelectionRange>> {
    let mut unique: Vec<Oid> = Vec::with_capacity(selected.len());
    for &oid in selected {
        if !unique.contains(&oid) {
            unique.push(oid);
        }
    }
    if unique.len() < 2 {
        return Ok(None);
    }
    let head = unique[0];
    let oldest = *unique.last().expect("two or more entries");

    // A range needs the oldest end to be an ancestor of the newest one. The
    // merge base decides that: when `oldest` is an ancestor, every merge base
    // is an ancestor of `oldest`, so the best base is `oldest` itself. When the
    // best base is the other end, or there is none (unrelated histories), the
    // selection is not one range and the two trees are compared directly.
    let ancestor = merge_base(oldest, head)? == Some(oldest);

    // Merges are followed through their first parent. A side branch merged
    // inside the range contributes through the merge's own tree, so its commits
    // are not part of the walk and never count as skipped.
    let mut chain = Vec::new();
    let mut on_first_parent_line = false;
    if ancestor {
        let mut cursor = first_parent(head)?;
        while let Some(commit) = cursor {
            if commit == oldest {
                on_first_parent_line = true;
                break;
            }
            chain.push(commit);
            cursor = first_parent(commit)?;
        }
    }

    // Both counts stay linear in the walk: the selection is usually small, so
    // scanning it per visited commit costs less than a set of every commit.
    let mut in_chain = vec![false; unique.len()];
    for commit in &chain {
        if let Some(index) = unique.iter().position(|selected| selected == commit) {
            in_chain[index] = true;
        }
    }
    let selected_in_chain = in_chain.iter().filter(|found| **found).count();
    let skipped = if on_first_parent_line {
        chain.len() - selected_in_chain
    } else {
        // The oldest is only behind a merge's other parent, so the walk went
        // past it: the commits it visited say nothing about the range.
        0
    };
    let off_chain = in_chain[1..unique.len() - 1]
        .iter()
        .filter(|found| !**found)
        .count();

    let kind = if !ancestor {
        SelectionRangeKind::Diverged
    } else if on_first_parent_line && skipped == 0 && off_chain == 0 {
        SelectionRangeKind::Contiguous
    } else if unique.len() == 2 {
        SelectionRangeKind::TwoEndpoints
    } else {
        SelectionRangeKind::Gapped { skipped }
    };

    Ok(Some(SelectionRange {
        head,
        oldest,
        range_base: first_parent(oldest)?,
        kind,
    }))
}

impl Repo {
    /// The pair a multi-selection compares. `selected` is in display order,
    /// newest first.
    ///
    /// The merge base and the first-parent walk both use the commit graph when
    /// the repository has one; a missing graph only makes them slower.
    pub fn selection_range(&self, selected: &[Oid]) -> Result<Option<SelectionRange>> {
        let repo = self.local();
        let cache = repo.commit_graph_if_enabled().ok().flatten();
        let mut graph = repo.revision_graph::<Commit<Flags>>(cache.as_ref());
        plan_selection_range(
            selected,
            |oid| {
                let commit = repo.find_commit(oid).map_err(Error::gix)?;
                Ok(commit.parent_ids().next().map(|id| id.detach()))
            },
            |a, b| {
                if a == b {
                    return Ok(Some(a));
                }
                repo.merge_base_with_graph(a, b, &mut graph)
                    .map_err(Error::gix)
                    .map(|id| id.map(|id| id.detach()))
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRepo;

    fn plan(repo: &TestRepo, selected: &[Oid]) -> SelectionRange {
        repo.open()
            .selection_range(selected)
            .expect("plan the range")
            .expect("at least two commits")
    }

    /// Five commits `a..=e` on `main`, plus a topic branch that forks at `b`.
    struct Fixture {
        repo: TestRepo,
        a: Oid,
        b: Oid,
        c: Oid,
        d: Oid,
        e: Oid,
        topic: Oid,
    }

    fn fixture() -> Fixture {
        let repo = TestRepo::new();
        repo.write("a.txt", "a\n");
        let a = repo.commit("a");
        repo.write("b.txt", "b\n");
        let b = repo.commit("b");
        repo.write("c.txt", "c\n");
        let c = repo.commit("c");
        repo.write("d.txt", "d\n");
        let d = repo.commit("d");
        repo.write("e.txt", "e\n");
        let e = repo.commit("e");

        repo.git(&["checkout", "-q", "-b", "topic", &b.to_string()]);
        repo.write("topic.txt", "topic\n");
        let topic = repo.commit("topic");

        Fixture {
            repo,
            a,
            b,
            c,
            d,
            e,
            topic,
        }
    }

    #[test]
    fn adjacent_commits_compare_the_older_ones_parent_with_the_newer_one() {
        let f = fixture();
        let range = plan(&f.repo, &[f.e, f.d, f.c]);
        assert_eq!(range.head, f.e);
        assert_eq!(range.oldest, f.c);
        assert_eq!(range.range_base, Some(f.b));
        assert_eq!(range.kind, SelectionRangeKind::Contiguous);
        assert_eq!(range.base(SelectionRangeMode::Range), Some(f.b));
        assert_eq!(range.base(SelectionRangeMode::BetweenEndpoints), Some(f.c));
    }

    #[test]
    fn a_selection_that_starts_at_a_root_compares_against_the_empty_tree() {
        let f = fixture();
        let range = plan(&f.repo, &[f.b, f.a]);
        assert_eq!(range.head, f.b);
        assert_eq!(range.oldest, f.a);
        assert_eq!(range.range_base, None, "a root has no parent");
        assert_eq!(range.base(SelectionRangeMode::Range), None);
        assert_eq!(range.kind, SelectionRangeKind::Contiguous);

        // The same root in a wider selection keeps the empty-tree base.
        let wide = plan(&f.repo, &[f.c, f.a]);
        assert_eq!(wide.range_base, None);
    }

    #[test]
    fn three_or_more_commits_with_a_gap_count_the_commits_left_out() {
        let f = fixture();
        // e, c and b: `d` sits between the ends and was not selected.
        let range = plan(&f.repo, &[f.e, f.c, f.b]);
        assert_eq!(range.kind, SelectionRangeKind::Gapped { skipped: 1 });
        assert_eq!(range.range_base, Some(f.a));

        // e and a: b, c and d are between them.
        let wide = plan(&f.repo, &[f.e, f.a]);
        assert_eq!(wide.kind, SelectionRangeKind::TwoEndpoints);
        let three = plan(&f.repo, &[f.e, f.d, f.a]);
        assert_eq!(three.kind, SelectionRangeKind::Gapped { skipped: 2 });
        assert_eq!(three.range_base, None);
    }

    #[test]
    fn exactly_two_non_adjacent_commits_offer_both_comparisons() {
        let f = fixture();
        let range = plan(&f.repo, &[f.d, f.b]);
        assert_eq!(range.kind, SelectionRangeKind::TwoEndpoints);
        assert_eq!(range.range_base, Some(f.a));
        assert_eq!(range.base(SelectionRangeMode::Range), Some(f.a));
        assert_eq!(
            range.base(SelectionRangeMode::BetweenEndpoints),
            Some(f.b),
            "the two-endpoint comparison starts at the older selection itself"
        );
    }

    #[test]
    fn a_merge_inside_the_range_follows_its_first_parent() {
        let f = fixture();
        f.repo.git(&["checkout", "-q", "main"]);
        f.repo
            .git(&["merge", "-q", "--no-ff", "-m", "merge topic", "topic"]);
        let merge = f.repo.head();

        let parents = f.repo.git(&["rev-list", "--parents", "-n", "1", "HEAD"]);
        assert_eq!(
            parents.split_whitespace().count(),
            3,
            "the fixture needs a merge with two parents: {parents}"
        );

        // merge -> e -> d: the second parent is not walked, so no gap.
        let range = plan(&f.repo, &[merge, f.e, f.d]);
        assert_eq!(range.kind, SelectionRangeKind::Contiguous);
        assert_eq!(range.range_base, Some(f.c));

        // merge -> d skips e, which is still part of the diff.
        let gapped = plan(&f.repo, &[merge, f.d]);
        assert_eq!(gapped.kind, SelectionRangeKind::TwoEndpoints);
        assert_eq!(gapped.range_base, Some(f.c));

        // A commit selected from the merged side is not on the first-parent
        // line, so the range is gapped by e.
        let side = plan(&f.repo, &[merge, f.topic, f.d]);
        assert_eq!(side.kind, SelectionRangeKind::Gapped { skipped: 1 });

        // The merged tip is an ancestor of the merge, but only through the
        // merge's second parent. There is no first-parent line to count along,
        // so the pair keeps both comparisons and no skipped note.
        let behind_merge = plan(&f.repo, &[merge, f.topic]);
        assert_eq!(behind_merge.kind, SelectionRangeKind::TwoEndpoints);
        assert_eq!(behind_merge.range_base, Some(f.b));
    }

    #[test]
    fn commits_on_different_branches_have_no_range() {
        let f = fixture();
        // `e` and `topic` forked at `b`: neither contains the other. Use git's
        // own answer as the reference for the best merge base.
        let expected = f
            .repo
            .git(&["merge-base", &f.e.to_string(), &f.topic.to_string()]);
        assert_eq!(expected.trim(), f.b.to_string());

        let range = plan(&f.repo, &[f.e, f.topic]);
        assert_eq!(range.kind, SelectionRangeKind::Diverged);
        assert_eq!(range.head, f.e);
        assert_eq!(range.oldest, f.topic);
        assert_eq!(
            range.base(SelectionRangeMode::BetweenEndpoints),
            Some(f.topic)
        );
        assert_eq!(
            range.base(SelectionRangeMode::Range),
            Some(f.topic),
            "a diverged selection has no range; both modes name the older end"
        );
    }

    #[test]
    fn one_commit_or_a_repeated_one_has_no_range() {
        let f = fixture();
        assert_eq!(
            f.repo
                .open()
                .selection_range(&[f.e])
                .expect("plan one commit"),
            None
        );
        assert_eq!(
            f.repo
                .open()
                .selection_range(&[f.e, f.e])
                .expect("plan a repeated commit"),
            None
        );
    }
}
