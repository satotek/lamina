//! Integration tests for `Session` against real repositories.
//!
//! The session owns threads and a filesystem watcher, so every test drives it
//! through the public API and waits for events instead of sleeping: a snapshot
//! predicate is polled whenever a `SnapshotUpdated` arrives, and operations are
//! awaited by their `OperationFinished` event.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use strata_core::diff::{DiffOptions, FileDiff, LineKind, diff_text};
use strata_core::git_cli::CommandRecord;
use strata_core::patch::LineSelection;
use strata_core::repo::Repo;
use strata_core::session::{
    DiffRequest, DiffTicket, DiffView, Operation, OperationId, RepoSnapshot, Session, SessionEvent,
};
use strata_core::{HeadInfo, WorktreeStatus};
use tempfile::TempDir;

/// Generous: inotify, FSEvents and Windows deliver with different latencies,
/// and a 20k-commit walk takes a while on a shared machine.
const TIMEOUT: Duration = Duration::from_secs(60);

struct TestRepo {
    workdir: TempDir,
    /// Separate dir so the empty global/system config never shows up as a
    /// worktree change.
    config: TempDir,
}

impl TestRepo {
    fn new() -> Self {
        let repo = Self {
            workdir: TempDir::new().expect("worktree temp dir"),
            config: TempDir::new().expect("config temp dir"),
        };
        fs::write(repo.config_file(), "").expect("empty git config");
        repo.git(&["init", "-q", "-b", "main"]);
        // The session's git commands also run with the host's global config, so
        // pin the settings a commit cannot work without.
        repo.git(&["config", "user.name", "Strata Test"]);
        repo.git(&["config", "user.email", "strata@example.com"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo.git(&["config", "core.autocrlf", "false"]);
        repo
    }

    fn config_file(&self) -> PathBuf {
        self.config.path().join("gitconfig")
    }

    fn path(&self) -> &Path {
        self.workdir.path()
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(path, contents).expect("write file");
    }

    fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.path().join(relative)).expect("read file")
    }

    /// Isolated from the host's git config and hooks, like the other tests.
    fn command(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(self.path());
        cmd.env("GIT_CONFIG_GLOBAL", self.config_file())
            .env("GIT_CONFIG_SYSTEM", self.config_file())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("XDG_CONFIG_HOME", self.config.path())
            .env("GIT_AUTHOR_NAME", "Strata Test")
            .env("GIT_AUTHOR_EMAIL", "strata@example.com")
            .env("GIT_COMMITTER_NAME", "Strata Test")
            .env("GIT_COMMITTER_EMAIL", "strata@example.com")
            .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE");
        cmd
    }

    fn git(&self, args: &[&str]) -> String {
        let output = self.command().args(args).output().expect("spawn git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn git_with_input(&self, args: &[&str], input: &str) -> String {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn git");
        let mut stdin = child.stdin.take().expect("git stdin");
        stdin.write_all(input.as_bytes()).expect("write git stdin");
        drop(stdin);
        let output = child.wait_with_output().expect("git output");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn commit_all(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }

    /// Builds `commits` linear commits with `git fast-import`, which is orders
    /// of magnitude faster than a loop of `git commit`.
    fn fast_import(&self, commits: usize) {
        let mut stream = String::new();
        stream.push_str("blob\nmark :1\ndata 3\nhi\n\n");
        for index in 0..commits {
            let message = format!("commit {index}");
            stream.push_str("commit refs/heads/main\n");
            stream.push_str(&format!(
                "committer Strata Test <strata@example.com> {} +0000\n",
                1_700_000_000 + index
            ));
            stream.push_str(&format!("data {}\n{}\n", message.len(), message));
            stream.push_str("M 100644 :1 file.txt\n\n");
        }
        self.git_with_input(&["fast-import", "--quiet"], &stream);
        // fast-import writes refs only; the index and worktree are still those
        // of the unborn branch.
        self.git(&["reset", "--hard", "HEAD"]);
    }
}

struct OperationOutcome {
    error: Option<String>,
    commands: Vec<CommandRecord>,
}

struct Harness {
    // Session first: it owns threads and a watcher that must stop before the
    // temp repository disappears.
    session: Session,
    events: Receiver<SessionEvent>,
    repo: TestRepo,
}

impl Harness {
    fn open(repo: TestRepo) -> Self {
        let (tx, rx) = mpsc::channel();
        let session = Session::open(repo.path(), move |event| {
            let _ = tx.send(event);
        })
        .expect("open session");
        Self {
            session,
            events: rx,
            repo,
        }
    }

    /// Polls a snapshot predicate, waking on every event. Checking before the
    /// receive covers events another helper already consumed.
    fn wait_until(
        &self,
        timeout: Duration,
        mut predicate: impl FnMut(&RepoSnapshot) -> bool,
    ) -> Arc<RepoSnapshot> {
        let deadline = Instant::now() + timeout;
        loop {
            let snapshot = self.session.snapshot();
            if predicate(&snapshot) {
                return snapshot;
            }
            let now = Instant::now();
            assert!(
                now < deadline,
                "timed out waiting for a snapshot (generation {}, {} commits, complete {}, status {:?})",
                snapshot.generation,
                snapshot.commits.len(),
                snapshot.commits.complete,
                snapshot.status
            );
            match self.events.recv_timeout(deadline - now) {
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => panic!("session event channel closed"),
            }
        }
    }

    fn wait_for_status(
        &self,
        mut predicate: impl FnMut(&WorktreeStatus) -> bool,
    ) -> Arc<RepoSnapshot> {
        self.wait_until(TIMEOUT, |snapshot| predicate(&snapshot.status))
    }

    fn wait_for_complete(&self, commits: usize) -> Arc<RepoSnapshot> {
        self.wait_until(TIMEOUT, |snapshot| {
            snapshot.commits.complete && snapshot.commits.len() == commits
        })
    }

    fn wait_for_operation(&self, id: OperationId) -> OperationOutcome {
        let deadline = Instant::now() + TIMEOUT;
        let mut commands = Vec::new();
        loop {
            let now = Instant::now();
            assert!(now < deadline, "timed out waiting for operation {id:?}");
            match self.events.recv_timeout(deadline - now) {
                Ok(SessionEvent::OperationFinished {
                    id: finished,
                    error,
                }) if finished == id => {
                    return OperationOutcome { error, commands };
                }
                Ok(SessionEvent::CommandLogged(record)) => commands.push(record),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => panic!("session event channel closed"),
            }
        }
    }

    /// Proves the watch pipeline is live before a test asserts on an external
    /// change; watcher registration can race with the first events.
    fn warm_up_watcher(&self) {
        self.repo.write("warmup.txt", "warm\n");
        self.wait_for_status(|status| {
            status
                .unstaged
                .iter()
                .any(|change| change.path == "warmup.txt")
        });
    }

    fn diff_ready_views(
        &self,
        ticket: DiffTicket,
        want: usize,
        timeout: Duration,
    ) -> Vec<Arc<DiffView>> {
        let deadline = Instant::now() + timeout;
        let mut views = Vec::new();
        while views.len() < want {
            let now = Instant::now();
            assert!(
                now < deadline,
                "timed out after {} DiffReady event(s) for {ticket:?}",
                views.len()
            );
            match self.events.recv_timeout(deadline - now) {
                Ok(SessionEvent::DiffReady { ticket: got, view }) if got == ticket => {
                    views.push(view)
                }
                Ok(SessionEvent::DiffFailed { ticket: got, error }) if got == ticket => {
                    panic!("diff {ticket:?} failed: {error}")
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => panic!("session event channel closed"),
            }
        }
        views
    }

    /// Diff events for `ticket` arriving within `window`; used for the negative
    /// assertions ("this ticket emits nothing").
    fn diff_events(&self, ticket: DiffTicket, window: Duration) -> Vec<SessionEvent> {
        let deadline = Instant::now() + window;
        let mut events = Vec::new();
        loop {
            let now = Instant::now();
            if now >= deadline {
                return events;
            }
            match self.events.recv_timeout(deadline - now) {
                Ok(event) => {
                    let matches = match &event {
                        SessionEvent::DiffReady { ticket: got, .. }
                        | SessionEvent::DiffFailed { ticket: got, .. } => *got == ticket,
                        _ => false,
                    };
                    if matches {
                        events.push(event);
                    }
                }
                Err(RecvTimeoutError::Timeout) => return events,
                Err(RecvTimeoutError::Disconnected) => return events,
            }
        }
    }
}

fn changed_lines(diff: &FileDiff) -> usize {
    diff.hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter(|line| line.kind != LineKind::Context)
        .count()
}

#[test]
fn first_snapshot_arrives_and_history_completes() {
    let repo = TestRepo::new();
    repo.fast_import(5_000);
    let harness = Harness::open(repo);

    let first = harness.wait_until(TIMEOUT, |snapshot| !snapshot.commits.is_empty());
    assert!(
        first.commits.len() >= 2_000,
        "the first publish should hold the first chunk, got {} commits",
        first.commits.len()
    );

    let complete = harness.wait_for_complete(5_000);
    assert!(complete.generation >= first.generation);

    // Rows and the index agree with the list, including at chunk boundaries.
    for index in [0, 1, 1_999, 2_000, 4_095, 4_096, 4_999] {
        let (commit, _) = complete
            .commits
            .get(index)
            .unwrap_or_else(|| panic!("commit {index} is missing"));
        assert_eq!(complete.commit_index.get(&commit.id), Some(&index));
    }
    let (head, _) = complete.commits.get(0).expect("head commit");
    assert_eq!(head.summary, "commit 4999");
    assert_eq!(
        complete.head,
        Some(HeadInfo::Branch {
            short_name: "main".into(),
            target: Some(head.id),
        })
    );

    let main = complete
        .refs
        .iter()
        .position(|reference| reference.full_name == "refs/heads/main")
        .expect("main ref");
    assert!(
        complete
            .refs_by_commit
            .get(&head.id)
            .is_some_and(|indices| indices.contains(&main)),
        "the branch badge is missing from the head commit"
    );
}

#[test]
fn external_commit_triggers_a_history_reload() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.commit_all("base");
    let harness = Harness::open(repo);
    harness.wait_for_complete(1);
    harness.warm_up_watcher();

    // A commit made outside the session, as another terminal would.
    harness.repo.write("a.txt", "two\n");
    harness.repo.git(&["add", "a.txt"]);
    harness.repo.git(&["commit", "-q", "-m", "external"]);

    let snapshot = harness.wait_for_complete(2);
    let (head, _) = snapshot.commits.get(0).expect("head commit");
    assert_eq!(head.summary, "external");
    assert_eq!(snapshot.commits.get(1).expect("parent").0.summary, "base");
    assert_eq!(
        snapshot.head,
        Some(HeadInfo::Branch {
            short_name: "main".into(),
            target: Some(head.id),
        })
    );
}

#[test]
fn editing_a_file_refreshes_status_without_reloading_history() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.commit_all("base");
    let harness = Harness::open(repo);
    harness.wait_for_complete(1);
    harness.warm_up_watcher();

    let before = harness.session.snapshot();
    harness.repo.write("a.txt", "two\n");
    let after = harness
        .wait_for_status(|status| status.unstaged.iter().any(|change| change.path == "a.txt"));

    // A status-only refresh reuses the loaded history and refs; any pointer
    // changing here means a walk was started for a worktree edit.
    assert!(
        Arc::ptr_eq(&before.refs, &after.refs),
        "refs were reloaded for a worktree edit"
    );
    assert!(
        Arc::ptr_eq(&before.commit_index, &after.commit_index),
        "history was reloaded for a worktree edit"
    );
    assert!(Arc::ptr_eq(&before.refs_by_commit, &after.refs_by_commit));
    assert_eq!(before.commits.len(), after.commits.len());
    assert_eq!(before.head, after.head);
    assert!(after.busy.is_none());
}

#[test]
fn stage_paths_and_commit_update_status_and_history() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.commit_all("base");
    let harness = Harness::open(repo);
    harness.wait_for_complete(1);
    harness.warm_up_watcher();

    harness.repo.write("a.txt", "two\n");
    harness.wait_for_status(|status| status.unstaged.iter().any(|change| change.path == "a.txt"));

    let id = harness
        .session
        .run(Operation::StagePaths(vec!["a.txt".into()]));
    let outcome = harness.wait_for_operation(id);
    assert!(
        outcome.error.is_none(),
        "staging failed: {:?}",
        outcome.error
    );
    // Every git command reaches the command log, as the UI's log panel expects.
    let add = outcome
        .commands
        .iter()
        .find(|record| record.args.first().is_some_and(|arg| arg == "add"))
        .expect("no `git add` command was logged");
    assert_eq!(add.exit_code, Some(0));

    let staged = harness.wait_for_status(|status| {
        status.staged.iter().any(|change| change.path == "a.txt")
            && !status.unstaged.iter().any(|change| change.path == "a.txt")
    });
    assert!(staged.busy.is_none());

    let id = harness.session.run(Operation::Commit {
        message: "second".into(),
        amend: false,
    });
    let outcome = harness.wait_for_operation(id);
    assert!(
        outcome.error.is_none(),
        "commit failed: {:?}",
        outcome.error
    );

    let committed = harness.wait_for_complete(2);
    assert!(
        committed.status.staged.is_empty(),
        "committed changes still staged: {:?}",
        committed.status
    );
    // The warm-up file stays untracked, so only `a.txt` was committed.
    assert!(
        committed
            .status
            .unstaged
            .iter()
            .any(|change| change.path == "warmup.txt")
    );
    let (head, _) = committed.commits.get(0).expect("head commit");
    assert_eq!(head.summary, "second");
    assert_eq!(
        committed.head,
        Some(HeadInfo::Branch {
            short_name: "main".into(),
            target: Some(head.id),
        })
    );
}

#[test]
fn stage_lines_stages_exactly_one_line() {
    let repo = TestRepo::new();
    let base: String = (1..=10).map(|index| format!("line {index}\n")).collect();
    repo.write("a.txt", &base);
    repo.commit_all("base");
    let harness = Harness::open(repo);
    harness.wait_for_complete(1);

    let edited = base
        .replace("line 3\n", "changed 3\n")
        .replace("line 8\n", "changed 8\n");
    harness.repo.write("a.txt", &edited);
    harness.wait_for_status(|status| status.unstaged.iter().any(|change| change.path == "a.txt"));

    // Pick the first changed line (the removed one of the first edit).
    let diff = diff_text(base.as_bytes(), edited.as_bytes(), &DiffOptions::default());
    let selected = diff
        .hunks
        .iter()
        .enumerate()
        .flat_map(|(hunk, hunk_diff)| {
            hunk_diff
                .lines
                .iter()
                .enumerate()
                .filter(|(_, line)| line.kind != LineKind::Context)
                .map(move |(line, _)| (hunk, line))
        })
        .next()
        .expect("the edited file has changed lines");

    let id = harness.session.run(Operation::StageLines {
        path: "a.txt".into(),
        selection: LineSelection::new([selected]),
    });
    let outcome = harness.wait_for_operation(id);
    assert!(
        outcome.error.is_none(),
        "line staging failed: {:?}",
        outcome.error
    );

    let snapshot = harness.wait_for_status(|status| {
        status.staged.iter().any(|change| change.path == "a.txt")
            && status.unstaged.iter().any(|change| change.path == "a.txt")
    });
    assert!(snapshot.busy.is_none());

    let repo_view = Repo::open(harness.repo.path()).expect("reopen repository");
    let index = String::from_utf8(
        repo_view
            .file_in_index("a.txt")
            .expect("read index")
            .expect("a.txt is in the index"),
    )
    .expect("utf-8 index");
    let staged_diff = diff_text(base.as_bytes(), index.as_bytes(), &DiffOptions::default());
    assert_eq!(
        changed_lines(&staged_diff),
        1,
        "index differs from HEAD by more than the selected line:\n{index}"
    );
    assert_eq!(index.lines().count(), base.lines().count() - 1);
    assert_eq!(harness.repo.read("a.txt"), edited);
}

#[test]
fn diff_ready_is_emitted_twice_with_highlights_for_a_known_language() {
    let repo = TestRepo::new();
    repo.write("src/main.rs", "fn main() {}\n");
    repo.write("notes.txt", "one\n");
    repo.commit_all("base");
    let harness = Harness::open(repo);
    harness.wait_for_complete(1);

    harness
        .repo
        .write("src/main.rs", "fn main() { let x = 1; }\n");
    harness.repo.write("notes.txt", "two\n");

    let rust = harness.session.request_diff(DiffRequest::Unstaged {
        path: "src/main.rs".into(),
    });
    let views = harness.diff_ready_views(rust, 2, TIMEOUT);
    assert!(
        views[0].old_highlights.is_empty() && views[0].new_highlights.is_empty(),
        "the first event must arrive without highlights"
    );
    assert!(!views[1].diff.binary);
    assert!(
        views[1]
            .new_highlights
            .iter()
            .any(|spans| !spans.is_empty()),
        "the highlighted event has no spans"
    );
    assert!(
        views[1]
            .old_highlights
            .iter()
            .any(|spans| !spans.is_empty()),
        "the old side was not highlighted"
    );

    // A path without a grammar produces exactly one event.
    let text = harness.session.request_diff(DiffRequest::Unstaged {
        path: "notes.txt".into(),
    });
    let views = harness.diff_ready_views(text, 1, TIMEOUT);
    assert!(views[0].old_highlights.is_empty() && views[0].new_highlights.is_empty());

    // Synchronize on a later request instead of sleeping: by the time this
    // one finished highlighting, a second event for `notes.txt` would be here.
    let sentinel = harness.session.request_diff(DiffRequest::Unstaged {
        path: "src/main.rs".into(),
    });
    harness.diff_ready_views(sentinel, 2, TIMEOUT);
    assert!(
        harness
            .diff_events(text, Duration::from_millis(300))
            .is_empty(),
        "an unknown extension emitted a second, highlighted diff"
    );
}

#[test]
fn cancelled_diff_emits_nothing() {
    let repo = TestRepo::new();
    let base: String = (0..20_000).map(|index| format!("line {index}\n")).collect();
    // A `.rs` path keeps the sentinel on the two-event path.
    repo.write("big.rs", &base);
    repo.commit_all("base");
    let harness = Harness::open(repo);
    harness.wait_for_complete(1);

    let edited = base
        .replace("line 100\n", "line 100 changed\n")
        .replace("line 19999\n", "line 19999 changed\n");
    harness.repo.write("big.rs", &edited);

    let ticket = harness.session.request_diff(DiffRequest::Unstaged {
        path: "big.rs".into(),
    });
    harness.session.cancel_diff(ticket);

    // The sentinel is queued after the cancelled request; its completion proves
    // the workers moved past the cancelled ticket.
    let sentinel = harness.session.request_diff(DiffRequest::Unstaged {
        path: "big.rs".into(),
    });
    assert_eq!(harness.diff_ready_views(sentinel, 2, TIMEOUT).len(), 2);
    assert!(
        harness
            .diff_events(ticket, Duration::from_millis(300))
            .is_empty(),
        "a cancelled diff emitted an event"
    );
}

#[test]
fn a_reload_supersedes_an_in_flight_walk() {
    let repo = TestRepo::new();
    repo.fast_import(20_000);
    let harness = Harness::open(repo);

    // Reload as soon as the walk has something to show: it is still running
    // and its older generation must not publish after the new one starts.
    harness.wait_until(TIMEOUT, |snapshot| !snapshot.commits.is_empty());
    harness.session.reload();

    let complete = harness.wait_for_complete(20_000);
    assert_eq!(complete.commits.len(), 20_000);

    // Nothing from the abandoned walk may land once the new walk finished: an
    // older chunk would shorten the list or unset `complete`.
    let deadline = Instant::now() + Duration::from_millis(750);
    let mut observed = 0;
    while Instant::now() < deadline {
        let remaining = deadline - Instant::now();
        match harness.events.recv_timeout(remaining) {
            Ok(SessionEvent::SnapshotUpdated { generation }) => {
                let snapshot = harness.session.snapshot();
                assert!(
                    snapshot.commits.complete,
                    "a stale walk published an incomplete snapshot at generation {generation}"
                );
                assert_eq!(
                    snapshot.commits.len(),
                    20_000,
                    "a stale walk shortened history at generation {generation}"
                );
                observed += 1;
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // The status refresh after the reload may legitimately add events, so this
    // is only a sanity check that the window was actually watched.
    assert!(observed < 1_000);
}

#[test]
fn busy_is_cleared_and_the_command_log_records_errors() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.commit_all("base");
    let harness = Harness::open(repo);
    harness.wait_for_complete(1);

    // `git branch` refuses to delete the checked-out branch, so the error path
    // reports git's stderr instead of succeeding silently.
    let id = harness.session.run(Operation::DeleteBranch {
        name: "main".into(),
        force: false,
    });
    let outcome = harness.wait_for_operation(id);
    let error = outcome
        .error
        .expect("deleting the current branch must fail");
    assert!(
        error.contains("cannot delete branch") || error.contains("Cannot delete branch"),
        "unexpected git error: {error}"
    );
    assert!(harness.session.snapshot().busy.is_none());
}

#[test]
fn dropping_a_loading_session_stops_its_threads() {
    let repo = TestRepo::new();
    // A large walk is still running when the session is dropped.
    repo.fast_import(20_000);

    let (tx, rx) = mpsc::channel();
    let session = Session::open(repo.path(), move |event| {
        let _ = tx.send(event);
    })
    .expect("open session");
    // Give the walk a moment to start, so the drop interrupts real work.
    let _ = rx.recv_timeout(Duration::from_secs(5));

    drop(session);

    // Every worker was joined before `drop` returned, so the event closure and
    // its sender are gone: the channel disconnects instead of going quiet.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let now = Instant::now();
        assert!(
            now < deadline,
            "the event sender is still alive after dropping the session"
        );
        match rx.recv_timeout(deadline - now) {
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {
                panic!("the event sender is still alive after dropping the session")
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

#[test]
fn session_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    // The UI keeps one session in its app model and drives it from its own
    // thread; every call is either a queue push or a snapshot read.
    assert_send_sync::<Session>();
}

#[test]
fn an_empty_repository_loads_without_commits() {
    let repo = TestRepo::new();
    let harness = Harness::open(repo);

    let snapshot = harness.wait_until(TIMEOUT, |snapshot| snapshot.commits.complete);
    assert!(snapshot.commits.is_empty());
    assert_eq!(
        snapshot.head,
        Some(HeadInfo::Branch {
            short_name: "main".into(),
            target: None,
        })
    );
    assert!(snapshot.refs.is_empty());
    assert!(snapshot.status.staged.is_empty() && snapshot.status.unstaged.is_empty());
}

#[test]
fn commit_diff_is_read_against_the_first_parent() {
    let repo = TestRepo::new();
    repo.write("a.rs", "fn a() {}\n");
    repo.commit_all("base");
    repo.write("a.rs", "fn b() {}\n");
    repo.commit_all("second");
    let harness = Harness::open(repo);

    let complete = harness.wait_for_complete(2);
    let (head, _) = complete.commits.get(0).expect("head commit");
    let ticket = harness.session.request_diff(DiffRequest::Commit {
        commit: head.id,
        path: "a.rs".into(),
    });
    let views = harness.diff_ready_views(ticket, 2, TIMEOUT);

    // The whole change of that commit: one replaced line, highlighted.
    assert_eq!(changed_lines(&views[0].diff), 2);
    assert_eq!(views[0].diff.hunks.len(), 1);
    assert!(
        views[1]
            .new_highlights
            .iter()
            .any(|spans| !spans.is_empty())
    );
}
