//! The history loader: one walk at a time, published in chunks.

use std::collections::HashMap;
use std::mem;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use crate::graph::GraphLayout;
use crate::graph::GraphRows;
use crate::repo::{CommitGraphThresholds, CommitWalk};
use crate::session::publish::Shared;
use crate::session::{COMMIT_CHUNK, CommitList, FIRST_COMMIT_CHUNK, HistoryScope, SessionEvent};
use crate::{CommitInfo, HeadInfo, Oid, RefInfo, RemoteInfo, Result, StashInfo, UpstreamInfo};

pub(crate) struct LoadCommand {
    pub(crate) generation: u64,
}

pub(crate) fn run(shared: Arc<Shared>, rx: Receiver<LoadCommand>) {
    while let Ok(command) = rx.recv() {
        if shared.is_shutdown() {
            return;
        }
        // A newer reload superseded this command before the loader got to it.
        if command.generation != shared.reload_generation() {
            continue;
        }
        if let Err(error) = load(&shared, command.generation) {
            shared.emit(SessionEvent::LoadFailed {
                error: error.to_string(),
            });
        }
    }
}

fn load(shared: &Arc<Shared>, generation: u64) -> Result<()> {
    let head = shared.repo.head()?;
    let refs = Arc::new(shared.repo.refs()?);
    let refs_by_commit = Arc::new(refs_by_commit(&refs));
    // A stash moves `refs/stash`, so it reloads with the other refs.
    let stashes = Arc::new(shared.repo.stashes()?);
    // Tracking state is computed from refs, so it belongs to the same snapshot
    // update: the UI must never show badges for upstreams that lag behind them.
    let upstreams = Arc::new(shared.repo.upstreams()?);
    // Remotes come from the configuration, not from refs: one that was never
    // fetched still has to be offered for fetching and pushing.
    let remotes = shared.repo.remotes()?;
    // The scope is read here, not carried by the command: a reload that
    // superseded this one fails the generation check before any walking starts.
    // The walk builds its reachability state before yielding the first commit,
    // which is the longest uncancellable window of a load. A drop or a newer
    // reload must stop it there, not only between published rows.
    let cancelled: Arc<dyn Fn() -> bool + Send + Sync> = {
        let shared = Arc::clone(shared);
        Arc::new(move || !shared.is_current(generation))
    };
    let started = Instant::now();
    let walk = match shared.history_scope() {
        HistoryScope::All => shared
            .repo
            .walk_commits_with(&shared.hidden_refs(), Some(cancelled))?,
        HistoryScope::Refs(specs) => shared
            .repo
            .walk_commits_from_refs_with(&specs, Some(cancelled))?,
    };
    // Building the walk is the reachability pass that a commit-graph shortens.
    // Measuring it here is what lets the UI suggest a graph only on a
    // repository large enough to need one.
    let walk_time = started.elapsed();
    let advice = shared
        .repo
        .commit_graph_facts(&walk, walk_time)
        .advise(CommitGraphThresholds::default());
    if let Some(advice) = advice {
        shared.emit(SessionEvent::CommitGraphSuggested { advice });
    }

    let estimated = walk.estimated_commits().unwrap_or(0);
    Loader {
        shared: shared.as_ref(),
        generation,
        head,
        refs,
        refs_by_commit,
        stashes,
        upstreams,
        remotes,
        commits: CommitList::default(),
        index: HashMap::with_capacity(estimated),
        loaded: 0,
        published: 0,
        next_index_publish: 16_384,
        pending_commits: Vec::with_capacity(FIRST_COMMIT_CHUNK),
        pending_rows: GraphRows::with_capacity(FIRST_COMMIT_CHUNK, 0, 0),
        last_passes_len: 0,
        last_edges_len: 0,
    }
    .run(walk)
}

fn refs_by_commit(refs: &[RefInfo]) -> HashMap<Oid, Vec<usize>> {
    let mut map: HashMap<Oid, Vec<usize>> = HashMap::with_capacity(refs.len());
    for (index, reference) in refs.iter().enumerate() {
        map.entry(reference.target).or_default().push(index);
    }
    for indices in map.values_mut() {
        indices.shrink_to_fit();
    }
    map.shrink_to_fit();
    map
}

struct Loader<'a> {
    shared: &'a Shared,
    generation: u64,
    head: HeadInfo,
    refs: Arc<Vec<RefInfo>>,
    refs_by_commit: Arc<HashMap<Oid, Vec<usize>>>,
    stashes: Arc<Vec<StashInfo>>,
    upstreams: Arc<HashMap<String, UpstreamInfo>>,
    remotes: Vec<RemoteInfo>,
    commits: CommitList,
    index: HashMap<Oid, usize>,
    loaded: usize,
    published: usize,
    next_index_publish: usize,
    pending_commits: Vec<CommitInfo>,
    pending_rows: GraphRows,
    last_passes_len: usize,
    last_edges_len: usize,
}

impl Loader<'_> {
    fn run(mut self, walk: CommitWalk) -> Result<()> {
        let mut layout = GraphLayout::new();

        for item in walk {
            // A reload superseded this walk: its older rows must never reach a
            // snapshot that the newer walk already owns.
            if !self.shared.is_current(self.generation) {
                return Ok(());
            }

            let commit = item?;
            layout.push(commit.id, &commit.parents, &mut self.pending_rows);
            self.index.insert(commit.id, self.loaded);
            self.loaded += 1;
            self.pending_commits.push(commit);

            if self.publish_due() && !self.publish(false) {
                return Ok(());
            }
        }

        self.publish(true);
        Ok(())
    }

    fn publish_due(&self) -> bool {
        publish_due(self.published, self.loaded)
    }

    fn tight_index(&self) -> HashMap<Oid, usize> {
        let mut copy = HashMap::with_capacity(self.index.len());
        copy.extend(self.index.iter().map(|(&id, &row)| (id, row)));
        copy
    }

    fn publish(&mut self, complete: bool) -> bool {
        let batch_passes = self.pending_rows.passes_len();
        let batch_edges = self.pending_rows.edges_len();
        if self.pending_commits.len() >= COMMIT_CHUNK / 2 {
            self.last_passes_len = batch_passes;
            self.last_edges_len = batch_edges;
        }

        // `extend` moves full chunks without copying once aligned to `COMMIT_CHUNK`.
        self.commits.extend(
            mem::take(&mut self.pending_commits),
            mem::take(&mut self.pending_rows),
        );
        self.commits.complete = complete;
        if complete {
            self.commits.shrink_to_fit();
        }

        let commit_index = if complete {
            let mut index = mem::take(&mut self.index);
            index.shrink_to_fit();
            Some(Arc::new(index))
        } else if self.published == 0 {
            Some(Arc::new(self.tight_index()))
        } else if self.loaded >= self.next_index_publish {
            while self.next_index_publish <= self.loaded {
                self.next_index_publish = self.next_index_publish.saturating_mul(4);
            }
            Some(Arc::new(self.tight_index()))
        } else {
            None
        };

        self.published = self.loaded;
        if !complete {
            let next_commits = if self.published == FIRST_COMMIT_CHUNK {
                COMMIT_CHUNK - FIRST_COMMIT_CHUNK
            } else {
                COMMIT_CHUNK
            };
            self.pending_commits = Vec::with_capacity(next_commits);
            self.pending_rows = GraphRows::with_capacity(
                next_commits,
                self.last_passes_len + self.last_passes_len / 8,
                self.last_edges_len + self.last_edges_len / 8,
            );
        }

        let commits = self.commits.clone();
        let head = self.head.clone();
        let refs = Arc::clone(&self.refs);
        let refs_by_commit = Arc::clone(&self.refs_by_commit);
        let stashes = Arc::clone(&self.stashes);
        let upstreams = Arc::clone(&self.upstreams);
        let remotes = self.remotes.clone();

        self.shared
            .publish_for_generation(self.generation, move |snapshot| {
                snapshot.head = Some(head);
                snapshot.refs = refs;
                snapshot.stashes = stashes;
                snapshot.upstreams = upstreams;
                snapshot.remotes = remotes;
                snapshot.refs_by_commit = refs_by_commit;
                snapshot.commits = commits;
                if let Some(commit_index) = commit_index {
                    snapshot.commit_index = commit_index;
                }
            })
    }
}

/// Whether a walk that loaded `loaded` commits and published `published` of
/// them should publish again.
///
/// The first publish lands at `FIRST_COMMIT_CHUNK`; the second completes the
/// first `COMMIT_CHUNK` so all later publishes stay chunk-aligned and move
/// into `CommitList` without copying partial tails.
fn publish_due(published: usize, loaded: usize) -> bool {
    if published == 0 {
        loaded >= FIRST_COMMIT_CHUNK
    } else if published < COMMIT_CHUNK {
        loaded >= COMMIT_CHUNK
    } else {
        loaded - published >= COMMIT_CHUNK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_publish_is_a_small_chunk() {
        assert!(!publish_due(0, FIRST_COMMIT_CHUNK - 1));
        assert!(publish_due(0, FIRST_COMMIT_CHUNK));
    }

    #[test]
    fn second_publish_completes_the_first_chunk_then_waits_for_full_chunks() {
        let published = FIRST_COMMIT_CHUNK;
        assert!(!publish_due(published, COMMIT_CHUNK - 1));
        assert!(publish_due(published, COMMIT_CHUNK));

        let aligned = COMMIT_CHUNK;
        assert!(!publish_due(aligned, aligned + COMMIT_CHUNK - 1));
        assert!(publish_due(aligned, aligned + COMMIT_CHUNK));
        // A walk that ends earlier publishes only its tail, from `run`.
        assert!(!publish_due(
            aligned + COMMIT_CHUNK,
            aligned + COMMIT_CHUNK + 1
        ));
    }
}
