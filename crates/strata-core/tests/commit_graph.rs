//! Integration tests for commit-graph detection and writing.
//!
//! Every repository is a `TempDir` driven through the `git` CLI with an
//! isolated configuration, so the host's git settings cannot change a result.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use strata_core::git_cli::Git;
use strata_core::repo::{CommitGraphAdvice, CommitGraphThresholds, Repo};
use strata_core::session::{Operation, Session, SessionEvent, SessionOptions};
use tempfile::TempDir;

const BASE_TIME: i64 = 1_700_000_000;
const TIMEOUT: Duration = Duration::from_secs(60);

struct Fixture {
    _tmp: TempDir,
    repo: PathBuf,
    empty_config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().expect("temp dir");
        let repo = tmp.path().join("repo");
        let empty_config = tmp.path().join("empty-gitconfig");
        fs::write(&empty_config, b"").expect("empty config");
        fs::create_dir_all(&repo).expect("repo dir");
        let fixture = Self {
            _tmp: tmp,
            repo,
            empty_config,
        };
        fixture.git(&["init", "--quiet", "--initial-branch=main"]);
        fixture
    }

    fn path(&self) -> &Path {
        &self.repo
    }

    fn git(&self, args: &[&str]) -> String {
        let output = self.command(args).output().expect("git is runnable");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(&self.repo)
            .env("GIT_CONFIG_GLOBAL", &self.empty_config)
            .env("GIT_CONFIG_SYSTEM", &self.empty_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Strata Author")
            .env("GIT_AUTHOR_EMAIL", "author@example.com")
            .env("GIT_COMMITTER_NAME", "Strata Committer")
            .env("GIT_COMMITTER_EMAIL", "committer@example.com")
            .env("GIT_AUTHOR_DATE", format!("@{BASE_TIME} +0000"))
            .env("GIT_COMMITTER_DATE", format!("@{BASE_TIME} +0000"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn commit(&self, file: &str, contents: &str, message: &str) {
        fs::write(self.repo.join(file), contents).expect("write file");
        self.git(&["add", "--", file]);
        self.git(&["commit", "--quiet", "-m", message]);
    }

    fn write_graph(&self) {
        self.git(&["commit-graph", "write", "--reachable"]);
    }

    fn graph_path(&self) -> PathBuf {
        self.repo.join(".git/objects/info/commit-graph")
    }

    fn open(&self) -> Repo {
        Repo::open(&self.repo).expect("open repository")
    }

    fn git_handle(&self) -> Git {
        Git::new(self.repo.clone())
            .with_env("GIT_CONFIG_GLOBAL", &self.empty_config)
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
            .with_env("GIT_AUTHOR_NAME", "Strata Author")
            .with_env("GIT_AUTHOR_EMAIL", "author@example.com")
            .with_env("GIT_COMMITTER_NAME", "Strata Committer")
            .with_env("GIT_COMMITTER_EMAIL", "committer@example.com")
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
fn the_session_suggests_a_write_for_a_stale_graph() {
    let fixture = Fixture::new();
    fixture.commit("a.txt", "one\n", "first");
    fixture.write_graph();
    fixture.commit("b.txt", "late\n", "late");

    let (tx, rx): (_, Receiver<SessionEvent>) = mpsc::channel();
    let _session = Session::open_with(fixture.path(), SessionOptions::default(), move |event| {
        let _ = tx.send(event);
    })
    .expect("open session");

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for the offer");
        match rx.recv_timeout(deadline - now) {
            Ok(SessionEvent::CommitGraphSuggested { advice }) => {
                assert_eq!(advice, CommitGraphAdvice::Stale);
                return;
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
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

fn wait_for_operation(events: &Receiver<SessionEvent>, id: strata_core::session::OperationId) {
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
