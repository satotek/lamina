//! The blame worker: line attribution runs on its own thread.
//!
//! Blame of a long-lived file walks a large part of the history and is far
//! slower than a search or a path history over the loaded snapshot. Running it
//! on the query worker would hold those answers hostage, so it gets its own
//! thread and its own channel. Tickets still come from [`QueryState`], which
//! keeps cancellation and numbering shared with every other query.

use std::sync::Arc;
use std::sync::mpsc::Receiver;

use crate::Oid;
use crate::session::publish::Shared;
use crate::session::queries::QueryState;
use crate::session::{QueryTicket, SessionEvent};

pub(crate) struct BlameJob {
    pub(crate) ticket: QueryTicket,
    pub(crate) commit: Oid,
    pub(crate) path: String,
}

pub(crate) fn run(shared: Arc<Shared>, state: Arc<QueryState>, jobs: Receiver<BlameJob>) {
    while let Ok(job) = jobs.recv() {
        if shared.is_shutdown() {
            return;
        }
        // A selection that moved on before the worker picked the job up never
        // gets an answer.
        if state.is_cancelled(job.ticket) {
            state.forget(job.ticket);
            continue;
        }

        let result = shared
            .repo
            .blame_cancellable(job.commit, &job.path, &shared.git, &|| {
                state.is_cancelled(job.ticket) || shared.is_shutdown()
            });
        match result {
            Ok(Some(blame)) if !state.is_cancelled(job.ticket) && !shared.is_shutdown() => {
                shared.emit(SessionEvent::BlameReady {
                    ticket: job.ticket,
                    blame: Arc::new(blame),
                });
            }
            Ok(Some(_)) | Ok(None) => {}
            Err(error) if !state.is_cancelled(job.ticket) && !shared.is_shutdown() => {
                shared.emit(SessionEvent::QueryFailed {
                    ticket: job.ticket,
                    error: error.to_string(),
                });
            }
            Err(_) => {}
        }
        state.forget(job.ticket);
    }
}
