//! A memory-lean commit walk for path-limited history.
//!
//! [`Walk`](super::walk) keeps a node per commit so it can order the whole
//! history, which costs hundreds of megabytes on a million-commit repository.
//! A path walk only needs the order: it first counts, per commit, how many
//! children the walk still has to emit, then emits by date and drops each
//! commit's counter as it goes. The result is the same `--date-order` sequence
//! as [`Walk`](super::walk) (and therefore as `Repo::walk_commits`) without the
//! reachability graph.
//!
//! No node, parent list or lane state is retained per commit: the counting pass
//! keeps one word per reachable commit and the emission drops it, so a path walk
//! costs a fraction of a full walk's memory.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use gix::objs::FindExt;

use super::{CancelCheck, Shallow};
use crate::{Error, Oid, Result};

/// One row: the commit and its first parent, the only parent a path walk
/// compares its tree against.
pub(crate) struct PathCommit {
    pub(crate) id: Oid,
    pub(crate) parent: Option<Oid>,
}

/// A commit whose children have all been emitted, with the metadata read at
/// that moment so the emission itself needs no second read.
struct Ready {
    time: i64,
    /// Assignment order, the tie-breaker for equal committer times.
    sequence: u32,
    id: Oid,
    parents: Box<[Oid]>,
}

impl Ord for Ready {
    /// Newest first; equal times in assignment order, like the full walk's
    /// ready queue.
    fn cmp(&self, other: &Self) -> Ordering {
        self.time
            .cmp(&other.time)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

impl PartialOrd for Ready {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Ready {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time && self.sequence == other.sequence
    }
}

impl Eq for Ready {}

/// The commit-graph can be larger than the walk (it may cover refs the history
/// view leaves out), so its commit count is only used as a pre-allocation hint
/// up to this many commits.
const MAX_RESERVED_COMMITS: usize = 1 << 20;

pub(crate) struct PathWalk {
    objects: gix::OdbHandle,
    graph: Option<gix::commitgraph::Graph>,
    /// Commits whose parents are outside a shallow clone.
    shallow: Shallow,
    /// Children still to be emitted, plus one: a commit is ready when it is
    /// back to one.
    pending: HashMap<Oid, u32>,
    ready: BinaryHeap<Ready>,
    next_sequence: u32,
    object_buf: Vec<u8>,
    parents_buf: Vec<Oid>,
    /// `None` for the public, uncancellable walk.
    cancelled: Option<CancelCheck>,
    finished: bool,
}

impl PathWalk {
    /// Counts the children of everything reachable from `tips`, then leaves the
    /// tips in the ready queue.
    pub(crate) fn new(
        objects: gix::OdbHandle,
        graph: Option<gix::commitgraph::Graph>,
        tips: &[Oid],
        shallow: Shallow,
        cancelled: Option<CancelCheck>,
    ) -> Result<Self> {
        let mut walk = Self {
            objects,
            graph,
            shallow,
            pending: HashMap::new(),
            ready: BinaryHeap::new(),
            next_sequence: 0,
            object_buf: Vec::new(),
            parents_buf: Vec::new(),
            cancelled,
            finished: false,
        };
        // The graph knows how many commits exist, which avoids growing the
        // counter table through a rehash with two live tables.
        if let Some(graph) = walk.graph.as_ref() {
            let commits = (graph.num_commits() as usize).min(MAX_RESERVED_COMMITS);
            walk.pending.reserve(commits);
        }
        walk.count_children(tips)?;
        Ok(walk)
    }

    fn count_children(&mut self, tips: &[Oid]) -> Result<()> {
        for &tip in tips {
            self.pending.entry(tip).or_insert(1);
        }

        let mut visit: Vec<Oid> = tips.to_vec();
        while let Some(id) = visit.pop() {
            // A cancelled path query must stop the walk itself, even while it is
            // only counting.
            if self.is_cancelled() {
                self.finish();
                return Ok(());
            }
            self.metadata(&id)?;
            for position in 0..self.parents_buf.len() {
                let parent = self.parents_buf[position];
                let newly_seen = !self.pending.contains_key(&parent);
                *self.pending.entry(parent).or_insert(1) += 1;
                if newly_seen {
                    visit.push(parent);
                }
            }
        }

        let mut ready = Vec::new();
        for &tip in tips {
            if self.pending.get(&tip) == Some(&1) {
                ready.push(tip);
            }
        }
        for id in ready {
            let time = self.metadata(&id)?;
            let parents = self.parents_buf.clone().into_boxed_slice();
            self.push_ready(id, time, parents);
        }
        Ok(())
    }

    /// The next commit in `--date-order`, or `None` once the walk is over.
    pub(crate) fn next(&mut self) -> Option<Result<PathCommit>> {
        if self.finished {
            return None;
        }
        if self.is_cancelled() {
            self.finish();
            return None;
        }
        let Some(commit) = self.ready.pop() else {
            self.finish();
            return None;
        };
        // Emitted commits are never referenced again, so their counters go.
        self.pending.remove(&commit.id);

        let parent = commit.parents.first().copied();
        let mut ready = Vec::new();
        for parent in commit.parents.iter() {
            let Some(count) = self.pending.get_mut(parent) else {
                continue;
            };
            *count -= 1;
            if *count == 1 {
                ready.push(*parent);
            }
        }
        for id in ready {
            if self.is_cancelled() {
                self.finish();
                return None;
            }
            match self.metadata(&id) {
                Ok(time) => {
                    let parents = self.parents_buf.clone().into_boxed_slice();
                    self.push_ready(id, time, parents);
                }
                Err(error) => {
                    self.finish();
                    return Some(Err(error));
                }
            }
        }
        Some(Ok(PathCommit {
            id: commit.id,
            parent,
        }))
    }

    fn push_ready(&mut self, id: Oid, time: i64, parents: Box<[Oid]>) {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.ready.push(Ready {
            time,
            sequence,
            id,
            parents,
        });
    }

    fn finish(&mut self) {
        self.finished = true;
        self.pending = HashMap::new();
        self.ready.clear();
    }

    fn is_cancelled(&self) -> bool {
        match &self.cancelled {
            Some(cancelled) => cancelled(),
            None => false,
        }
    }

    /// Committer time of `id`, with its parents left in `parents_buf`.
    ///
    /// The commit-graph answers both without parsing the commit object; a
    /// missing or corrupt graph falls back to the object database.
    fn metadata(&mut self, id: &Oid) -> Result<i64> {
        self.parents_buf.clear();
        // A shallow boundary is a root: stop before following ids this clone
        // does not have, which would otherwise fail the whole walk.
        let boundary = self.shallow.is_boundary(*id);
        if let Some(graph) = self.graph.as_ref()
            && let Some(commit) = graph.commit_by_id(id)
        {
            let time = commit.committer_timestamp() as i64;
            let mut corrupt = false;
            for parent in commit.iter_parents().take_while(|_| !boundary) {
                match parent {
                    Ok(position) => self
                        .parents_buf
                        .push(graph.commit_at(position).id().to_owned()),
                    Err(_) => {
                        corrupt = true;
                        break;
                    }
                }
            }
            if !corrupt {
                return Ok(time);
            }
            // A corrupt graph must not hide history: drop it and read from the
            // object database from here on.
            self.graph = None;
            self.parents_buf.clear();
        }

        let iter = self
            .objects
            .find_commit_iter(id, &mut self.object_buf)
            .map_err(Error::gix)?;
        if !boundary {
            self.parents_buf.extend(iter.parent_ids());
        }
        let time = iter
            .committer()
            .map_err(Error::gix)?
            // Malformed dates must not fail the whole walk; git tolerates them.
            .time()
            .map_or(0, |time| time.seconds);
        Ok(time)
    }
}
