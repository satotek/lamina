//! Upstream tracking of local branches. Owned by the upstream task.

use std::collections::{HashMap, HashSet};

use gix::bstr::ByteSlice;
use gix::remote::Direction;
use gix::revision::plumbing::merge_base::Flags;
use gix::revwalk::graph::Commit;

use super::Repo;
use crate::{Error, Oid, Result, UpstreamInfo};

impl Repo {
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
                let bases: Vec<Oid> = repo
                    .merge_bases_many_with_graph(local.id, &[upstream.id], &mut graph)
                    .map_err(Error::gix)?
                    .into_iter()
                    .map(|id| id.detach())
                    .collect();
                (
                    count_exclusive(&mut graph, local.id, &bases)?,
                    count_exclusive(&mut graph, upstream.id, &bases)?,
                )
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
