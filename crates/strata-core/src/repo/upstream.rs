//! Upstream tracking of local branches. Owned by the upstream task.

use std::collections::{HashMap, HashSet};

use gix::bstr::ByteSlice;
use gix::remote::Direction;
use gix::revision::plumbing::merge_base::Flags;
use gix::revwalk::graph::Commit;

use super::Repo;
use super::log::Shallow;
use crate::{Error, Oid, Result, UpstreamInfo};

/// The remote branch a local branch's upstream is configured to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamTarget {
    pub remote: String,
    /// Branch name on the remote, without `refs/heads/`.
    pub branch: String,
}

impl Repo {
    /// The remote and remote branch behind `branch`'s upstream.
    ///
    /// Unlike [`upstreams`](Self::upstreams), which follows the fetch refspec
    /// to a remote-tracking ref, this reports the configured values even when
    /// that ref is gone: a remote-side rename needs the old remote branch name
    /// in order to delete it. `branch.<name>.remote = .` names another local
    /// branch and has no remote side, so it yields `None`.
    pub fn branch_upstream_target(&self, branch: &str) -> Result<Option<UpstreamTarget>> {
        let mut repo = self.local();
        let _ = repo.reload();
        let config = repo.config_snapshot();
        let config = config.plumbing();
        let branch = branch.as_bytes().as_bstr();
        let (Some(remote), Some(merge)) = (
            config.string_by("branch", Some(branch), "remote"),
            config.string_by("branch", Some(branch), "merge"),
        ) else {
            return Ok(None);
        };
        if remote.as_bstr() == b".".as_bstr() {
            return Ok(None);
        }
        let Some(merge) = std::str::from_utf8(merge.as_ref())
            .ok()
            .and_then(|merge| merge.strip_prefix("refs/heads/"))
        else {
            return Ok(None);
        };
        Ok(Some(UpstreamTarget {
            remote: remote.to_string(),
            branch: merge.to_owned(),
        }))
    }

    /// Upstream state of every local branch that has one configured, keyed by
    /// the branch's short name (e.g. `main`).
    ///
    /// A local branch is included only when both `branch.<name>.remote` and
    /// `branch.<name>.merge` are set, and when the remote's fetch refspecs map
    /// the configured merge ref to a tracking ref. That is git's notion of
    /// "has an upstream": a remote without a matching refspec has nothing for
    /// the branch to fast-forward from. `branch.<name>.remote = .` names
    /// another local branch, which is used directly.
    ///
    /// A branch whose tracking ref is missing is still reported, with
    /// `gone = true`; a branch pointing at an object that cannot be read is
    /// skipped so one broken tip cannot hide the others.
    pub fn upstreams(&self) -> Result<HashMap<String, UpstreamInfo>> {
        let mut repo = self.local();
        // The configuration is snapshotted when the repository is opened, but a
        // push with `--set-upstream` adds `branch.<name>.*` while the session
        // stays open. Reopening here is the only way to see those edits; a
        // failure keeps the snapshot, since stale tracking state beats none.
        let _ = repo.reload();
        // A missing or corrupt commit-graph is not fatal: the graph still reads
        // commits from the object database.
        let cache = repo.commit_graph_if_enabled().ok().flatten();
        // One graph for all branches: merge-base search and the counting walk
        // below keep the commits they decode, so history shared by many
        // branches is read from the object database only once.
        let mut graph = repo.revision_graph::<Commit<Flags>>(cache.as_ref());
        // A shallow clone lacks the commits below its boundary, so some
        // ahead/behind counts cannot be computed at all.
        let shallow = Shallow::load(&repo)?;
        let config = repo.config_snapshot();
        let config = config.plumbing();
        let mut upstreams = HashMap::new();

        let platform = repo.references().map_err(Error::gix)?;
        let references = platform.all().map_err(Error::gix)?;
        for reference in references {
            let reference = reference.map_err(Error::gix)?;
            let name = reference.name();
            if !name.as_bstr().starts_with(b"refs/heads/") {
                continue;
            }
            let short = name.shorten().to_string();
            let short_bstr = short.as_bytes().as_bstr();
            let remote_value = config.string_by("branch", Some(short_bstr), "remote");
            let merge_value = config.string_by("branch", Some(short_bstr), "merge");
            let (Some(remote_value), Some(merge_value)) = (remote_value, merge_value) else {
                continue;
            };
            // `branch.<name>.remote = .` names another local branch, which no
            // fetch refspec maps; the merge ref itself is the tracking ref.
            let tracking = if remote_value.as_bstr() == b".".as_bstr() {
                match gix::refs::FullName::try_from(merge_value.as_bstr()) {
                    Ok(tracking) => tracking,
                    Err(_) => continue,
                }
            } else {
                let Some(tracking) = repo.branch_remote_tracking_ref_name(name, Direction::Fetch)
                else {
                    continue;
                };
                tracking.map_err(Error::gix)?
            };
            let tracking_name = tracking.shorten().to_string();

            let mut local_ref = reference;
            let Ok(local) = local_ref.peel_to_commit() else {
                continue;
            };

            let Some(mut tracking_ref) = repo.try_find_reference(&tracking).map_err(Error::gix)?
            else {
                // Configured, but the remote-tracking ref does not exist: the
                // branch has no fetchable state to compare against.
                upstreams.insert(
                    short,
                    UpstreamInfo {
                        name: tracking_name,
                        ahead: 0,
                        behind: 0,
                        gone: true,
                    },
                );
                continue;
            };
            let Ok(upstream) = tracking_ref.peel_to_commit() else {
                continue;
            };

            let (ahead, behind) = if local.id == upstream.id {
                (0, 0)
            } else {
                // A shallow clone has no trustworthy merge base: it may be
                // below the boundary, and gix then reports a wrong one instead
                // of failing. Walk each side with the other hidden, which is
                // what `git rev-list --left-right --count a...b` does and what
                // gix's revision walk already bounds at the shallow boundary.
                let counts = if shallow.is_empty() {
                    (|| {
                        let bases: Vec<Oid> = repo
                            .merge_bases_many_with_graph(local.id, &[upstream.id], &mut graph)
                            .map_err(Error::gix)?
                            .into_iter()
                            .map(|id| id.detach())
                            .collect();
                        Ok::<_, Error>((
                            count_exclusive(&mut graph, local.id, &bases)?,
                            count_exclusive(&mut graph, upstream.id, &bases)?,
                        ))
                    })()
                } else {
                    count_exclusive_shallow(&repo, &shallow, local.id, upstream.id)
                };
                match counts {
                    Ok(counts) => counts,
                    // A count that still cannot be computed must not fail the
                    // whole snapshot; the branch keeps its upstream with 0/0.
                    Err(_) if !shallow.is_empty() => (0, 0),
                    Err(error) => return Err(error),
                }
            };

            upstreams.insert(
                short,
                UpstreamInfo {
                    name: tracking_name,
                    ahead,
                    behind,
                    gone: false,
                },
            );
        }

        Ok(upstreams)
    }
}

/// How many commits each side has that the other does not, in a shallow clone.
///
/// A merge base below the boundary cannot be found, so both histories are read
/// up to their boundary commits (their roots) and compared. That is what
/// `git rev-list --left-right --count a...b` counts there.
fn count_exclusive_shallow(
    repo: &gix::Repository,
    shallow: &Shallow,
    local: Oid,
    upstream: Oid,
) -> Result<(u32, u32)> {
    let local_history = reachable(repo, shallow, local)?;
    let upstream_history = reachable(repo, shallow, upstream)?;
    let ahead = local_history.difference(&upstream_history).count();
    let behind = upstream_history.difference(&local_history).count();
    Ok((
        u32::try_from(ahead).unwrap_or(u32::MAX),
        u32::try_from(behind).unwrap_or(u32::MAX),
    ))
}

/// Every commit reachable from `tip`, stopping at a shallow boundary.
fn reachable(repo: &gix::Repository, shallow: &Shallow, tip: Oid) -> Result<HashSet<Oid>> {
    let mut seen = HashSet::new();
    let mut stack = vec![tip];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) || shallow.is_boundary(id) {
            continue;
        }
        // The boundary was skipped above, so this never asks for a parent that
        // the clone does not have.
        for parent in repo.find_commit(id).map_err(Error::gix)?.parent_ids() {
            stack.push(parent.detach());
        }
    }
    Ok(seen)
}

/// Commits reachable from `tip` but from none of `bases`, i.e. what a side of
/// `git rev-list --left-right --count a...b` counts.
///
/// Walking stops at the merge bases instead of marking the other tip's whole
/// history: everything at or below a merge base is shared, so only the commits
/// that actually differ between the two tips are visited.
fn count_exclusive(
    graph: &mut gix::revwalk::Graph<'_, '_, Commit<Flags>>,
    tip: Oid,
    bases: &[Oid],
) -> Result<u32> {
    if bases.contains(&tip) {
        return Ok(0);
    }

    let mut visited = HashSet::new();
    let mut stack = vec![tip];
    visited.insert(tip);
    let mut count = 0u32;
    while let Some(id) = stack.pop() {
        if bases.contains(&id) {
            continue;
        }
        let commit = graph
            .get_or_insert_full_commit(id, |_| {})
            .map_err(Error::gix)?;
        let Some(commit) = commit else {
            // A parent id survived into the graph but not into the object
            // database (a shallow clone, for instance); it cannot be counted.
            continue;
        };
        count += 1;
        for parent in &commit.parents {
            if !bases.contains(parent) && visited.insert(*parent) {
                stack.push(*parent);
            }
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use crate::test_support::shallow_clone_fixture;

    #[test]
    fn an_ahead_behind_count_below_the_boundary_matches_git() {
        let (_source, clone) = shallow_clone_fixture(&["--depth", "3"]);
        // The merge's two parents are inside the clone; their common ancestor
        // is below the boundary, so the merge base cannot be found.
        let first = clone.rev("HEAD^1^1");
        let second = clone.rev("HEAD^1^2");
        let first = first.to_string();
        let second = second.to_string();
        clone.git(&["branch", "mine", &first]);
        clone.git(&["branch", "other", &second]);
        clone.git(&["config", "branch.mine.remote", "."]);
        clone.git(&["config", "branch.mine.merge", "refs/heads/other"]);

        let upstreams = clone
            .open()
            .upstreams()
            .expect("one uncomputable count must not fail the snapshot");
        let mine = upstreams.get("mine").expect("mine still has an upstream");
        let count = clone.git(&["rev-list", "--left-right", "--count", "mine...other"]);
        let mut fields = count
            .split_whitespace()
            .map(|field| field.parse::<u32>().expect("count"));
        let expected = (
            fields.next().expect("ahead"),
            fields.next().expect("behind"),
        );
        assert_eq!(
            (mine.ahead, mine.behind),
            expected,
            "each side counts up to the boundary, like git does"
        );

        let main = upstreams.get("main").expect("main tracks origin/main");
        assert_eq!((main.ahead, main.behind), (0, 0));
    }
}
