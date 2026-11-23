//! In-process helper: reads stay responsive while a diff is in flight, and both
//! diff views (plain, then highlighted) reach the client.

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use lamina_core::HeadInfo;
use lamina_core::session::{DiffRequest, RepositorySession, SessionEvent};
use lamina_core::test_support::TestRepo;
use lamina_remote_client::{RemoteSession, RemoteTarget};

const TIMEOUT: Duration = Duration::from_secs(30);

struct PipeRead {
    rx: Receiver<Vec<u8>>,
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
    let (client_tx, server_rx) = mpsc::channel();
    let (server_tx, client_rx) = mpsc::channel();
    (
        PipeRead {
            rx: client_rx,
            pending: Vec::new(),
            offset: 0,
        },
        PipeWrite { tx: client_tx },
        PipeRead {
            rx: server_rx,
            pending: Vec::new(),
            offset: 0,
        },
        PipeWrite { tx: server_tx },
    )
}

fn open(
    repo: &TestRepo,
) -> (
    RemoteSession,
    Receiver<SessionEvent>,
    thread::JoinHandle<()>,
) {
    let (client_read, client_write, server_read, server_write) = pipe();
    let root = repo.path().to_path_buf();
    let server = thread::spawn(move || {
        lamina_remote::serve(server_read, server_write, root).expect("serve");
    });
    let (tx, rx) = mpsc::channel();
    let session = RemoteSession::connect(
        client_read,
        client_write,
        RemoteTarget::new("in-process"),
        move |event| {
            let _ = tx.send(event);
        },
    )
    .expect("connect");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if session.snapshot().commits.complete && session.snapshot().head.is_some() {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(SessionEvent::LoadFailed { error }) => panic!("load failed: {error}"),
            Ok(_) => {}
            Err(error) => panic!("snapshot did not complete: {error}"),
        }
    }
    (session, rx, server)
}

fn head_oid(session: &RemoteSession) -> lamina_core::Oid {
    match session.snapshot().head.clone().expect("head") {
        HeadInfo::Branch {
            target: Some(target),
            ..
        } => target,
        HeadInfo::Detached { target } => target,
        HeadInfo::Branch { target: None, .. } => panic!("unborn branch"),
    }
}

#[test]
fn a_read_returns_during_a_diff_and_both_views_arrive() {
    let repo = TestRepo::new();
    repo.write("src/lib.rs", "pub fn one() {}\n");
    repo.commit("initial import");
    repo.write("src/lib.rs", "pub fn one() {}\npub fn two() {}\n");
    repo.commit("extend the library");

    let (session, events, server) = open(&repo);
    let commit = head_oid(&session);
    let ticket = session.request_diff(DiffRequest::Commit {
        commit,
        path: "src/lib.rs".into(),
    });
    // The diff request has returned. A read must not wait for either view.
    let message = session
        .reader()
        .commit_message(commit)
        .expect("commit message while the diff runs");
    assert!(message.contains("extend the library"), "{message}");

    let mut highlights = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    while highlights.len() < 2 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(SessionEvent::DiffReady {
                ticket: ready,
                view,
            }) if ready == ticket => {
                let any = view.new_highlights.iter().any(|line| !line.is_empty())
                    || view.old_highlights.iter().any(|line| !line.is_empty());
                highlights.push(any);
            }
            Ok(SessionEvent::DiffFailed {
                ticket: failed,
                error,
            }) if failed == ticket => panic!("diff failed: {error}"),
            Ok(SessionEvent::LoadFailed { error }) => panic!("load failed: {error}"),
            Ok(_) => {}
            Err(error) => panic!("both diff views did not arrive: {error}"),
        }
    }
    assert!(
        !highlights[0],
        "the first view is the plain diff, before highlighting"
    );
    assert!(
        highlights[1],
        "the second view carries the highlighted spans"
    );

    drop(session);
    server.join().expect("server thread");
}

#[test]
fn cancelling_a_diff_does_not_block_the_next_one() {
    let repo = TestRepo::new();
    repo.write("README.md", "alpha\n");
    repo.commit("initial import");

    let (session, events, server) = open(&repo);
    let commit = head_oid(&session);
    let cancelled = session.request_diff(DiffRequest::Commit {
        commit,
        path: "README.md".into(),
    });
    session.cancel_diff(cancelled);
    let ticket = session.request_diff(DiffRequest::Commit {
        commit,
        path: "README.md".into(),
    });

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(SessionEvent::DiffReady { ticket: ready, .. }) if ready == ticket => break,
            Ok(SessionEvent::DiffFailed {
                ticket: failed,
                error,
            }) if failed == ticket => panic!("replacement diff failed: {error}"),
            Ok(SessionEvent::LoadFailed { error }) => panic!("load failed: {error}"),
            Ok(_) => {}
            Err(error) => panic!("replacement diff did not arrive: {error}"),
        }
    }
    drop(session);
    server.join().expect("server thread");
}

/// A diff that reads the working tree must not read past the checkout: the
/// diff text would carry the outside file's contents back over the wire.
#[test]
fn worktree_diff_paths_that_escape_are_refused() {
    let repo = TestRepo::new();
    repo.write("README.md", "alpha\n");
    repo.commit("initial import");
    // A file just outside the checkout; a helper that diffed it would send
    // this text back as the added side of the diff.
    let outside = repo
        .path()
        .parent()
        .expect("tempdir parent")
        .join("lamina-diff-outside.txt");
    std::fs::write(&outside, b"file contents outside the checkout\n").expect("outside file");

    let (session, events, server) = open(&repo);
    let commit = head_oid(&session);
    let outside_name = outside
        .file_name()
        .expect("file name")
        .to_string_lossy()
        .into_owned();
    let cases = [
        (
            DiffRequest::Unstaged {
                path: format!("../{outside_name}"),
            },
            "worktree",
        ),
        (
            DiffRequest::CompareWorktree {
                base: commit,
                path: format!("../{outside_name}"),
            },
            "worktree",
        ),
        (
            DiffRequest::Unstaged {
                path: ".git/config".into(),
            },
            ".git",
        ),
        (
            DiffRequest::CompareWorktree {
                base: commit,
                path: if cfg!(windows) {
                    r"C:\Windows\win.ini".into()
                } else {
                    "/etc/hostname".into()
                },
            },
            "worktree",
        ),
    ];
    for (request, needle) in cases {
        let ticket = session.request_diff(request);
        let deadline = Instant::now() + TIMEOUT;
        let error = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match events.recv_timeout(remaining) {
                Ok(SessionEvent::DiffFailed {
                    ticket: failed,
                    error,
                }) if failed == ticket => break error,
                Ok(SessionEvent::DiffReady { ticket: ready, .. }) if ready == ticket => {
                    panic!("a diff outside the checkout was answered")
                }
                Ok(SessionEvent::LoadFailed { error }) => panic!("load failed: {error}"),
                Ok(_) => {}
                Err(error) => panic!("no answer for an escaping diff path: {error}"),
            }
        };
        assert!(
            error.contains(needle),
            "expected {needle:?} in the rejection: {error}"
        );
    }

    // A repository-relative path still diffs, so the check refuses only
    // what leaves the worktree.
    let ticket = session.request_diff(DiffRequest::Unstaged {
        path: "README.md".into(),
    });
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(SessionEvent::DiffReady { ticket: ready, .. }) if ready == ticket => break,
            Ok(SessionEvent::DiffFailed {
                ticket: failed,
                error,
            }) if failed == ticket => panic!("a clean path was refused: {error}"),
            Ok(SessionEvent::LoadFailed { error }) => panic!("load failed: {error}"),
            Ok(_) => {}
            Err(error) => panic!("a clean unstaged diff did not arrive: {error}"),
        }
    }
    drop(session);
    server.join().expect("server thread");
}
