//! Status refreshes: index and worktree only, never a history walk.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use crate::session::SessionEvent;
use crate::session::publish::Shared;

/// One refresh request. An `ack` lets the operation worker wait until the
/// refreshed snapshot is visible before it clears `busy`.
#[derive(Default)]
pub(crate) struct RefreshStatus {
    pub(crate) ack: Option<Sender<()>>,
}

pub(crate) fn run(shared: Arc<Shared>, rx: Receiver<RefreshStatus>) {
    while let Ok(request) = rx.recv() {
        match shared.repo.status() {
            Ok(status) => {
                shared.publish_if_changed(|snapshot| {
                    if snapshot.status.as_ref() == &status {
                        return false;
                    }
                    snapshot.status = Arc::new(status);
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
