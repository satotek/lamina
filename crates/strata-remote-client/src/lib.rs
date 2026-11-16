//! Client side of the remote repository protocol.
//!
//! [`RemoteSession`] implements [`RepositorySession`] over any byte stream, so
//! the UI can render a repository served by `strata-remote` the same way it
//! renders a local one. The first slice (P0a) covers the snapshot, the commit
//! reads and commit file diffs; everything else reports "not supported over
//! remote yet" instead of pretending to work.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, Weak};

use strata_core::diff::DiffOptions;
use strata_core::git_cli::CommandLog;
use strata_core::lfs::{LfsInfo, LfsLock, LfsPointer};
use strata_core::merge::ConflictStyle;
use strata_core::repo::{MergeTool, SelectionRange, UpstreamTarget};
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
    RemoteError, Request, Response, rows_from_dto,
};

/// Returned by every operation this slice does not carry over the wire.
pub const UNSUPPORTED: &str = "not supported over remote yet";

/// A byte stream that carries the protocol in both directions.
///
/// The blanket implementation accepts any `Read + Write`, which covers an SSH
/// child's stdin/stdout and the in-memory duplex used by tests.
pub trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

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

/// A [`RepositorySession`] backed by a `strata-remote` helper.
pub struct RemoteSession {
    inner: Arc<Inner>,
    location: RepoLocation,
}

struct Inner {
    conn: Mutex<Connection>,
    snapshot: RwLock<Arc<RepoSnapshot>>,
    events: Box<dyn Fn(SessionEvent) + Send + Sync>,
    next_ticket: AtomicU64,
    next_operation: AtomicU64,
    /// Remote paths, kept for the `&Path` accessors the traits require. They
    /// are not valid on this machine and must not be used for local I/O.
    git_dir: PathBuf,
    root: PathBuf,
    /// Settings the UI may change without a repository round trip in P0a.
    hidden_refs: Mutex<Vec<String>>,
    history_scope: Mutex<HistoryScope>,
}

struct Connection {
    stream: Box<dyn Stream>,
    next_stream: u16,
}

impl Connection {
    fn send(&mut self, request: Request) -> Result<u16> {
        let stream = self.next_stream;
        self.next_stream = self.next_stream.wrapping_add(1);
        proto::write_frame(&mut self.stream, stream, &Message::Request(request))
            .map_err(|error| Error::other(format!("remote write failed: {error}")))?;
        Ok(stream)
    }

    fn receive(&mut self) -> Result<proto::Frame> {
        proto::read_frame(&mut self.stream)
            .map_err(|error| Error::other(format!("remote read failed: {error}")))?
            .ok_or_else(|| Error::other("the remote helper closed the connection"))
    }
}

impl RemoteSession {
    /// Performs the handshake and fetches the first snapshot.
    ///
    /// Blocks until the helper answered the handshake and published a complete
    /// snapshot. Call it off the UI thread.
    pub fn connect(
        stream: impl Stream + 'static,
        target: RemoteTarget,
        on_event: impl Fn(SessionEvent) + Send + Sync + 'static,
    ) -> Result<Self> {
        let mut conn = Connection {
            stream: Box::new(stream),
            next_stream: 0,
        };
        let ack = handshake(&mut conn)?;
        let inner = Arc::new(Inner {
            conn: Mutex::new(conn),
            snapshot: RwLock::new(Arc::new(empty_snapshot())),
            events: Box::new(on_event),
            next_ticket: AtomicU64::new(0),
            next_operation: AtomicU64::new(0),
            git_dir: PathBuf::from(ack.git_dir),
            root: PathBuf::from(ack.root.clone()),
            hidden_refs: Mutex::new(Vec::new()),
            history_scope: Mutex::new(HistoryScope::All),
        });
        let session = Self {
            inner,
            location: RepoLocation::Ssh {
                host: target.host,
                port: target.port,
                path: ack.root,
            },
        };
        session.subscribe();
        session.wait_for_snapshot()?;
        Ok(session)
    }

    /// Asks the helper to start pushing snapshots.
    fn subscribe(&self) {
        match self.inner.exchange(Request::SubscribeSnapshots) {
            Ok(Response::Error(error)) => (self.inner.events)(SessionEvent::LoadFailed {
                error: error.message,
            }),
            Ok(_) => {}
            Err(error) => (self.inner.events)(SessionEvent::LoadFailed {
                error: error.to_string(),
            }),
        }
    }

    /// Pumps events until the helper finished walking history.
    ///
    /// Responses are the only place the synchronous client reads frames, so
    /// a ping is what lets trailing snapshot events reach `apply_event`.
    fn wait_for_snapshot(&self) -> Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if read(&self.inner.snapshot).commits.complete {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(Error::other(
                    "timed out waiting for the remote snapshot to complete",
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            let _ = self.inner.exchange(Request::Ping { nonce: 0 });
        }
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
}

/// The parts of the handshake the session keeps.
struct HelloAck {
    root: String,
    git_dir: String,
}

fn handshake(conn: &mut Connection) -> Result<HelloAck> {
    let stream = conn.send(Request::Hello {
        protocol: proto::PROTOCOL_VERSION,
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        build: option_env!("STRATA_BUILD_ID").unwrap_or("dev").to_owned(),
    })?;
    loop {
        let frame = conn.receive()?;
        if frame.stream != stream {
            continue;
        }
        match frame.message {
            Message::Response(Response::HelloAck { root, git_dir, .. }) => {
                return Ok(HelloAck { root, git_dir });
            }
            Message::Response(Response::VersionMismatch { expected, got }) => {
                return Err(Error::other(format!(
                    "remote protocol version mismatch: helper speaks {expected}, app speaks {got}"
                )));
            }
            Message::Response(Response::Error(error)) => return Err(remote_error(error)),
            // Snapshots may already be on the wire; the explicit reload after
            // connect replaces them, so nothing is lost by skipping them here.
            Message::Response(_) | Message::Event(_) => continue,
            Message::Request(_) => {
                return Err(Error::other(
                    "the remote helper sent a request to the client",
                ));
            }
        }
    }
}

impl Inner {
    /// Sends a request and returns its response, applying any events that
    /// arrive meanwhile.
    fn exchange(&self, request: Request) -> Result<Response> {
        let mut conn = lock(&self.conn);
        let stream = conn.send(request)?;
        loop {
            let frame = conn.receive()?;
            match frame.message {
                Message::Response(response) if frame.stream == stream => return Ok(response),
                Message::Response(_) => continue,
                Message::Event(event) => self.apply_event(event),
                Message::Request(_) => {
                    return Err(Error::other(
                        "the remote helper sent a request to the client",
                    ));
                }
            }
        }
    }

    /// Like [`exchange`](Self::exchange), turning a protocol error response
    /// into a core error.
    fn request(&self, request: Request) -> Result<Response> {
        match self.exchange(request)? {
            Response::Error(error) => Err(remote_error(error)),
            other => Ok(other),
        }
    }

    fn apply_event(&self, event: Event) {
        match event {
            Event::SnapshotReset { snapshot } => match RepoSnapshot::try_from(snapshot) {
                Ok(snapshot) => {
                    let generation = snapshot.generation;
                    *write(&self.snapshot) = Arc::new(snapshot);
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
        }
    }

    fn request_diff_with(&self, request: DiffRequest, options: DiffOptions) -> DiffTicket {
        let ticket = DiffTicket(self.next_ticket.fetch_add(1, Ordering::SeqCst) + 1);
        let result = (|| -> Result<()> {
            let mut conn = lock(&self.conn);
            let stream = conn.send(Request::Diff {
                ticket: ticket.0,
                request: DiffRequestDto::from(&request),
                options: DiffOptionsDto::from(&options),
            })?;
            loop {
                let frame = conn.receive()?;
                match frame.message {
                    Message::Response(Response::Ok) if frame.stream == stream => continue,
                    Message::Response(Response::Error(error)) if frame.stream == stream => {
                        return Err(remote_error(error));
                    }
                    Message::Response(_) => continue,
                    Message::Event(Event::DiffReady {
                        ticket: ready,
                        view,
                    }) if ready == ticket.0 => {
                        // Emitted while the connection is locked, like every
                        // other event; callbacks must not call back into the
                        // session, or they would wait on this lock.
                        self.apply_event(Event::DiffReady {
                            ticket: ready,
                            view,
                        });
                        return Ok(());
                    }
                    Message::Event(Event::DiffFailed {
                        ticket: failed,
                        error,
                    }) if failed == ticket.0 => {
                        (self.events)(SessionEvent::DiffFailed { ticket, error });
                        return Ok(());
                    }
                    Message::Event(event) => self.apply_event(event),
                    Message::Request(_) => {
                        return Err(Error::other(
                            "the remote helper sent a request to the client",
                        ));
                    }
                }
            }
        })();
        if let Err(error) = result {
            self.emit_diff_failure(ticket, error.to_string());
        }
        ticket
    }

    fn emit_diff_failure(&self, ticket: DiffTicket, error: String) {
        (self.events)(SessionEvent::DiffFailed { ticket, error });
    }

    fn reload(&self) {
        match self.exchange(Request::Reload) {
            Ok(Response::Error(error)) => (self.events)(SessionEvent::LoadFailed {
                error: error.message,
            }),
            Ok(_) => {}
            Err(error) => (self.events)(SessionEvent::LoadFailed {
                error: error.to_string(),
            }),
        }
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

    fn run(&self, _op: Operation) -> OperationId {
        let id = OperationId(self.inner.next_operation.fetch_add(1, Ordering::SeqCst) + 1);
        self.emit(SessionEvent::OperationFinished {
            id,
            error: Some(UNSUPPORTED.to_owned()),
        });
        id
    }

    fn answer_askpass(&self, _id: OperationId, _answer: Option<String>) -> bool {
        false
    }

    fn request_diff(&self, request: DiffRequest) -> DiffTicket {
        self.request_diff_with(request, DiffOptions::default())
    }

    fn request_diff_with(&self, request: DiffRequest, options: DiffOptions) -> DiffTicket {
        self.inner.request_diff_with(request, options)
    }

    fn cancel_diff(&self, ticket: DiffTicket) {
        // Best effort: a request already in flight holds the connection, so
        // this waits for it and then cancels a ticket that has finished.
        let _ = self
            .inner
            .exchange(Request::CancelDiff { ticket: ticket.0 });
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
        self.inner.reload();
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

/// Reader calls P0a carries over the wire.
struct RemoteReader {
    inner: Weak<Inner>,
    /// Copied out of the session so `git_dir` can hand out a `&Path`.
    git_dir: PathBuf,
}

impl RemoteReader {
    fn call(&self, request: Request) -> Result<Response> {
        self.inner
            .upgrade()
            .ok_or_else(|| Error::other("the remote session was closed"))?
            .request(request)
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

    fn commits_between(&self, _base: Oid, _tip: Oid) -> Result<Vec<Oid>> {
        Err(Error::other(UNSUPPORTED))
    }

    fn is_ancestor(&self, _ancestor: Oid, _descendant: Oid) -> Result<bool> {
        Err(Error::other(UNSUPPORTED))
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
        Err(Error::other(UNSUPPORTED))
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
}

/// CLI helpers have no remote equivalent in P0a.
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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}
