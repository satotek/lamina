//! Diff workers: two threads, so a pinned diff does not queue behind the
//! selection the user just clicked.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};

use crate::Result;
use crate::diff::{DiffOptions, diff_text};
use crate::highlight::{Highlighter, language_for_path};
use crate::lfs::{self, LfsDiff, LfsDiffSummary};
use crate::repo::Repo;
use crate::session::publish::Shared;
use crate::session::{
    DiffRequest, DiffTicket, DiffView, SessionEvent, first_parent, head_file, worktree_as_git,
};

/// A view never shows text this large, and parsing it would stall the worker.
const MAX_HIGHLIGHT_BYTES: usize = 1024 * 1024;
/// Same limit for LFS objects: beyond it the summary is more useful than a
/// diff the pane cannot highlight.
const MAX_LFS_DIFF_BYTES: usize = 1024 * 1024;

pub(crate) struct DiffJob {
    pub(crate) ticket: DiffTicket,
    pub(crate) request: DiffRequest,
    pub(crate) options: DiffOptions,
}

pub(crate) fn run(shared: Arc<Shared>, jobs: Arc<Mutex<Receiver<DiffJob>>>) {
    let mut highlighter = Highlighter::new();
    loop {
        let job = {
            let guard = jobs.lock().unwrap_or_else(PoisonError::into_inner);
            guard.recv()
        };
        let Ok(job) = job else {
            return;
        };
        if shared.is_shutdown() {
            return;
        }

        // Cancelled while still queued: the request may have scrolled away
        // before a worker even looked at it.
        if shared.is_cancelled(job.ticket) {
            shared.forget_cancellation(job.ticket);
            continue;
        }

        if let Err(error) = compute(&shared, &job, &mut highlighter)
            && !shared.is_cancelled(job.ticket)
        {
            shared.emit(SessionEvent::DiffFailed {
                ticket: job.ticket,
                error: error.to_string(),
            });
        }
        shared.forget_cancellation(job.ticket);
    }
}

fn compute(shared: &Shared, job: &DiffJob, highlighter: &mut Highlighter) -> Result<()> {
    let (old, new) = read_sides(&shared.repo, &job.request)?;
    let (old_bytes, new_bytes, lfs) = diff_inputs(&shared.repo, old, new);

    let diff = diff_text(&old_bytes, &new_bytes, &job.options);
    let binary = diff.binary;
    let view = Arc::new(DiffView {
        request: job.request.clone(),
        diff,
        old_highlights: Arc::new(Vec::new()),
        new_highlights: Arc::new(Vec::new()),
        lfs,
    });
    if !emit(shared, job.ticket, Arc::clone(&view)) || binary {
        return Ok(());
    }

    let Some(language) = language_for_path(path_of(&job.request)) else {
        return Ok(());
    };
    if old_bytes.len() > MAX_HIGHLIGHT_BYTES || new_bytes.len() > MAX_HIGHLIGHT_BYTES {
        return Ok(());
    }

    let highlighted = Arc::new(DiffView {
        request: view.request.clone(),
        diff: view.diff.clone(),
        old_highlights: Arc::new(
            highlighter.highlight(language, &String::from_utf8_lossy(&old_bytes)),
        ),
        new_highlights: Arc::new(
            highlighter.highlight(language, &String::from_utf8_lossy(&new_bytes)),
        ),
        lfs: view.lfs.clone(),
    });
    emit(shared, job.ticket, highlighted);
    Ok(())
}

/// The bytes to diff, after replacing LFS pointers with their objects where
/// both sides can be read, plus the metadata the pane shows above the diff.
fn diff_inputs(
    repo: &Repo,
    old: Option<Vec<u8>>,
    new: Option<Vec<u8>>,
) -> (Vec<u8>, Vec<u8>, Option<LfsDiffSummary>) {
    match lfs::resolve_diff(
        old.as_deref(),
        new.as_deref(),
        repo.common_dir(),
        MAX_LFS_DIFF_BYTES,
    ) {
        LfsDiff::NotLfs => (old.unwrap_or_default(), new.unwrap_or_default(), None),
        LfsDiff::Real { old, new, summary } => (old, new, Some(summary)),
        // The summary replaces the hunks entirely; diffing the pointer text
        // would show a three-line metadata change as if it were the file.
        LfsDiff::Summary(summary) => (Vec::new(), Vec::new(), Some(summary)),
    }
}

/// Cancellation is checked at emit time, not only on dequeue: a ticket can be
/// cancelled while its diff is being computed.
fn emit(shared: &Shared, ticket: DiffTicket, view: Arc<DiffView>) -> bool {
    if shared.is_cancelled(ticket) {
        return false;
    }
    shared.emit(SessionEvent::DiffReady { ticket, view });
    true
}

/// The two sides of a diff; `None` means the path does not exist there.
type Sides = (Option<Vec<u8>>, Option<Vec<u8>>);

fn read_sides(repo: &Repo, request: &DiffRequest) -> Result<Sides> {
    match request {
        DiffRequest::Commit { commit, path } => {
            let new = repo.file_at_commit(*commit, path)?;
            let old = match first_parent(repo, *commit)? {
                Some(parent) => repo.file_at_commit(parent, path)?,
                // A root commit has no parent: diff against nothing added.
                None => None,
            };
            Ok((old, new))
        }
        DiffRequest::Staged { path } => Ok((head_file(repo, path)?, repo.file_in_index(path)?)),
        DiffRequest::Unstaged { path } => {
            Ok((repo.file_in_index(path)?, worktree_as_git(repo, path)?))
        }
        DiffRequest::Compare {
            base,
            head,
            old_path,
            path,
        } => {
            let old = match base {
                Some(base) => {
                    let old_path = old_path.as_deref().unwrap_or(path);
                    repo.file_at_commit(*base, old_path)?
                }
                // Unrelated histories compare against nothing.
                None => None,
            };
            Ok((old, repo.file_at_commit(*head, path)?))
        }
    }
}

fn path_of(request: &DiffRequest) -> &str {
    match request {
        DiffRequest::Commit { path, .. }
        | DiffRequest::Staged { path }
        | DiffRequest::Unstaged { path }
        | DiffRequest::Compare { path, .. } => path,
    }
}
