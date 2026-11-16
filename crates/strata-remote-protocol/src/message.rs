//! Message enums carried by frames.
//!
//! A frame's kind byte says which enum the payload holds, so a response can
//! never be mistaken for a request by a decoder that skipped the handshake.

use serde::{Deserialize, Serialize};

use crate::dto::{
    CommitDto, DiffOptionsDto, DiffRequestDto, DiffViewDto, FileChangeDto, GraphRowDto, HeadDto,
    ObjectId, SnapshotDto, TreeEntryDto,
};

/// A message the app sends to the helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// First message on a stream. The helper answers with [`Response::HelloAck`]
    /// or [`Response::VersionMismatch`].
    Hello {
        protocol: u32,
        app_version: String,
        build: String,
    },
    /// Liveness probe; the helper echoes `nonce` so a late reply is ignorable.
    Ping {
        nonce: u64,
    },
    /// Start receiving snapshots: the helper pushes its current snapshot and
    /// every later update as `SnapshotReset`/`SnapshotAppend` events.
    SubscribeSnapshots,
    /// Republish the current snapshot and history.
    Reload,
    Head,
    CommitChanges {
        commit: ObjectId,
    },
    TreeEntries {
        commit: ObjectId,
        dir: String,
    },
    BlobAt {
        commit: ObjectId,
        path: String,
    },
    CommitMessage {
        commit: ObjectId,
    },
    Parents {
        commit: ObjectId,
    },
    /// Compute a diff. The result arrives as [`Event::DiffReady`] or
    /// [`Event::DiffFailed`] carrying the same `ticket`.
    Diff {
        ticket: u64,
        request: DiffRequestDto,
        options: DiffOptionsDto,
    },
    /// Best-effort cancellation of an in-flight ticket.
    CancelDiff {
        ticket: u64,
    },
}

/// A direct answer to one [`Request`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    HelloAck {
        /// Repository root as the helper sees it. For a bare repository this is
        /// the git directory.
        root: String,
        git_dir: String,
        bare: bool,
    },
    VersionMismatch {
        /// Protocol the helper speaks.
        expected: u32,
        /// Protocol the app asked for.
        got: u32,
    },
    Pong {
        nonce: u64,
    },
    Head {
        head: Option<HeadDto>,
    },
    CommitChanges {
        changes: Vec<FileChangeDto>,
    },
    TreeEntries {
        entries: Vec<TreeEntryDto>,
    },
    BlobAt {
        data: Vec<u8>,
    },
    CommitMessage {
        message: String,
    },
    Parents {
        parents: Vec<ObjectId>,
    },
    /// The request was accepted; a follow-up event carries the result when the
    /// request has one.
    Ok,
    Error(RemoteError),
}

/// An asynchronous update the helper pushes to the app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// Everything except the commit list. The commits that belong to this
    /// generation follow as one or more [`Event::SnapshotAppend`] messages.
    SnapshotReset {
        snapshot: SnapshotDto,
    },
    SnapshotAppend {
        generation: u64,
        commits: Vec<CommitDto>,
        rows: Vec<GraphRowDto>,
        /// True on the last append of this generation.
        complete: bool,
    },
    DiffReady {
        ticket: u64,
        view: Box<DiffViewDto>,
    },
    DiffFailed {
        ticket: u64,
        error: String,
    },
    /// The helper could not read the repository (deleted, permissions, ...).
    LoadFailed {
        error: String,
    },
}

/// Coarse classification of a remote failure, so the client can keep the
/// message readable without parsing prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorKind {
    Io,
    Git,
    NotFound,
    NotSupported,
    Protocol,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteError {
    pub kind: ErrorKind,
    pub message: String,
    /// The failing command's stderr, when the helper ran one.
    pub stderr: Option<String>,
    /// Exit code of that command.
    pub code: Option<i32>,
}

impl RemoteError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            stderr: None,
            code: None,
        }
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Other, message)
    }
}

/// One decoded frame payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    Request(Request),
    Response(Response),
    Event(Event),
}
