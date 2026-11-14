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
//! When a commit-graph is present, commits inside it are addressed by their
//! dense `u32` graph [`Position`](gix::commitgraph::Position) instead of hashing
//! 20-byte object ids, and parent edges, committer timestamps, and root tree
//! ids are read directly from the mapped graph without binary searches or
//! per-node heap allocations.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use gix::commitgraph::Position;
use gix::objs::FindExt;
use gix::objs::commit::ref_iter::Token as CommitToken;

use super::{CancelCheck, Shallow};
use crate::{Error, Oid, Result};

/// One row: the commit, its first parent, and graph-resolved tree/position
/// hints so callers do not repeat binary searches or commit object reads.
#[derive(Clone, Copy)]
pub(crate) struct PathCommit {
    pub(crate) id: Oid,
    pub(crate) parent: Option<Oid>,
    pub(crate) tree: Option<Oid>,
    pub(crate) parent_tree: Option<Oid>,
    pub(crate) graph_pos: Option<u32>,
    pub(crate) is_merge: bool,
}

/// A commit whose children have all been emitted. Stored compactly without
/// heap-allocated parent slices; parents are resolved from the commit-graph or
/// `extra_nodes` when popped.
#[derive(Clone, Copy, Eq, PartialEq)]
struct Ready {
    time: i64,
    /// Assignment order, the tie-breaker for equal committer times.
    sequence: u32,
    node: u32,
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

/// Sentinel for an [`ExtraNode`] whose parent list has not been resolved into
/// `extra_parents` yet.
const UNRESOLVED_PARENTS: u32 = u32::MAX;

/// Metadata for a commit not covered by the commit-graph, read once on first
/// encounter so the path walk never parses the commit object twice.
struct ExtraNode {
    id: Oid,
    tree: Oid,
    time: i64,
    parents_start: u32,
    parents_len: u32,
}

pub(crate) struct PathWalk {
    objects: gix::OdbHandle,
    graph: Option<gix::commitgraph::Graph>,
    graph_len: u32,
    /// True if reading edges from `graph` encountered corruption; falls back to
    /// reading metadata from `objects` while keeping `OIDF`/`OIDL` node ids.
    graph_corrupt: bool,
    /// Commits whose parents are outside a shallow clone.
    shallow: Shallow,
    /// Children still to be emitted, plus one: `0` means unseen, `1` means
    /// ready. Indexed by `node` (`0..graph_len` in graph, `graph_len..` in
    /// `extra_nodes`).
    pending: Vec<u32>,
    extra_nodes: Vec<ExtraNode>,
    extra_by_id: HashMap<Oid, u32>,
    extra_parents: Vec<u32>,
    ready: BinaryHeap<Ready>,
    next_sequence: u32,
    object_buf: Vec<u8>,
    parents_buf: Vec<u32>,
    oid_parents_buf: Vec<Oid>,
    /// `None` for the public, uncancellable walk.
    cancelled: Option<CancelCheck>,
    finished: bool,
    total_commits: usize,
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
        let graph_len = graph
            .as_ref()
            .map_or(0, gix::commitgraph::Graph::num_commits);
        let mut walk = Self {
            objects,
            graph,
            graph_len,
            graph_corrupt: false,
            shallow,
            pending: vec![0; graph_len as usize],
            extra_nodes: Vec::new(),
            extra_by_id: HashMap::new(),
            extra_parents: Vec::new(),
            ready: BinaryHeap::new(),
            next_sequence: 0,
            object_buf: Vec::new(),
            parents_buf: Vec::with_capacity(4),
            oid_parents_buf: Vec::with_capacity(4),
            cancelled,
            finished: false,
            total_commits: 0,
        };
        walk.count_children(tips)?;
        Ok(walk)
    }

    pub(crate) fn total_commits(&self) -> usize {
        self.total_commits
    }

    fn count_children(&mut self, tips: &[Oid]) -> Result<()> {
        let mut pending_oid_parents: HashMap<u32, Box<[Oid]>> = HashMap::new();
        let mut tip_nodes = Vec::with_capacity(tips.len());
        let mut visit = Vec::with_capacity(tips.len());

        for &tip in tips {
            let node = self.resolve_oid(tip, &mut pending_oid_parents)?;
            tip_nodes.push(node);
            if self.pending[node as usize] == 0 {
                self.pending[node as usize] = 1;
                self.total_commits += 1;
                visit.push(node);
            }
        }

        while let Some(node) = visit.pop() {
            // A cancelled path query must stop the walk itself, even while it is
            // only counting.
            if self.is_cancelled() {
                self.finish();
                return Ok(());
            }
            self.load_parent_nodes_for_count(node, &mut pending_oid_parents)?;
            for i in 0..self.parents_buf.len() {
                let parent = self.parents_buf[i];
                let slot = &mut self.pending[parent as usize];
                let newly_seen = *slot == 0;
                if newly_seen {
                    *slot = 2;
                    self.total_commits += 1;
                    visit.push(parent);
                } else {
                    *slot += 1;
                }
            }
        }

        let mut ready_tips = Vec::new();
        for node in tip_nodes {
            if self.pending[node as usize] == 1 {
                ready_tips.push(node);
            }
        }
        for node in ready_tips {
            let time = self.node_time(node)?;
            self.push_ready(node, time);
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
        let node = commit.node;
        // Emitted commits are never referenced again, so clear their counter.
        self.pending[node as usize] = 0;

        let (id, tree, graph_pos) = match self.load_parents_for_emit(node) {
            Ok(info) => info,
            Err(error) => {
                self.finish();
                return Some(Err(error));
            }
        };

        let is_merge = self.parents_buf.len() > 1;
        let first_parent = self.parents_buf.first().copied();
        let (parent, parent_tree) = match first_parent {
            Some(p0) => self.parent_summary(p0),
            None => (None, None),
        };

        for i in 0..self.parents_buf.len() {
            let parent_node = self.parents_buf[i];
            let slot = &mut self.pending[parent_node as usize];
            if *slot == 0 {
                continue;
            }
            *slot -= 1;
            if *slot == 1 {
                if self.is_cancelled() {
                    self.finish();
                    return None;
                }
                match self.node_time(parent_node) {
                    Ok(time) => self.push_ready(parent_node, time),
                    Err(error) => {
                        self.finish();
                        return Some(Err(error));
                    }
                }
            }
        }

        Some(Ok(PathCommit {
            id,
            parent,
            tree,
            parent_tree,
            graph_pos,
            is_merge,
        }))
    }

    fn push_ready(&mut self, node: u32, time: i64) {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.ready.push(Ready {
            time,
            sequence,
            node,
        });
    }

    fn finish(&mut self) {
        self.finished = true;
        self.pending = Vec::new();
        self.extra_nodes = Vec::new();
        self.extra_by_id = HashMap::new();
        self.extra_parents = Vec::new();
        self.ready.clear();
    }

    fn is_cancelled(&self) -> bool {
        match &self.cancelled {
            Some(cancelled) => cancelled(),
            None => false,
        }
    }

    fn is_boundary(&self, id: Oid) -> bool {
        !self.shallow.is_empty() && self.shallow.is_boundary(id)
    }

    fn id_of(&self, node: u32) -> Oid {
        if node < self.graph_len {
            self.graph
                .as_ref()
                .expect("graph_len > 0 implies graph")
                .id_at(Position(node))
                .to_owned()
        } else {
            self.extra_nodes[(node - self.graph_len) as usize].id
        }
    }

    fn parent_summary(&self, parent_node: u32) -> (Option<Oid>, Option<Oid>) {
        if parent_node < self.graph_len {
            let graph = self.graph.as_ref().expect("graph_len > 0 implies graph");
            if !self.graph_corrupt {
                let c = graph.commit_at(Position(parent_node));
                return (Some(c.id().to_owned()), Some(c.root_tree_id().to_owned()));
            }
            return (Some(graph.id_at(Position(parent_node)).to_owned()), None);
        }
        let extra = &self.extra_nodes[(parent_node - self.graph_len) as usize];
        (Some(extra.id), Some(extra.tree))
    }

    fn resolve_oid(
        &mut self,
        id: Oid,
        pending_oid_parents: &mut HashMap<u32, Box<[Oid]>>,
    ) -> Result<u32> {
        if let Some(graph) = self.graph.as_ref()
            && let Some(pos) = graph.lookup(id)
        {
            return Ok(pos.0);
        }
        if let Some(&node) = self.extra_by_id.get(&id) {
            return Ok(node);
        }
        let (tree, time) = self.read_commit_odb(&id)?;
        let node = self.graph_len + self.extra_nodes.len() as u32;
        self.extra_nodes.push(ExtraNode {
            id,
            tree,
            time,
            parents_start: 0,
            parents_len: UNRESOLVED_PARENTS,
        });
        self.extra_by_id.insert(id, node);
        self.pending.push(0);
        if !self.oid_parents_buf.is_empty() {
            pending_oid_parents.insert(node, self.oid_parents_buf.clone().into_boxed_slice());
        } else {
            // Zero parents can be recorded immediately without boxing.
            let extra = self.extra_nodes.last_mut().expect("just pushed");
            extra.parents_len = 0;
        }
        Ok(node)
    }

    fn load_parent_nodes_for_count(
        &mut self,
        node: u32,
        pending_oid_parents: &mut HashMap<u32, Box<[Oid]>>,
    ) -> Result<()> {
        self.parents_buf.clear();
        if node < self.graph_len {
            let graph = self.graph.as_ref().expect("graph_len > 0 implies graph");
            let id = graph.id_at(Position(node)).to_owned();
            if self.is_boundary(id) {
                return Ok(());
            }
            if !self.graph_corrupt {
                let commit = graph.commit_at(Position(node));
                let mut corrupt = false;
                for parent in commit.iter_parents() {
                    match parent {
                        Ok(pos) => self.parents_buf.push(pos.0),
                        Err(_) => {
                            corrupt = true;
                            break;
                        }
                    }
                }
                if !corrupt {
                    return Ok(());
                }
                self.graph_corrupt = true;
                self.parents_buf.clear();
            }
            self.read_commit_odb(&id)?;
            for i in 0..self.oid_parents_buf.len() {
                let parent_id = self.oid_parents_buf[i];
                let parent_node = self.resolve_oid(parent_id, pending_oid_parents)?;
                self.parents_buf.push(parent_node);
            }
            return Ok(());
        }

        let extra_idx = (node - self.graph_len) as usize;
        let id = self.extra_nodes[extra_idx].id;
        if self.is_boundary(id) {
            pending_oid_parents.remove(&node);
            self.extra_nodes[extra_idx].parents_len = 0;
            return Ok(());
        }
        let len = self.extra_nodes[extra_idx].parents_len;
        if len != UNRESOLVED_PARENTS {
            let start = self.extra_nodes[extra_idx].parents_start as usize;
            self.parents_buf
                .extend_from_slice(&self.extra_parents[start..start + len as usize]);
            return Ok(());
        }
        let Some(parent_oids) = pending_oid_parents.remove(&node) else {
            self.extra_nodes[extra_idx].parents_len = 0;
            return Ok(());
        };
        let start = self.extra_parents.len() as u32;
        for &parent_oid in parent_oids.iter() {
            let parent_node = self.resolve_oid(parent_oid, pending_oid_parents)?;
            self.extra_parents.push(parent_node);
        }
        let resolved_len = self.extra_parents.len() as u32 - start;
        self.extra_nodes[extra_idx].parents_start = start;
        self.extra_nodes[extra_idx].parents_len = resolved_len;
        self.parents_buf
            .extend_from_slice(&self.extra_parents[start as usize..]);
        Ok(())
    }

    fn load_parents_for_emit(&mut self, node: u32) -> Result<(Oid, Option<Oid>, Option<u32>)> {
        self.parents_buf.clear();
        if node < self.graph_len {
            let graph = self.graph.as_ref().expect("graph_len > 0 implies graph");
            let id = graph.id_at(Position(node)).to_owned();
            let boundary = self.is_boundary(id);
            if !self.graph_corrupt {
                let commit = graph.commit_at(Position(node));
                let tree = commit.root_tree_id().to_owned();
                if boundary {
                    return Ok((id, Some(tree), Some(node)));
                }
                let mut corrupt = false;
                for parent in commit.iter_parents() {
                    match parent {
                        Ok(pos) => self.parents_buf.push(pos.0),
                        Err(_) => {
                            corrupt = true;
                            break;
                        }
                    }
                }
                if !corrupt {
                    return Ok((id, Some(tree), Some(node)));
                }
                self.graph_corrupt = true;
                self.parents_buf.clear();
            }
            if boundary {
                return Ok((id, None, None));
            }
            self.read_commit_odb(&id)?;
            for i in 0..self.oid_parents_buf.len() {
                let parent_id = self.oid_parents_buf[i];
                if let Some(pnode) = self.lookup_existing_node(parent_id) {
                    self.parents_buf.push(pnode);
                }
            }
            return Ok((id, None, None));
        }

        let extra = &self.extra_nodes[(node - self.graph_len) as usize];
        let id = extra.id;
        let tree = extra.tree;
        if !self.is_boundary(id) && extra.parents_len != UNRESOLVED_PARENTS {
            let start = extra.parents_start as usize;
            let end = start + extra.parents_len as usize;
            self.parents_buf
                .extend_from_slice(&self.extra_parents[start..end]);
        }
        Ok((id, Some(tree), None))
    }

    fn lookup_existing_node(&self, id: Oid) -> Option<u32> {
        if let Some(graph) = self.graph.as_ref()
            && let Some(pos) = graph.lookup(id)
        {
            return Some(pos.0);
        }
        self.extra_by_id.get(&id).copied()
    }

    fn node_time(&mut self, node: u32) -> Result<i64> {
        if node < self.graph_len {
            if !self.graph_corrupt {
                return Ok(self
                    .graph
                    .as_ref()
                    .expect("graph_len > 0 implies graph")
                    .commit_at(Position(node))
                    .committer_timestamp() as i64);
            }
            let id = self.id_of(node);
            let (_, time) = self.read_commit_odb(&id)?;
            return Ok(time);
        }
        Ok(self.extra_nodes[(node - self.graph_len) as usize].time)
    }

    fn read_commit_odb(&mut self, id: &Oid) -> Result<(Oid, i64)> {
        self.oid_parents_buf.clear();
        let iter = self
            .objects
            .find_commit_iter(id, &mut self.object_buf)
            .map_err(Error::gix)?;
        let mut tree = Oid::null(id.kind());
        let mut time = 0i64;
        for token in iter {
            match token.map_err(Error::gix)? {
                CommitToken::Tree { id } => tree = id,
                CommitToken::Parent { id } => self.oid_parents_buf.push(id),
                CommitToken::Committer { signature } => {
                    time = signature.time().map_or(0, |t| t.seconds);
                    break;
                }
                _ => {}
            }
        }
        Ok((tree, time))
    }
}
