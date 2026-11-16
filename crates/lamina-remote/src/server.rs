//! Headless helper that serves one repository over a byte stream.
//!
//! The app starts this binary over `ssh -T host -- lamina-remote --stdio`, so
//! stdout carries framed protocol messages only. Every diagnostic goes to
//! stderr; mixing the two would corrupt the stream.
//!
//! Snapshot frames are large and numerous. They wait on a bounded queue so a
//! slow app applies backpressure to the history walk instead of the helper
//! buffering the whole repository. Responses, pings, diffs and query events
//! skip that queue: a read has to stay answerable while history is still
//! streaming, and a cancel has to reach a query that is still running.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use lamina_core::session::{DiffTicket, QueryTicket, RepositorySession, Session, SessionEvent};
use lamina_remote_protocol::{
    self as proto, CommitDto, DiffRequestDto, DiffViewDto, Event, GraphRowDto, HeadDto, Message,
    PROTOCOL_VERSION, QueryEvent, QueryRequest, RemoteError, Request, Response, SnapshotDto,
};

/// Number of commits per snapshot frame. Large enough to keep round trips rare,
/// small enough that a first snapshot is not one multi-megabyte frame.
const SNAPSHOT_CHUNK: usize = 1024;

/// How many snapshot frames may sit encoded but not yet written. One is enough
/// to keep the writer busy; a second would only delay a response that arrived
/// while the first was still queued.
const SNAPSHOT_QUEUE: usize = 1;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Identifies the helper build in `--self-check` and in error reports.
pub fn build_id() -> String {
    format!(
        "{}+{}",
        env!("CARGO_PKG_VERSION"),
        option_env!("LAMINA_BUILD_ID").unwrap_or("dev")
    )
}

struct ServerShared {
    /// `None` until `Session::open` returns, and again once shutdown starts.
    ///
    /// An `Arc` so a request can clone it and drop the mutex before `run_as`.
    /// Holding the mutex across `run_as` deadlocks: the worker publishes a
    /// snapshot, and that publish locks this mutex again. The clone lives only
    /// for the send. A worker must not keep it, because `Session`'s drop joins
    /// the workers.
    session: Mutex<Option<Arc<Session>>>,
    outbox: Arc<Outbox>,
    /// Maps between the client's diff tickets and the local session's tickets.
    diff_routes: Mutex<DiffRoutes>,
    /// Maps between the client's query tickets and the local session's tickets.
    ///
    /// A missing entry means the client cancelled or the query already ended.
    /// Events that arrive after that are dropped, including ones the worker
    /// had already produced.
    query_routes: Mutex<QueryRoutes>,
    /// One snapshot publish at a time, so two generations cannot interleave
    /// their chunks on the wire.
    snapshot_publish: Mutex<()>,
    /// Snapshot frames stay off the wire until the client asks. The walk may
    /// already have published into the local session by then.
    subscribed: AtomicBool,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

#[derive(Default)]
struct DiffRoutes {
    local_to_client: HashMap<DiffTicket, u64>,
    client_to_local: HashMap<u64, DiffTicket>,
}

#[derive(Default)]
struct QueryRoutes {
    local_to_client: HashMap<QueryTicket, u64>,
    client_to_local: HashMap<u64, QueryTicket>,
}

/// Responses and other small control frames, kept ahead of snapshot chunks.
struct Outbox {
    state: Mutex<OutState>,
    cond: Condvar,
}

struct OutState {
    high: VecDeque<Vec<u8>>,
    low: VecDeque<Vec<u8>>,
    closed: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Outbox {
    fn new() -> Self {
        Self {
            state: Mutex::new(OutState {
                high: VecDeque::new(),
                low: VecDeque::new(),
                closed: false,
            }),
            cond: Condvar::new(),
        }
    }

    fn push_high(&self, frame: Vec<u8>) -> bool {
        let mut state = lock(&self.state);
        if state.closed {
            return false;
        }
        state.high.push_back(frame);
        self.cond.notify_one();
        true
    }

    /// Blocks while `SNAPSHOT_QUEUE` frames are already waiting, so the caller
    /// (the history walk) slows down instead of allocating the rest of the walk.
    fn push_low(&self, frame: Vec<u8>) -> bool {
        let mut state = lock(&self.state);
        while state.low.len() >= SNAPSHOT_QUEUE && !state.closed {
            state = self
                .cond
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if state.closed {
            return false;
        }
        state.low.push_back(frame);
        self.cond.notify_one();
        true
    }

    /// Control frames first. A snapshot frame already inside `write` cannot be
    /// overtaken; everything still queued can.
    fn pop(&self) -> Option<Vec<u8>> {
        let mut state = lock(&self.state);
        loop {
            if let Some(frame) = state.high.pop_front() {
                return Some(frame);
            }
            if let Some(frame) = state.low.pop_front() {
                // The walk may be waiting for this slot.
                self.cond.notify_all();
                return Some(frame);
            }
            if state.closed {
                return None;
            }
            state = self
                .cond
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn close(&self) {
        let mut state = lock(&self.state);
        state.closed = true;
        self.cond.notify_all();
    }
}

/// Serves `root` on the given stream until the reader reaches EOF.
///
/// Returns once the session is dropped; the writer thread flushes every queued
/// frame before joining, so a reply to the last request is not lost.
pub fn serve(
    reader: impl Read + Send + 'static,
    writer: impl Write + Send + 'static,
    root: impl AsRef<Path>,
) -> Result<(), BoxError> {
    serve_chunked(reader, writer, root, SNAPSHOT_CHUNK)
}

fn serve_chunked(
    mut reader: impl Read + Send + 'static,
    writer: impl Write + Send + 'static,
    root: impl AsRef<Path>,
    chunk: usize,
) -> Result<(), BoxError> {
    let root = root.as_ref().to_path_buf();
    let outbox = Arc::new(Outbox::new());
    let writer_outbox = Arc::clone(&outbox);
    let writer_thread = thread::Builder::new()
        .name("lamina-remote-writer".to_owned())
        .spawn(move || write_frames(writer_outbox, writer))?;

    let shared = Arc::new(ServerShared {
        session: Mutex::new(None),
        outbox,
        diff_routes: Mutex::new(DiffRoutes::default()),
        query_routes: Mutex::new(QueryRoutes::default()),
        snapshot_publish: Mutex::new(()),
        subscribed: AtomicBool::new(false),
        workers: Mutex::new(Vec::new()),
    });

    let callback_shared = Arc::clone(&shared);
    let chunk_size = chunk.max(1);
    let session = Session::open(&root, move |event| {
        on_session_event(&callback_shared, chunk_size, event);
    })?;
    *lock(&shared.session) = Some(Arc::new(session));

    while let Some(frame) = proto::read_frame(&mut reader)? {
        let stream = frame.stream;
        match frame.message {
            Message::Request(request) => dispatch(&shared, stream, chunk_size, request),
            Message::Response(_) | Message::Event(_) => {
                // The helper only answers; a peer sending these is confused.
                push_high(
                    &shared,
                    stream,
                    &Message::Response(Response::Error(RemoteError::new(
                        proto::ErrorKind::Protocol,
                        "the helper only accepts requests",
                    ))),
                );
            }
        }
    }

    // Closing the outbox unblocks a snapshot publish before the session join
    // waits for the loader thread that is inside that publish.
    shared.outbox.close();
    let session = lock(&shared.session).take();
    drop(session);
    let workers = std::mem::take(&mut *lock(&shared.workers));
    for worker in workers {
        let _ = worker.join();
    }
    drop(shared);
    let _ = writer_thread.join();
    Ok(())
}

fn write_frames(outbox: Arc<Outbox>, mut writer: impl Write) {
    while let Some(frame) = outbox.pop() {
        if writer
            .write_all(&frame)
            .and_then(|()| writer.flush())
            .is_err()
        {
            break;
        }
    }
    // A failed write must also wake the walk, or it stays parked on the queue.
    outbox.close();
}

fn dispatch(shared: &Arc<ServerShared>, stream: u16, chunk: usize, request: Request) {
    if is_blocking_read(&request) {
        let reader = match session_reader(shared) {
            Ok(reader) => reader,
            Err(response) => {
                push_high(shared, stream, &Message::Response(response));
                return;
            }
        };
        // The request loop has to stay free for Ping, CancelDiff and CancelQuery.
        // A blob read can take longer than the client's idle timeout.
        let task = Arc::clone(shared);
        // A failed spawn used to drop the request, and the client waited until
        // its own timeout. Answer on this thread instead.
        if !spawn_worker(shared, move || {
            let response = answer_read(&reader, request);
            push_high(&task, stream, &Message::Response(response));
        }) {
            push_high(
                shared,
                stream,
                &Message::Response(Response::Error(RemoteError::other(
                    "could not start a read on the remote helper",
                ))),
            );
        }
        return;
    }

    if let Some(response) = answer_control(shared, chunk, request) {
        push_high(shared, stream, &Message::Response(response));
    }
}

fn is_blocking_read(request: &Request) -> bool {
    matches!(
        request,
        Request::Head
            | Request::CommitChanges { .. }
            | Request::TreeEntries { .. }
            | Request::BlobAt { .. }
            | Request::CommitMessage { .. }
            | Request::Parents { .. }
            | Request::CommitsBetween { .. }
            | Request::IsAncestor { .. }
            | Request::MergeMessage
            | Request::FileInWorktree { .. }
            | Request::ConflictSides { .. }
            | Request::ConflictStyle
            | Request::MergeBase { .. }
            | Request::ThreeWayTree { .. }
            | Request::ThreeWayBlobs { .. }
            | Request::ThreeWayRoles { .. }
            | Request::ThreeWaySideTree { .. }
            | Request::CompareSideBlob { .. }
    )
}

fn session_reader(
    shared: &ServerShared,
) -> Result<Arc<dyn lamina_core::session::RepoReader>, Response> {
    let guard = lock(&shared.session);
    match guard.as_ref() {
        Some(session) => Ok(session.reader()),
        None => Err(Response::Error(RemoteError::other(
            "the repository session is not ready",
        ))),
    }
}

fn spawn_worker(shared: &Arc<ServerShared>, work: impl FnOnce() + Send + 'static) -> bool {
    match thread::Builder::new()
        .name("lamina-remote-rpc".to_owned())
        .spawn(work)
    {
        Ok(handle) => {
            lock(&shared.workers).push(handle);
            true
        }
        Err(error) => {
            eprintln!("lamina-remote: failed to start a read: {error}");
            false
        }
    }
}

fn answer_control(shared: &Arc<ServerShared>, chunk: usize, request: Request) -> Option<Response> {
    let guard = lock(&shared.session);
    let Some(session) = guard.as_ref() else {
        return Some(Response::Error(RemoteError::other(
            "the repository session is not ready",
        )));
    };

    Some(match request {
        Request::Hello { protocol, .. } => {
            if protocol != PROTOCOL_VERSION {
                return Some(Response::VersionMismatch {
                    expected: PROTOCOL_VERSION,
                    got: protocol,
                });
            }
            let reader = session.reader();
            let root = reader
                .workdir()
                .unwrap_or_else(|| reader.git_dir())
                .display()
                .to_string();
            Response::HelloAck {
                root,
                git_dir: reader.git_dir().display().to_string(),
                bare: reader.workdir().is_none(),
            }
        }
        Request::Ping { nonce } => Response::Pong { nonce },
        Request::SubscribeSnapshots => {
            shared.subscribed.store(true, Ordering::SeqCst);
            let task = Arc::clone(shared);
            // Publishing on this thread would hold the request loop for the
            // whole walk. The loader's own callback applies the backpressure.
            if !spawn_worker(shared, move || publish_current(&task, chunk)) {
                return Some(Response::Error(RemoteError::other(
                    "could not publish the snapshot",
                )));
            }
            Response::Ok
        }
        Request::Reload => {
            session.reload();
            let task = Arc::clone(shared);
            if !spawn_worker(shared, move || publish_current(&task, chunk)) {
                return Some(Response::Error(RemoteError::other(
                    "could not publish the snapshot",
                )));
            }
            Response::Ok
        }
        Request::Diff {
            ticket,
            request,
            options,
        } => {
            let request = match DiffRequestDto::try_into(request) {
                Ok(request) => request,
                Err(error) => return Some(Response::Error(protocol_error(error))),
            };
            // The mapping is installed under the lock the callback also takes,
            // so a diff that finishes immediately cannot report an unknown
            // ticket and vanish.
            let mut routes = lock(&shared.diff_routes);
            let local = session.request_diff_with(request, options.into());
            routes.local_to_client.insert(local, ticket);
            routes.client_to_local.insert(ticket, local);
            Response::Ok
        }
        Request::CancelDiff { ticket } => {
            let mut routes = lock(&shared.diff_routes);
            if let Some(local) = routes.client_to_local.remove(&ticket) {
                routes.local_to_client.remove(&local);
                session.cancel_diff(local);
            }
            Response::Ok
        }
        Request::Query { ticket, query } => {
            // The mapping is installed under the lock the callback also takes,
            // so a query that finishes immediately cannot report an unknown
            // ticket and vanish. The lock is taken before the session call for
            // the same reason.
            let mut routes = lock(&shared.query_routes);
            let local = match start_query(session, query) {
                Ok(local) => local,
                Err(error) => return Some(Response::Error(protocol_error(error))),
            };
            routes.local_to_client.insert(local, ticket);
            routes.client_to_local.insert(ticket, local);
            Response::Ok
        }
        Request::CancelQuery { ticket } => {
            // Drop the route before cancelling. A result the worker emits
            // after this finds no ticket and is not forwarded.
            let mut routes = lock(&shared.query_routes);
            if let Some(local) = routes.client_to_local.remove(&ticket) {
                routes.local_to_client.remove(&local);
                session.cancel_query(local);
            }
            Response::Ok
        }
        Request::Head
        | Request::CommitChanges { .. }
        | Request::TreeEntries { .. }
        | Request::BlobAt { .. }
        | Request::CommitMessage { .. }
        | Request::Parents { .. }
        | Request::CommitsBetween { .. }
        | Request::IsAncestor { .. }
        | Request::MergeMessage
        | Request::FileInWorktree { .. }
        | Request::ConflictSides { .. }
        | Request::ConflictStyle
        | Request::MergeBase { .. }
        | Request::ThreeWayTree { .. }
        | Request::ThreeWayBlobs { .. }
        | Request::ThreeWayRoles { .. }
        | Request::ThreeWaySideTree { .. }
        | Request::CompareSideBlob { .. } => {
            // `dispatch` sends these to a worker. Reaching here is a bug.
            Response::Error(RemoteError::other(
                "internal error: a read was handled on the request loop",
            ))
        }
        Request::Run { id, operation } => {
            let session = Arc::clone(session);
            drop(guard);
            let operation = match lamina_core::session::Operation::try_from(operation) {
                Ok(operation) => operation,
                Err(error) => {
                    return Some(Response::Error(protocol_error(error)));
                }
            };
            // ResolveWithContent writes bytes itself, so a path has to be
            // checked before git sees it. A symlink whose name is a normal
            // relative path still has to be resolved on this machine; that
            // check lives in the write, which runs here.
            if let Some(path) = escaping_write_path(&operation) {
                return Some(Response::Error(RemoteError::other(format!(
                    "path must stay inside the worktree: {path}"
                ))));
            }
            session.run_as(lamina_core::session::OperationId(id), operation);
            Response::Ok
        }
        Request::CancelOperation { id } => {
            let session = Arc::clone(session);
            drop(guard);
            session.cancel_operation(lamina_core::session::OperationId(id));
            Response::Ok
        }
        Request::AnswerAskpass { id, answer } => {
            let session = Arc::clone(session);
            drop(guard);
            // `answer` is the secret when the prompt said so. It is handed to
            // the waiting git child and must not be logged.
            let _ = session.answer_askpass(lamina_core::session::OperationId(id), answer);
            Response::Ok
        }
    })
}

fn answer_read(reader: &Arc<dyn lamina_core::session::RepoReader>, request: Request) -> Response {
    match request {
        Request::Head => match reader.head() {
            Ok(head) => Response::Head {
                head: Some(HeadDto::from(&head)),
            },
            Err(error) => Response::Error(remote_error(error)),
        },
        Request::CommitChanges { commit } => {
            let result = proto::ObjectId::try_into(commit)
                .map_err(protocol_error)
                .and_then(|commit| reader.commit_changes(commit).map_err(remote_error));
            match result {
                Ok(changes) => Response::CommitChanges {
                    changes: changes.iter().map(Into::into).collect(),
                },
                Err(error) => Response::Error(error),
            }
        }
        Request::TreeEntries { commit, dir } => {
            let result = proto::ObjectId::try_into(commit)
                .map_err(protocol_error)
                .and_then(|commit| reader.tree_entries(commit, &dir).map_err(remote_error));
            match result {
                Ok(entries) => Response::TreeEntries {
                    entries: entries.iter().map(Into::into).collect(),
                },
                Err(error) => Response::Error(error),
            }
        }
        Request::BlobAt { commit, path } => {
            let result = proto::ObjectId::try_into(commit)
                .map_err(protocol_error)
                .and_then(|commit| reader.blob_at(commit, &path).map_err(remote_error));
            match result {
                Ok(data) => Response::BlobAt { data },
                Err(error) => Response::Error(error),
            }
        }
        Request::CommitMessage { commit } => {
            let result = proto::ObjectId::try_into(commit)
                .map_err(protocol_error)
                .and_then(|commit| reader.commit_message(commit).map_err(remote_error));
            match result {
                Ok(message) => Response::CommitMessage { message },
                Err(error) => Response::Error(error),
            }
        }
        Request::Parents { commit } => {
            let result = proto::ObjectId::try_into(commit)
                .map_err(protocol_error)
                .and_then(|commit| reader.parents(commit).map_err(remote_error));
            match result {
                Ok(parents) => Response::Parents {
                    parents: parents.into_iter().map(Into::into).collect(),
                },
                Err(error) => Response::Error(error),
            }
        }
        Request::CommitsBetween { base, tip } => {
            let result = (|| {
                let base = proto::ObjectId::try_into(base).map_err(protocol_error)?;
                let tip = proto::ObjectId::try_into(tip).map_err(protocol_error)?;
                reader.commits_between(base, tip).map_err(remote_error)
            })();
            match result {
                Ok(commits) => Response::CommitsBetween {
                    commits: commits.into_iter().map(Into::into).collect(),
                },
                Err(error) => Response::Error(error),
            }
        }
        Request::IsAncestor {
            ancestor,
            descendant,
        } => {
            let result = (|| {
                let ancestor = proto::ObjectId::try_into(ancestor).map_err(protocol_error)?;
                let descendant = proto::ObjectId::try_into(descendant).map_err(protocol_error)?;
                reader
                    .is_ancestor(ancestor, descendant)
                    .map_err(remote_error)
            })();
            match result {
                Ok(yes) => Response::IsAncestor { yes },
                Err(error) => Response::Error(error),
            }
        }
        Request::MergeMessage => match reader.merge_message() {
            Ok(message) => Response::MergeMessage { message },
            Err(error) => Response::Error(remote_error(error)),
        },
        Request::FileInWorktree { path } => {
            if let Err(error) = require_worktree_path(&path) {
                return Response::Error(error);
            }
            match reader.file_in_worktree(&path) {
                Ok(data) => Response::FileInWorktree { data },
                Err(error) => Response::Error(remote_error(error)),
            }
        }
        Request::ConflictSides { path } => match reader.conflict_sides(&path) {
            Ok(sides) => Response::ConflictSides {
                base: sides.base,
                ours: sides.ours,
                theirs: sides.theirs,
            },
            Err(error) => Response::Error(remote_error(error)),
        },
        Request::ConflictStyle => Response::ConflictStyle {
            style: reader.conflict_style().into(),
        },
        Request::MergeBase { a, b } => {
            let result = (|| {
                let a = proto::ObjectId::try_into(a).map_err(protocol_error)?;
                let b = proto::ObjectId::try_into(b).map_err(protocol_error)?;
                reader.merge_base(a, b).map_err(remote_error)
            })();
            match result {
                Ok(commit) => Response::MergeBase {
                    commit: commit.map(Into::into),
                },
                Err(error) => Response::Error(error),
            }
        }
        Request::ThreeWayTree { left, base, right } => {
            let result = (|| {
                let left = proto::ObjectId::try_into(left).map_err(protocol_error)?;
                let base = proto::ObjectId::try_into(base).map_err(protocol_error)?;
                let right = proto::ObjectId::try_into(right).map_err(protocol_error)?;
                reader
                    .three_way_tree(left, base, right)
                    .map_err(remote_error)
            })();
            match result {
                Ok(tree) => Response::ThreeWayTree {
                    tree: Box::new(proto::ThreeWayTreeCompareDto::from(&tree)),
                },
                Err(error) => Response::Error(error),
            }
        }
        Request::ThreeWayBlobs {
            left,
            base,
            right,
            path,
        } => {
            let result = (|| {
                let left = proto::ObjectId::try_into(left).map_err(protocol_error)?;
                let base = proto::ObjectId::try_into(base).map_err(protocol_error)?;
                let right = proto::ObjectId::try_into(right).map_err(protocol_error)?;
                reader
                    .three_way_blobs(left, base, right, &path)
                    .map_err(remote_error)
            })();
            match result {
                Ok(blobs) => Response::ThreeWayBlobs { blobs },
                Err(error) => Response::Error(error),
            }
        }
        Request::ThreeWayRoles { commits } => {
            let result = (|| {
                let commits = [
                    proto::ObjectId::try_into(commits[0]).map_err(protocol_error)?,
                    proto::ObjectId::try_into(commits[1]).map_err(protocol_error)?,
                    proto::ObjectId::try_into(commits[2]).map_err(protocol_error)?,
                ];
                reader.three_way_roles(commits).map_err(remote_error)
            })();
            match result {
                Ok(roles) => Response::ThreeWayRoles {
                    left: roles.left.into(),
                    base: roles.base.into(),
                    right: roles.right.into(),
                },
                Err(error) => Response::Error(error),
            }
        }
        Request::ThreeWaySideTree { sides } => {
            let result = (|| {
                let sides = [
                    lamina_core::compare::RepoCompareSide::try_from(sides[0])
                        .map_err(protocol_error)?,
                    lamina_core::compare::RepoCompareSide::try_from(sides[1])
                        .map_err(protocol_error)?,
                    lamina_core::compare::RepoCompareSide::try_from(sides[2])
                        .map_err(protocol_error)?,
                ];
                reader.three_way_side_tree(sides).map_err(remote_error)
            })();
            match result {
                Ok(tree) => Response::ThreeWayTree {
                    tree: Box::new(proto::ThreeWayTreeCompareDto::from(&tree)),
                },
                Err(error) => Response::Error(error),
            }
        }
        Request::CompareSideBlob { side, path } => {
            let side = match lamina_core::compare::RepoCompareSide::try_from(side) {
                Ok(side) => side,
                Err(error) => return Response::Error(protocol_error(error)),
            };
            // The worktree side joins the path onto the checkout. A commit or
            // the index only looks the path up inside git, which cannot leave
            // the repository.
            if side.is_worktree()
                && let Err(error) = require_worktree_path(&path)
            {
                return Response::Error(error);
            }
            match reader.compare_side_blob(side, &path) {
                Ok(data) => Response::CompareSideBlob { data },
                Err(error) => Response::Error(remote_error(error)),
            }
        }
        other => Response::Error(RemoteError::other(format!(
            "internal error: {other:?} is not a repository read"
        ))),
    }
}

/// A path a conflict write would touch, when it is not a single relative path
/// inside the worktree. `None` means every path the operation writes is safe
/// to hand to git, or the operation does not write a worktree path.
fn escaping_write_path(operation: &lamina_core::session::Operation) -> Option<&str> {
    let paths: Vec<&str> = match operation {
        lamina_core::session::Operation::ResolveWithContent { path, .. }
        | lamina_core::session::Operation::ResolveConflict { path, .. } => vec![path.as_str()],
        lamina_core::session::Operation::MarkResolved(paths) => {
            paths.iter().map(String::as_str).collect()
        }
        _ => return None,
    };
    paths
        .into_iter()
        .find(|path| require_worktree_path(path).is_err())
}

/// Rejects an empty path, an absolute path, `..` and a `.git` component.
///
/// The bytes of a resolved file are written with `std::fs`, which does not
/// apply git's pathspec rules. A symlink that points outside is not visible
/// here; the write resolves it and refuses when the destination leaves the
/// worktree.
fn require_worktree_path(path: &str) -> Result<(), RemoteError> {
    let relative = Path::new(path);
    if relative.as_os_str().is_empty() {
        return Err(RemoteError::other("path is empty"));
    }
    if relative.is_absolute() {
        return Err(RemoteError::other(format!(
            "path must stay inside the worktree: {path}"
        )));
    }
    for component in relative.components() {
        match component {
            // Case-insensitive because Windows and default macOS checkouts
            // are, so `.GIT` names the repository as well.
            Component::Normal(name)
                if name
                    .to_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case(".git")) =>
            {
                return Err(RemoteError::other(format!("path points into .git: {path}")));
            }
            Component::Normal(_) => {}
            _ => {
                return Err(RemoteError::other(format!(
                    "path must stay inside the worktree: {path}"
                )));
            }
        }
    }
    Ok(())
}

fn on_session_event(shared: &Arc<ServerShared>, chunk: usize, event: SessionEvent) {
    match event {
        SessionEvent::SnapshotUpdated { .. } => publish_current(shared, chunk),
        SessionEvent::DiffReady { ticket, view } => {
            // Both the plain view and the later highlighted view are forwarded.
            // The route stays until the highlighted one, a failure, or a cancel,
            // because the local session emits the second only when it can.
            let client = lock(&shared.diff_routes)
                .local_to_client
                .get(&ticket)
                .copied();
            let Some(client) = client else {
                return;
            };
            let highlighted = !view.old_highlights.is_empty() || !view.new_highlights.is_empty();
            if highlighted || view.diff.binary {
                forget_diff(shared, ticket);
            }
            push_high(
                shared,
                0,
                &Message::Event(Event::DiffReady {
                    ticket: client,
                    view: Box::new(DiffViewDto::from(&*view)),
                }),
            );
        }
        SessionEvent::DiffFailed { ticket, error } => {
            let client = forget_diff(shared, ticket);
            if let Some(client) = client {
                push_high(
                    shared,
                    0,
                    &Message::Event(Event::DiffFailed {
                        ticket: client,
                        error,
                    }),
                );
            }
        }
        SessionEvent::LoadFailed { error } => {
            push_high(shared, 0, &Message::Event(Event::LoadFailed { error }));
        }
        SessionEvent::OperationStarted { id } => {
            push_high(
                shared,
                0,
                &Message::Event(Event::OperationStarted { id: id.0 }),
            );
        }
        SessionEvent::OperationProgress { id, text, percent } => {
            push_high(
                shared,
                0,
                &Message::Event(Event::OperationProgress {
                    id: id.0,
                    text,
                    percent,
                }),
            );
        }
        SessionEvent::OperationFinished { id, error } => {
            push_high(
                shared,
                0,
                &Message::Event(Event::OperationFinished { id: id.0, error }),
            );
        }
        SessionEvent::AskpassPrompt { id, prompt, secret } => {
            // The prompt text is not the answer. The answer only travels back
            // as `AnswerAskpass`, and this callback must not log either.
            push_high(
                shared,
                0,
                &Message::Event(Event::AskpassPrompt {
                    id: id.0,
                    prompt,
                    secret,
                }),
            );
        }
        SessionEvent::SignaturesInvalidated => {
            // Unticketed: a reload clears the helper's cache, and the client
            // has to drop badges it already drew.
            push_high(shared, 0, &Message::Event(Event::SignaturesInvalidated));
        }
        // The command log and the commit-graph offer stay on the helper. A
        // remote tab has no local checkout to write the graph into.
        other => {
            if let Some((local, body)) = proto::query_from_session(other) {
                let terminal = body.is_terminal();
                forward_query(shared, local, body, terminal);
            }
        }
    }
}

fn start_query(
    session: &Session,
    query: QueryRequest,
) -> Result<QueryTicket, proto::ProtocolError> {
    use lamina_core::Oid;
    let object = |id| Oid::try_from(id);
    Ok(match query {
        QueryRequest::Search(query) => session.search_commits(query.into()),
        QueryRequest::Path { path } => session.path_history(path),
        QueryRequest::StreamingPath { path } => session.path_history_streaming(path),
        QueryRequest::CommitStat(commit) => session.request_commit_stat(object(commit)?),
        QueryRequest::Compare { a, b, mode } => {
            session.request_compare(object(a)?, object(b)?, mode.into())
        }
        QueryRequest::CompareFrom { base, head } => {
            session.request_compare_from(base.map(object).transpose()?, object(head)?)
        }
        QueryRequest::CompareWorktree { base } => {
            session.request_compare_with_worktree(object(base)?)
        }
        QueryRequest::SubmodulesAndWorktrees => session.request_submodules_and_worktrees(),
        QueryRequest::Reflog { limit } => {
            let limit = usize::try_from(limit)
                .map_err(|_| proto::ProtocolError::Malformed("reflog limit does not fit"))?;
            session.request_reflog(limit)
        }
        QueryRequest::InProgressDetails => session.request_in_progress_details(),
        QueryRequest::Blame { commit, path } => session.request_blame(object(commit)?, path),
        QueryRequest::ContentSearch(query) => session.content_search_streaming(query.into()),
        QueryRequest::Signatures { commits } => {
            let commits = commits
                .into_iter()
                .map(object)
                .collect::<Result<Vec<_>, _>>()?;
            session.request_commit_signatures(commits)
        }
    })
}

fn forward_query(
    shared: &Arc<ServerShared>,
    local: QueryTicket,
    event: QueryEvent,
    terminal: bool,
) {
    let client = if terminal {
        forget_query(shared, local)
    } else {
        lock(&shared.query_routes)
            .local_to_client
            .get(&local)
            .copied()
    };
    let Some(client) = client else {
        return;
    };
    push_high(
        shared,
        0,
        &Message::Event(Event::Query {
            ticket: client,
            event,
        }),
    );
}

fn forget_query(shared: &ServerShared, ticket: QueryTicket) -> Option<u64> {
    let mut routes = lock(&shared.query_routes);
    let client = routes.local_to_client.remove(&ticket)?;
    routes.client_to_local.remove(&client);
    Some(client)
}

fn forget_diff(shared: &ServerShared, ticket: DiffTicket) -> Option<u64> {
    let mut routes = lock(&shared.diff_routes);
    let client = routes.local_to_client.remove(&ticket)?;
    routes.client_to_local.remove(&client);
    Some(client)
}

fn publish_current(shared: &Arc<ServerShared>, chunk: usize) {
    if !shared.subscribed.load(Ordering::SeqCst) {
        return;
    }
    let snapshot = {
        let guard = lock(&shared.session);
        guard.as_ref().map(|session| session.snapshot())
    };
    let Some(snapshot) = snapshot else {
        return;
    };
    let _publish = lock(&shared.snapshot_publish);
    if !shared.subscribed.load(Ordering::SeqCst) {
        return;
    }
    push_snapshot(shared, &snapshot, chunk);
}

/// Publishes a snapshot as one reset followed by commit chunks.
fn push_snapshot(
    shared: &Arc<ServerShared>,
    snapshot: &lamina_core::session::RepoSnapshot,
    chunk: usize,
) {
    if !push_low(
        shared,
        0,
        &Message::Event(Event::SnapshotReset {
            snapshot: SnapshotDto::from(snapshot),
        }),
    ) {
        return;
    }

    let generation = snapshot.generation;
    let total = snapshot.commits.len();
    // Only the local list's own flag says the walk finished; an unfinished
    // snapshot can still contain zero commits.
    let finished = snapshot.commits.complete;
    let mut start = 0;
    while start < total {
        let end = (start + chunk).min(total);
        let mut commits = Vec::with_capacity(end - start);
        let mut rows = Vec::with_capacity(end - start);
        for index in start..end {
            if let Some((commit, row)) = snapshot.commits.get(index) {
                commits.push(CommitDto::from(commit));
                rows.push(GraphRowDto::from(row));
            }
        }
        start = end;
        if !push_low(
            shared,
            0,
            &Message::Event(Event::SnapshotAppend {
                generation,
                commits,
                rows,
                complete: finished && start >= total,
            }),
        ) {
            return;
        }
    }

    if total == 0 {
        // An empty history still needs a terminator so the client knows the
        // snapshot is complete.
        push_low(
            shared,
            0,
            &Message::Event(Event::SnapshotAppend {
                generation,
                commits: Vec::new(),
                rows: Vec::new(),
                complete: finished,
            }),
        );
    }
}

fn push_high(shared: &Arc<ServerShared>, stream: u16, message: &Message) -> bool {
    enqueue(shared, stream, message, true)
}

fn push_low(shared: &Arc<ServerShared>, stream: u16, message: &Message) -> bool {
    enqueue(shared, stream, message, false)
}

fn enqueue(shared: &Arc<ServerShared>, stream: u16, message: &Message, high: bool) -> bool {
    let mut frame = Vec::new();
    if let Err(error) = proto::write_frame(&mut frame, stream, message) {
        eprintln!(
            "lamina-remote: failed to encode {}: {error}",
            describe(message)
        );
        return false;
    }
    if high {
        shared.outbox.push_high(frame)
    } else {
        shared.outbox.push_low(frame)
    }
}

fn describe(message: &Message) -> &'static str {
    match message {
        Message::Request(_) => "request",
        Message::Response(_) => "response",
        Message::Event(_) => "event",
    }
}

fn remote_error(error: lamina_core::Error) -> RemoteError {
    match error {
        lamina_core::Error::GitCommand { args, code, stderr } => RemoteError {
            kind: proto::ErrorKind::Git,
            message: format!("git {args} failed"),
            stderr: Some(stderr),
            code,
        },
        lamina_core::Error::NotARepository(path) => RemoteError {
            kind: proto::ErrorKind::NotFound,
            message: format!("not a git repository: {}", path.display()),
            stderr: None,
            code: None,
        },
        lamina_core::Error::Io(error) => RemoteError {
            kind: proto::ErrorKind::Io,
            message: error.to_string(),
            stderr: None,
            code: None,
        },
        other => RemoteError::other(other.to_string()),
    }
}

fn protocol_error(error: proto::ProtocolError) -> RemoteError {
    RemoteError::new(proto::ErrorKind::Protocol, error.to_string())
}

/// The repository root the helper was pointed at, for the `--stdio` entry
/// point that parses its own arguments.
pub fn parse_root(value: Option<&str>) -> Result<PathBuf, BoxError> {
    match value {
        Some(value) => Ok(PathBuf::from(value)),
        None => Ok(std::env::current_dir()?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_queued_response_overtakes_a_snapshot_frame() {
        let outbox = Outbox::new();
        assert!(outbox.push_low(b"snapshot".to_vec()));
        assert!(outbox.push_high(b"response".to_vec()));
        assert_eq!(outbox.pop().as_deref(), Some(&b"response"[..]));
        assert_eq!(outbox.pop().as_deref(), Some(&b"snapshot"[..]));
    }

    #[test]
    fn the_snapshot_queue_blocks_until_a_slot_frees() {
        let outbox = Arc::new(Outbox::new());
        assert!(outbox.push_low(b"snapshot".to_vec()));
        let blocked = Arc::clone(&outbox);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            assert!(blocked.push_low(b"next".to_vec()));
            let _ = done_tx.send(());
        });
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "a second snapshot frame must wait for the writer"
        );
        assert_eq!(outbox.pop().as_deref(), Some(&b"snapshot"[..]));
        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("freeing the slot unblocks the walk");
        assert_eq!(outbox.pop().as_deref(), Some(&b"next"[..]));
        worker.join().expect("queue thread");
    }
}
