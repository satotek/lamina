//! Headless helper that serves one repository over a byte stream.
//!
//! The app starts this binary over `ssh -T host -- strata-remote --stdio`, so
//! stdout carries framed protocol messages only. Every diagnostic goes to
//! stderr; mixing the two would corrupt the stream.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

use strata_core::session::{DiffTicket, RepositorySession, Session, SessionEvent};
use strata_remote_protocol::{
    self as proto, CommitDto, DiffRequestDto, DiffViewDto, Event, GraphRowDto, HeadDto, Message,
    PROTOCOL_VERSION, RemoteError, Request, Response, SnapshotDto,
};

/// Number of commits per snapshot frame. Large enough to keep round trips rare,
/// small enough that a first snapshot is not one multi-megabyte frame.
const SNAPSHOT_CHUNK: usize = 1024;

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
    out: Sender<Vec<u8>>,
    /// Maps between the client's diff tickets and the local session's tickets.
    diff_routes: Mutex<DiffRoutes>,
}

#[derive(Default)]
struct DiffRoutes {
    local_to_client: HashMap<DiffTicket, u64>,
    client_to_local: HashMap<u64, DiffTicket>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Serves `root` on the given stream until the reader reaches EOF.
///
/// Returns once the session is dropped; the writer thread flushes every queued
/// frame before joining, so a reply to the last request is not lost.
pub fn serve(
    mut reader: impl Read + Send + 'static,
    writer: impl Write + Send + 'static,
    root: impl AsRef<Path>,
) -> Result<(), BoxError> {
    let root = root.as_ref().to_path_buf();
    let (out, frames) = mpsc::channel::<Vec<u8>>();
    let writer_thread = thread::Builder::new()
        .name("strata-remote-writer".to_owned())
        .spawn(move || {
            let mut writer = writer;
            for frame in frames {
                if writer
                    .write_all(&frame)
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
        })?;

    let shared = Arc::new(ServerShared {
        session: Mutex::new(None),
        out: out.clone(),
        diff_routes: Mutex::new(DiffRoutes::default()),
    });

    let callback_shared = Arc::clone(&shared);
    let session = Session::open(&root, move |event| {
        on_session_event(&callback_shared, event);
    })?;
    // The initial load may have published before the session was stored, so
    // the first snapshot is pushed here instead of relying on the event alone.
    push_snapshot(&shared, &session.snapshot());
    *lock(&shared.session) = Some(session);

    while let Some(frame) = proto::read_frame(&mut reader)? {
        let stream = frame.stream;
        match frame.message {
            Message::Request(request) => {
                let response = handle_request(&shared, request);
                push(&shared, stream, &Message::Response(response));
            }
            Message::Response(_) | Message::Event(_) => {
                // The helper only answers; a peer sending these is confused.
                push(
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

    // EOF is the shutdown signal. Dropping the session stops its workers and
    // closes the callback's channel senders before the writer joins.
    let session = lock(&shared.session).take();
    drop(session);
    drop(shared);
    drop(out);
    let _ = writer_thread.join();
    Ok(())
}

fn handle_request(shared: &Arc<ServerShared>, request: Request) -> Response {
    let guard = lock(&shared.session);
    let Some(session) = guard.as_ref() else {
        return Response::Error(RemoteError::other("the repository session is not ready"));
    };

    match request {
        Request::Hello { protocol, .. } => {
            if protocol != PROTOCOL_VERSION {
                return Response::VersionMismatch {
                    expected: PROTOCOL_VERSION,
                    got: protocol,
                };
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
            // The session starts loading on open, so this snapshot may still
            // be incomplete; later SnapshotUpdated events push the rest.
            push_snapshot(shared, &session.snapshot());
            Response::Ok
        }
        Request::Reload => {
            session.reload();
            // Push the current snapshot synchronously so the client does not
            // have to guess when the reload's events arrive.
            push_snapshot(shared, &session.snapshot());
            Response::Ok
        }
        Request::Head => match session.reader().head() {
            Ok(head) => Response::Head {
                head: Some(HeadDto::from(&head)),
            },
            Err(error) => Response::Error(remote_error(error)),
        },
        Request::CommitChanges { commit } => {
            let result = proto::ObjectId::try_into(commit)
                .map_err(protocol_error)
                .and_then(|commit| {
                    session
                        .reader()
                        .commit_changes(commit)
                        .map_err(remote_error)
                });
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
                .and_then(|commit| {
                    session
                        .reader()
                        .tree_entries(commit, &dir)
                        .map_err(remote_error)
                });
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
                .and_then(|commit| {
                    session
                        .reader()
                        .blob_at(commit, &path)
                        .map_err(remote_error)
                });
            match result {
                Ok(data) => Response::BlobAt { data },
                Err(error) => Response::Error(error),
            }
        }
        Request::CommitMessage { commit } => {
            let result = proto::ObjectId::try_into(commit)
                .map_err(protocol_error)
                .and_then(|commit| {
                    session
                        .reader()
                        .commit_message(commit)
                        .map_err(remote_error)
                });
            match result {
                Ok(message) => Response::CommitMessage { message },
                Err(error) => Response::Error(error),
            }
        }
        Request::Parents { commit } => {
            let result = proto::ObjectId::try_into(commit)
                .map_err(protocol_error)
                .and_then(|commit| session.reader().parents(commit).map_err(remote_error));
            match result {
                Ok(parents) => Response::Parents {
                    parents: parents.into_iter().map(Into::into).collect(),
                },
                Err(error) => Response::Error(error),
            }
        }
        Request::Diff {
            ticket,
            request,
            options,
        } => {
            let request = match DiffRequestDto::try_into(request) {
                Ok(request) => request,
                Err(error) => return Response::Error(protocol_error(error)),
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
    }
}

fn on_session_event(shared: &Arc<ServerShared>, event: SessionEvent) {
    match event {
        SessionEvent::SnapshotUpdated { .. } => {
            let snapshot = lock(&shared.session)
                .as_ref()
                .map(|session| session.snapshot());
            if let Some(snapshot) = snapshot {
                push_snapshot(shared, &snapshot);
            }
        }
        SessionEvent::DiffReady { ticket, view } => {
            let client = route_ticket(shared, ticket);
            if let Some(client) = client {
                push(
                    shared,
                    0,
                    &Message::Event(Event::DiffReady {
                        ticket: client,
                        view: Box::new(DiffViewDto::from(&*view)),
                    }),
                );
            }
        }
        SessionEvent::DiffFailed { ticket, error } => {
            let client = route_ticket(shared, ticket);
            if let Some(client) = client {
                push(
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
            push(shared, 0, &Message::Event(Event::LoadFailed { error }));
        }
        // Operations, progress and queries are outside P0a; the client never
        // asks for them, so nothing is forwarded.
        _ => {}
    }
}

/// Publishes a snapshot as one reset followed by commit chunks.
fn push_snapshot(shared: &Arc<ServerShared>, snapshot: &strata_core::session::RepoSnapshot) {
    push(
        shared,
        0,
        &Message::Event(Event::SnapshotReset {
            snapshot: SnapshotDto::from(snapshot),
        }),
    );

    let generation = snapshot.generation;
    let total = snapshot.commits.len();
    // Only the local list's own flag says the walk finished; an unfinished
    // snapshot can still contain zero commits.
    let finished = snapshot.commits.complete;
    let mut start = 0;
    while start < total {
        let end = (start + SNAPSHOT_CHUNK).min(total);
        let mut commits = Vec::with_capacity(end - start);
        let mut rows = Vec::with_capacity(end - start);
        for index in start..end {
            if let Some((commit, row)) = snapshot.commits.get(index) {
                commits.push(CommitDto::from(commit));
                rows.push(GraphRowDto::from(row));
            }
        }
        start = end;
        push(
            shared,
            0,
            &Message::Event(Event::SnapshotAppend {
                generation,
                commits,
                rows,
                complete: finished && start >= total,
            }),
        );
    }

    if total == 0 {
        // An empty history still needs a terminator so the client knows the
        // snapshot is complete.
        push(
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

fn push(shared: &Arc<ServerShared>, stream: u16, message: &Message) {
    let mut frame = Vec::new();
    if let Err(error) = proto::write_frame(&mut frame, stream, message) {
        eprintln!(
            "strata-remote: failed to encode {}: {error}",
            describe(message)
        );
        return;
    }
    // A closed channel means the writer thread already exited; there is no
    // one left to tell.
    let _ = shared.out.send(frame);
}

fn describe(message: &Message) -> &'static str {
    match message {
        Message::Request(_) => "request",
        Message::Response(_) => "response",
        Message::Event(_) => "event",
    }
}

/// Resolves a local ticket and forgets the route, so a single diff produces
/// exactly one client event even though the local session emits a second,
/// highlighted view afterwards.
fn route_ticket(shared: &Arc<ServerShared>, ticket: DiffTicket) -> Option<u64> {
    let mut routes = lock(&shared.diff_routes);
    let client = routes.local_to_client.remove(&ticket)?;
    routes.client_to_local.remove(&client);
    Some(client)
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
