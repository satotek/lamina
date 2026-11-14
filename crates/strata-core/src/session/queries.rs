//! The query worker: commit search, path-limited history and commit stats off
//! the UI thread.
//!
//! The two history queries read the published snapshot and answer with row
//! indices into it, tagged with the snapshot generation they saw. A commit stat
//! reads the repository directly and answers with its own event. A query
//! cancelled before its answer is ready emits nothing at all.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::session::publish::Shared;
use crate::session::{
    CommitList, CommitQuery, PathHistoryCommit, QueryTicket, RepoSnapshot, SessionEvent,
};
use crate::{CommitInfo, Oid, Result};

/// A hash prefix shorter than this matches too much to be a useful filter.
const MIN_HASH_CHARS: usize = 4;

/// Cancellation is polled between batches so a long search stops promptly.
const CANCEL_CHECK_ROWS: usize = 4_096;

/// A streaming path walk sends whatever it has after this many matched commits,
/// and at least every [`PATH_BATCH_INTERVAL`], so the file history fills in while
/// an old history is still being walked.
const PATH_BATCH_COMMITS: usize = 1_024;
const PATH_BATCH_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) struct QueryJob {
    pub(crate) ticket: QueryTicket,
    pub(crate) kind: QueryKind,
}

pub(crate) enum QueryKind {
    Search(CommitQuery),
    /// One `QueryReady` when the walk ends.
    Path(String),
    /// `PathHistoryBatch` per batch while the walk runs, then `PathHistoryDone`.
    StreamingPath(String),
    /// Line counts of a commit against its first parent.
    CommitStat(Oid),
    /// Submodules and linked worktrees, read directly from the repository.
    SubmodulesAndWorktrees,
    /// Details of an unfinished merge/rebase/cherry-pick/revert/bisect/am.
    InProgressDetails,
    /// HEAD's and the local branches' reflogs, at most this many entries each.
    Reflog(usize),
}

/// Ticket allocation and cancellation, shared between the session and the
/// worker. Query tickets are numbered independently of diff tickets.
pub(crate) struct QueryState {
    next: AtomicU64,
    cancelled: Mutex<HashSet<QueryTicket>>,
}

impl QueryState {
    pub(crate) fn new() -> Self {
        Self {
            next: AtomicU64::new(0),
            cancelled: Mutex::new(HashSet::new()),
        }
    }

    pub(crate) fn next_ticket(&self) -> QueryTicket {
        QueryTicket(self.next.fetch_add(1, Ordering::SeqCst) + 1)
    }

    pub(crate) fn cancel(&self, ticket: QueryTicket) {
        lock(&self.cancelled).insert(ticket);
    }

    pub(crate) fn is_cancelled(&self, ticket: QueryTicket) -> bool {
        lock(&self.cancelled).contains(&ticket)
    }

    /// Drops the ticket's cancellation once its job is done, so the set only
    /// holds tickets whose job is still alive.
    pub(crate) fn forget(&self, ticket: QueryTicket) {
        lock(&self.cancelled).remove(&ticket);
    }
}

pub(crate) fn run(shared: Arc<Shared>, state: Arc<QueryState>, jobs: Receiver<QueryJob>) {
    while let Ok(job) = jobs.recv() {
        if shared.is_shutdown() {
            return;
        }
        // Cancelled while still queued: the selection moved on before the
        // worker even looked at the job.
        if state.is_cancelled(job.ticket) {
            state.forget(job.ticket);
            continue;
        }

        let failure = answer(Arc::clone(&shared), Arc::clone(&state), &job)
            .err()
            .map(|error| error.to_string());
        if let Some(error) = failure
            && !state.is_cancelled(job.ticket)
        {
            shared.emit(SessionEvent::QueryFailed {
                ticket: job.ticket,
                error,
            });
        }
        state.forget(job.ticket);
    }
}

fn answer(shared: Arc<Shared>, state: Arc<QueryState>, job: &QueryJob) -> Result<()> {
    match &job.kind {
        QueryKind::CommitStat(commit) => {
            let stat = shared.repo.commit_stat(*commit)?;
            if state.is_cancelled(job.ticket) || shared.is_shutdown() {
                return Ok(());
            }
            shared.emit(SessionEvent::CommitStatReady {
                ticket: job.ticket,
                stat: Arc::new(stat),
            });
        }
        QueryKind::SubmodulesAndWorktrees => {
            let submodules = shared.repo.submodules()?;
            let worktrees = shared.repo.worktrees()?;
            if state.is_cancelled(job.ticket) || shared.is_shutdown() {
                return Ok(());
            }
            shared.emit(SessionEvent::SubmodulesAndWorktreesReady {
                ticket: job.ticket,
                submodules: Arc::new(submodules),
                worktrees: Arc::new(worktrees),
            });
        }
        QueryKind::InProgressDetails => {
            let details = shared.repo.in_progress_details()?;
            if state.is_cancelled(job.ticket) || shared.is_shutdown() {
                return Ok(());
            }
            shared.emit(SessionEvent::InProgressDetailsReady {
                ticket: job.ticket,
                details,
            });
        }
        QueryKind::Reflog(limit) => {
            let entries = shared.repo.reflog(*limit)?;
            if state.is_cancelled(job.ticket) || shared.is_shutdown() {
                return Ok(());
            }
            shared.emit(SessionEvent::ReflogReady {
                ticket: job.ticket,
                entries: Arc::new(entries),
            });
        }
        QueryKind::Search(query) => {
            let snapshot = shared.snapshot();
            let generation = snapshot.generation;
            let Some(rows) = search(&snapshot.commits, query, || state.is_cancelled(job.ticket))
            else {
                return Ok(());
            };
            emit_ready(&shared, &state, job.ticket, generation, rows);
        }
        QueryKind::Path(path) => {
            let snapshot = shared.snapshot();
            let generation = snapshot.generation;
            // A path that never matches keeps the walk busy for the whole
            // history, so cancellation has to reach the walk itself.
            let Some(rows) = path_rows(&shared, &state, job.ticket, &snapshot, path)? else {
                return Ok(());
            };
            emit_ready(&shared, &state, job.ticket, generation, rows);
        }
        QueryKind::StreamingPath(path) => {
            stream_path_history(&shared, &state, job.ticket, path)?;
        }
    }
    Ok(())
}

/// The snapshot may have been replaced while the query ran; the rows still
/// belong to `generation`, which the UI uses to decide whether to apply them.
fn emit_ready(
    shared: &Shared,
    state: &QueryState,
    ticket: QueryTicket,
    generation: u64,
    rows: Vec<usize>,
) {
    if state.is_cancelled(ticket) || shared.is_shutdown() {
        return;
    }
    shared.emit(SessionEvent::QueryReady {
        ticket,
        generation,
        rows: Arc::new(rows),
    });
}

/// Matching rows of `snapshot` for a path walk, in walk order. `None` when the
/// ticket was cancelled or the session is shutting down.
fn path_rows(
    shared: &Arc<Shared>,
    state: &Arc<QueryState>,
    ticket: QueryTicket,
    snapshot: &RepoSnapshot,
    path: &str,
) -> Result<Option<Vec<usize>>> {
    let cancelled = {
        let shared = Arc::clone(shared);
        let state = Arc::clone(state);
        Arc::new(move || state.is_cancelled(ticket) || shared.is_shutdown())
    };
    let mut history = shared.repo.path_history_cancellable(path, cancelled)?;
    let mut rows = Vec::new();
    // Drive the walk by hand so cancellation and shutdown are noticed between
    // commits instead of only when a matching row arrives.
    loop {
        if state.is_cancelled(ticket) || shared.is_shutdown() {
            return Ok(None);
        }
        let Some(id) = history.next() else {
            break;
        };
        let id = id?;
        // Commits outside the snapshot were never loaded at this generation, so
        // they have no row to select.
        if let Some(&row) = snapshot.commit_index.get(&id) {
            rows.push(row);
        }
    }
    Ok(Some(rows))
}

/// Walks `path` directly against the repository, streaming `PathHistoryBatch`
/// events in walk order without depending on graph snapshots.
fn stream_path_history(
    shared: &Arc<Shared>,
    state: &Arc<QueryState>,
    ticket: QueryTicket,
    path: &str,
) -> Result<()> {
    let cancelled = {
        let shared = Arc::clone(shared);
        let state = Arc::clone(state);
        Arc::new(move || state.is_cancelled(ticket) || shared.is_shutdown())
    };
    let mut history = shared
        .repo
        .path_history_entries_cancellable(path, cancelled)?;
    let stream = BatchStream::new(Arc::clone(shared), Arc::clone(state), ticket);
    let mut batch = Vec::new();
    let mut parents_buf = Vec::new();
    let mut last_batch = Instant::now();
    let mut sent_first = false;
    loop {
        if state.is_cancelled(ticket) || shared.is_shutdown() {
            return Ok(());
        }
        let Some(entry) = history.next() else {
            break;
        };
        let entry = entry?;
        let commit = history.read_commit_info(entry.commit, &mut parents_buf)?;
        batch.push(PathHistoryCommit {
            commit,
            path: entry.path,
        });
        if !sent_first
            || batch.len() >= PATH_BATCH_COMMITS
            || last_batch.elapsed() >= PATH_BATCH_INTERVAL
        {
            if !stream.batch(std::mem::take(&mut batch)) {
                return Ok(());
            }
            sent_first = true;
            last_batch = Instant::now();
        }
    }
    stream.finish(batch);
    Ok(())
}

/// Batched delivery of a streaming path walk. Holding the shared state, not a
/// borrow of it, keeps the walk loop free to read the repository.
struct BatchStream {
    shared: Arc<Shared>,
    state: Arc<QueryState>,
    ticket: QueryTicket,
}

impl BatchStream {
    fn new(shared: Arc<Shared>, state: Arc<QueryState>, ticket: QueryTicket) -> Self {
        Self {
            shared,
            state,
            ticket,
        }
    }

    /// Sends one batch. `false` means the walk must stop without emitting more.
    fn batch(&self, entries: Vec<PathHistoryCommit>) -> bool {
        if self.state.is_cancelled(self.ticket) || self.shared.is_shutdown() {
            return false;
        }
        self.shared.emit(SessionEvent::PathHistoryBatch {
            ticket: self.ticket,
            entries: Arc::new(entries),
        });
        true
    }

    /// Sends the tail and then `PathHistoryDone`, unless the walk was cancelled.
    fn finish(&self, rows: Vec<PathHistoryCommit>) {
        if !rows.is_empty() && !self.batch(rows) {
            return;
        }
        if self.state.is_cancelled(self.ticket) || self.shared.is_shutdown() {
            return;
        }
        self.shared.emit(SessionEvent::PathHistoryDone {
            ticket: self.ticket,
        });
    }
}

/// Row indices of matching commits in display order, or `None` when the query
/// was cancelled.
fn search(
    commits: &CommitList,
    query: &CommitQuery,
    cancelled: impl Fn() -> bool,
) -> Option<Vec<usize>> {
    let matcher = Matcher::new(query);
    let mut rows = Vec::new();
    for index in 0..commits.len() {
        if index % CANCEL_CHECK_ROWS == 0 && cancelled() {
            return None;
        }
        let Some((commit, _)) = commits.get(index) else {
            continue;
        };
        if matcher.matches(commit) {
            rows.push(index);
        }
    }
    Some(rows)
}

/// A query prepared for repeated matching: the needle is lowercased once, not
/// per commit.
struct Matcher {
    needle: String,
    /// Lowercase hex prefix, when the text can be a hash and hash matching is on.
    hash_prefix: Option<Vec<u8>>,
    in_summary: bool,
    in_author: bool,
}

impl Matcher {
    fn new(query: &CommitQuery) -> Self {
        let text = query.text.trim();
        let hash_prefix = (query.in_hash
            && text.len() >= MIN_HASH_CHARS
            && text.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| text.to_ascii_lowercase().into_bytes());
        Self {
            needle: text.to_lowercase(),
            hash_prefix,
            in_summary: query.in_summary,
            in_author: query.in_author,
        }
    }

    fn matches(&self, commit: &CommitInfo) -> bool {
        // An empty search box is a reset, not a filter that matches nothing.
        if self.needle.is_empty() {
            return true;
        }
        if let Some(prefix) = &self.hash_prefix
            && has_prefix(commit.id, prefix)
        {
            return true;
        }
        (self.in_summary && contains_ignore_case(&commit.summary, &self.needle))
            || (self.in_author
                && (contains_ignore_case(&commit.author_name, &self.needle)
                    || contains_ignore_case(&commit.author_email, &self.needle)))
    }
}

/// Whether the commit id's hex form starts with `prefix`, which must already be
/// lowercase hex.
fn has_prefix(id: Oid, prefix: &[u8]) -> bool {
    let bytes = id.as_bytes();
    if prefix.len() > bytes.len() * 2 {
        return false;
    }
    prefix.iter().enumerate().all(|(index, digit)| {
        let byte = bytes[index / 2];
        let nibble = if index % 2 == 0 {
            byte >> 4
        } else {
            byte & 0x0f
        };
        nibble == hex_value(*digit)
    })
}

fn hex_value(digit: u8) -> u8 {
    match digit {
        b'0'..=b'9' => digit - b'0',
        b'a'..=b'f' => digit - b'a' + 10,
        b'A'..=b'F' => digit - b'A' + 10,
        _ => u8::MAX,
    }
}

/// Case-insensitive substring search. ASCII needles scan bytes: a match can only
/// consist of ASCII bytes, so it cannot start inside a multi-byte character.
/// Non-ASCII needles fall back to lowercasing the haystack.
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.is_ascii() {
        let needle = needle.as_bytes();
        let haystack = haystack.as_bytes();
        needle.len() <= haystack.len()
            && haystack
                .windows(needle.len())
                .any(|window| window.eq_ignore_ascii_case(needle))
    } else {
        haystack.to_lowercase().contains(needle)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::graph::GraphRows;

    const SYNTHETIC_COMMITS: usize = 200_000;

    /// A list shaped like a loaded log: one row per commit, display order, with
    /// a summary and author that only some commits share.
    fn synthetic_list(count: usize) -> CommitList {
        let mut commits = Vec::with_capacity(count);
        let mut rows = GraphRows::new();
        for index in 0..count {
            // `push_row` exists for producers with a synthetic lane model.
            rows.push_row(0, 0, 1, &[], &[]);
            // Keep the index in the leading bytes so a hex prefix is selective,
            // as real ids are.
            let mut bytes = [0u8; 20];
            bytes[..4].copy_from_slice(&(index as u32).to_be_bytes());
            commits.push(CommitInfo {
                id: Oid::from(bytes),
                parents: Vec::new(),
                summary: if index % 1_000 == 0 {
                    format!("Release {index}")
                } else {
                    format!("commit {index}")
                },
                author_name: if index % 2_000 == 0 {
                    "Ada Lovelace".to_string()
                } else {
                    "Strata Test".to_string()
                },
                author_email: "strata@example.com".to_string(),
                author_time: index as i64,
                committer_time: index as i64,
            });
        }
        let mut list = CommitList::default();
        list.extend(commits, rows);
        list
    }

    #[test]
    #[ignore = "perf: 200k synthetic commits, meaningful in release"]
    fn search_over_200k_commits_is_fast() {
        let list = synthetic_list(SYNTHETIC_COMMITS);
        let target = SYNTHETIC_COMMITS / 2;
        let target_prefix = list.get(target).expect("commit").0.id.to_string()[..8].to_string();
        let cases = [
            CommitQuery {
                text: "release".to_string(),
                in_summary: true,
                in_author: false,
                in_hash: false,
            },
            CommitQuery {
                text: "ada lovelace".to_string(),
                in_summary: false,
                in_author: true,
                in_hash: false,
            },
            CommitQuery {
                text: target_prefix,
                in_summary: false,
                in_author: false,
                in_hash: true,
            },
        ];

        for query in &cases {
            let start = Instant::now();
            let rows = search(&list, query, || false).expect("not cancelled");
            let elapsed = start.elapsed();
            eprintln!(
                "search {:?} over 200k commits: {elapsed:?} ({} rows)",
                query.text,
                rows.len()
            );
            assert!(!rows.is_empty());
            // Debug builds are for coverage, not for the timing contract.
            let limit = if cfg!(debug_assertions) {
                Duration::from_secs(2)
            } else {
                Duration::from_millis(50)
            };
            assert!(
                elapsed < limit,
                "search took {elapsed:?}, expected under {limit:?}"
            );
        }
        assert_eq!(search(&list, &cases[2], || false), Some(vec![target]));
    }
}
