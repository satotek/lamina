//! Message enums carried by frames.
//!
//! A frame's kind byte says which enum the payload holds, so a response can
//! never be mistaken for a request by a decoder that skipped the handshake.

use serde::{Deserialize, Serialize};

use crate::dto::{
    CommitDto, ConflictStyleDto, DiffOptionsDto, DiffRequestDto, DiffViewDto, FileChangeDto,
    GraphRowDto, HeadDto, ObjectId, OperationDto, QueryEvent, QueryRequest, SnapshotDto,
    TreeEntryDto,
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
    /// Run a mutation. Progress and the outcome arrive as events carrying `id`,
    /// which is the client's operation id so both sides name the same job.
    Run {
        id: u64,
        operation: OperationDto,
    },
    /// Kill the git child of a running operation, or skip it if it has not
    /// started yet.
    CancelOperation {
        id: u64,
    },
    /// Answer a credential or host-key prompt. `None` cancels it.
    ///
    /// The answer is a secret whenever the prompt said so. It must not be
    /// written to a log.
    AnswerAskpass {
        id: u64,
        answer: Option<String>,
    },
    /// Commits reachable from `tip` but not from `base`, oldest first.
    /// A rebase confirmation uses this to name the commits it would replay.
    CommitsBetween {
        base: ObjectId,
        tip: ObjectId,
    },
    /// Whether `ancestor` is an ancestor of `descendant`.
    IsAncestor {
        ancestor: ObjectId,
        descendant: ObjectId,
    },
    /// The message git prepared for an in-progress merge, when there is one.
    MergeMessage,
    /// Run a background query on the helper's session. Results arrive as
    /// [`Event::Query`] carrying the same `ticket`. The ticket is the client's,
    /// so a streaming batch and the event that ends it name one job.
    Query {
        ticket: u64,
        query: QueryRequest,
    },
    /// Stop the helper-side query for `ticket`. A result that was already
    /// queued may still arrive; the client drops it.
    CancelQuery {
        ticket: u64,
    },
    /// Worktree bytes of `path`, or `None` when the path is absent. `path` is
    /// relative to the repository; the helper rejects anything that would
    /// leave it.
    FileInWorktree {
        path: String,
    },
    /// Index stages 1, 2 and 3 of a conflicted `path`.
    ConflictSides {
        path: String,
    },
    /// `merge.conflictStyle`, which only changes how unresolved text is shown.
    ConflictStyle,
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
    CommitsBetween {
        commits: Vec<ObjectId>,
    },
    IsAncestor {
        yes: bool,
    },
    MergeMessage {
        message: Option<String>,
    },
    FileInWorktree {
        data: Option<Vec<u8>>,
    },
    /// Stages are `None` when that side deleted the path.
    ConflictSides {
        base: Option<Vec<u8>>,
        ours: Option<Vec<u8>>,
        theirs: Option<Vec<u8>>,
    },
    ConflictStyle {
        style: ConflictStyleDto,
    },
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
    OperationStarted {
        id: u64,
    },
    /// A progress line from the git child. `percent` is set when the line
    /// carries one.
    OperationProgress {
        id: u64,
        text: String,
        percent: Option<u8>,
    },
    /// `error` is git's stderr when the command failed, or a cancel message.
    OperationFinished {
        id: u64,
        error: Option<String>,
    },
    /// A credential or host-key question raised by git on the helper.
    ///
    /// `secret` is true when the answer must be masked. The answer itself
    /// travels back as [`Request::AnswerAskpass`] and is never an event.
    AskpassPrompt {
        id: u64,
        prompt: String,
        secret: bool,
    },
    /// A result of [`Request::Query`]. `ticket` is the client's id. Streaming
    /// queries send several of these; the last one is terminal.
    Query {
        ticket: u64,
        event: QueryEvent,
    },
    /// The helper cleared its signature cache. Visible commits should be
    /// requested again. This is not ticketed: it follows a reload, not a query.
    SignaturesInvalidated,
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
