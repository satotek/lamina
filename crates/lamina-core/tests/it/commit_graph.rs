//! Integration tests for commit-graph detection and writing.
//!
//! Every repository is a `TempDir` driven through the `git` CLI with an
//! isolated configuration, so the host's git settings cannot change a result.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use lamina_core::git_cli::Git;
use lamina_core::repo::{CommitGraphAdvice, CommitGraphThresholds, Repo};
use lamina_core::session::{Operation, Session, SessionEvent, SessionOptions};
use lamina_core::test_support::TestRepo;

const TIMEOUT: Duration = Duration::from_secs(60);

/// The shared fixture plus the graph facts the tests compare.
struct Fixture {
    repo: TestRepo,
}

impl Fixture {
    fn new() -> Self {
        Self {
            repo: TestRepo::new(),
        }
    }

    fn path(&self) -> &Path {
        self.repo.path()
    }

    fn git(&self, args: &[&str]) -> String {
        self.repo.git(args).trim_end().to_owned()
    }

    fn commit(&self, file: &str, contents: &str, message: &str) {
        self.repo.write(file, contents);
        self.git(&["add", "--", file]);
        self.git(&["commit", "--quiet", "-m", message]);
    }

    fn write_graph(&self) {
        self.git(&["commit-graph", "write", "--reachable"]);
    }

    fn graph_path(&self) -> PathBuf {
        self.repo.path().join(".git/objects/info/commit-graph")
    }

    fn open(&self) -> Repo {
        self.repo.open()
    }

    fn git_handle(&self) -> Git {
        self.repo.git_handle()
    }

    fn facts(&self) -> (bool, usize, Option<CommitGraphAdvice>) {
        let repo = self.open();
        let started = Instant::now();
        let walk = repo.walk_commits().expect("commit walk");
        let facts = repo.commit_graph_facts(&walk, started.elapsed());
        // A zero minimum makes the test independent of how fast this machine
        // walks a three-commit history.
        let advice = facts.advise(CommitGraphThresholds {
            min_commits: 0,
            min_walk: Duration::ZERO,
        });
        (facts.has_graph, facts.commits_outside_graph, advice)
    }
}

#[test]
fn a_repository_without_a_graph_is_offered_one() {
    let fixture = Fixture::new();
    fixture.commit("a.txt", "one\n", "first");
    fixture.commit("a.txt", "two\n", "second");
    assert!(!fixture.graph_path().exists());

    let (has_graph, outside, advice) = fixture.facts();
    assert!(!has_graph);
    assert_eq!(outside, 0);
    assert_eq!(advice, Some(CommitGraphAdvice::Missing));
}

#[test]
fn a_complete_graph_is_not_offered() {
    let fixture = Fixture::new();
    fixture.commit("a.txt", "one\n", "first");
    fixture.commit("a.txt", "two\n", "second");
    fixture.write_graph();
    assert!(fixture.graph_path().exists());

    let (has_graph, outside, advice) = fixture.facts();
    assert!(has_graph);
    assert_eq!(outside, 0);
    assert_eq!(advice, None);
}

#[test]
fn a_graph_missing_new_commits_is_stale() {
    let fixture = Fixture::new();
    fixture.commit("a.txt", "one\n", "first");
    fixture.write_graph();
    // History written after the graph is exactly what the next write catches.
    fixture.commit("b.txt", "late\n", "late");

    let (has_graph, outside, advice) = fixture.facts();
    assert!(has_graph);
    assert!(outside > 0, "the late commit is outside the graph");
    assert_eq!(advice, Some(CommitGraphAdvice::Stale));
}

#[test]
fn the_write_operation_creates_the_commit_graph() {
    let fixture = Fixture::new();
    fixture.commit("a.txt", "one\n", "first");
    assert!(!fixture.graph_path().exists());

    fixture
        .git_handle()
        .write_commit_graph()
        .expect("write commit-graph");

    assert!(fixture.graph_path().exists());
    let (has_graph, outside, advice) = fixture.facts();
    assert!(has_graph);
    assert_eq!(outside, 0);
    assert_eq!(advice, None);
}

#[test]
fn the_session_does_not_nag_about_a_slightly_stale_graph() {
    // `git gc` leaves a graph behind that every later commit outgrows; a small
    // repository a few commits past it loads instantly and needs no offer.
    let fixture = Fixture::new();
    fixture.commit("a.txt", "one\n", "first");
    fixture.write_graph();
    fixture.commit("b.txt", "late\n", "late");

    let (tx, rx): (_, Receiver<SessionEvent>) = mpsc::channel();
    let session = Session::open_with(fixture.path(), SessionOptions::default(), move |event| {
        let _ = tx.send(event);
    })
    .expect("open session");

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for the history");
        match rx.recv_timeout(deadline - now) {
            Ok(SessionEvent::CommitGraphSuggested { advice }) => {
                panic!("unexpected commit-graph offer: {advice:?}")
            }
            // The offer is decided before the first snapshot, so a complete
            // history without one means none is coming.
            Ok(SessionEvent::SnapshotUpdated { .. }) if session.snapshot().commits.complete => {
                return;
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("session event channel closed"),
        }
    }
}

#[test]
fn the_session_writes_a_graph_as_a_normal_operation() {
    let fixture = Fixture::new();
    fixture.commit("a.txt", "one\n", "first");
    assert!(!fixture.graph_path().exists());

    let (tx, rx): (_, Receiver<SessionEvent>) = mpsc::channel();
    let session = Session::open_with(fixture.path(), SessionOptions::default(), move |event| {
        let _ = tx.send(event);
    })
    .expect("open session");

    let id = session.run(Operation::WriteCommitGraph);
    wait_for_operation(&rx, id);
    assert!(fixture.graph_path().exists());
}

fn wait_for_operation(events: &Receiver<SessionEvent>, id: lamina_core::session::OperationId) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for operation {id:?}");
        match events.recv_timeout(deadline - now) {
            Ok(SessionEvent::OperationFinished {
                id: finished,
                error,
            }) if finished == id => {
                assert_eq!(error, None, "commit-graph write failed");
                return;
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("session event channel closed"),
        }
    }
}
