//! The helper is not involved. A thread speaks the protocol on the other end
//! of an in-memory pipe, so these tests cover the client without `ssh`.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use lamina_core::Oid;
use lamina_core::session::{DiffRequest, DiffTicket, QueryTicket, RepositorySession, SessionEvent};
use lamina_remote_protocol::{
    self as proto, CommitDto, CommitStatDto, DiffRequestDto, DiffViewDto, Event, FileDiffDto,
    GraphRowDto, HeadDto, HighlightKindDto, HighlightSpanDto, Message, ObjectId, QueryEvent,
    RepoStateDto, Request, Response, SnapshotDto, SpanDto, WorktreeStatusDto,
};

use crate::session::TransportAttach;
use crate::{CONNECTION_LOST, ClientConfig, RemoteSession, RemoteTarget};

struct PipeRead {
    rx: mpsc::Receiver<Vec<u8>>,
    pending: Vec<u8>,
    offset: usize,
}

impl Read for PipeRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.offset == self.pending.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.pending = chunk;
                    self.offset = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let available = &self.pending[self.offset..];
        let take = available.len().min(buf.len());
        buf[..take].copy_from_slice(&available[..take]);
        self.offset += take;
        Ok(take)
    }
}

struct PipeWrite {
    tx: mpsc::Sender<Vec<u8>>,
}

impl Write for PipeWrite {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.tx.send(buf.to_vec());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn pipe() -> (PipeRead, PipeWrite, PipeRead, PipeWrite) {
    let (client_tx, peer_rx) = mpsc::channel();
    let (peer_tx, client_rx) = mpsc::channel();
    (
        PipeRead {
            rx: client_rx,
            pending: Vec::new(),
            offset: 0,
        },
        PipeWrite { tx: client_tx },
        PipeRead {
            rx: peer_rx,
            pending: Vec::new(),
            offset: 0,
        },
        PipeWrite { tx: peer_tx },
    )
}

fn write_msg(writer: &mut impl Write, stream: u16, message: &Message) {
    proto::write_frame(writer, stream, message).expect("write frame");
}

fn read_msg(reader: &mut impl Read) -> Option<(u16, Message)> {
    proto::read_frame(reader)
        .expect("read frame")
        .map(|frame| (frame.stream, frame.message))
}

/// Pings are answered so a slow test is not declared dead. Every other
/// request is returned to the caller.
fn next_request(reader: &mut impl Read, writer: &mut impl Write) -> Option<(u16, Request)> {
    loop {
        let (stream, message) = read_msg(reader)?;
        match message {
            Message::Request(Request::Ping { nonce }) => {
                write_msg(writer, stream, &Message::Response(Response::Pong { nonce }));
            }
            Message::Request(request) => return Some((stream, request)),
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}

fn oid_pair(byte: u8) -> (Oid, ObjectId) {
    let hex = format!("{byte:02x}").repeat(20);
    let oid = Oid::from_hex(hex.as_bytes()).expect("oid");
    (oid, ObjectId::from_bytes([byte; 20]))
}

fn snapshot_dto() -> SnapshotDto {
    SnapshotDto {
        generation: 1,
        head: None,
        refs: Vec::new(),
        upstreams: Vec::new(),
        stashes: Vec::new(),
        remotes: Vec::new(),
        status: WorktreeStatusDto::default(),
        state: RepoStateDto::Normal,
        conflicts: Vec::new(),
        busy: None,
    }
}

fn commit_dto(id: ObjectId) -> CommitDto {
    CommitDto {
        id,
        parents: Vec::new(),
        summary: "imported".into(),
        author_name: "dev".into(),
        author_email: "dev@example".into(),
        author_time: 0,
        committer_time: 0,
        shallow_boundary: false,
    }
}

fn row() -> GraphRowDto {
    GraphRowDto {
        node_lane: 0,
        node_color: 0,
        width: 1,
        passes: Vec::new(),
        edges: Vec::new(),
    }
}

fn send_chunk(writer: &mut impl Write, id: ObjectId, complete: bool) {
    write_msg(
        writer,
        0,
        &Message::Event(Event::SnapshotReset {
            snapshot: snapshot_dto(),
        }),
    );
    write_msg(
        writer,
        0,
        &Message::Event(Event::SnapshotAppend {
            generation: 1,
            commits: vec![commit_dto(id)],
            rows: vec![row()],
            complete,
        }),
    );
}

fn hello(reader: &mut impl Read, writer: &mut impl Write) {
    let (stream, request) = next_request(reader, writer).expect("hello");
    assert!(matches!(request, Request::Hello { .. }), "{request:?}");
    write_msg(
        writer,
        stream,
        &Message::Response(Response::HelloAck {
            root: "/repo".into(),
            git_dir: "/repo/.git".into(),
            bare: false,
            watching: true,
        }),
    );
}

fn accept_subscribe(reader: &mut impl Read, writer: &mut impl Write) {
    let (stream, request) = next_request(reader, writer).expect("subscribe");
    assert!(
        matches!(request, Request::SubscribeSnapshots),
        "{request:?}"
    );
    write_msg(writer, stream, &Message::Response(Response::Ok));
}

fn diff_view(highlighted: bool) -> DiffViewDto {
    let (_, commit) = oid_pair(7);
    DiffViewDto {
        request: DiffRequestDto::Commit {
            commit,
            path: "src/lib.rs".into(),
        },
        diff: FileDiffDto::default(),
        old_highlights: Vec::new(),
        new_highlights: if highlighted {
            vec![vec![HighlightSpanDto {
                range: SpanDto { start: 0, end: 2 },
                kind: HighlightKindDto::Keyword,
            }]]
        } else {
            Vec::new()
        },
        lfs: None,
        old_text: String::new(),
        new_text: "fn".into(),
        old_encoding: "UTF-8".into(),
        new_encoding: "UTF-8".into(),
        gaps: Vec::new(),
        image: None,
    }
}

#[test]
fn responses_are_matched_when_they_arrive_out_of_order() {
    let (client_read, client_write, mut peer_read, mut peer_write) = pipe();
    let peer = thread::spawn(move || {
        hello(&mut peer_read, &mut peer_write);
        accept_subscribe(&mut peer_read, &mut peer_write);
        let (_, id) = oid_pair(1);
        send_chunk(&mut peer_write, id, true);

        // Both reads have to be in flight before either answer, which the old
        // single-mutex client could not do.
        let mut pending = Vec::new();
        while pending.len() < 2 {
            pending.push(next_request(&mut peer_read, &mut peer_write).expect("two reads"));
        }
        for (stream, request) in pending.into_iter().rev() {
            let response = match request {
                Request::Head => Response::Head {
                    head: Some(HeadDto::Branch {
                        short_name: "main".into(),
                        target: Some(id),
                    }),
                },
                Request::CommitMessage { .. } => Response::CommitMessage {
                    message: "hello from the side\n".into(),
                },
                other => panic!("unexpected request: {other:?}"),
            };
            write_msg(&mut peer_write, stream, &Message::Response(response));
        }
    });

    let session = RemoteSession::connect(
        client_read,
        client_write,
        RemoteTarget::new("scripted"),
        |_| {},
    )
    .expect("connect");

    let (oid, _) = oid_pair(1);
    let reader = session.reader();
    let reader_message = Arc::clone(&reader);
    let message = thread::spawn(move || reader_message.commit_message(oid));
    let head = thread::spawn(move || reader.head());

    let head = head.join().expect("head thread").expect("head");
    let message = message.join().expect("message thread").expect("message");
    match head {
        lamina_core::HeadInfo::Branch { short_name, target } => {
            assert_eq!(short_name, "main");
            assert_eq!(target, Some(oid));
        }
        other => panic!("head: {other:?}"),
    }
    assert_eq!(message, "hello from the side\n");
    peer.join().expect("peer");
}

#[test]
fn a_read_completes_while_the_snapshot_is_unfinished() {
    let (client_read, client_write, mut peer_read, mut peer_write) = pipe();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let peer = thread::spawn(move || {
        hello(&mut peer_read, &mut peer_write);
        accept_subscribe(&mut peer_read, &mut peer_write);
        let (_, id) = oid_pair(3);
        send_chunk(&mut peer_write, id, false);

        let (stream, request) = next_request(&mut peer_read, &mut peer_write).expect("read");
        assert!(
            matches!(request, Request::CommitMessage { .. }),
            "{request:?}"
        );
        write_msg(
            &mut peer_write,
            stream,
            &Message::Response(Response::CommitMessage {
                message: "while history streams\n".into(),
            }),
        );
        // Hold the rest of the history until the client has observed it.
        let _ = release_rx.recv();
        write_msg(
            &mut peer_write,
            0,
            &Message::Event(Event::SnapshotAppend {
                generation: 1,
                commits: vec![commit_dto(ObjectId::from_bytes([4; 20]))],
                rows: vec![row()],
                complete: true,
            }),
        );
    });

    let session = RemoteSession::connect(
        client_read,
        client_write,
        RemoteTarget::new("scripted"),
        |_| {},
    )
    .expect("connect returns on the first chunk");
    assert!(!session.snapshot().commits.complete);
    assert!(!session.snapshot().commits.is_empty());

    let (oid, _) = oid_pair(3);
    let message = session
        .reader()
        .commit_message(oid)
        .expect("read during the snapshot");
    assert_eq!(message, "while history streams\n");
    assert!(
        !session.snapshot().commits.complete,
        "the read returned before the snapshot finished"
    );
    release_tx
        .send(())
        .expect("release the rest of the snapshot");
    peer.join().expect("peer");
}

#[test]
fn cancel_is_written_while_a_read_is_unanswered() {
    let (client_read, client_write, mut peer_read, mut peer_write) = pipe();
    let (saw_read_tx, saw_read_rx) = mpsc::channel::<()>();
    let peer = thread::spawn(move || {
        hello(&mut peer_read, &mut peer_write);
        accept_subscribe(&mut peer_read, &mut peer_write);
        let (_, id) = oid_pair(5);
        send_chunk(&mut peer_write, id, true);

        let (blob_stream, request) = next_request(&mut peer_read, &mut peer_write).expect("blob");
        assert!(matches!(request, Request::BlobAt { .. }), "{request:?}");
        saw_read_tx.send(()).expect("saw the read");

        let started = Instant::now();
        let (cancel_stream, request) =
            next_request(&mut peer_read, &mut peer_write).expect("cancel");
        assert!(matches!(request, Request::CancelDiff { .. }), "{request:?}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cancel waited behind the unanswered read"
        );
        write_msg(
            &mut peer_write,
            blob_stream,
            &Message::Response(Response::BlobAt {
                data: b"file".to_vec(),
            }),
        );
        write_msg(
            &mut peer_write,
            cancel_stream,
            &Message::Response(Response::Ok),
        );
    });

    let session = RemoteSession::connect(
        client_read,
        client_write,
        RemoteTarget::new("scripted"),
        |_| {},
    )
    .expect("connect");
    let (oid, _) = oid_pair(5);
    let reader = session.reader();
    let blob = thread::spawn(move || reader.blob_at(oid, "README.md"));
    saw_read_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("blob request");
    session.cancel_diff(DiffTicket(9));
    let data = blob.join().expect("blob thread").expect("blob");
    assert_eq!(data, b"file");
    peer.join().expect("peer");
}

#[test]
fn both_diff_views_are_delivered() {
    let (client_read, client_write, mut peer_read, mut peer_write) = pipe();
    let peer = thread::spawn(move || {
        hello(&mut peer_read, &mut peer_write);
        accept_subscribe(&mut peer_read, &mut peer_write);
        let (_, id) = oid_pair(7);
        send_chunk(&mut peer_write, id, true);

        let (stream, request) = next_request(&mut peer_read, &mut peer_write).expect("diff");
        let Request::Diff { ticket, .. } = request else {
            panic!("expected a diff, got {request:?}");
        };
        write_msg(&mut peer_write, stream, &Message::Response(Response::Ok));
        for highlighted in [false, true] {
            write_msg(
                &mut peer_write,
                0,
                &Message::Event(Event::DiffReady {
                    ticket,
                    view: Box::new(diff_view(highlighted)),
                }),
            );
        }
    });

    let (tx, rx) = mpsc::channel();
    let session = RemoteSession::connect(
        client_read,
        client_write,
        RemoteTarget::new("scripted"),
        move |event| {
            let _ = tx.send(event);
        },
    )
    .expect("connect");
    let (oid, _) = oid_pair(7);
    let ticket = session.request_diff(DiffRequest::Commit {
        commit: oid,
        path: "src/lib.rs".into(),
    });

    let mut highlighted = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while highlighted.len() < 2 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(SessionEvent::DiffReady {
                ticket: ready,
                view,
            }) if ready == ticket => {
                let any = view.new_highlights.iter().any(|line| !line.is_empty());
                highlighted.push(any);
            }
            Ok(SessionEvent::DiffFailed { error, .. }) => panic!("diff failed: {error}"),
            Ok(_) => {}
            Err(error) => panic!("diff views did not both arrive: {error}"),
        }
    }
    assert_eq!(highlighted, vec![false, true]);
    peer.join().expect("peer");
}

#[test]
fn missed_pings_report_a_lost_connection_and_keep_the_snapshot() {
    let (client_read, client_write, mut peer_read, mut peer_write) = pipe();
    // The write end has to stay open. Dropping it is an EOF, and the client
    // would report the loss before the ping interval had a chance to fire.
    let (hold_tx, hold_rx) = mpsc::channel::<()>();
    let peer = thread::spawn(move || {
        hello(&mut peer_read, &mut peer_write);
        accept_subscribe(&mut peer_read, &mut peer_write);
        let (_, id) = oid_pair(9);
        send_chunk(&mut peer_write, id, true);
        let _ = hold_rx.recv();
    });

    let (tx, rx) = mpsc::channel();
    let session = RemoteSession::connect_with(
        client_read,
        client_write,
        RemoteTarget::new("scripted"),
        move |event| {
            let _ = tx.send(event);
        },
        ClientConfig {
            ping_interval: Duration::from_millis(40),
            missed_pings: 3,
            first_chunk_timeout: Duration::from_secs(5),
        },
    )
    .expect("connect");
    let snapshot = session.snapshot();
    assert!(snapshot.commits.complete);
    assert_eq!(snapshot.commits.len(), 1);

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut lost = None;
    while lost.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(SessionEvent::LoadFailed { error }) => lost = Some(error),
            Ok(_) => {}
            Err(error) => panic!("connection was not reported lost: {error}"),
        }
    }
    let lost = lost.expect("load failed");
    assert!(
        lost.contains(CONNECTION_LOST),
        "the app should be able to match this text: {lost}"
    );
    assert!(
        session.snapshot().commits.complete,
        "the last snapshot stays readable"
    );
    assert!(
        session.connection_rtt().is_none(),
        "a lost connection keeps no round trip"
    );

    let started = Instant::now();
    let error = session.reader().head().expect_err("request after loss");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "a later request waited instead of failing"
    );
    assert!(error.to_string().contains(CONNECTION_LOST), "{error}");
    drop(hold_tx);
    peer.join().expect("peer");
}

#[test]
fn an_answered_ping_records_the_round_trip_until_the_link_drops() {
    let (client_read, client_write, mut peer_read, mut peer_write) = pipe();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_peer = Arc::clone(&stop);
    let peer = thread::spawn(move || {
        hello(&mut peer_read, &mut peer_write);
        accept_subscribe(&mut peer_read, &mut peer_write);
        let (_, id) = oid_pair(8);
        send_chunk(&mut peer_write, id, true);
        while !stop_peer.load(Ordering::SeqCst) {
            let Some((stream, message)) = read_msg(&mut peer_read) else {
                return;
            };
            if let Message::Request(Request::Ping { nonce }) = message {
                write_msg(
                    &mut peer_write,
                    stream,
                    &Message::Response(Response::Pong { nonce }),
                );
            }
        }
    });

    let (tx, rx) = mpsc::channel();
    let session = RemoteSession::connect_with(
        client_read,
        client_write,
        RemoteTarget::new("scripted"),
        move |event| {
            let _ = tx.send(event);
        },
        ClientConfig {
            ping_interval: Duration::from_millis(40),
            missed_pings: 3,
            first_chunk_timeout: Duration::from_secs(5),
        },
    )
    .expect("connect");

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut sample = None;
    while sample.is_none() {
        if let Some(rtt) = session.connection_rtt() {
            sample = Some(rtt);
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let sample = sample.expect("an answered ping should record a round trip");
    assert!(
        sample < Duration::from_secs(2),
        "round trip was not an idle probe: {sample:?}"
    );

    stop.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut lost = false;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(SessionEvent::LoadFailed { error }) => {
                assert!(error.contains(CONNECTION_LOST), "{error}");
                lost = true;
                break;
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => panic!("connection was not reported lost: {error}"),
        }
    }
    assert!(lost, "closing the helper should report the connection lost");
    assert!(
        session.connection_rtt().is_none(),
        "the sample from before the drop should be gone"
    );
    peer.join().expect("peer");
}

#[test]
fn a_cancelled_streaming_query_drops_a_late_result() {
    let (client_read, client_write, mut peer_read, mut peer_write) = pipe();
    let peer = thread::spawn(move || {
        hello(&mut peer_read, &mut peer_write);
        accept_subscribe(&mut peer_read, &mut peer_write);
        let (_, id) = oid_pair(4);
        send_chunk(&mut peer_write, id, true);

        let (stream, request) = next_request(&mut peer_read, &mut peer_write).expect("query");
        assert!(
            matches!(request, Request::Query { .. }),
            "expected a query, got {request:?}"
        );
        let Request::Query { ticket, .. } = request else {
            unreachable!("matched above");
        };
        write_msg(&mut peer_write, stream, &Message::Response(Response::Ok));

        let (cancel_stream, request) =
            next_request(&mut peer_read, &mut peer_write).expect("cancel");
        assert!(
            matches!(request, Request::CancelQuery { ticket: cancelled } if cancelled == ticket),
            "expected a cancel for {ticket}, got {request:?}"
        );
        write_msg(
            &mut peer_write,
            cancel_stream,
            &Message::Response(Response::Ok),
        );
        // Already on the wire when the client gave up. It must not surface.
        write_msg(
            &mut peer_write,
            0,
            &Message::Event(Event::Query {
                ticket,
                event: QueryEvent::PathHistoryDone,
            }),
        );

        let (stat_stream, request) = next_request(&mut peer_read, &mut peer_write).expect("stat");
        let Request::Query {
            ticket: stat_ticket,
            ..
        } = request
        else {
            panic!("expected a follow-up query, got {request:?}");
        };
        write_msg(
            &mut peer_write,
            stat_stream,
            &Message::Response(Response::Ok),
        );
        write_msg(
            &mut peer_write,
            0,
            &Message::Event(Event::Query {
                ticket: stat_ticket,
                event: QueryEvent::CommitStat(CommitStatDto {
                    files: Vec::new(),
                    added: 0,
                    removed: 0,
                }),
            }),
        );
    });

    let (tx, rx) = mpsc::channel();
    let session = RemoteSession::connect(
        client_read,
        client_write,
        RemoteTarget::new("scripted"),
        move |event| {
            let _ = tx.send(event);
        },
    )
    .expect("connect");
    let cancelled = session.path_history_streaming("README.md".into());
    session.cancel_query(cancelled);
    let (oid, _) = oid_pair(4);
    let stat = session.request_commit_stat(oid);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(SessionEvent::CommitStatReady { ticket, .. }) if ticket == stat => break,
            Ok(event) if query_ticket(&event) == Some(cancelled) => {
                panic!("cancelled query delivered {event:?}");
            }
            Ok(_) => {}
            Err(error) => panic!("follow-up stat did not arrive: {error}"),
        }
    }
    peer.join().expect("peer");
}

#[test]
fn a_dropped_helper_expires_an_outstanding_query() {
    let (client_read, client_write, mut peer_read, mut peer_write) = pipe();
    let peer = thread::spawn(move || {
        hello(&mut peer_read, &mut peer_write);
        accept_subscribe(&mut peer_read, &mut peer_write);
        let (_, id) = oid_pair(6);
        send_chunk(&mut peer_write, id, true);

        let (stream, request) = next_request(&mut peer_read, &mut peer_write).expect("query");
        assert!(matches!(request, Request::Query { .. }), "{request:?}");
        write_msg(&mut peer_write, stream, &Message::Response(Response::Ok));
        // Dropping the write end is the helper going away with the ticket open.
    });

    let (tx, rx) = mpsc::channel();
    let session = RemoteSession::connect(
        client_read,
        client_write,
        RemoteTarget::new("scripted"),
        move |event| {
            let _ = tx.send(event);
        },
    )
    .expect("connect");
    let ticket = session.path_history_streaming("README.md".into());

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(SessionEvent::QueryFailed {
                ticket: failed,
                error,
            }) if failed == ticket => {
                assert!(
                    error.contains(CONNECTION_LOST),
                    "outstanding ticket should expire with the connection: {error}"
                );
                break;
            }
            Ok(_) => {}
            Err(error) => panic!("query was not expired: {error}"),
        }
    }
    peer.join().expect("peer");
}

#[test]
fn a_meta_frame_between_chunks_does_not_swallow_the_rest_of_the_history() {
    let (client_read, client_write, mut peer_read, mut peer_write) = pipe();
    let (hold_tx, hold_rx) = mpsc::channel::<()>();
    let peer = thread::spawn(move || {
        hello(&mut peer_read, &mut peer_write);
        accept_subscribe(&mut peer_read, &mut peer_write);
        let (_, first) = oid_pair(10);
        let (_, second) = oid_pair(11);
        // The helper streams generation 1. A status-only change then queues
        // a small meta frame, which the helper sends ahead of the snapshot
        // chunks still waiting in its low-priority queue.
        write_msg(
            &mut peer_write,
            0,
            &Message::Event(Event::SnapshotReset {
                snapshot: snapshot_dto(),
            }),
        );
        write_msg(
            &mut peer_write,
            0,
            &Message::Event(Event::SnapshotAppend {
                generation: 1,
                commits: vec![commit_dto(first)],
                rows: vec![row()],
                complete: false,
            }),
        );
        let mut bumped = snapshot_dto();
        bumped.generation = 2;
        write_msg(
            &mut peer_write,
            0,
            &Message::Event(Event::SnapshotMeta { snapshot: bumped }),
        );
        write_msg(
            &mut peer_write,
            0,
            &Message::Event(Event::SnapshotAppend {
                generation: 1,
                commits: vec![commit_dto(second)],
                rows: vec![row()],
                complete: true,
            }),
        );
        // Dropping the write end is an EOF, which would race the client's
        // own first-chunk wait.
        let _ = hold_rx.recv();
    });

    let session = RemoteSession::connect(
        client_read,
        client_write,
        RemoteTarget::new("scripted"),
        |_| {},
    )
    .expect("connect");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = session.snapshot();
        if snapshot.commits.complete {
            assert_eq!(
                snapshot.commits.len(),
                2,
                "the chunk behind the meta frame was dropped"
            );
            assert_eq!(
                snapshot.generation, 2,
                "the meta frame still bumps the snapshot's own generation"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the history never completed behind the meta frame"
        );
        thread::sleep(Duration::from_millis(10));
    }
    drop(hold_tx);
    peer.join().expect("peer");
}

fn query_ticket(event: &SessionEvent) -> Option<QueryTicket> {
    Some(match event {
        SessionEvent::QueryReady { ticket, .. }
        | SessionEvent::CommitStatReady { ticket, .. }
        | SessionEvent::CompareReady { ticket, .. }
        | SessionEvent::BlameProgress { ticket, .. }
        | SessionEvent::BlameReady { ticket, .. }
        | SessionEvent::QueryFailed { ticket, .. }
        | SessionEvent::PathHistoryBatch { ticket, .. }
        | SessionEvent::PathHistoryDone { ticket }
        | SessionEvent::ContentSearchBatch { ticket, .. }
        | SessionEvent::ContentSearchProgress { ticket, .. }
        | SessionEvent::ContentSearchDone { ticket, .. }
        | SessionEvent::SubmodulesAndWorktreesReady { ticket, .. }
        | SessionEvent::ReflogReady { ticket, .. }
        | SessionEvent::InProgressDetailsReady { ticket, .. }
        | SessionEvent::CommitSignaturesDetected { ticket, .. }
        | SessionEvent::CommitSignaturesReady { ticket, .. } => *ticket,
        _ => return None,
    })
}

/// A write end that can be told to park inside `write`, the way a real pipe
/// parks the caller when the peer stops draining it.
struct StalledWrite {
    inner: PipeWrite,
    stall: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    /// How many writes are (or were) parked; the test waits for the first.
    entered: Arc<AtomicUsize>,
}

impl Write for StalledWrite {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.stall.load(Ordering::SeqCst) {
            self.entered.fetch_add(1, Ordering::SeqCst);
            while !self.release.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
            }
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[test]
fn dropping_a_session_with_a_stalled_write_does_not_hang() {
    let (client_read, client_write, mut peer_read, mut peer_write) = pipe();
    let (peer_exit_tx, peer_exit_rx) = mpsc::channel::<()>();
    let peer = thread::spawn(move || {
        hello(&mut peer_read, &mut peer_write);
        accept_subscribe(&mut peer_read, &mut peer_write);
        let (_, id) = oid_pair(11);
        send_chunk(&mut peer_write, id, true);
        // Stay connected until the transport is killed; closing early would
        // end the client's read and hide the stalled write.
        let _ = peer_exit_rx.recv();
    });

    let stall = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicUsize::new(0));
    let writer = StalledWrite {
        inner: client_write,
        stall: Arc::clone(&stall),
        release: Arc::clone(&release),
        entered: Arc::clone(&entered),
    };
    let hook_release = Arc::clone(&release);
    let hook_exit = peer_exit_tx.clone();
    let session = RemoteSession::connect_attached(
        client_read,
        writer,
        RemoteTarget::new("scripted"),
        |_| {},
        ClientConfig {
            ping_interval: Duration::from_millis(40),
            missed_pings: 3,
            first_chunk_timeout: Duration::from_secs(5),
        },
        TransportAttach {
            diagnostic: Arc::new(String::new),
            // Killing ssh is what ends a write parked on a full pipe; the
            // fake only has to unblock the parked `write`.
            disconnect: Some(Box::new(move || {
                hook_release.store(true, Ordering::SeqCst);
                let _ = hook_exit.send(());
            })),
            retain: None,
        },
    )
    .expect("connect");

    // The next ping parks inside `write`, holding the writer mutex.
    stall.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(5);
    while entered.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < deadline,
            "no write reached the stall; the ping never fired"
        );
        thread::sleep(Duration::from_millis(5));
    }

    let (drop_tx, drop_rx) = mpsc::channel::<()>();
    let dropper = thread::spawn(move || {
        drop(session);
        let _ = drop_tx.send(());
    });
    // Shutdown used to join the ping thread before killing the transport, so
    // the parked write never returned and the drop never finished.
    drop_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("dropping the session must not wait for a stalled write");
    dropper.join().expect("drop thread");
    drop(peer_exit_tx);
    peer.join().expect("peer");
}
