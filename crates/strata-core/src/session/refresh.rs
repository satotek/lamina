//! Status refreshes: index and worktree only, never a history walk, plus the
//! configuration-derived remotes and upstreams when `.git/config` changed.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use crate::session::SessionEvent;
use crate::session::publish::Shared;
use crate::{ConflictEntry, RemoteInfo, RepoState, Result, UpstreamInfo, WorktreeStatus};

/// One refresh request. An `ack` lets the operation worker wait until the
/// refreshed snapshot is visible before it clears `busy`. `config` asks for the
/// configuration-derived fields on top of the status refresh.
#[derive(Default)]
pub(crate) struct RefreshStatus {
    pub(crate) ack: Option<Sender<()>>,
    pub(crate) config: bool,
}

pub(crate) fn run(shared: Arc<Shared>, rx: Receiver<RefreshStatus>) {
    while let Ok(request) = rx.recv() {
        match read(&shared) {
            Ok((status, state, conflicts)) => {
                shared.publish_if_changed(|snapshot| {
                    if snapshot.status.as_ref() == &status
                        && snapshot.state == state
                        && snapshot.conflicts.as_ref() == &conflicts
                    {
                        return false;
                    }
                    snapshot.status = Arc::new(status);
                    snapshot.state = state;
                    snapshot.conflicts = Arc::new(conflicts);
                    true
                });
            }
            Err(error) => shared.emit(SessionEvent::LoadFailed {
                error: error.to_string(),
            }),
        }
        // Remotes and upstreams come from the configuration, so a config change
        // refreshes exactly those and leaves history and status untouched.
        if request.config {
            match read_config(&shared) {
                Ok((remotes, upstreams)) => {
                    shared.publish_if_changed(|snapshot| {
                        if snapshot.remotes == remotes && snapshot.upstreams.as_ref() == &upstreams
                        {
                            return false;
                        }
                        snapshot.remotes = remotes;
                        snapshot.upstreams = Arc::new(upstreams);
                        true
                    });
                }
                Err(error) => shared.emit(SessionEvent::LoadFailed {
                    error: error.to_string(),
                }),
            }
        }
        // Acknowledge even when the session is shutting down: the operation
        // worker is waiting for this before it stops, and a lost ack would
        // wedge it.
        if let Some(ack) = request.ack {
            let _ = ack.send(());
        }
    }
}

/// The state and its unmerged paths describe the same index as the status, so
/// they are read together and published as one snapshot.
fn read(shared: &Shared) -> Result<(WorktreeStatus, RepoState, Vec<ConflictEntry>)> {
    let status = shared.repo.status()?;
    let state = shared.repo.state()?;
    let conflicts = shared.repo.conflicts()?;
    Ok((status, state, conflicts))
}

fn read_config(shared: &Shared) -> Result<(Vec<RemoteInfo>, HashMap<String, UpstreamInfo>)> {
    let remotes = shared.repo.remotes()?;
    let upstreams = shared.repo.upstreams()?;
    Ok((remotes, upstreams))
}
