//! Client side of the remote repository protocol.
//!
//! [`RemoteSession`] implements [`RepositorySession`] over any byte stream, so
//! the UI can render a repository served by `strata-remote` the same way it
//! renders a local one. Reads, diffs and the snapshot stream share one
//! connection: a reader thread demultiplexes frames by stream id, and writes
//! take their own lock, so a slow diff cannot block a read or a cancellation.
//!
//! The event callback is the same shape [`Session::open`](strata_core::session::Session::open)
//! uses. It runs on the reader thread. It may call [`RemoteSession::snapshot`];
//! it must not drop the session or wait on another remote call, or it will
//! deadlock the reader that would deliver the answer.
//!
//! A lost connection is reported as [`SessionEvent::LoadFailed`] with
//! [`CONNECTION_LOST`]. The last snapshot stays readable. Later requests fail
//! immediately with that error.

mod session;

pub use session::{CONNECTION_LOST, ClientConfig, RemoteSession, RemoteTarget, UNSUPPORTED};

#[cfg(test)]
mod tests;
