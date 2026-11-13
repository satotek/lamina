//! When writing a commit-graph would make history loading cheaper.
//!
//! A missing commit-graph makes the first walk build reachability from every
//! commit object. The loader measures that pass and offers the user a write;
//! nothing here writes anything by itself. A graph that exists but does not
//! cover some reachable commits is stale, which the walk reports while it runs.

use std::time::Duration;

use super::CommitWalk;
use super::Repo;

/// A repository with fewer commits than this is fast enough without a graph.
pub const DEFAULT_MIN_COMMITS: usize = 50_000;
/// A graph counts as stale once this fraction of `min_commits` lies outside it.
const STALE_FRACTION: usize = 5;

/// A walk that took at least this long is worth offering a graph for.
pub const DEFAULT_MIN_WALK: Duration = Duration::from_millis(500);

/// When the loader has no commit count (there is no graph to read one from),
/// the walk time is the only cheap signal left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitGraphThresholds {
    pub min_commits: usize,
    pub min_walk: Duration,
}

impl Default for CommitGraphThresholds {
    fn default() -> Self {
        Self {
            min_commits: DEFAULT_MIN_COMMITS,
            min_walk: DEFAULT_MIN_WALK,
        }
    }
}

/// Why the history loader wants to offer a `git commit-graph write`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitGraphAdvice {
    /// There is no graph and the walk was expensive enough to justify one.
    Missing,
    /// A graph exists but does not cover every reachable commit.
    Stale,
}

/// What the walk observed about the repository's commit-graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitGraphFacts {
    pub has_graph: bool,
    /// Number of commits in the graph, when it could be read.
    pub commit_count: Option<usize>,
    /// Reachable commits the walk had to read from the object database.
    pub commits_outside_graph: usize,
    /// How long building the walk's reachability state took.
    pub walk_time: Duration,
}

impl CommitGraphFacts {
    pub fn advise(self, thresholds: CommitGraphThresholds) -> Option<CommitGraphAdvice> {
        if self.has_graph {
            // `git gc` writes a graph that every later commit leaves behind, so
            // a few uncovered commits are normal and cost nothing; only offer a
            // rewrite once the uncovered part is itself expensive to walk.
            let costly = self.commits_outside_graph >= thresholds.min_commits / STALE_FRACTION
                || self.walk_time >= thresholds.min_walk;
            (self.commits_outside_graph > 0 && costly).then_some(CommitGraphAdvice::Stale)
        } else if self
            .commit_count
            .is_some_and(|count| count >= thresholds.min_commits)
            || self.walk_time >= thresholds.min_walk
        {
            Some(CommitGraphAdvice::Missing)
        } else {
            None
        }
    }
}

impl Repo {
    /// The commit-graph facts a walk reported, paired with how long the walk
    /// took to become ready. Tests inject their own thresholds; the session
    /// uses [`CommitGraphThresholds::default`].
    pub fn commit_graph_facts(&self, walk: &CommitWalk, walk_time: Duration) -> CommitGraphFacts {
        CommitGraphFacts {
            has_graph: walk.has_commit_graph(),
            commit_count: walk.estimated_commits(),
            commits_outside_graph: walk.commits_outside_graph(),
            walk_time,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(
        has_graph: bool,
        commit_count: Option<usize>,
        outside: usize,
        walk: Duration,
    ) -> CommitGraphFacts {
        CommitGraphFacts {
            has_graph,
            commit_count,
            commits_outside_graph: outside,
            walk_time: walk,
        }
    }

    #[test]
    fn a_large_repository_without_a_graph_is_offered_one() {
        let thresholds = CommitGraphThresholds {
            min_commits: 50_000,
            min_walk: Duration::from_secs(60),
        };
        assert_eq!(
            facts(false, Some(80_000), 0, Duration::ZERO).advise(thresholds),
            Some(CommitGraphAdvice::Missing)
        );
        assert_eq!(
            facts(false, Some(10), 0, Duration::ZERO).advise(thresholds),
            None
        );
    }

    #[test]
    fn a_slow_walk_without_a_known_commit_count_is_offered_one() {
        let thresholds = CommitGraphThresholds {
            min_commits: 50_000,
            min_walk: Duration::from_millis(500),
        };
        assert_eq!(
            facts(false, None, 0, Duration::from_secs(3)).advise(thresholds),
            Some(CommitGraphAdvice::Missing)
        );
        assert_eq!(
            facts(false, None, 0, Duration::from_millis(100)).advise(thresholds),
            None
        );
    }

    #[test]
    fn a_graph_that_misses_reachable_commits_is_stale() {
        let thresholds = CommitGraphThresholds::default();
        assert_eq!(
            facts(true, Some(10), 2, Duration::ZERO).advise(thresholds),
            None,
            "a handful of commits past the graph is normal after any commit"
        );
        assert_eq!(
            facts(true, Some(100_000), 20_000, Duration::ZERO).advise(thresholds),
            Some(CommitGraphAdvice::Stale)
        );
        assert_eq!(
            facts(true, Some(10), 2, Duration::from_secs(1)).advise(thresholds),
            Some(CommitGraphAdvice::Stale)
        );
        assert_eq!(
            facts(true, Some(10), 0, Duration::from_secs(30)).advise(thresholds),
            None
        );
    }
}
