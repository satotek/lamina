//! The helper's filesystem watcher, over a real `lamina-remote --stdio`.
//!
//! The helper process is the one that watches. An in-process server would
//! share the test's address space and could not show that dropping stdin
//! joins that watcher.

#![cfg(unix)]

use std::collections::HashSet;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use lamina_core::session::{RepositorySession, SessionEvent};
use lamina_core::test_support::TestRepo;
use lamina_remote_client::{RemoteSession, RemoteTarget};

const TIMEOUT: Duration = Duration::from_secs(30);
/// Longer than the session's quiet window, so a refresh the watcher owed
/// would have arrived.
const QUIET: Duration = Duration::from_millis(500);

struct Helper {
    child: Child,
    remote: Option<RemoteSession>,
    events: Receiver<SessionEvent>,
}

impl Helper {
    fn spawn(repo: &TestRepo, watch: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lamina-remote"));
        command
            .arg("--stdio")
            .arg("--root")
            .arg(repo.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if !watch {
            // The helper treats this as "do not install a watch", the same
            // outcome as an inotify limit, without exhausting the host's.
            command.env("LAMINA_REMOTE_NO_WATCH", "1");
        }
        let mut child = command.spawn().expect("spawn lamina-remote");
        let stdin = child.stdin.take().expect("helper stdin");
        let stdout = child.stdout.take().expect("helper stdout");
        let (tx, rx) = mpsc::channel();
        let remote =
            RemoteSession::connect(stdout, stdin, RemoteTarget::new("stdio"), move |event| {
                let _ = tx.send(event);
            })
            .expect("connect to the spawned helper");
        let helper = Self {
            child,
            remote: Some(remote),
            events: rx,
        };
        helper.wait_until(|snapshot| snapshot.commits.complete && snapshot.head.is_some());
        helper
    }

    fn remote(&self) -> &RemoteSession {
        self.remote.as_ref().expect("helper still connected")
    }

    fn wait_until(&self, mut pred: impl FnMut(&lamina_core::session::RepoSnapshot) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if pred(&self.remote().snapshot()) {
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for the helper");
            match self.events.recv_timeout(remaining) {
                Ok(SessionEvent::LoadFailed { error }) => panic!("helper load failed: {error}"),
                Ok(_) => {}
                Err(error) => panic!("helper event stream ended: {error}"),
            }
        }
    }

    /// Drain events until the stream has been quiet, so a later assertion is
    /// not charged for the snapshot the connection just finished.
    ///
    /// It waits for a full quiet period after the last event. Returning after
    /// the first one let the initial load's later status publish land after
    /// the test's own edit, which read as the helper reacting to it.
    fn settle(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            match self.events.recv_timeout(QUIET) {
                Ok(_) => continue,
                Err(_) => return,
            }
        }
        panic!("the helper never went quiet");
    }

    fn has_path(&self, path: &str) -> bool {
        let snapshot = self.remote().snapshot();
        snapshot
            .status
            .unstaged
            .iter()
            .chain(snapshot.status.staged.iter())
            .any(|change| change.path == path)
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        // Closing stdin is the helper's shutdown signal. Kill only if it is
        // still alive when the test is done, so a failed assertion cannot
        // leave a watcher running.
        drop(self.remote.take());
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn an_outside_write_appears_without_reload() {
    let repo = TestRepo::new();
    repo.write("README.md", "alpha\n");
    repo.commit("alpha");
    let helper = Helper::spawn(&repo, true);
    assert!(
        helper.remote().watches_filesystem(),
        "a helper that could watch should say so"
    );

    repo.write("outside.txt", "from outside\n");
    helper.wait_until(|_| helper.has_path("outside.txt"));
    let snapshot = helper.remote().snapshot();
    assert!(
        snapshot.commits.complete && snapshot.head.is_some(),
        "a worktree update must not drop the history already delivered"
    );
}

#[test]
fn a_burst_of_writes_is_coalesced() {
    let repo = TestRepo::new();
    repo.write("README.md", "alpha\n");
    repo.commit("alpha");
    let helper = Helper::spawn(&repo, true);
    // One edit first: registration can race with the earliest events.
    repo.write("warmup.txt", "warm\n");
    helper.wait_until(|_| helper.has_path("warmup.txt"));
    helper.settle();
    let baseline = helper.remote().snapshot().generation;

    const COUNT: usize = 80;
    for index in 0..COUNT {
        repo.write(&format!("burst/file{index}.txt"), "burst\n");
    }

    let mut generations = HashSet::new();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        assert!(
            Instant::now() < deadline,
            "the burst never reached the client"
        );
        match helper.events.recv_timeout(QUIET) {
            Ok(SessionEvent::SnapshotUpdated { generation }) => {
                if generation > baseline {
                    generations.insert(generation);
                }
            }
            Ok(SessionEvent::LoadFailed { error }) => panic!("helper load failed: {error}"),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {
                if (0..COUNT).all(|index| helper.has_path(&format!("burst/file{index}.txt"))) {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => panic!("helper event stream ended"),
        }
    }
    // The quiet window may still hold one trailing publish.
    loop {
        match helper.events.recv_timeout(QUIET) {
            Ok(SessionEvent::SnapshotUpdated { generation }) if generation > baseline => {
                generations.insert(generation);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    assert!(!generations.is_empty(), "80 writes produced no snapshot");
    assert!(
        generations.len() <= 8,
        "80 writes produced {} snapshot pushes",
        generations.len()
    );
}

#[test]
fn a_branch_created_with_plain_git_appears() {
    let repo = TestRepo::new();
    repo.write("README.md", "alpha\n");
    repo.commit("alpha");
    let helper = Helper::spawn(&repo, true);
    helper.settle();

    repo.git(&["branch", "side"]);
    helper.wait_until(|snapshot| {
        snapshot
            .refs
            .iter()
            .any(|reference| reference.full_name == "refs/heads/side")
    });
}

#[test]
fn ignored_file_and_object_churn_produce_no_push() {
    let repo = TestRepo::new();
    repo.write(".gitignore", "*.log\n");
    repo.write("README.md", "alpha\n");
    repo.commit("alpha");
    let helper = Helper::spawn(&repo, true);
    repo.write("warmup.txt", "warm\n");
    helper.wait_until(|_| helper.has_path("warmup.txt"));
    helper.settle();
    let baseline = helper.remote().snapshot().generation;

    for index in 0..20 {
        repo.write(&format!("noise{index}.log"), "ignored\n");
    }
    // A loose object is git-dir bookkeeping. The watcher drops it locally,
    // so the helper must not turn it into a snapshot either.
    repo.git(&["hash-object", "-w", "--", "README.md"]);

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match helper.events.recv_timeout(Duration::from_millis(200)) {
            Ok(SessionEvent::SnapshotUpdated { generation }) if generation > baseline => {
                panic!("ignored churn published generation {generation}");
            }
            Ok(SessionEvent::LoadFailed { error }) => panic!("helper load failed: {error}"),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("helper event stream ended"),
        }
    }
    assert!(
        !helper.has_path("noise0.log"),
        "an ignored file must not show up as a change"
    );
    assert_eq!(helper.remote().snapshot().generation, baseline);
}

#[test]
fn stdin_eof_shuts_the_watcher_down() {
    let repo = TestRepo::new();
    repo.write("README.md", "alpha\n");
    repo.commit("alpha");
    let mut helper = Helper::spawn(&repo, true);
    // Prove the watcher is the one that will be joined, not a helper that
    // never started it.
    repo.write("outside.txt", "from outside\n");
    helper.wait_until(|_| helper.has_path("outside.txt"));

    drop(helper.remote.take());
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match helper.child.try_wait() {
            Ok(Some(status)) => {
                assert!(status.success(), "helper exited with {status}");
                return;
            }
            Ok(None) => {
                assert!(
                    Instant::now() < deadline,
                    "helper was still running after stdin closed"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("wait failed: {error}"),
        }
    }
}

#[test]
fn an_unwatched_helper_stays_quiet_until_reload() {
    let repo = TestRepo::new();
    repo.write("README.md", "alpha\n");
    repo.commit("alpha");
    let helper = Helper::spawn(&repo, false);
    assert!(
        !helper.remote().watches_filesystem(),
        "the helper should report that it is not watching"
    );
    helper.settle();
    let baseline = helper.remote().snapshot().generation;

    repo.write("outside.txt", "from outside\n");
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match helper.events.recv_timeout(Duration::from_millis(200)) {
            // A late publish from the initial load may still arrive; only one
            // that has seen the outside edit proves the helper was watching.
            Ok(SessionEvent::SnapshotUpdated { generation })
                if generation > baseline && helper.has_path("outside.txt") =>
            {
                panic!("an unwatched helper published the outside edit in generation {generation}");
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("helper event stream ended"),
        }
    }
    assert!(!helper.has_path("outside.txt"));

    helper.remote().reload();
    helper.wait_until(|_| helper.has_path("outside.txt"));
}
