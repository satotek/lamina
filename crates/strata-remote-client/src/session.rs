//! Concurrent [`RemoteSession`]: one reader, many in-flight calls.
//!
//! The reader thread is the only thing that reads the stream. It pairs each
//! response with the caller that sent that stream id and turns events into
//! [`SessionEvent`]s. Callers wait on their own channel, so a diff that has not
//! finished does not stop a `commit_message` or a `CancelDiff` write.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use strata_core::compare::{RepoCompareFile, RepoCompareSide, ThreeWayTreeCompare};
use strata_core::diff::DiffOptions;
use strata_core::git_cli::CommandLog;
use strata_core::lfs::{LfsInfo, LfsLock, LfsPointer};
use strata_core::merge::ConflictStyle;
use strata_core::repo::{MergeTool, SelectionRange, ThreeWayRoles, UpstreamTarget};
use strata_core::session::{
    CommitList, CommitQuery, ContentSearchQuery, DiffRequest, DiffTicket, DiffView, GitExtras,
    HistoryScope, Operation, OperationId, QueryTicket, RepoLocation, RepoReader, RepoSnapshot,
    RepositorySession, SessionEvent,
};
use strata_core::{
    ConflictSides, Error, FileChange, HeadInfo, Oid, RepoState, Result, TreeEntry, WorktreeStatus,
};
use strata_remote_protocol::{
    self as proto, DiffOptionsDto, DiffRequestDto, ErrorKind, Event, Message, ObjectId,
    OperationDto, RemoteError, Request, Response, rows_from_dto,
};

/// Returned by every operation this slice does not carry over the wire.
pub const UNSUPPORTED: &str = "not supported over remote yet";

/// [`SessionEvent::LoadFailed`] text when the helper stops answering.
///
/// The last snapshot stays readable. Further requests fail with the same text
/// instead of waiting out another timeout. `ssh` stderr, when the transport
/// captured any, is appended after a colon.
pub const CONNECTION_LOST: &str = "connection to the remote repository was lost";

/// How often an idle connection is probed, and how many unanswered probes drop it.
const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_MISSED_PINGS: u32 = 3;
const DEFAULT_FIRST_CHUNK: Duration = Duration::from_secs(30);

/// Where the helper is running. The repository path comes from the helper's
/// handshake, because it is the one that resolved `~` and relative paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTarget {
    pub host: String,
    pub port: Option<u16>,
}

impl RemoteTarget {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: None,
        }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }
}

/// Knobs a test shortens. Production uses [`ClientConfig::default`].
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// A ping goes out after this long with no inbound frame.
    pub ping_interval: Duration,
    /// Unanswered idle intervals before the connection is dropped.
    pub missed_pings: u32,
    /// How long [`RemoteSession::connect`] waits for the first snapshot chunk.
    pub first_chunk_timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            ping_interval: DEFAULT_PING_INTERVAL,
            missed_pings: DEFAULT_MISSED_PINGS,
            first_chunk_timeout: DEFAULT_FIRST_CHUNK,
        }
    }
}

/// Extra pieces an `ssh` transport keeps alive for the session.
pub(crate) struct TransportAttach {
    pub diagnostic: Arc<dyn Fn() -> String + Send + Sync>,
    pub disconnect: Option<Box<dyn FnOnce() + Send>>,
    /// Process handles and the askpass host. Dropped after [`Inner::shutdown`]
    /// so the child still exists when the disconnect hook kills it.
    pub retain: Option<Box<dyn Send>>,
}

impl Default for TransportAttach {
    fn default() -> Self {
        Self {
            diagnostic: Arc::new(String::new),
            disconnect: None,
            retain: None,
        }
    }
}

/// A [`RepositorySession`] backed by a `strata-remote` helper.
pub struct RemoteSession {
    inner: Arc<Inner>,
    location: RepoLocation,
    /// Process handles. Not read; their `Drop` runs after [`Drop`] so `ssh`
    /// still exists when the disconnect hook kills it.
    #[allow(dead_code)]
    retain: Option<Box<dyn Send>>,
}

struct OpRejected {
    error: Error,
    /// The connection loss already emitted `OperationFinished` for this id.
    reported: bool,
}

struct Inner {
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    pending: Mutex<HashMap<u16, Sender<Response>>>,
    /// Diff requests do not wait. The stream id is how an `Error` response
    /// finds the ticket to fail.
    diff_streams: Mutex<HashMap<u16, DiffTicket>>,
    /// `Run` does not wait. An `Error` response fails the operation; `Ok`
    /// means later events carry the outcome.
    operation_streams: Mutex<HashMap<u16, OperationId>>,
    /// Operations whose `OperationFinished` has not arrived. A lost connection
    /// finishes each of them so the UI does not stay busy.
    inflight: Mutex<HashSet<OperationId>>,
    next_stream: AtomicU16,
    snapshot: RwLock<Arc<RepoSnapshot>>,
    /// Bumped under `park` so a waiter cannot miss the wakeup for a chunk.
    snapshot_note: AtomicU64,
    events: Box<dyn Fn(SessionEvent) + Send + Sync>,
    next_ticket: AtomicU64,
    next_operation: AtomicU64,
    git_dir: PathBuf,
    root: PathBuf,
    hidden_refs: Mutex<Vec<String>>,
    history_scope: Mutex<HistoryScope>,
    lost: AtomicBool,
    closing: AtomicBool,
    lost_text: Mutex<Option<String>>,
    last_rx: Mutex<Instant>,
    missed: AtomicU32,
    ping_interval: Duration,
    missed_limit: u32,
    first_chunk_timeout: Duration,
    diagnostic: Arc<dyn Fn() -> String + Send + Sync>,
    disconnect: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    park: Mutex<()>,
    wake: Condvar,
    reader: Mutex<Option<JoinHandle<()>>>,
    reader_id: Mutex<Option<ThreadId>>,
    ping: Mutex<Option<JoinHandle<()>>>,
}

struct HelloAck {
    root: String,
    git_dir: String,
}

impl RemoteSession {
    /// Handshakes and waits for the first snapshot chunk.
    ///
    /// `reader` and `writer` are the two directions of one connection. They are
    /// driven on separate threads so a write (a read request, a cancel, a ping)
    /// does not wait for the frame currently being received. Returns when the
    /// helper has accepted the snapshot subscription and at least one chunk has
    /// arrived; the rest of the history continues to stream in as
    /// [`SessionEvent::SnapshotUpdated`].
    pub fn connect(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        target: RemoteTarget,
        on_event: impl Fn(SessionEvent) + Send + Sync + 'static,
    ) -> Result<Self> {
        Self::connect_with(reader, writer, target, on_event, ClientConfig::default())
    }

    /// [`connect`](Self::connect) with a test-supplied ping and chunk timeout.
    pub fn connect_with(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        target: RemoteTarget,
        on_event: impl Fn(SessionEvent) + Send + Sync + 'static,
        config: ClientConfig,
    ) -> Result<Self> {
        Self::connect_attached(
            reader,
            writer,
            target,
            on_event,
            config,
            TransportAttach::default(),
        )
    }

    pub(crate) fn connect_attached(
        mut reader: impl Read + Send + 'static,
        mut writer: impl Write + Send + 'static,
        target: RemoteTarget,
        on_event: impl Fn(SessionEvent) + Send + Sync + 'static,
        config: ClientConfig,
        mut attach: TransportAttach,
    ) -> Result<Self> {
        let (ack, early) = match handshake(&mut reader, &mut writer) {
            Ok(value) => value,
            Err(error) => {
                // The reader thread does not exist yet, so shutdown cannot
                // reap an ssh child that failed before HelloAck.
                let diagnostic = Arc::clone(&attach.diagnostic);
                if let Some(disconnect) = attach.disconnect.take() {
                    disconnect();
                }
                return Err(annotate(error, diagnostic.as_ref()));
            }
        };
        let inner = Arc::new(Inner {
            writer: Mutex::new(Some(Box::new(writer))),
            pending: Mutex::new(HashMap::new()),
            diff_streams: Mutex::new(HashMap::new()),
            operation_streams: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashSet::new()),
            next_stream: AtomicU16::new(2),
            snapshot: RwLock::new(Arc::new(empty_snapshot())),
            snapshot_note: AtomicU64::new(0),
            events: Box::new(on_event),
            next_ticket: AtomicU64::new(0),
            next_operation: AtomicU64::new(0),
            git_dir: PathBuf::from(&ack.git_dir),
            root: PathBuf::from(&ack.root),
            hidden_refs: Mutex::new(Vec::new()),
            history_scope: Mutex::new(HistoryScope::All),
            lost: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            lost_text: Mutex::new(None),
            last_rx: Mutex::new(Instant::now()),
            missed: AtomicU32::new(0),
            ping_interval: config.ping_interval,
            missed_limit: config.missed_pings.max(1),
            first_chunk_timeout: config.first_chunk_timeout,
            diagnostic: attach.diagnostic,
            disconnect: Mutex::new(attach.disconnect),
            park: Mutex::new(()),
            wake: Condvar::new(),
            reader: Mutex::new(None),
            reader_id: Mutex::new(None),
            ping: Mutex::new(None),
        });
        // Anything the helper sent before HelloAck is part of the same stream.
        // Applying it here, before the reader starts, keeps those frames from
        // sitting unread in a buffer this handshake did not keep.
        for event in early {
            inner.apply_event(event);
        }

        let reader_inner = Arc::clone(&inner);
        let reader_handle = thread::Builder::new()
            .name("strata-remote-read".to_owned())
            .spawn(move || reader_loop(reader_inner, reader))
            .map_err(|error| Error::other(format!("could not start the remote reader: {error}")))?;
        *lock(&inner.reader) = Some(reader_handle);

        let ping_inner = Arc::clone(&inner);
        let ping_handle = match thread::Builder::new()
            .name("strata-remote-ping".to_owned())
            .spawn(move || ping_loop(ping_inner))
        {
            Ok(handle) => handle,
            Err(error) => {
                inner.shutdown();
                return Err(Error::other(format!(
                    "could not start the remote keepalive: {error}"
                )));
            }
        };
        *lock(&inner.ping) = Some(ping_handle);

        let session = Self {
            inner,
            location: RepoLocation::Ssh {
                host: target.host,
                port: target.port,
                path: ack.root,
            },
            retain: attach.retain,
        };
        session.finish_connect()?;
        Ok(session)
    }

    fn finish_connect(&self) -> Result<()> {
        match self.inner.call(Request::SubscribeSnapshots)? {
            Response::Ok => {}
            other => return Err(unexpected(other)),
        }
        self.inner.wait_for_first_chunk()
    }

    fn emit(&self, event: SessionEvent) {
        (self.inner.events)(event);
    }

    fn unsupported_query(&self) -> QueryTicket {
        self.emit(SessionEvent::QueryFailed {
            ticket: QueryTicket(0),
            error: UNSUPPORTED.to_owned(),
        });
        QueryTicket(0)
    }

    /// Stop a mutation that was started with [`RepositorySession::run`].
    ///
    /// The helper kills the git child, or skips the job if it has not started.
    /// The outcome still arrives as [`SessionEvent::OperationFinished`].
    pub fn cancel_operation(&self, id: OperationId) {
        let _ = self
            .inner
            .send_detached(Request::CancelOperation { id: id.0 });
    }
}

impl Drop for RemoteSession {
    fn drop(&mut self) {
        // `retain` (the ssh child) drops after this returns, so shutdown can
        // still close stdin and read the resulting EOF before the process is
        // killed.
        self.inner.shutdown();
    }
}

fn handshake(reader: &mut impl Read, writer: &mut impl Write) -> Result<(HelloAck, Vec<Event>)> {
    let stream = 1u16;
    proto::write_frame(
        writer,
        stream,
        &Message::Request(Request::Hello {
            protocol: proto::PROTOCOL_VERSION,
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            build: option_env!("STRATA_BUILD_ID").unwrap_or("dev").to_owned(),
        }),
    )
    .map_err(|error| Error::other(format!("remote write failed: {error}")))?;

    let mut early = Vec::new();
    loop {
        let frame = read_frame(reader)?;
        if frame.stream != stream {
            if let Message::Event(event) = frame.message {
                early.push(event);
            }
            continue;
        }
        match frame.message {
            Message::Response(Response::HelloAck { root, git_dir, .. }) => {
                return Ok((HelloAck { root, git_dir }, early));
            }
            Message::Response(Response::VersionMismatch { expected, got }) => {
                return Err(Error::other(format!(
                    "remote protocol version mismatch: helper speaks {expected}, app speaks {got}"
                )));
            }
            Message::Response(Response::Error(error)) => return Err(remote_error(error)),
            Message::Event(event) => early.push(event),
            Message::Response(_) => continue,
            Message::Request(_) => {
                return Err(Error::other(
                    "the remote helper sent a request to the client",
                ));
            }
        }
    }
}

fn read_frame(reader: &mut impl Read) -> Result<proto::Frame> {
    proto::read_frame(reader)
        .map_err(|error| Error::other(format!("remote read failed: {error}")))?
        .ok_or_else(|| Error::other("the remote helper closed the connection"))
}

fn reader_loop(inner: Arc<Inner>, mut reader: impl Read) {
    *lock(&inner.reader_id) = Some(thread::current().id());
    loop {
        if inner.closing.load(Ordering::SeqCst) {
            break;
        }
        match proto::read_frame(&mut reader) {
            Ok(Some(frame)) => {
                *lock(&inner.last_rx) = Instant::now();
                inner.missed.store(0, Ordering::SeqCst);
                inner.dispatch(frame);
            }
            Ok(None) => {
                inner.fail(inner.failure_text(None));
                break;
            }
            Err(error) => {
                inner.fail(inner.failure_text(Some(error.to_string())));
                break;
            }
        }
    }
}

fn ping_loop(inner: Arc<Inner>) {
    let interval = inner.ping_interval;
    let mut guard = lock(&inner.park);
    loop {
        let (next, _) = inner
            .wake
            .wait_timeout(guard, interval)
            .unwrap_or_else(PoisonError::into_inner);
        guard = next;
        if inner.closing.load(Ordering::SeqCst) || inner.lost.load(Ordering::SeqCst) {
            break;
        }
        // `park` is also taken by snapshot wakeups. Drop it before touching
        // `last_rx`, which the reader writes immediately before taking `park`.
        drop(guard);
        let idle = lock(&inner.last_rx).elapsed() >= interval;
        if !idle {
            inner.missed.store(0, Ordering::SeqCst);
            guard = lock(&inner.park);
            continue;
        }
        let missed = inner.missed.fetch_add(1, Ordering::SeqCst) + 1;
        if missed >= inner.missed_limit && lock(&inner.last_rx).elapsed() >= interval {
            inner.fail(inner.failure_text(None));
            break;
        }
        // Stream 0 is never a caller's request. The pong only exists so the
        // reader sees a frame and clears the idle count; nobody is waiting.
        if inner
            .write_message(
                0,
                &Message::Request(Request::Ping {
                    nonce: u64::from(missed),
                }),
            )
            .is_err()
        {
            break;
        }
        guard = lock(&inner.park);
    }
}

impl Inner {
    fn shutdown(&self) {
        if self.closing.swap(true, Ordering::SeqCst) {
            return;
        }
        self.wake.notify_all();
        if let Some(handle) = lock(&self.ping).take() {
            let _ = handle.join();
        }
        {
            let mut writer = lock(&self.writer);
            *writer = None;
        }
        if let Some(disconnect) = lock(&self.disconnect).take() {
            disconnect();
        }
        if let Some(handle) = lock(&self.reader).take() {
            let reader_id = *lock(&self.reader_id);
            if reader_id == Some(thread::current().id()) {
                // The event callback dropped the session. Joining here would
                // wait for this same thread.
                drop(handle);
            } else {
                let _ = handle.join();
            }
        }
    }

    fn failure_text(&self, detail: Option<String>) -> String {
        let diagnostic = (self.diagnostic)();
        let diagnostic = diagnostic.trim();
        match (detail, diagnostic.is_empty()) {
            (Some(detail), true) => format!("{CONNECTION_LOST}: {detail}"),
            (Some(detail), false) => format!("{CONNECTION_LOST}: {detail}: {diagnostic}"),
            (None, true) => CONNECTION_LOST.to_owned(),
            (None, false) => format!("{CONNECTION_LOST}: {diagnostic}"),
        }
    }

    fn lost_error(&self) -> Error {
        let stored = lock(&self.lost_text).clone();
        Error::other(stored.unwrap_or_else(|| CONNECTION_LOST.to_owned()))
    }

    fn fail(&self, message: String) {
        if self.lost.swap(true, Ordering::SeqCst) {
            return;
        }
        *lock(&self.lost_text) = Some(message.clone());
        let pending = std::mem::take(&mut *lock(&self.pending));
        drop(pending);
        let diffs = std::mem::take(&mut *lock(&self.diff_streams));
        let _operations = std::mem::take(&mut *lock(&self.operation_streams));
        let inflight = std::mem::take(&mut *lock(&self.inflight));
        {
            let _park = lock(&self.park);
            self.wake.notify_all();
        }
        // A drop already set `closing`: the app closed the session on purpose,
        // and a LoadFailed for that would look like the repository broke.
        if self.closing.load(Ordering::SeqCst) {
            return;
        }
        (self.events)(SessionEvent::LoadFailed {
            error: message.clone(),
        });
        for ticket in diffs.into_values() {
            (self.events)(SessionEvent::DiffFailed {
                ticket,
                error: message.clone(),
            });
        }
        for id in inflight {
            (self.events)(SessionEvent::OperationFinished {
                id,
                error: Some(message.clone()),
            });
        }
    }

    fn ensure_open(&self) -> Result<()> {
        if self.lost.load(Ordering::SeqCst) || self.closing.load(Ordering::SeqCst) {
            Err(self.lost_error())
        } else {
            Ok(())
        }
    }

    fn alloc_stream(&self) -> Result<u16> {
        for _ in 0..u16::MAX {
            let id = self.next_stream.fetch_add(1, Ordering::SeqCst);
            if id == 0 {
                continue;
            }
            // Separate statements so the two maps are never locked together.
            let pending = lock(&self.pending).contains_key(&id);
            let diff = lock(&self.diff_streams).contains_key(&id);
            let operation = lock(&self.operation_streams).contains_key(&id);
            if !pending && !diff && !operation {
                return Ok(id);
            }
        }
        Err(Error::other("too many remote requests are in flight"))
    }

    fn write_message(&self, stream: u16, message: &Message) -> Result<()> {
        let mut guard = lock(&self.writer);
        if self.lost.load(Ordering::SeqCst) || self.closing.load(Ordering::SeqCst) {
            return Err(self.lost_error());
        }
        let Some(writer) = guard.as_mut() else {
            return Err(self.lost_error());
        };
        // `writer` is `&mut Box<dyn Write>`, which is sized. `as_mut()` would
        // yield `&mut dyn Write` and `write_frame`'s generic could not name it.
        let result = proto::write_frame(writer, stream, message);
        drop(guard);
        result.map_err(|error| {
            let message = format!("{CONNECTION_LOST}: {error}");
            self.fail(message);
            self.lost_error()
        })
    }

    fn call(&self, request: Request) -> Result<Response> {
        self.ensure_open()?;
        let (tx, rx) = mpsc::channel();
        let stream = self.alloc_stream()?;
        lock(&self.pending).insert(stream, tx);
        if self.lost.load(Ordering::SeqCst) {
            lock(&self.pending).remove(&stream);
            return Err(self.lost_error());
        }
        if let Err(error) = self.write_message(stream, &Message::Request(request)) {
            lock(&self.pending).remove(&stream);
            return Err(error);
        }
        match rx.recv() {
            Ok(Response::Error(error)) => Err(remote_error(error)),
            Ok(other) => Ok(other),
            Err(_) => Err(self.lost_error()),
        }
    }

    /// `Run` returns before the git command does. The stream stays registered
    /// until the helper accepts or rejects it, so an `Error` still finds the id.
    ///
    /// `reported` is true when a broken write already finished the operation
    /// through [`Inner::fail`]. The caller must not emit a second time.
    fn send_operation(&self, id: OperationId, operation: OperationDto) -> Result<(), OpRejected> {
        if let Err(error) = self.ensure_open() {
            return Err(OpRejected {
                error,
                reported: false,
            });
        }
        let stream = match self.alloc_stream() {
            Ok(stream) => stream,
            Err(error) => {
                return Err(OpRejected {
                    error,
                    reported: false,
                });
            }
        };
        lock(&self.operation_streams).insert(stream, id);
        lock(&self.inflight).insert(id);
        if let Err(error) = self.write_message(
            stream,
            &Message::Request(Request::Run {
                id: id.0,
                operation,
            }),
        ) {
            lock(&self.operation_streams).remove(&stream);
            // `fail` takes the whole set and emits. If the id is still here,
            // that did not happen and the caller has to finish it.
            let reported = !lock(&self.inflight).remove(&id);
            return Err(OpRejected { error, reported });
        }
        Ok(())
    }

    /// Emit `OperationFinished` once. The helper event and a lost connection
    /// can both try; the second one finds the id already gone.
    fn finish_operation(&self, id: OperationId, error: Option<String>) {
        if lock(&self.inflight).remove(&id) {
            (self.events)(SessionEvent::OperationFinished { id, error });
        }
    }

    /// Writes a frame and does not wait. Cancellation and diff requests use
    /// this so they return while another call is still outstanding.
    fn send_detached(&self, request: Request) -> Result<u16> {
        self.ensure_open()?;
        let stream = self.alloc_stream()?;
        self.write_message(stream, &Message::Request(request))?;
        Ok(stream)
    }

    fn dispatch(&self, frame: proto::Frame) {
        match frame.message {
            Message::Response(response) => {
                if let Some(tx) = lock(&self.pending).remove(&frame.stream) {
                    let _ = tx.send(response);
                    return;
                }
                // `Ok` means later events carry the ticket. Leaving the stream
                // registered would leak an id until the u16 space wrapped.
                if let Some(ticket) = lock(&self.diff_streams).remove(&frame.stream)
                    && let Response::Error(error) = response
                {
                    (self.events)(SessionEvent::DiffFailed {
                        ticket,
                        error: error.message,
                    });
                    return;
                }
                // `Ok` means later events carry the outcome. An `Error` is the
                // outcome: the helper rejected the operation before it started.
                if let Some(id) = lock(&self.operation_streams).remove(&frame.stream)
                    && let Response::Error(error) = response
                {
                    self.finish_operation(id, Some(error.message));
                }
            }
            Message::Event(event) => self.apply_event(event),
            Message::Request(_) => {
                self.fail(format!(
                    "{CONNECTION_LOST}: the remote helper sent a request to the client"
                ));
            }
        }
    }

    fn apply_event(&self, event: Event) {
        match event {
            Event::SnapshotReset { snapshot } => match RepoSnapshot::try_from(snapshot) {
                Ok(snapshot) => {
                    let generation = snapshot.generation;
                    *write(&self.snapshot) = Arc::new(snapshot);
                    self.note_snapshot();
                    (self.events)(SessionEvent::SnapshotUpdated { generation });
                }
                Err(error) => (self.events)(SessionEvent::LoadFailed {
                    error: error.to_string(),
                }),
            },
            Event::SnapshotAppend {
                generation,
                commits,
                rows,
                complete,
            } => {
                let mut guard = write(&self.snapshot);
                if guard.generation != generation {
                    // A reset for a newer snapshot already replaced this list.
                    return;
                }
                let mut next = RepoSnapshot::clone(&guard);
                let mut index = (*next.commit_index).clone();
                let base = next.commits.len();
                let mut infos = Vec::with_capacity(commits.len());
                for (offset, commit) in commits.into_iter().enumerate() {
                    match strata_core::CommitInfo::try_from(commit) {
                        Ok(commit) => {
                            index.insert(commit.id, base + offset);
                            infos.push(commit);
                        }
                        Err(error) => {
                            drop(guard);
                            (self.events)(SessionEvent::LoadFailed {
                                error: error.to_string(),
                            });
                            return;
                        }
                    }
                }
                if infos.len() != rows.len() {
                    drop(guard);
                    (self.events)(SessionEvent::LoadFailed {
                        error: "remote snapshot chunk has mismatched commits and rows".to_owned(),
                    });
                    return;
                }
                next.commits.extend(infos, rows_from_dto(&rows));
                next.commits.complete = complete;
                next.commit_index = Arc::new(index);
                *guard = Arc::new(next);
                drop(guard);
                self.note_snapshot();
                (self.events)(SessionEvent::SnapshotUpdated { generation });
            }
            Event::DiffReady { ticket, view } => match DiffView::try_from(*view) {
                Ok(view) => (self.events)(SessionEvent::DiffReady {
                    ticket: DiffTicket(ticket),
                    view: Arc::new(view),
                }),
                Err(error) => (self.events)(SessionEvent::DiffFailed {
                    ticket: DiffTicket(ticket),
                    error: error.to_string(),
                }),
            },
            Event::DiffFailed { ticket, error } => (self.events)(SessionEvent::DiffFailed {
                ticket: DiffTicket(ticket),
                error,
            }),
            Event::LoadFailed { error } => (self.events)(SessionEvent::LoadFailed { error }),
            Event::OperationStarted { id } => {
                (self.events)(SessionEvent::OperationStarted {
                    id: OperationId(id),
                });
            }
            Event::OperationProgress { id, text, percent } => {
                (self.events)(SessionEvent::OperationProgress {
                    id: OperationId(id),
                    text,
                    percent,
                });
            }
            Event::OperationFinished { id, error } => {
                self.finish_operation(OperationId(id), error);
            }
            Event::AskpassPrompt { id, prompt, secret } => {
                (self.events)(SessionEvent::AskpassPrompt {
                    id: OperationId(id),
                    prompt,
                    secret,
                });
            }
        }
    }

    fn note_snapshot(&self) {
        // The waiter rechecks this counter while holding `park`, which is the
        // same lock `notify` takes, so a chunk cannot land between the check
        // and the wait.
        let _park = lock(&self.park);
        self.snapshot_note.fetch_add(1, Ordering::SeqCst);
        self.wake.notify_all();
    }

    fn snapshot_ready(&self) -> bool {
        let snapshot = read(&self.snapshot);
        snapshot.commits.complete || !snapshot.commits.is_empty()
    }

    fn wait_for_first_chunk(&self) -> Result<()> {
        let deadline = Instant::now() + self.first_chunk_timeout;
        loop {
            if self.lost.load(Ordering::SeqCst) {
                return Err(self.lost_error());
            }
            if self.snapshot_ready() {
                return Ok(());
            }
            let seen = self.snapshot_note.load(Ordering::SeqCst);
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::other(
                    "timed out waiting for the first remote snapshot chunk",
                ));
            }
            let guard = lock(&self.park);
            if self.snapshot_note.load(Ordering::SeqCst) != seen || self.lost.load(Ordering::SeqCst)
            {
                continue;
            }
            let (_guard, waited) = self
                .wake
                .wait_timeout(guard, remaining)
                .unwrap_or_else(PoisonError::into_inner);
            if waited.timed_out() && !self.snapshot_ready() && !self.lost.load(Ordering::SeqCst) {
                return Err(Error::other(
                    "timed out waiting for the first remote snapshot chunk",
                ));
            }
        }
    }

    fn request_diff_with(&self, request: DiffRequest, options: DiffOptions) -> DiffTicket {
        let ticket = DiffTicket(self.next_ticket.fetch_add(1, Ordering::SeqCst) + 1);
        let send = (|| -> Result<()> {
            self.ensure_open()?;
            let stream = self.alloc_stream()?;
            lock(&self.diff_streams).insert(stream, ticket);
            if let Err(error) = self.write_message(
                stream,
                &Message::Request(Request::Diff {
                    ticket: ticket.0,
                    request: DiffRequestDto::from(&request),
                    options: DiffOptionsDto::from(&options),
                }),
            ) {
                lock(&self.diff_streams).remove(&stream);
                return Err(error);
            }
            Ok(())
        })();
        if let Err(error) = send {
            (self.events)(SessionEvent::DiffFailed {
                ticket,
                error: error.to_string(),
            });
        }
        ticket
    }
}

impl RepositorySession for RemoteSession {
    fn location(&self) -> RepoLocation {
        self.location.clone()
    }

    fn reader(&self) -> Arc<dyn RepoReader> {
        Arc::new(RemoteReader {
            inner: Arc::downgrade(&self.inner),
            git_dir: self.inner.git_dir.clone(),
        })
    }

    fn git_extras(&self) -> Arc<dyn GitExtras> {
        Arc::new(RemoteExtras {
            root: self.inner.root.clone(),
            log: Arc::new(CommandLog::new()),
        })
    }

    fn snapshot(&self) -> Arc<RepoSnapshot> {
        Arc::clone(&read(&self.inner.snapshot))
    }

    fn run(&self, op: Operation) -> OperationId {
        let id = OperationId(self.inner.next_operation.fetch_add(1, Ordering::SeqCst) + 1);
        let Ok(operation) = OperationDto::try_from(&op) else {
            self.emit(SessionEvent::OperationFinished {
                id,
                error: Some(UNSUPPORTED.to_owned()),
            });
            return id;
        };
        if let Err(rejected) = self.inner.send_operation(id, operation)
            && !rejected.reported
        {
            self.emit(SessionEvent::OperationFinished {
                id,
                error: Some(rejected.error.to_string()),
            });
        }
        id
    }

    fn answer_askpass(&self, id: OperationId, answer: Option<String>) -> bool {
        // True when the answer was written. The helper drops it if the prompt
        // is already gone; the modal does not retry.
        self.inner
            .send_detached(Request::AnswerAskpass { id: id.0, answer })
            .is_ok()
    }

    fn request_diff(&self, request: DiffRequest) -> DiffTicket {
        self.request_diff_with(request, DiffOptions::default())
    }

    fn request_diff_with(&self, request: DiffRequest, options: DiffOptions) -> DiffTicket {
        self.inner.request_diff_with(request, options)
    }

    fn cancel_diff(&self, ticket: DiffTicket) {
        // Not `call`: an in-flight read must not delay the cancel frame.
        let _ = self
            .inner
            .send_detached(Request::CancelDiff { ticket: ticket.0 });
    }

    fn search_commits(&self, _query: CommitQuery) -> QueryTicket {
        self.unsupported_query()
    }

    fn path_history(&self, _path: String) -> QueryTicket {
        self.unsupported_query()
    }

    fn path_history_streaming(&self, _path: String) -> QueryTicket {
        self.unsupported_query()
    }

    fn request_commit_stat(&self, _commit: Oid) -> QueryTicket {
        self.unsupported_query()
    }

    fn request_compare(
        &self,
        _a: Oid,
        _b: Oid,
        _mode: strata_core::session::CompareMode,
    ) -> QueryTicket {
        self.unsupported_query()
    }

    fn request_compare_from(&self, _base: Option<Oid>, _head: Oid) -> QueryTicket {
        self.unsupported_query()
    }

    fn request_compare_with_worktree(&self, _base: Oid) -> QueryTicket {
        self.unsupported_query()
    }

    fn request_submodules_and_worktrees(&self) -> QueryTicket {
        self.unsupported_query()
    }

    fn request_reflog(&self, _limit: usize) -> QueryTicket {
        self.unsupported_query()
    }

    fn request_in_progress_details(&self) -> QueryTicket {
        self.unsupported_query()
    }

    fn request_blame(&self, _commit: Oid, _path: String) -> QueryTicket {
        self.unsupported_query()
    }

    fn content_search_streaming(&self, _query: ContentSearchQuery) -> QueryTicket {
        self.unsupported_query()
    }

    fn request_commit_signatures(&self, _commits: Vec<Oid>) -> QueryTicket {
        self.unsupported_query()
    }

    fn cancel_query(&self, _ticket: QueryTicket) {}

    fn reload(&self) {
        if let Err(error) = self.inner.send_detached(Request::Reload) {
            self.emit(SessionEvent::LoadFailed {
                error: error.to_string(),
            });
        }
    }

    fn set_history_scope(&self, scope: HistoryScope) {
        *lock(&self.inner.history_scope) = scope;
    }

    fn history_scope(&self) -> HistoryScope {
        lock(&self.inner.history_scope).clone()
    }

    fn set_hidden_refs(&self, refs: Vec<String>) {
        *lock(&self.inner.hidden_refs) = refs;
    }

    fn hidden_refs(&self) -> Vec<String> {
        lock(&self.inner.hidden_refs).clone()
    }
}

/// Reader calls carried over the wire.
struct RemoteReader {
    inner: std::sync::Weak<Inner>,
    /// Copied out of the session so `git_dir` can hand out a `&Path`.
    git_dir: PathBuf,
}

impl RemoteReader {
    fn call(&self, request: Request) -> Result<Response> {
        self.inner
            .upgrade()
            .ok_or_else(|| Error::other("the remote session was closed"))?
            .call(request)
    }
}

fn object_id(oid: Oid) -> ObjectId {
    ObjectId::from(oid)
}

impl RepoReader for RemoteReader {
    fn workdir(&self) -> Option<&Path> {
        // The remote working tree is not on this machine.
        None
    }

    fn git_dir(&self) -> &Path {
        // The remote git directory, exposed because the trait requires a path.
        // It must not be used for local filesystem access.
        &self.git_dir
    }

    fn head(&self) -> Result<HeadInfo> {
        match self.call(Request::Head)? {
            Response::Head { head } => head
                .map(|head| {
                    HeadInfo::try_from(head).map_err(|error| Error::other(error.to_string()))
                })
                .transpose()?
                .ok_or_else(|| Error::other("the remote repository has no HEAD")),
            other => Err(unexpected(other)),
        }
    }

    fn commit_changes(&self, commit: Oid) -> Result<Vec<FileChange>> {
        match self.call(Request::CommitChanges {
            commit: object_id(commit),
        })? {
            Response::CommitChanges { changes } => {
                Ok(changes.into_iter().map(Into::into).collect())
            }
            other => Err(unexpected(other)),
        }
    }

    fn tree_entries(&self, commit: Oid, dir_path: &str) -> Result<Vec<TreeEntry>> {
        match self.call(Request::TreeEntries {
            commit: object_id(commit),
            dir: dir_path.to_owned(),
        })? {
            Response::TreeEntries { entries } => Ok(entries.into_iter().map(Into::into).collect()),
            other => Err(unexpected(other)),
        }
    }

    fn blob_at(&self, commit: Oid, path: &str) -> Result<Vec<u8>> {
        match self.call(Request::BlobAt {
            commit: object_id(commit),
            path: path.to_owned(),
        })? {
            Response::BlobAt { data } => Ok(data),
            other => Err(unexpected(other)),
        }
    }

    fn file_in_worktree(&self, _path: &str) -> Result<Option<Vec<u8>>> {
        Err(Error::other(UNSUPPORTED))
    }

    fn working_tree_encoding(&self, _path: &str) -> Option<String> {
        None
    }

    fn lfs_info_at(&self, _commit: Oid, _path: &str) -> Result<Option<LfsInfo>> {
        Err(Error::other(UNSUPPORTED))
    }

    fn lfs_info_in_index(&self, _path: &str) -> Result<Option<LfsInfo>> {
        Err(Error::other(UNSUPPORTED))
    }

    fn lfs_info_in_worktree(&self, _path: &str) -> Result<Option<LfsInfo>> {
        Err(Error::other(UNSUPPORTED))
    }

    fn uses_lfs(&self) -> bool {
        false
    }

    fn read_lfs_object(&self, _pointer: &LfsPointer, _max_bytes: usize) -> Result<Option<Vec<u8>>> {
        Err(Error::other(UNSUPPORTED))
    }

    fn parents(&self, commit: Oid) -> Result<Vec<Oid>> {
        match self.call(Request::Parents {
            commit: object_id(commit),
        })? {
            Response::Parents { parents } => parents
                .into_iter()
                .map(|parent| {
                    Oid::try_from(parent).map_err(|error| Error::other(error.to_string()))
                })
                .collect(),
            other => Err(unexpected(other)),
        }
    }

    fn commits_between(&self, base: Oid, tip: Oid) -> Result<Vec<Oid>> {
        match self.call(Request::CommitsBetween {
            base: object_id(base),
            tip: object_id(tip),
        })? {
            Response::CommitsBetween { commits } => commits
                .into_iter()
                .map(|commit| {
                    Oid::try_from(commit).map_err(|error| Error::other(error.to_string()))
                })
                .collect(),
            other => Err(unexpected(other)),
        }
    }

    fn is_ancestor(&self, ancestor: Oid, descendant: Oid) -> Result<bool> {
        match self.call(Request::IsAncestor {
            ancestor: object_id(ancestor),
            descendant: object_id(descendant),
        })? {
            Response::IsAncestor { yes } => Ok(yes),
            other => Err(unexpected(other)),
        }
    }

    fn commit_message(&self, commit: Oid) -> Result<String> {
        match self.call(Request::CommitMessage {
            commit: object_id(commit),
        })? {
            Response::CommitMessage { message } => Ok(message),
            other => Err(unexpected(other)),
        }
    }

    fn merge_message(&self) -> Result<Option<String>> {
        match self.call(Request::MergeMessage)? {
            Response::MergeMessage { message } => Ok(message),
            other => Err(unexpected(other)),
        }
    }

    fn conflict_sides(&self, _path: &str) -> Result<ConflictSides> {
        Err(Error::other(UNSUPPORTED))
    }

    fn conflict_style(&self) -> ConflictStyle {
        ConflictStyle::default()
    }

    fn merge_tool(&self) -> Option<MergeTool> {
        None
    }

    fn selection_range(&self, _selected: &[Oid]) -> Result<Option<SelectionRange>> {
        Err(Error::other(UNSUPPORTED))
    }

    fn commit_changed_file_count(&self, _commit: Oid) -> Result<usize> {
        Err(Error::other(UNSUPPORTED))
    }

    fn branch_upstream_target(&self, _branch: &str) -> Result<Option<UpstreamTarget>> {
        Err(Error::other(UNSUPPORTED))
    }

    fn resolve_commit(&self, _spec: &str) -> Result<Option<Oid>> {
        Err(Error::other(UNSUPPORTED))
    }

    fn merge_base(&self, _a: Oid, _b: Oid) -> Result<Option<Oid>> {
        Err(Error::other(UNSUPPORTED))
    }

    fn three_way_tree(&self, _left: Oid, _base: Oid, _right: Oid) -> Result<ThreeWayTreeCompare> {
        Err(Error::other(UNSUPPORTED))
    }

    fn three_way_blobs(
        &self,
        _left: Oid,
        _base: Oid,
        _right: Oid,
        _path: &str,
    ) -> Result<[Option<Vec<u8>>; 3]> {
        Err(Error::other(UNSUPPORTED))
    }

    fn three_way_roles(&self, _commits: [Oid; 3]) -> Result<ThreeWayRoles> {
        Err(Error::other(UNSUPPORTED))
    }

    fn compare_side_files(&self, _side: RepoCompareSide) -> Result<Vec<RepoCompareFile>> {
        Err(Error::other(UNSUPPORTED))
    }

    fn compare_side_blob(&self, _side: RepoCompareSide, _path: &str) -> Result<Option<Vec<u8>>> {
        Err(Error::other(UNSUPPORTED))
    }

    fn three_way_side_tree(&self, _sides: [RepoCompareSide; 3]) -> Result<ThreeWayTreeCompare> {
        Err(Error::other(UNSUPPORTED))
    }
}

/// CLI helpers have no remote equivalent in this slice.
struct RemoteExtras {
    root: PathBuf,
    log: Arc<CommandLog>,
}

impl GitExtras for RemoteExtras {
    fn workdir(&self) -> &Path {
        // The remote root; not a local path. Every method below refuses to run.
        &self.root
    }

    fn log_handle(&self) -> Arc<CommandLog> {
        Arc::clone(&self.log)
    }

    fn lfs_available(&self) -> bool {
        false
    }

    fn lfs_locks(&self) -> Result<Vec<LfsLock>> {
        Err(Error::other(UNSUPPORTED))
    }

    fn format_patch(&self, _commit: Oid) -> Result<Vec<u8>> {
        Err(Error::other(UNSUPPORTED))
    }
}

fn remote_error(error: RemoteError) -> Error {
    let detail = match error.stderr {
        Some(stderr) if !stderr.trim().is_empty() => {
            format!("{}: {}", error.message, stderr.trim())
        }
        _ => error.message,
    };
    let prefix = match error.kind {
        ErrorKind::Git => "remote git",
        ErrorKind::Io => "remote I/O",
        ErrorKind::NotFound => "remote",
        ErrorKind::NotSupported => "remote",
        ErrorKind::Protocol => "remote protocol",
        ErrorKind::Other => "remote",
    };
    Error::other(format!("{prefix}: {detail}"))
}

fn unexpected(response: Response) -> Error {
    Error::other(format!(
        "unexpected response from the remote helper: {response:?}"
    ))
}

fn empty_snapshot() -> RepoSnapshot {
    RepoSnapshot {
        generation: 0,
        head: None,
        refs: Arc::new(Vec::new()),
        refs_by_commit: Arc::new(HashMap::new()),
        commits: CommitList::default(),
        commit_index: Arc::new(HashMap::new()),
        status: Arc::new(WorktreeStatus::default()),
        stashes: Arc::new(Vec::new()),
        upstreams: Arc::new(HashMap::new()),
        remotes: Vec::new(),
        state: RepoState::Normal,
        conflicts: Arc::new(Vec::new()),
        busy: None,
    }
}

fn annotate(error: Error, diagnostic: &(dyn Fn() -> String + Send + Sync)) -> Error {
    let extra = diagnostic();
    let extra = extra.trim();
    if extra.is_empty() {
        error
    } else {
        Error::other(format!("{error}: {extra}"))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}
