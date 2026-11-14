//! The content-search worker: history `-S`/`-G` search runs on its own thread.
//!
//! Content search walks history and diffs first-parent trees across commits,
//! which can take seconds on large repositories. Running it on the query worker
//! would stall commit stats and snapshot searches while matches stream in, so
//! it gets its own thread and channel while sharing [`QueryState`] tickets and
//! cancellation.

use std::mem;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crate::Result;
use crate::repo::ContentSearchQuery;
use crate::session::publish::Shared;
use crate::session::queries::QueryState;
use crate::session::{HistoryScope, QueryTicket, SessionEvent};

const CONTENT_BATCH_COMMITS: usize = 256;
const CONTENT_BATCH_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) struct ContentSearchJob {
    pub(crate) ticket: QueryTicket,
    pub(crate) query: ContentSearchQuery,
}

pub(crate) fn run(shared: Arc<Shared>, state: Arc<QueryState>, jobs: Receiver<ContentSearchJob>) {
    while let Ok(job) = jobs.recv() {
        if shared.is_shutdown() {
            return;
        }
        if state.is_cancelled(job.ticket) {
            state.forget(job.ticket);
            continue;
        }

        let failure = stream_content_search(&shared, &state, job.ticket, &job.query)
            .err()
            .map(|error| error.to_string());
        if let Some(error) = failure
            && !state.is_cancelled(job.ticket)
            && !shared.is_shutdown()
        {
            shared.emit(SessionEvent::QueryFailed {
                ticket: job.ticket,
                error,
            });
        }
        state.forget(job.ticket);
    }
}

fn stream_content_search(
    shared: &Arc<Shared>,
    state: &Arc<QueryState>,
    ticket: QueryTicket,
    query: &ContentSearchQuery,
) -> Result<()> {
    let cancelled = {
        let shared = Arc::clone(shared);
        let state = Arc::clone(state);
        Arc::new(move || state.is_cancelled(ticket) || shared.is_shutdown())
    };
    let scope = shared.history_scope();
    let specs = match &scope {
        HistoryScope::All => None,
        HistoryScope::Refs(refs) => Some(refs.as_slice()),
    };
    let mut entries = shared
        .repo
        .content_search_cancellable(query, specs, cancelled)?;

    let mut batch = Vec::new();
    let mut last_emit = Instant::now();
    let mut sent_first = false;

    loop {
        if state.is_cancelled(ticket) || shared.is_shutdown() {
            return Ok(());
        }
        match entries.advance() {
            None => break,
            Some(Err(err)) => return Err(err),
            Some(Ok(Some(entry))) => {
                batch.push(entry);
                let progress = entries.progress();
                if !sent_first
                    || batch.len() >= CONTENT_BATCH_COMMITS
                    || last_emit.elapsed() >= CONTENT_BATCH_INTERVAL
                {
                    if state.is_cancelled(ticket) || shared.is_shutdown() {
                        return Ok(());
                    }
                    shared.emit(SessionEvent::ContentSearchBatch {
                        ticket,
                        matches: Arc::new(mem::take(&mut batch)),
                        scanned: progress.scanned,
                        total: progress.total,
                    });
                    sent_first = true;
                    last_emit = Instant::now();
                }
            }
            Some(Ok(None)) => {
                if state.is_cancelled(ticket) || shared.is_shutdown() {
                    return Ok(());
                }
                if last_emit.elapsed() >= CONTENT_BATCH_INTERVAL
                    || (!sent_first && !batch.is_empty())
                {
                    let progress = entries.progress();
                    if batch.is_empty() {
                        shared.emit(SessionEvent::ContentSearchProgress {
                            ticket,
                            scanned: progress.scanned,
                            total: progress.total,
                        });
                    } else {
                        shared.emit(SessionEvent::ContentSearchBatch {
                            ticket,
                            matches: Arc::new(mem::take(&mut batch)),
                            scanned: progress.scanned,
                            total: progress.total,
                        });
                        sent_first = true;
                    }
                    last_emit = Instant::now();
                }
            }
        }
    }

    if state.is_cancelled(ticket) || shared.is_shutdown() {
        return Ok(());
    }
    let progress = entries.progress();
    if !batch.is_empty() {
        shared.emit(SessionEvent::ContentSearchBatch {
            ticket,
            matches: Arc::new(batch),
            scanned: progress.scanned,
            total: progress.total,
        });
    }
    if state.is_cancelled(ticket) || shared.is_shutdown() {
        return Ok(());
    }
    shared.emit(SessionEvent::ContentSearchDone {
        ticket,
        scanned: progress.scanned,
        total: progress.total,
    });
    Ok(())
}
