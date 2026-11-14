//! The commit-signature verification worker: cheap `gix` header detection
//! followed by cancellable batched `git log` verification off the UI thread.
//!
//! Runs on a single dedicated worker thread so rapid scrolling never spawns
//! overlapping `git` processes (strict concurrency cap of 1). Results are
//! cached per `Oid` for the lifetime of the session and invalidated on reload
//! or git configuration changes.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};

use crate::repo::{CommitSignature, SignatureFormat};
use crate::session::QueryTicket;
use crate::session::SessionEvent;
use crate::session::publish::Shared;
use crate::session::queries::QueryState;
use crate::{Oid, Result};

pub(crate) struct SignatureJob {
    pub(crate) ticket: QueryTicket,
    pub(crate) commits: Vec<Oid>,
}

/// Per-session cache of commit signature verification results.
///
/// `None` caches a commit that `gix` confirmed has no signature header so
/// scrolling back over unsigned commits never re-reads their object headers.
pub(crate) struct SignatureCache {
    inner: Mutex<CacheInner>,
}

#[derive(Default)]
struct CacheInner {
    epoch: u64,
    entries: HashMap<Oid, Option<CommitSignature>>,
}

impl SignatureCache {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(CacheInner::default()),
        }
    }

    /// Clears all cached entries and advances the epoch so any in-flight
    /// verification from before invalidation will not repopulate stale data.
    /// Returns `true` if there were cached entries to drop.
    pub(crate) fn invalidate(&self) -> bool {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        guard.epoch = guard.epoch.wrapping_add(1);
        let had_entries = !guard.entries.is_empty();
        guard.entries.clear();
        had_entries
    }

    fn epoch(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .epoch
    }

    fn partition(&self, commits: &[Oid]) -> (u64, HashMap<Oid, CommitSignature>, Vec<Oid>) {
        let guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let mut cached = HashMap::new();
        let mut uncached = Vec::new();
        let mut seen = HashSet::with_capacity(commits.len());
        for &id in commits {
            if !seen.insert(id) {
                continue;
            }
            match guard.entries.get(&id) {
                Some(Some(signature)) => {
                    cached.insert(id, signature.clone());
                }
                Some(None) => {}
                None => uncached.push(id),
            }
        }
        (guard.epoch, cached, uncached)
    }

    fn insert_unsigned(&self, expected_epoch: u64, oids: &[Oid]) {
        if oids.is_empty() {
            return;
        }
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.epoch != expected_epoch {
            return;
        }
        for &id in oids {
            guard.entries.entry(id).or_insert(None);
        }
    }

    fn insert_verified(&self, expected_epoch: u64, verified: &HashMap<Oid, CommitSignature>) {
        if verified.is_empty() {
            return;
        }
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.epoch != expected_epoch {
            return;
        }
        for (id, sig) in verified {
            guard.entries.insert(*id, Some(sig.clone()));
        }
    }
}

pub(crate) fn run(
    shared: Arc<Shared>,
    state: Arc<QueryState>,
    cache: Arc<SignatureCache>,
    jobs: Receiver<SignatureJob>,
) {
    while let Ok(job) = jobs.recv() {
        if shared.is_shutdown() {
            return;
        }
        if state.is_cancelled(job.ticket) {
            state.forget(job.ticket);
            continue;
        }

        let failure = process_job(&shared, &state, &cache, &job)
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

fn process_job(
    shared: &Arc<Shared>,
    state: &Arc<QueryState>,
    cache: &Arc<SignatureCache>,
    job: &SignatureJob,
) -> Result<()> {
    let ticket = job.ticket;
    let is_cancelled = || state.is_cancelled(ticket) || shared.is_shutdown();

    let (epoch, mut signatures, uncached) = cache.partition(&job.commits);
    if is_cancelled() {
        return Ok(());
    }

    if uncached.is_empty() {
        shared.emit(SessionEvent::CommitSignaturesReady {
            ticket,
            signatures: Arc::new(signatures),
        });
        return Ok(());
    }

    let detected = shared.repo.commit_signature_formats(&uncached)?;
    if is_cancelled() {
        return Ok(());
    }

    let mut unsigned = Vec::new();
    let mut signed: Vec<(Oid, SignatureFormat)> = Vec::new();
    let mut detected_map: HashMap<Oid, SignatureFormat> = HashMap::new();
    for (id, format) in detected {
        match format {
            Some(fmt) => {
                signed.push((id, fmt));
                detected_map.insert(id, fmt);
            }
            None => unsigned.push(id),
        }
    }
    cache.insert_unsigned(epoch, &unsigned);

    if signed.is_empty() {
        if !is_cancelled() {
            shared.emit(SessionEvent::CommitSignaturesReady {
                ticket,
                signatures: Arc::new(signatures),
            });
        }
        return Ok(());
    }

    shared.emit(SessionEvent::CommitSignaturesDetected {
        ticket,
        formats: Arc::new(detected_map),
    });

    if is_cancelled() {
        return Ok(());
    }

    let hints = shared.repo.signature_config_hints();
    let Some(verified) = shared
        .git
        .verify_commit_signatures(&signed, &hints, &is_cancelled)?
    else {
        return Ok(());
    };

    if is_cancelled() {
        return Ok(());
    }

    // If the cache was invalidated while git was running, do not publish
    // results computed against the old configuration.
    if cache.epoch() != epoch {
        return Ok(());
    }

    cache.insert_verified(epoch, &verified);
    signatures.extend(verified);
    shared.emit(SessionEvent::CommitSignaturesReady {
        ticket,
        signatures: Arc::new(signatures),
    });
    Ok(())
}
