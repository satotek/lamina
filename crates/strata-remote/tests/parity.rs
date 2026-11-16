//! Parity tests: a real `strata-remote --stdio` helper and an in-process
//! helper must produce the same snapshots, reads and commit diff as a local
//! `Session` opened on the same repository.
//!
//! No SSH is involved. The pipe test exercises argument parsing, the spawned
//! binary and stdin EOF; the duplex test exercises the server loop directly
//! and runs on every platform.

use std::io::{Read, Write};
use std::process::Command;
#[cfg(unix)]
use std::process::Stdio;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use strata_core::diff::DiffOptions;
use strata_core::session::{
    DiffRequest, DiffTicket, DiffView, RepoSnapshot, RepositorySession, Session, SessionEvent,
};
use strata_core::test_support::TestRepo;
use strata_remote_client::{RemoteSession, RemoteTarget};

const TIMEOUT: Duration = Duration::from_secs(30);

/// Three commits, a branch, a tag and a stash.
fn fixture() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("README.md", "alpha\nbeta\n");
    repo.write("src/lib.rs", "pub fn one() {}\n");
    repo.commit("initial import");

    repo.write("README.md", "alpha\nbeta\ngamma\n");
    repo.commit("extend the readme");

    repo.git(&["branch", "feature"]);
    repo.git(&["tag", "v1"]);

    repo.write("src/lib.rs", "pub fn one() {}\npub fn two() {}\n");
    repo.git(&["stash", "push", "-m", "work in progress"]);
    repo
}

fn wait_for_local(session: &Session, events: &Receiver<SessionEvent>) -> Arc<RepoSnapshot> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let snapshot = session.snapshot();
        if snapshot.commits.complete && snapshot.head.is_some() {
            return snapshot;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(SessionEvent::LoadFailed { error }) => panic!("local load failed: {error}"),
            Ok(_) => {}
            Err(error) => panic!("local snapshot did not complete: {error}"),
        }
    }
}

fn wait_for_diff(events: &Receiver<SessionEvent>, ticket: DiffTicket) -> DiffView {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(SessionEvent::DiffReady {
                ticket: ready,
                view,
            }) if ready == ticket => return (*view).clone(),
            Ok(SessionEvent::DiffFailed {
                ticket: failed,
                error,
            }) if failed == ticket => {
                panic!("remote diff failed: {error}");
            }
            Ok(SessionEvent::LoadFailed { error }) => panic!("load failed: {error}"),
            Ok(_) => {}
            Err(error) => panic!("diff did not arrive: {error}"),
        }
    }
}

fn open_local(repo: &TestRepo) -> (Session, Receiver<SessionEvent>) {
    let (tx, rx) = mpsc::channel();
    let session = Session::open(repo.path(), move |event| {
        let _ = tx.send(event);
    })
    .expect("open local session");
    (session, rx)
}

fn assert_snapshots_match(local: &RepoSnapshot, remote: &RepoSnapshot) {
    assert_eq!(local.head, remote.head, "HEAD");
    assert_eq!(*local.refs, *remote.refs, "refs");
    assert_eq!(*local.stashes, *remote.stashes, "stashes");
    assert_eq!(*local.status, *remote.status, "status");
    assert_eq!(*local.upstreams, *remote.upstreams, "upstreams");
    assert_eq!(local.remotes, remote.remotes, "remotes");
    assert_eq!(local.state, remote.state, "state");
    assert_eq!(*local.conflicts, *remote.conflicts, "conflicts");
    assert_eq!(local.busy, remote.busy, "busy");
    assert_eq!(
        local.commits.len(),
        remote.commits.len(),
        "commit count from a complete walk"
    );
    assert!(remote.commits.complete, "remote snapshot is complete");

    for index in 0..local.commits.len() {
        let (local_commit, local_row) = local.commits.get(index).expect("local row");
        let (remote_commit, remote_row) = remote.commits.get(index).expect("remote row");
        assert_eq!(local_commit, remote_commit, "commit {index}");
        assert_eq!(local_row, remote_row, "graph row {index}");
    }
    assert_eq!(*local.commit_index, *remote.commit_index, "commit index");
}

fn assert_readers_match(local: &Session, remote: &RemoteSession) {
    let local_reader = local.reader();
    let remote_reader = remote.reader();

    let head = local_reader.head().expect("local head");
    assert_eq!(head, remote_reader.head().expect("remote head"), "head");

    let commit = match &head {
        strata_core::HeadInfo::Branch {
            target: Some(target),
            ..
        } => *target,
        strata_core::HeadInfo::Detached { target } => *target,
        strata_core::HeadInfo::Branch { target: None, .. } => panic!("unborn branch"),
    };

    assert_eq!(
        local_reader.commit_changes(commit).expect("local changes"),
        remote_reader
            .commit_changes(commit)
            .expect("remote changes"),
        "commit changes"
    );
    assert_eq!(
        local_reader.tree_entries(commit, "").expect("local tree"),
        remote_reader.tree_entries(commit, "").expect("remote tree"),
        "root tree entries"
    );
    assert_eq!(
        local_reader
            .tree_entries(commit, "src")
            .expect("local subtree"),
        remote_reader
            .tree_entries(commit, "src")
            .expect("remote subtree"),
        "subtree entries"
    );
    assert_eq!(
        local_reader
            .blob_at(commit, "README.md")
            .expect("local blob"),
        remote_reader
            .blob_at(commit, "README.md")
            .expect("remote blob"),
        "blob contents"
    );
    assert_eq!(
        local_reader.commit_message(commit).expect("local message"),
        remote_reader
            .commit_message(commit)
            .expect("remote message"),
        "commit message"
    );
    assert_eq!(
        local_reader.parents(commit).expect("local parents"),
        remote_reader.parents(commit).expect("remote parents"),
        "parents"
    );

    let root = local_reader.parents(commit).expect("local parents")[0];
    assert_eq!(
        local_reader.parents(root).expect("local root parents"),
        remote_reader.parents(root).expect("remote root parents"),
        "root commit has no parents"
    );
}

fn assert_diffs_match(
    local: &Session,
    local_events: &Receiver<SessionEvent>,
    remote: &RemoteSession,
    remote_events: &Receiver<SessionEvent>,
    request: DiffRequest,
) {
    let local_ticket = local.request_diff_with(request.clone(), DiffOptions::default());
    let remote_ticket = remote.request_diff_with(request, DiffOptions::default());

    let local_view = wait_for_diff(local_events, local_ticket);
    let remote_view = wait_for_diff(remote_events, remote_ticket);

    assert_eq!(local_view.request, remote_view.request, "diff request");
    assert_eq!(local_view.diff, remote_view.diff, "diff hunks");
    assert_eq!(local_view.old_text, remote_view.old_text, "old text");
    assert_eq!(local_view.new_text, remote_view.new_text, "new text");
    assert_eq!(
        local_view.old_encoding, remote_view.old_encoding,
        "old encoding"
    );
    assert_eq!(
        local_view.new_encoding, remote_view.new_encoding,
        "new encoding"
    );
    assert_eq!(*local_view.gaps, *remote_view.gaps, "gaps");
    assert_eq!(local_view.lfs, remote_view.lfs, "lfs summary");
    assert!(
        local_view.image.is_none() && remote_view.image.is_none(),
        "a markdown file has no image side"
    );
}

fn wait_for_remote(session: &RemoteSession, events: &Receiver<SessionEvent>) -> Arc<RepoSnapshot> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let snapshot = session.snapshot();
        if snapshot.commits.complete && snapshot.head.is_some() {
            return snapshot;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(SessionEvent::LoadFailed { error }) => panic!("remote load failed: {error}"),
            Ok(_) => {}
            Err(error) => panic!("remote snapshot did not complete: {error}"),
        }
    }
}

fn assert_parity(repo: &TestRepo, remote: &RemoteSession, remote_events: &Receiver<SessionEvent>) {
    let (local, local_events) = open_local(repo);
    let local_snapshot = wait_for_local(&local, &local_events);
    // `connect` returns after the first chunk. The walk may still be running.
    let remote_snapshot = wait_for_remote(remote, remote_events);
    assert_snapshots_match(&local_snapshot, &remote_snapshot);

    assert_readers_match(&local, remote);

    let head = match local_snapshot.head.as_ref().expect("head") {
        strata_core::HeadInfo::Branch {
            target: Some(target),
            ..
        } => *target,
        strata_core::HeadInfo::Detached { target } => *target,
        strata_core::HeadInfo::Branch { target: None, .. } => panic!("unborn branch"),
    };
    assert_diffs_match(
        &local,
        &local_events,
        remote,
        remote_events,
        DiffRequest::Commit {
            commit: head,
            path: "README.md".into(),
        },
    );

    assert_eq!(
        local
            .reader()
            .file_in_worktree("README.md")
            .expect("local worktree"),
        remote
            .reader()
            .file_in_worktree("README.md")
            .expect("remote worktree"),
        "worktree file"
    );
    assert_eq!(
        local.reader().conflict_style(),
        remote.reader().conflict_style(),
        "conflict style"
    );
}

#[test]
fn an_in_process_helper_matches_a_local_session() {
    let repo = fixture();
    let (client_read, client_write, server_read, server_write) = duplex();
    let root = repo.path().to_path_buf();
    let server = std::thread::spawn(move || strata_remote::serve(server_read, server_write, root));

    let (tx, rx) = mpsc::channel();
    let remote = RemoteSession::connect(
        client_read,
        client_write,
        RemoteTarget::new("local-duplex"),
        move |event| {
            let _ = tx.send(event);
        },
    )
    .expect("connect to the in-process helper");

    assert_parity(&repo, &remote, &rx);

    drop(remote);
    server
        .join()
        .expect("server thread")
        .expect("serve returns cleanly");
}

#[cfg(unix)]
#[test]
fn a_spawned_helper_matches_a_local_session() {
    let repo = fixture();
    let mut child = Command::new(env!("CARGO_BIN_EXE_strata-remote"))
        .arg("--stdio")
        .arg("--root")
        .arg(repo.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn strata-remote");
    let stdin = child.stdin.take().expect("helper stdin");
    let stdout = child.stdout.take().expect("helper stdout");

    let (tx, rx) = mpsc::channel();
    let remote = RemoteSession::connect(
        stdout,
        stdin,
        RemoteTarget::new("test-host"),
        move |event| {
            let _ = tx.send(event);
        },
    )
    .expect("connect to the spawned helper");

    assert_parity(&repo, &remote, &rx);
    drop(remote);
    let status = child.wait().expect("wait for helper");
    assert!(status.success(), "helper exited with {status}");
}

/// `stdin` EOF is the shutdown signal; the helper must exit successfully
/// instead of waiting for another frame.
#[cfg(unix)]
#[test]
fn closing_stdin_exits_the_helper_cleanly() {
    let repo = fixture();
    let mut child = Command::new(env!("CARGO_BIN_EXE_strata-remote"))
        .arg("--stdio")
        .arg("--root")
        .arg(repo.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn strata-remote");
    drop(child.stdin.take());
    let status = child.wait().expect("wait for helper");
    assert!(status.success(), "helper exited with {status}");
}

#[test]
fn self_check_reports_the_protocol_version_and_build() {
    let output = Command::new(env!("CARGO_BIN_EXE_strata-remote"))
        .arg("--self-check")
        .output()
        .expect("run self-check");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf-8 self-check");
    assert!(
        stdout.contains(&format!(
            "protocol: {}",
            strata_remote_protocol::PROTOCOL_VERSION
        )),
        "{stdout}"
    );
    assert!(stdout.contains("build: "), "{stdout}");
}

struct DuplexRead {
    rx: Receiver<Vec<u8>>,
    pending: Vec<u8>,
    offset: usize,
}

impl Read for DuplexRead {
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

struct DuplexWrite {
    tx: mpsc::Sender<Vec<u8>>,
}

impl Write for DuplexWrite {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.tx.send(buf.to_vec());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn duplex() -> (DuplexRead, DuplexWrite, DuplexRead, DuplexWrite) {
    let (client_tx, server_rx) = mpsc::channel();
    let (server_tx, client_rx) = mpsc::channel();
    (
        DuplexRead {
            rx: client_rx,
            pending: Vec::new(),
            offset: 0,
        },
        DuplexWrite { tx: client_tx },
        DuplexRead {
            rx: server_rx,
            pending: Vec::new(),
            offset: 0,
        },
        DuplexWrite { tx: server_tx },
    )
}
