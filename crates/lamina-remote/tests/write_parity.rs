//! A local `Session` and a `lamina-remote --stdio` helper must leave the same
//! repository after each write this revision adds.
//!
//! The two repositories are twins: the fixture pins the author and the
//! committer date, so the same commit sequence produces the same object ids.
//! Comparing `HEAD` and the worktree then says whether the helper ran the
//! operation git would have run locally. The pipe is unix-only.

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use lamina_core::session::{Operation, OperationId, RepositorySession, Session, SessionEvent};
use lamina_core::test_support::TestRepo;
use lamina_core::{ReflogEntry, ReflogKind};
use lamina_remote_client::{RemoteSession, RemoteTarget};

const TIMEOUT: Duration = Duration::from_secs(30);

fn two_commits() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("README.md", "one\n");
    repo.commit("first");
    repo.write("README.md", "two\n");
    repo.commit("second");
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

struct Helper {
    child: std::process::Child,
    session: RemoteSession,
    events: Receiver<SessionEvent>,
}

impl Helper {
    fn spawn(repo: &TestRepo) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_lamina-remote"))
            .arg("--stdio")
            .arg("--root")
            .arg(repo.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn lamina-remote");
        let stdin = child.stdin.take().expect("helper stdin");
        let stdout = child.stdout.take().expect("helper stdout");
        let (tx, rx) = mpsc::channel();
        let session = RemoteSession::connect(
            stdout,
            stdin,
            RemoteTarget::new("write-parity"),
            move |event| {
                let _ = tx.send(event);
            },
        )
        .expect("connect to the spawned helper");
        Self {
            child,
            session,
            events: rx,
        }
    }

    fn close(mut self) {
        drop(self.session);
        let status = self.child.wait().expect("wait for helper");
        assert!(status.success(), "helper exited with {status}");
    }
}

fn wait_finished(events: &Receiver<SessionEvent>, id: OperationId) -> Option<String> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(SessionEvent::OperationFinished {
                id: event_id,
                error,
            }) if event_id == id => return error,
            Ok(SessionEvent::LoadFailed { error }) => panic!("load failed: {error}"),
            Ok(_) => {}
            Err(error) => panic!("operation {id:?} did not finish: {error}"),
        }
    }
}

fn run_local(session: &Session, events: &Receiver<SessionEvent>, operation: Operation) {
    let id = session.run(operation);
    let error = wait_finished(events, id);
    assert_eq!(error, None, "local operation failed");
}

fn run_remote(helper: &Helper, operation: Operation) {
    let id = helper.session.run(operation);
    let error = wait_finished(&helper.events, id);
    assert_eq!(error, None, "remote operation failed");
}

/// `HEAD` and a clean or dirty worktree, which is what undo and redo move.
fn repository_state(repo: &TestRepo) -> (String, String) {
    let head = repo.git(&["rev-parse", "HEAD"]);
    let status = repo.git(&["status", "--porcelain"]);
    (head, status)
}

fn newest_head_commit(repo: &TestRepo) -> ReflogEntry {
    repo.open()
        .reflog(20)
        .expect("read reflog")
        .into_iter()
        .find(|entry| entry.reference == "HEAD" && entry.kind == ReflogKind::Commit)
        .expect("a HEAD commit entry")
}

#[test]
fn undo_and_redo_of_the_newest_action_match_a_local_session() {
    let local_repo = two_commits();
    let remote_repo = two_commits();
    let first = local_repo.rev("HEAD~1");
    assert_eq!(first, remote_repo.rev("HEAD~1"), "twins share object ids");
    let second = local_repo.head();

    let (local, local_events) = open_local(&local_repo);
    let helper = Helper::spawn(&remote_repo);

    // Captured before the reset. Redo replays this entry: its old id is where
    // undo leaves HEAD, and its new id is the commit to put back.
    let local_commit = newest_head_commit(&local_repo);
    let remote_commit = newest_head_commit(&remote_repo);
    assert_eq!(local_commit, remote_commit);
    assert_eq!(local_commit.new_id, second);

    run_local(&local, &local_events, Operation::UndoReflog { entry: None });
    run_remote(&helper, Operation::UndoReflog { entry: None });
    assert_eq!(local_repo.head(), first, "local undo moves to the parent");
    assert_eq!(
        repository_state(&local_repo),
        repository_state(&remote_repo),
        "undo leaves the same repository"
    );

    run_local(
        &local,
        &local_events,
        Operation::RedoReflog {
            entry: local_commit,
        },
    );
    run_remote(
        &helper,
        Operation::RedoReflog {
            entry: remote_commit,
        },
    );
    assert_eq!(
        repository_state(&local_repo),
        repository_state(&remote_repo),
        "redo leaves the same repository"
    );
    assert_eq!(local_repo.head(), second);

    helper.close();
}

#[test]
fn undo_of_a_chosen_reflog_entry_matches_a_local_session() {
    let local_repo = two_commits();
    let remote_repo = two_commits();
    let local_entry = newest_head_commit(&local_repo);
    let remote_entry = newest_head_commit(&remote_repo);
    assert_eq!(local_entry, remote_entry, "the commit entry is identical");

    let (local, local_events) = open_local(&local_repo);
    let helper = Helper::spawn(&remote_repo);
    run_local(
        &local,
        &local_events,
        Operation::UndoReflog {
            entry: Some(local_entry.clone()),
        },
    );
    run_remote(
        &helper,
        Operation::UndoReflog {
            entry: Some(remote_entry),
        },
    );
    assert_eq!(
        repository_state(&local_repo),
        repository_state(&remote_repo)
    );
    assert_eq!(local_repo.head(), local_entry.old_id);
    helper.close();
}
