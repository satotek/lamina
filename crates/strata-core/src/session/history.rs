//! The history loader: one walk at a time, published in chunks.

use std::collections::HashMap;
use std::mem;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use crate::graph::GraphLayout;
use crate::graph::GraphRows;
use crate::repo::CommitWalk;
use crate::session::publish::Shared;
use crate::session::{COMMIT_CHUNK, CommitList, SessionEvent};
use crate::{CommitInfo, HeadInfo, Oid, RefInfo, Result, StashInfo, UpstreamInfo};

pub(crate) struct LoadCommand {
    pub(crate) generation: u64,
}

/// The first chunk is published well before the walk finishes, so the log
/// appears while older history is still being read.
const FIRST_PUBLISH_COMMITS: usize = 2_000;

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

fn load(shared: &Shared, generation: u64) -> Result<()> {
    let head = shared.repo.head()?;
    let refs = Arc::new(shared.repo.refs()?);
    let refs_by_commit = Arc::new(refs_by_commit(&refs));
    // A stash moves `refs/stash`, so it reloads with the other refs.
    let stashes = Arc::new(shared.repo.stashes()?);
    // Tracking state is computed from refs, so it belongs to the same snapshot
    // update: the UI must never show badges for upstreams that lag behind them.
    let upstreams = Arc::new(shared.repo.upstreams()?);
    let walk = shared.repo.walk_commits()?;

    Loader {
        shared,
        generation,
        head,
        refs,
        refs_by_commit,
        stashes,
        upstreams,
        commits: CommitList::default(),
        index: HashMap::new(),
        loaded: 0,
        published: 0,
        pending_commits: Vec::new(),
        pending_rows: GraphRows::new(),
    }
    .run(walk)
}

fn refs_by_commit(refs: &[RefInfo]) -> HashMap<Oid, Vec<usize>> {
    let mut map: HashMap<Oid, Vec<usize>> = HashMap::new();
    for (index, reference) in refs.iter().enumerate() {
        map.entry(reference.target).or_default().push(index);
    }
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
    commits: CommitList,
    index: HashMap<Oid, usize>,
    loaded: usize,
    published: usize,
    pending_commits: Vec<CommitInfo>,
    pending_rows: GraphRows,
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
        if self.published == 0 {
            self.loaded >= FIRST_PUBLISH_COMMITS
        } else {
            self.loaded - self.published >= COMMIT_CHUNK
        }
    }

    fn publish(&mut self, complete: bool) -> bool {
        // `extend` copies only the last chunk, so a growing published list costs
        // one partial chunk per publish instead of the whole history.
        self.commits.extend(
            mem::take(&mut self.pending_commits),
            mem::take(&mut self.pending_rows),
        );
        self.commits.complete = complete;
        self.published = self.loaded;

        let commits = self.commits.clone();
        let commit_index = Arc::new(self.index.clone());
        let head = self.head.clone();
        let refs = Arc::clone(&self.refs);
        let refs_by_commit = Arc::clone(&self.refs_by_commit);
        let stashes = Arc::clone(&self.stashes);
        let upstreams = Arc::clone(&self.upstreams);

        self.shared
            .publish_for_generation(self.generation, move |snapshot| {
                snapshot.head = Some(head);
                snapshot.refs = refs;
                snapshot.stashes = stashes;
                snapshot.upstreams = upstreams;
                snapshot.refs_by_commit = refs_by_commit;
                snapshot.commits = commits;
                snapshot.commit_index = commit_index;
            })
    }
}
