//! Parity for ticketed queries: the same repository is read through a local
//! `Session` and through a `RemoteSession` talking to `lamina-remote`.
//!
//! The in-process helper runs on every platform. The spawned `--stdio` helper
//! is Unix, matching the other pipe tests. Batch boundaries are not compared:
//! a streaming walk may flush on a timer, so parity is the commits and counts
//! once the walk finishes.

use std::io::{Read, Write};
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use lamina_core::session::{
    CommitQuery, CompareMode, ContentSearchMode, ContentSearchQuery, QueryTicket, RepoSnapshot,
    RepositorySession, Session, SessionEvent,
};
use lamina_core::test_support::TestRepo;
use lamina_core::{CommitStat, Oid};
use lamina_remote_client::{CONNECTION_LOST, RemoteSession, RemoteTarget};

const TIMEOUT: Duration = Duration::from_secs(30);

struct History {
    repo: TestRepo,
    /// Kept alive so the submodule URL still resolves.
    _submodule: TestRepo,
    first: Oid,
    head: Oid,
}

fn history_fixture() -> History {
    let repo = TestRepo::new();
    repo.write("README.md", "alpha\n");
    repo.write("src/lib.rs", "pub fn one() {}\n");
    let first = repo.commit("initial import");

    repo.write("README.md", "alpha\nbeta\n");
    repo.commit("extend the readme");

    repo.git(&["branch", "feature"]);
    repo.write("src/lib.rs", "pub fn one() {}\npub fn two() {}\n");
    repo.commit("add two");

    let submodule = TestRepo::new();
    submodule.write("lib.txt", "lib\n");
    submodule.commit("lib");
    let url = format!("file://{}", submodule.path().display());
    repo.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        &url,
        "vendor/lib",
    ]);
    repo.commit("add a submodule");

    let linked = repo.path().parent().expect("temp parent").join("linked");
    repo.git(&[
        "worktree",
        "add",
        "-b",
        "linked",
        linked.to_str().expect("utf-8 worktree path"),
        "HEAD",
    ]);

    // Uncommitted, so a worktree comparison has something to report.
    repo.write("README.md", "alpha\nbeta\nedited\n");
    let head = repo.head();
    History {
        repo,
        _submodule: submodule,
        first,
        head,
    }
}

fn conflict_fixture() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("README.md", "base\n");
    repo.commit("base");
    repo.git(&["checkout", "-b", "side"]);
    repo.write("README.md", "side\n");
    repo.commit("side");
    repo.git(&["checkout", "main"]);
    repo.write("README.md", "main\n");
    repo.commit("main");
    let merge = repo.git_output(&["merge", "--no-edit", "side"]);
    assert!(
        !merge.status.success(),
        "the fixture merge should stop on README"
    );
    repo
}

fn open_local(repo: &TestRepo) -> (Session, Receiver<SessionEvent>) {
    let (tx, rx) = mpsc::channel();
    let session = Session::open(repo.path(), move |event| {
        let _ = tx.send(event);
    })
    .expect("open local session");
    (session, rx)
}

fn wait_until_loaded(snapshot_of: impl Fn() -> Arc<RepoSnapshot>, events: &Receiver<SessionEvent>) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let snapshot = snapshot_of();
        if snapshot.commits.complete && snapshot.head.is_some() {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(SessionEvent::LoadFailed { error }) => panic!("load failed: {error}"),
            Ok(_) => {}
            Err(error) => panic!("snapshot did not complete: {error}"),
        }
    }
}

fn ticket_of(event: &SessionEvent) -> Option<QueryTicket> {
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

fn is_terminal(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::QueryReady { .. }
            | SessionEvent::CommitStatReady { .. }
            | SessionEvent::CompareReady { .. }
            | SessionEvent::BlameReady { .. }
            | SessionEvent::QueryFailed { .. }
            | SessionEvent::PathHistoryDone { .. }
            | SessionEvent::ContentSearchDone { .. }
            | SessionEvent::SubmodulesAndWorktreesReady { .. }
            | SessionEvent::ReflogReady { .. }
            | SessionEvent::InProgressDetailsReady { .. }
            | SessionEvent::CommitSignaturesReady { .. }
    )
}

fn collect(events: &Receiver<SessionEvent>, ticket: QueryTicket) -> Vec<SessionEvent> {
    let deadline = Instant::now() + TIMEOUT;
    let mut matched = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(SessionEvent::QueryFailed {
                ticket: failed,
                error,
            }) if failed == ticket => {
                panic!("query {ticket:?} failed: {error}");
            }
            Ok(event) if ticket_of(&event) == Some(ticket) => {
                let done = is_terminal(&event);
                matched.push(event);
                if done {
                    return matched;
                }
            }
            Ok(_) => {}
            Err(error) => panic!("query {ticket:?} did not finish: {error}"),
        }
    }
}

fn commit_ids(snapshot: &RepoSnapshot, rows: &[usize]) -> Vec<Oid> {
    rows.iter()
        .map(|row| {
            snapshot
                .commits
                .get(*row)
                .unwrap_or_else(|| panic!("row {row} is outside the snapshot"))
                .0
                .id
        })
        .collect()
}

fn assert_same_rows(
    label: &str,
    local_snapshot: &RepoSnapshot,
    local_rows: &[usize],
    remote_snapshot: &RepoSnapshot,
    remote_rows: &[usize],
) {
    let local_ids = commit_ids(local_snapshot, local_rows);
    let remote_ids = commit_ids(remote_snapshot, remote_rows);
    assert_eq!(local_ids, remote_ids, "{label} commit ids");
    assert_eq!(local_rows, remote_rows, "{label} rows");
}

fn query_rows(events: &Receiver<SessionEvent>, ticket: QueryTicket) -> (u64, Vec<usize>) {
    let events = collect(events, ticket);
    let SessionEvent::QueryReady {
        generation, rows, ..
    } = events.last().expect("query ready")
    else {
        panic!("expected QueryReady, got {events:?}");
    };
    (*generation, (**rows).clone())
}

fn assert_queries(
    history: &History,
    remote: &RemoteSession,
    remote_events: &Receiver<SessionEvent>,
) {
    let (local, local_events) = open_local(&history.repo);
    wait_until_loaded(|| local.snapshot(), &local_events);
    wait_until_loaded(|| remote.snapshot(), remote_events);
    let local_snapshot = local.snapshot();

    let query = CommitQuery {
        text: "readme".into(),
        in_summary: true,
        in_author: false,
        in_hash: false,
    };
    let local_search = local.search_commits(query.clone());
    let remote_search = remote.search_commits(query);
    let (_, local_rows) = query_rows(&local_events, local_search);
    let (_, remote_rows) = query_rows(remote_events, remote_search);
    assert_same_rows(
        "commit search",
        &local.snapshot(),
        &local_rows,
        &remote.snapshot(),
        &remote_rows,
    );
    assert!(!local_rows.is_empty(), "the readme commits should match");

    let local_path = local.path_history("README.md".into());
    let remote_path = remote.path_history("README.md".into());
    let (_, local_rows) = query_rows(&local_events, local_path);
    let (_, remote_rows) = query_rows(remote_events, remote_path);
    assert_same_rows(
        "path history",
        &local.snapshot(),
        &local_rows,
        &remote.snapshot(),
        &remote_rows,
    );

    let local_stream = local.path_history_streaming("README.md".into());
    let remote_stream = remote.path_history_streaming("README.md".into());
    assert_eq!(
        path_entries(&local_events, local_stream),
        path_entries(remote_events, remote_stream),
        "streaming path history"
    );

    assert_eq!(
        commit_stat(&local_events, local.request_commit_stat(history.head)),
        commit_stat(remote_events, remote.request_commit_stat(history.head)),
        "commit stat"
    );

    for mode in [CompareMode::TwoDot, CompareMode::ThreeDot] {
        assert_eq!(
            comparison(
                &local_events,
                local.request_compare(history.first, history.head, mode)
            ),
            comparison(
                remote_events,
                remote.request_compare(history.first, history.head, mode)
            ),
            "compare {mode:?}"
        );
    }
    assert_eq!(
        comparison(
            &local_events,
            local.request_compare_from(Some(history.first), history.head)
        ),
        comparison(
            remote_events,
            remote.request_compare_from(Some(history.first), history.head)
        ),
        "compare from a base"
    );
    assert_eq!(
        comparison(
            &local_events,
            local.request_compare_from(None, history.head)
        ),
        comparison(
            remote_events,
            remote.request_compare_from(None, history.head)
        ),
        "compare from the empty tree"
    );
    let worktree = comparison(
        &local_events,
        local.request_compare_with_worktree(history.head),
    );
    assert!(
        worktree
            .files
            .iter()
            .any(|file| file.stat.path == "README.md"),
        "the dirty readme should show up in the worktree comparison"
    );
    assert_eq!(
        worktree,
        comparison(
            remote_events,
            remote.request_compare_with_worktree(history.head)
        ),
        "compare with the worktree"
    );

    assert_eq!(
        blame(
            &local_events,
            local.request_blame(history.head, "README.md".to_owned())
        ),
        blame(
            remote_events,
            remote.request_blame(history.head, "README.md".to_owned())
        ),
        "blame"
    );

    assert_eq!(
        reflog(&local_events, local.request_reflog(32)),
        reflog(remote_events, remote.request_reflog(32)),
        "reflog"
    );
    assert_eq!(
        in_progress(&local_events, local.request_in_progress_details()),
        in_progress(remote_events, remote.request_in_progress_details()),
        "in-progress details on a normal repository"
    );

    let (local_modules, local_trees) =
        extras(&local_events, local.request_submodules_and_worktrees());
    let (remote_modules, remote_trees) =
        extras(remote_events, remote.request_submodules_and_worktrees());
    assert_eq!(local_modules, remote_modules, "submodules");
    assert!(!local_modules.is_empty(), "the submodule should be listed");
    assert_eq!(local_trees, remote_trees, "worktrees");
    assert!(
        local_trees.len() >= 2,
        "the main checkout and the linked worktree should both be listed"
    );

    assert_eq!(
        content(
            &local_events,
            local.content_search_streaming(ContentSearchQuery {
                mode: ContentSearchMode::Substring("beta".into()),
                path: Some("README.md".into()),
            })
        ),
        content(
            remote_events,
            remote.content_search_streaming(ContentSearchQuery {
                mode: ContentSearchMode::Substring("beta".into()),
                path: Some("README.md".into()),
            })
        ),
        "substring content search"
    );
    let local_regex = content(
        &local_events,
        local.content_search_streaming(ContentSearchQuery {
            mode: ContentSearchMode::Regex("fn two".into()),
            path: None,
        }),
    );
    assert!(!local_regex.0.is_empty(), "the regex should match a commit");
    assert_eq!(
        local_regex,
        content(
            remote_events,
            remote.content_search_streaming(ContentSearchQuery {
                mode: ContentSearchMode::Regex("fn two".into()),
                path: None,
            })
        ),
        "regex content search"
    );

    let ids: Vec<Oid> = (0..local_snapshot.commits.len())
        .map(|index| local_snapshot.commits.get(index).expect("commit").0.id)
        .collect();
    assert_eq!(
        signatures(&local_events, local.request_commit_signatures(ids.clone())),
        signatures(remote_events, remote.request_commit_signatures(ids)),
        "signatures"
    );

    // Cancel must not fail the ticket, and the helper must still answer.
    let cancelled = remote.path_history_streaming("src/lib.rs".into());
    remote.cancel_query(cancelled);
    let stat = commit_stat(remote_events, remote.request_commit_stat(history.head));
    assert_eq!(
        stat,
        commit_stat(&local_events, local.request_commit_stat(history.head)),
        "a query after cancel"
    );
    let leftover = Instant::now() + Duration::from_millis(200);
    while Instant::now() < leftover {
        match remote_events.recv_timeout(Duration::from_millis(20)) {
            Ok(SessionEvent::QueryFailed { ticket, error }) if ticket == cancelled => {
                panic!("cancel reported a failure: {error}");
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

fn assert_conflict(
    repo: &TestRepo,
    remote: &RemoteSession,
    remote_events: &Receiver<SessionEvent>,
) {
    let (local, local_events) = open_local(repo);
    wait_until_loaded(|| local.snapshot(), &local_events);
    wait_until_loaded(|| remote.snapshot(), remote_events);
    let local_details = in_progress(&local_events, local.request_in_progress_details());
    assert!(
        matches!(
            local_details,
            Some(lamina_core::repo::InProgressDetails::Merge { .. })
        ),
        "the conflict fixture should report a merge, got {local_details:?}"
    );
    assert_eq!(
        local_details,
        in_progress(remote_events, remote.request_in_progress_details()),
        "in-progress merge details"
    );
}

fn path_entries(
    events: &Receiver<SessionEvent>,
    ticket: QueryTicket,
) -> Vec<lamina_core::session::PathHistoryCommit> {
    let mut entries = Vec::new();
    for event in collect(events, ticket) {
        match event {
            SessionEvent::PathHistoryBatch { entries: batch, .. } => {
                entries.extend(batch.iter().cloned());
            }
            SessionEvent::PathHistoryDone { .. } => {}
            other => panic!("unexpected path event: {other:?}"),
        }
    }
    entries
}

fn commit_stat(events: &Receiver<SessionEvent>, ticket: QueryTicket) -> CommitStat {
    let events = collect(events, ticket);
    match events.last() {
        Some(SessionEvent::CommitStatReady { stat, .. }) => (**stat).clone(),
        other => panic!("expected a commit stat, got {other:?}"),
    }
}

fn comparison(
    events: &Receiver<SessionEvent>,
    ticket: QueryTicket,
) -> lamina_core::repo::Comparison {
    let events = collect(events, ticket);
    match events.last() {
        Some(SessionEvent::CompareReady { comparison, .. }) => (**comparison).clone(),
        other => panic!("expected a comparison, got {other:?}"),
    }
}

fn blame(events: &Receiver<SessionEvent>, ticket: QueryTicket) -> lamina_core::repo::Blame {
    let events = collect(events, ticket);
    match events.last() {
        Some(SessionEvent::BlameReady { blame, .. }) => (**blame).clone(),
        other => panic!("expected blame, got {other:?}"),
    }
}

fn reflog(events: &Receiver<SessionEvent>, ticket: QueryTicket) -> Vec<lamina_core::ReflogEntry> {
    let events = collect(events, ticket);
    match events.last() {
        Some(SessionEvent::ReflogReady { entries, .. }) => (**entries).clone(),
        other => panic!("expected a reflog, got {other:?}"),
    }
}

fn in_progress(
    events: &Receiver<SessionEvent>,
    ticket: QueryTicket,
) -> Option<lamina_core::repo::InProgressDetails> {
    let events = collect(events, ticket);
    match events.last() {
        Some(SessionEvent::InProgressDetailsReady { details, .. }) => details.clone(),
        other => panic!("expected in-progress details, got {other:?}"),
    }
}

fn extras(
    events: &Receiver<SessionEvent>,
    ticket: QueryTicket,
) -> (
    Vec<lamina_core::repo::SubmoduleInfo>,
    Vec<lamina_core::repo::WorktreeInfo>,
) {
    let events = collect(events, ticket);
    match events.last() {
        Some(SessionEvent::SubmodulesAndWorktreesReady {
            submodules,
            worktrees,
            ..
        }) => ((**submodules).clone(), (**worktrees).clone()),
        other => panic!("expected submodule and worktree lists, got {other:?}"),
    }
}

fn content(
    events: &Receiver<SessionEvent>,
    ticket: QueryTicket,
) -> (Vec<lamina_core::repo::ContentSearchEntry>, usize, usize) {
    let mut matches = Vec::new();
    let mut scanned = 0;
    let mut total = 0;
    for event in collect(events, ticket) {
        match event {
            SessionEvent::ContentSearchBatch {
                matches: batch,
                scanned: seen,
                total: expected,
                ..
            } => {
                matches.extend(batch.iter().cloned());
                scanned = seen;
                total = expected;
            }
            SessionEvent::ContentSearchProgress {
                scanned: seen,
                total: expected,
                ..
            } => {
                scanned = seen;
                total = expected;
            }
            SessionEvent::ContentSearchDone {
                scanned: seen,
                total: expected,
                ..
            } => {
                scanned = seen;
                total = expected;
            }
            other => panic!("unexpected content-search event: {other:?}"),
        }
    }
    (matches, scanned, total)
}

fn signatures(
    events: &Receiver<SessionEvent>,
    ticket: QueryTicket,
) -> std::collections::HashMap<Oid, lamina_core::repo::CommitSignature> {
    let events = collect(events, ticket);
    match events.last() {
        Some(SessionEvent::CommitSignaturesReady { signatures, .. }) => (**signatures).clone(),
        other => panic!("expected signatures, got {other:?}"),
    }
}

fn connect_in_process(
    repo: &TestRepo,
) -> (
    RemoteSession,
    Receiver<SessionEvent>,
    std::thread::JoinHandle<Result<(), lamina_remote::BoxError>>,
) {
    let (client_read, client_write, server_read, server_write) = duplex();
    let root = repo.path().to_path_buf();
    let server = std::thread::spawn(move || lamina_remote::serve(server_read, server_write, root));
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
    (remote, rx, server)
}

#[test]
fn an_in_process_helper_matches_local_queries() {
    let history = history_fixture();
    let (remote, events, server) = connect_in_process(&history.repo);
    assert_queries(&history, &remote, &events);
    drop(remote);
    server
        .join()
        .expect("server thread")
        .expect("serve returns cleanly");
}

#[test]
fn an_in_process_helper_reports_merge_details() {
    let repo = conflict_fixture();
    let (remote, events, server) = connect_in_process(&repo);
    assert_conflict(&repo, &remote, &events);
    drop(remote);
    server
        .join()
        .expect("server thread")
        .expect("serve returns cleanly");
}

#[cfg(unix)]
#[test]
fn a_spawned_helper_matches_local_queries() {
    let history = history_fixture();
    let (remote, events, mut child) = spawn_helper(&history.repo);
    assert_queries(&history, &remote, &events);
    drop(remote);
    let status = child.wait().expect("wait for helper");
    assert!(status.success(), "helper exited with {status}");
}

#[cfg(unix)]
#[test]
fn a_spawned_helper_reports_merge_details() {
    let repo = conflict_fixture();
    let (remote, events, mut child) = spawn_helper(&repo);
    assert_conflict(&repo, &remote, &events);
    drop(remote);
    let status = child.wait().expect("wait for helper");
    assert!(status.success(), "helper exited with {status}");
}

/// The helper is already gone, so the ticket cannot be answered. The client
/// expires it instead of waiting out a ping timeout.
#[cfg(unix)]
#[test]
fn a_killed_helper_expires_the_next_query() {
    let history = history_fixture();
    let (remote, events, mut child) = spawn_helper(&history.repo);
    wait_until_loaded(|| remote.snapshot(), &events);
    child.kill().expect("kill helper");
    let ticket = remote.request_commit_stat(history.head);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(SessionEvent::QueryFailed {
                ticket: failed,
                error,
            }) if failed == ticket => {
                assert!(
                    error.contains(CONNECTION_LOST),
                    "expired ticket should name the lost connection: {error}"
                );
                break;
            }
            Ok(SessionEvent::LoadFailed { error }) => {
                assert!(error.contains(CONNECTION_LOST), "{error}");
            }
            Ok(_) => {}
            Err(error) => panic!("query was not expired after the helper died: {error}"),
        }
    }
    let _ = child.wait();
}

#[cfg(unix)]
fn spawn_helper(repo: &TestRepo) -> (RemoteSession, Receiver<SessionEvent>, std::process::Child) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_lamina-remote"))
        .arg("--stdio")
        .arg("--root")
        .arg(repo.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn lamina-remote");
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
    (remote, rx, child)
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
