//! A local `Session` and a `strata-remote --stdio` helper must leave the same
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

use strata_core::session::{Operation, OperationId, RepositorySession, Session, SessionEvent};
use strata_core::test_support::TestRepo;
use strata_core::{ReflogEntry, ReflogKind};
use strata_remote_client::{RemoteSession, RemoteTarget};

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

fn sibling(repo: &TestRepo, name: &str) -> std::path::PathBuf {
    repo.path()
        .parent()
        .expect("the repository sits in a temp dir")
        .join(name)
}

/// Worktree list with this machine's temp directory replaced, so two twins
/// can be compared even though their absolute paths differ.
fn worktree_list(repo: &TestRepo) -> String {
    let parent = repo
        .path()
        .parent()
        .expect("parent")
        .to_string_lossy()
        .into_owned();
    repo.git(&["worktree", "list", "--porcelain"])
        .replace(&parent, "$PARENT")
}

#[test]
fn adding_removing_and_pruning_a_worktree_matches_a_local_session() {
    let local_repo = two_commits();
    let remote_repo = two_commits();
    let local_head = local_repo.head();
    let remote_head = remote_repo.head();
    assert_eq!(local_head, remote_head);

    let (local, local_events) = open_local(&local_repo);
    let helper = Helper::spawn(&remote_repo);

    run_local(
        &local,
        &local_events,
        Operation::WorktreeAdd {
            path: sibling(&local_repo, "topic"),
            branch: None,
            new_branch: Some("topic".into()),
            commit: Some(local_head),
        },
    );
    run_remote(
        &helper,
        Operation::WorktreeAdd {
            path: sibling(&remote_repo, "topic"),
            branch: None,
            new_branch: Some("topic".into()),
            commit: Some(remote_head),
        },
    );
    assert_eq!(worktree_list(&local_repo), worktree_list(&remote_repo));
    assert_eq!(
        local_repo.git(&["rev-parse", "topic"]),
        remote_repo.git(&["rev-parse", "topic"])
    );

    run_local(
        &local,
        &local_events,
        Operation::WorktreeRemove {
            path: sibling(&local_repo, "topic"),
            force: false,
        },
    );
    run_remote(
        &helper,
        Operation::WorktreeRemove {
            path: sibling(&remote_repo, "topic"),
            force: false,
        },
    );
    assert_eq!(worktree_list(&local_repo), worktree_list(&remote_repo));
    assert!(
        !local_repo
            .git(&["worktree", "list", "--porcelain"])
            .contains("topic"),
        "remove drops the linked worktree"
    );

    // Prune drops a registration whose directory is already gone.
    let local_gone = sibling(&local_repo, "gone");
    let remote_gone = sibling(&remote_repo, "gone");
    let local_gone_text = local_gone.to_string_lossy();
    let remote_gone_text = remote_gone.to_string_lossy();
    let local_spec = local_head.to_string();
    let remote_spec = remote_head.to_string();
    local_repo.git(&[
        "worktree",
        "add",
        "--detach",
        "--",
        local_gone_text.as_ref(),
        &local_spec,
    ]);
    remote_repo.git(&[
        "worktree",
        "add",
        "--detach",
        "--",
        remote_gone_text.as_ref(),
        &remote_spec,
    ]);
    std::fs::remove_dir_all(&local_gone).expect("delete local checkout");
    std::fs::remove_dir_all(&remote_gone).expect("delete remote checkout");
    run_local(&local, &local_events, Operation::WorktreePrune);
    run_remote(&helper, Operation::WorktreePrune);
    assert_eq!(worktree_list(&local_repo), worktree_list(&remote_repo));
    assert!(
        !local_repo
            .git(&["worktree", "list", "--porcelain"])
            .contains("gone")
    );

    helper.close();
}

/// A superproject whose submodule checkout is one commit behind the gitlink.
///
/// The objects are already in the submodule, so the update does not have to
/// clone over `file://`. That transport is refused unless the invoking git
/// command opts in, and the helper runs plain git.
fn superproject() -> (TestRepo, TestRepo) {
    let library = TestRepo::new();
    library.write("lib.txt", "lib\n");
    let first = library.commit("lib");
    let repo = TestRepo::new();
    repo.write("README.md", "super\n");
    repo.commit("super");
    let url = format!("file://{}", library.path().display());
    repo.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "--",
        &url,
        "libs/demo",
    ]);
    repo.commit("add demo");
    library.write("lib.txt", "lib2\n");
    library.commit("lib2");
    repo.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "update",
        "--remote",
        "--",
        "libs/demo",
    ]);
    repo.commit("bump demo");
    let first = first.to_string();
    repo.git(&["-C", "libs/demo", "checkout", "-q", &first]);
    (repo, library)
}

fn submodule_checkout(repo: &TestRepo) -> String {
    format!(
        "{}{}",
        repo.git(&["-C", "libs/demo", "rev-parse", "HEAD"]),
        repo.git(&["submodule", "status"])
    )
}

#[test]
fn updating_a_submodule_matches_a_local_session() {
    let (local_repo, _local_library) = superproject();
    let (remote_repo, _remote_library) = superproject();
    assert_eq!(
        submodule_checkout(&local_repo),
        submodule_checkout(&remote_repo),
        "twins start one commit behind the same gitlink"
    );

    let (local, local_events) = open_local(&local_repo);
    let helper = Helper::spawn(&remote_repo);
    let update = Operation::SubmoduleUpdate {
        paths: vec!["libs/demo".into()],
        init: true,
    };
    run_local(&local, &local_events, update.clone());
    run_remote(&helper, update);
    assert_eq!(
        submodule_checkout(&local_repo),
        submodule_checkout(&remote_repo)
    );
    assert_eq!(
        local_repo.git(&["-C", "libs/demo", "rev-parse", "HEAD"]),
        local_repo.git(&["rev-parse", "HEAD:libs/demo"]),
        "update checks out the recorded commit"
    );
    helper.close();
}
