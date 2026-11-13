//! Status refreshes: index and worktree only, never a history walk.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use crate::session::SessionEvent;
use crate::session::publish::Shared;
use crate::{ConflictEntry, RepoState, Result, WorktreeStatus};

/// One refresh request. An `ack` lets the operation worker wait until the
/// refreshed snapshot is visible before it clears `busy`.
#[derive(Default)]
pub(crate) struct RefreshStatus {
    pub(crate) ack: Option<Sender<()>>,
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
