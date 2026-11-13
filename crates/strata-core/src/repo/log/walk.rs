//! Incremental date-ordered topological commit walk.
//!
//! This mirrors git's topo walk (`init_topo_walk` / `expand_topo_walk` in
//! `revision.c`): in-degree counting records how many children of a commit
//! still have to be emitted, and a priority queue keyed by committer date picks
//! the next commit among those that are ready. Generation numbers from the
//! commit-graph bound the eager in-degree pass, so a repository with a graph
//! yields its first commits without walking all of history. Ties on the
//! committer date pop in insertion order, as git's `prio_queue` does.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use gix::objs::FindExt;

use super::CancelCheck;
use crate::{Error, Result};

/// Commits outside the commit-graph have no generation number, so they can
/// never fall below a cutoff and are always part of the in-degree pass.
const INFINITE_GENERATION: u32 = u32::MAX;
/// Sentinel for a node whose parents were not loaded yet.
const NO_PARENTS: u32 = u32::MAX;

const SEEN: u8 = 1;
const EXPLORED: u8 = 1 << 1;
const IN_DEGREE: u8 = 1 << 2;
const ADDED: u8 = 1 << 3;

/// `(generation, committer time, node)`; newest and deepest first. Used by the
/// explore and in-degree passes, whose pop order does not affect the result.
type GenerationKey = (u32, i64, u32);

/// `(committer time, insertion order, node)`; newest first with equal dates
/// resolved in insertion order, matching git's `prio_queue`.
type DateKey = (i64, Reverse<u32>, u32);

struct Node {
    id: gix::ObjectId,
    /// Commit-graph generation number, or [`INFINITE_GENERATION`] if unknown.
    generation: u32,
    time: i64,
    in_degree: u32,
    flags: u8,
    parents_start: u32,
    /// Number of entries in the parent arena, or [`NO_PARENTS`] until loaded.
    parents_len: u32,
}

/// The fields needed to order a commit, read together with its parents when it
/// has to come from the object database.
struct Metadata {
    generation: u32,
    time: i64,
    parents: Option<Box<[gix::ObjectId]>>,
}

/// Outcome of reading a commit's parents from the commit-graph.
enum GraphParents {
    Found,
    /// The commit is not in the graph, which is normal for history written after
    /// the last `git commit-graph write`.
    Unknown,
    /// The graph itself is unreadable.
    Corrupt,
}

pub(super) struct Walk {
    objects: gix::OdbHandle,
    graph: Option<gix::commitgraph::Graph>,
    /// Whether a graph was loaded, even after a corrupt one is dropped.
    graph_present: bool,
    /// Reachable commits read from the object database instead of the graph.
    commits_outside_graph: usize,
    nodes: Vec<Node>,
    by_id: HashMap<gix::ObjectId, u32>,
    /// Parent node indices, one run per node, to avoid re-reading commits.
    parent_arena: Vec<u32>,
    /// Parents parsed alongside metadata for commits missing from the
    /// commit-graph, kept out of `Node` so graph-backed nodes stay compact.
    preloaded_parents: HashMap<u32, Box<[gix::ObjectId]>>,
    explore: BinaryHeap<GenerationKey>,
    in_degree_queue: BinaryHeap<GenerationKey>,
    ready: BinaryHeap<DateKey>,
    next_sequence: u32,
    min_generation: u32,
    parents_buf: Vec<gix::ObjectId>,
    object_buf: Vec<u8>,
    /// `None` for an uncancellable walk, which is every walk the UI starts.
    cancelled: Option<CancelCheck>,
    estimated_commits: Option<usize>,
}

impl Walk {
    /// Collect the reachable commits under `tips`, ready to emit in
    /// date-ordered topological order.
    pub(super) fn new(
        objects: gix::OdbHandle,
        graph: Option<gix::commitgraph::Graph>,
        tips: &[gix::ObjectId],
        cancelled: Option<CancelCheck>,
        reserve_from_graph: bool,
    ) -> Result<Self> {
        let estimated_commits = if reserve_from_graph {
            graph.as_ref().map(|g| g.num_commits() as usize)
        } else {
            None
        };
        let initial = estimated_commits.unwrap_or(0);
        let mut walk = Walk {
            objects,
            graph_present: graph.is_some(),
            graph,
            commits_outside_graph: 0,
            nodes: Vec::with_capacity(initial),
            by_id: HashMap::with_capacity(initial),
            parent_arena: Vec::with_capacity(initial),
            preloaded_parents: HashMap::new(),
            explore: BinaryHeap::new(),
            in_degree_queue: BinaryHeap::new(),
            ready: BinaryHeap::new(),
            next_sequence: 0,
            min_generation: INFINITE_GENERATION,
            parents_buf: Vec::new(),
            object_buf: Vec::new(),
            cancelled,
            estimated_commits,
        };

        let mut seeds = Vec::with_capacity(tips.len());
        for &tip in tips {
            let index = walk.node(tip)?;
            let node = &mut walk.nodes[index as usize];
            if node.flags & SEEN != 0 {
                continue;
            }
            node.flags |= SEEN | EXPLORED | IN_DEGREE;
            // A tip counts as its own already-emitted child, so it is ready as
            // soon as no other tip reaches it.
            node.in_degree = 1;
            walk.min_generation = walk.min_generation.min(node.generation);
            walk.explore.push((node.generation, node.time, index));
            walk.in_degree_queue
                .push((node.generation, node.time, index));
            seeds.push(index);
        }

        walk.compute_in_degrees_to_depth(walk.min_generation)?;
        if walk.is_cancelled() {
            return Ok(walk);
        }

        for index in seeds {
            if walk.nodes[index as usize].in_degree == 1 {
                walk.push_ready(index);
            }
        }

        Ok(walk)
    }

    pub(super) fn estimated_commits(&self) -> Option<usize> {
        self.estimated_commits
    }

    pub(super) fn has_commit_graph(&self) -> bool {
        self.graph_present
    }

    pub(super) fn commits_outside_graph(&self) -> usize {
        self.commits_outside_graph
    }

    /// The next commit in `--date-order`, or `None` once every reachable
    /// commit was emitted or the walk was cancelled.
    pub(super) fn next_id(&mut self) -> Result<Option<gix::ObjectId>> {
        if self.is_cancelled() {
            return Ok(None);
        }
        let Some((_, _, index)) = self.ready.pop() else {
            return Ok(None);
        };
        self.nodes[index as usize].in_degree = 0;
        self.expand(index as usize)?;
        Ok(Some(self.nodes[index as usize].id))
    }

    fn is_cancelled(&self) -> bool {
        match &self.cancelled {
            Some(cancelled) => cancelled(),
            None => false,
        }
    }

    fn push_ready(&mut self, index: u32) {
        let time = self.nodes[index as usize].time;
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.ready.push((time, Reverse(sequence), index));
    }

    /// Return the node for `id`, reading its metadata on first sight.
    fn node(&mut self, id: gix::ObjectId) -> Result<u32> {
        if let Some(&index) = self.by_id.get(&id) {
            return Ok(index);
        }
        let metadata = self.metadata(&id)?;
        let index = self.nodes.len() as u32;
        self.nodes.push(Node {
            id,
            generation: metadata.generation,
            time: metadata.time,
            in_degree: 0,
            flags: 0,
            parents_start: 0,
            parents_len: NO_PARENTS,
        });
        if let Some(parents) = metadata.parents {
            self.preloaded_parents.insert(index, parents);
        }
        self.by_id.insert(id, index);
        Ok(index)
    }

    /// Generation number and committer time, from the commit-graph when it
    /// knows the commit and from the object database otherwise.
    fn metadata(&mut self, id: &gix::ObjectId) -> Result<Metadata> {
        if let Some(commit) = self.graph.as_ref().and_then(|graph| graph.commit_by_id(id)) {
            return Ok(Metadata {
                generation: commit.generation(),
                time: commit.committer_timestamp() as i64,
                parents: None,
            });
        }
        if self.graph.is_some() {
            // The graph is usable but predates this commit: history written
            // after the last `git commit-graph write`.
            self.commits_outside_graph += 1;
        }
        let iter = self
            .objects
            .find_commit_iter(id, &mut self.object_buf)
            .map_err(Error::gix)?;
        // `CommitRefIter` is a copy, so parents and committer time share one
        // parse of the object.
        let parents: Box<[gix::ObjectId]> = iter.parent_ids().collect();
        let time = iter
            .committer()
            .map_err(Error::gix)?
            // Malformed dates must not fail the whole log; git also tolerates them.
            .time()
            .map_or(0, |time| time.seconds);
        Ok(Metadata {
            generation: INFINITE_GENERATION,
            time,
            parents: Some(parents),
        })
    }

    /// Load the parent list of `index` into the arena and resolve parent nodes.
    fn load_parents(&mut self, index: usize) -> Result<()> {
        if self.nodes[index].parents_len != NO_PARENTS {
            return Ok(());
        }
        self.parents_buf.clear();
        if let Some(parents) = self.preloaded_parents.remove(&(index as u32)) {
            self.parents_buf.extend_from_slice(&parents);
        } else {
            let id = self.nodes[index].id;
            match self.graph_parents(&id) {
                GraphParents::Found => {}
                GraphParents::Unknown => self.odb_parents(&id)?,
                GraphParents::Corrupt => {
                    // A corrupt graph must not hide the history: drop it and read
                    // this commit, and every later one, from the object database.
                    self.graph = None;
                    self.graph_present = false;
                    self.odb_parents(&id)?;
                }
            }
        }

        let start = self.parent_arena.len() as u32;
        for position in 0..self.parents_buf.len() {
            let parent = self.parents_buf[position];
            let parent_index = self.node(parent)?;
            self.parent_arena.push(parent_index);
        }
        let node = &mut self.nodes[index];
        node.parents_start = start;
        node.parents_len = self.parent_arena.len() as u32 - start;
        Ok(())
    }

    /// Fill the parent buffer from the commit-graph.
    fn graph_parents(&mut self, id: &gix::ObjectId) -> GraphParents {
        let Some(graph) = self.graph.as_ref() else {
            return GraphParents::Unknown;
        };
        let Some(commit) = graph.commit_by_id(id) else {
            return GraphParents::Unknown;
        };
        for parent in commit.iter_parents() {
            let Ok(parent) = parent else {
                self.parents_buf.clear();
                return GraphParents::Corrupt;
            };
            self.parents_buf
                .push(graph.commit_at(parent).id().to_owned());
        }
        GraphParents::Found
    }

    fn odb_parents(&mut self, id: &gix::ObjectId) -> Result<()> {
        let objects = &self.objects;
        let iter = objects
            .find_commit_iter(id, &mut self.object_buf)
            .map_err(Error::gix)?;
        for parent in iter.parent_ids() {
            self.parents_buf.push(parent);
        }
        Ok(())
    }

    fn parent_range(&self, index: usize) -> std::ops::Range<usize> {
        let node = &self.nodes[index];
        let start = node.parents_start as usize;
        start..start + node.parents_len as usize
    }

    fn compute_in_degrees_to_depth(&mut self, cutoff: u32) -> Result<()> {
        while let Some(&(generation, _, _)) = self.in_degree_queue.peek() {
            if generation < cutoff || self.is_cancelled() {
                break;
            }
            self.in_degree_step()?;
        }
        Ok(())
    }

    fn in_degree_step(&mut self) -> Result<()> {
        let Some((generation, _, index)) = self.in_degree_queue.pop() else {
            return Ok(());
        };
        self.explore_to_depth(generation)?;
        self.load_parents(index as usize)?;

        for position in self.parent_range(index as usize) {
            let parent = self.parent_arena[position] as usize;
            let node = &mut self.nodes[parent];
            node.in_degree = if node.in_degree == 0 {
                2
            } else {
                node.in_degree + 1
            };
            if node.flags & IN_DEGREE == 0 {
                node.flags |= IN_DEGREE;
                self.in_degree_queue
                    .push((node.generation, node.time, parent as u32));
            }
        }
        Ok(())
    }

    fn explore_to_depth(&mut self, cutoff: u32) -> Result<()> {
        while let Some(&(generation, _, _)) = self.explore.peek() {
            if generation < cutoff || self.is_cancelled() {
                break;
            }
            self.explore_step()?;
        }
        Ok(())
    }

    fn explore_step(&mut self) -> Result<()> {
        let Some((_, _, index)) = self.explore.pop() else {
            return Ok(());
        };
        let index = index as usize;
        self.load_parents(index)?;
        self.mark_parents_seen(index);

        for position in self.parent_range(index) {
            let parent = self.parent_arena[position] as usize;
            let node = &mut self.nodes[parent];
            if node.flags & EXPLORED == 0 {
                node.flags |= EXPLORED;
                self.explore
                    .push((node.generation, node.time, parent as u32));
            }
        }
        Ok(())
    }

    /// Mark every parent of `index` as seen. Idempotent, as git's
    /// `process_parents` is for the same traversal.
    fn mark_parents_seen(&mut self, index: usize) {
        if self.nodes[index].flags & ADDED != 0 {
            return;
        }
        self.nodes[index].flags |= ADDED;
        for position in self.parent_range(index) {
            let parent = self.parent_arena[position] as usize;
            self.nodes[parent].flags |= SEEN;
        }
    }

    /// Emit `index` from its children's point of view: every parent loses one
    /// child and becomes ready once none is left.
    fn expand(&mut self, index: usize) -> Result<()> {
        self.load_parents(index)?;
        self.mark_parents_seen(index);

        for position in self.parent_range(index) {
            let parent = self.parent_arena[position] as usize;
            let generation = self.nodes[parent].generation;
            if generation < self.min_generation {
                self.min_generation = generation;
                self.compute_in_degrees_to_depth(generation)?;
            }
            self.nodes[parent].in_degree -= 1;
            if self.nodes[parent].in_degree == 1 {
                self.push_ready(parent as u32);
            }
        }
        Ok(())
    }
}
