//! Headless helper that serves one repository over a byte stream.
//!
//! The app starts this binary over `ssh -T host -- strata-remote --stdio`, so
//! stdout carries framed protocol messages only. Every diagnostic goes to
//! stderr; mixing the two would corrupt the stream.
//!
//! Snapshot frames are large and numerous. They wait on a bounded queue so a
//! slow app applies backpressure to the history walk instead of the helper
//! buffering the whole repository. Responses, pings and diff events skip that
//! queue: a read has to stay answerable while history is still streaming.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use strata_core::session::{DiffTicket, RepositorySession, Session, SessionEvent};
use strata_remote_protocol::{
    self as proto, CommitDto, DiffRequestDto, DiffViewDto, Event, GraphRowDto, HeadDto, Message,
    PROTOCOL_VERSION, RemoteError, Request, Response, SnapshotDto,
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
        option_env!("STRATA_BUILD_ID").unwrap_or("dev")
    )
}

struct ServerShared {
    /// `None` until `Session::open` returns, and again once shutdown starts.
    session: Mutex<Option<Session>>,
    outbox: Arc<Outbox>,
    /// Maps between the client's diff tickets and the local session's tickets.
    diff_routes: Mutex<DiffRoutes>,
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
        .name("strata-remote-writer".to_owned())
        .spawn(move || write_frames(writer_outbox, writer))?;

    let shared = Arc::new(ServerShared {
        session: Mutex::new(None),
        outbox,
        diff_routes: Mutex::new(DiffRoutes::default()),
        snapshot_publish: Mutex::new(()),
        subscribed: AtomicBool::new(false),
        workers: Mutex::new(Vec::new()),
    });

    let callback_shared = Arc::clone(&shared);
    let chunk_size = chunk.max(1);
    let session = Session::open(&root, move |event| {
        on_session_event(&callback_shared, chunk_size, event);
    })?;
    *lock(&shared.session) = Some(session);

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
        // The request loop has to stay free for Ping and CancelDiff. A blob
        // read can take longer than the client's idle timeout.
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
    )
}

fn session_reader(
    shared: &ServerShared,
) -> Result<Arc<dyn strata_core::session::RepoReader>, Response> {
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
        .name("strata-remote-rpc".to_owned())
        .spawn(work)
    {
        Ok(handle) => {
            lock(&shared.workers).push(handle);
            true
        }
        Err(error) => {
            eprintln!("strata-remote: failed to start a read: {error}");
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
        Request::Head
        | Request::CommitChanges { .. }
        | Request::TreeEntries { .. }
        | Request::BlobAt { .. }
        | Request::CommitMessage { .. }
        | Request::Parents { .. } => {
            // `dispatch` sends these to a worker. Reaching here is a bug.
            Response::Error(RemoteError::other(
                "internal error: a read was handled on the request loop",
            ))
        }
    })
}

fn answer_read(reader: &Arc<dyn strata_core::session::RepoReader>, request: Request) -> Response {
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
        other => Response::Error(RemoteError::other(format!(
            "internal error: {other:?} is not a repository read"
        ))),
    }
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
        // Operations, progress and queries are outside this slice; the client
        // never asks for them, so nothing is forwarded.
        _ => {}
    }
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
    snapshot: &strata_core::session::RepoSnapshot,
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
            "strata-remote: failed to encode {}: {error}",
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

fn remote_error(error: strata_core::Error) -> RemoteError {
    match error {
        strata_core::Error::GitCommand { args, code, stderr } => RemoteError {
            kind: proto::ErrorKind::Git,
            message: format!("git {args} failed"),
            stderr: Some(stderr),
            code,
        },
        strata_core::Error::NotARepository(path) => RemoteError {
            kind: proto::ErrorKind::NotFound,
            message: format!("not a git repository: {}", path.display()),
            stderr: None,
            code: None,
        },
        strata_core::Error::Io(error) => RemoteError {
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
