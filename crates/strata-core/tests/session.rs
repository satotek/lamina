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
    DiffRequest, DiffTicket, DiffView, FIRST_COMMIT_CHUNK, HistoryScope, Operation, OperationId,
    RepoSnapshot, Session, SessionEvent, SessionOptions,
};
use strata_core::{HeadInfo, RefKind, WorktreeStatus};
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

    fn repo_handle(&self) -> Repo {
        Repo::open(self.path()).expect("open repository")
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
        Self::open_with(repo, SessionOptions::default())
    }

    fn open_with(repo: TestRepo, options: SessionOptions) -> Self {
        let (tx, rx) = mpsc::channel();
        let session = Session::open_with(repo.path(), options, move |event| {
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
        first.commits.len() >= FIRST_COMMIT_CHUNK,
        "the first publish should hold at least the small first chunk, got {} commits",
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
fn dropping_a_session_during_the_initial_walk_is_prompt() {
    let repo = TestRepo::new();
    repo.fast_import(20_000);

    // How long the walk builds its reachability state before yielding anything,
    // measured on the same fixture so the bound scales with the machine. This is
    // the window a drop has to interrupt.
    let started = Instant::now();
    let walk = repo.repo_handle().walk_commits().expect("start walk");
    let walk_setup = started.elapsed();
    drop(walk);
    assert!(
        walk_setup > Duration::from_millis(50),
        "fixture is too fast ({walk_setup:?}) to show a long initial walk"
    );

    let (tx, _rx) = mpsc::channel();
    let session = Session::open(repo.path(), move |event| {
        let _ = tx.send(event);
    })
    .expect("open session");
    // Let the loader thread reach the walk, past the cheap reads before it.
    std::thread::sleep(walk_setup / 4);

    let started = Instant::now();
    drop(session);
    let dropped = started.elapsed();
    assert!(
        dropped < walk_setup / 2,
        "dropping the session waited {dropped:?} for the initial walk ({walk_setup:?})"
    );
}

#[test]
fn adding_a_remote_with_git_updates_remotes_without_a_reload() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.commit_all("base");
    let harness = Harness::open(repo);
    harness.wait_for_complete(1);
    harness.warm_up_watcher();

    let before = harness.session.snapshot();
    assert!(before.remotes.is_empty());

    // As another terminal would; the session is never told to reload.
    harness
        .repo
        .git(&["remote", "add", "origin", "https://example.com/origin.git"]);

    let after = harness.wait_until(TIMEOUT, |snapshot| {
        snapshot
            .remotes
            .iter()
            .any(|remote| remote.name == "origin")
    });
    let origin = after
        .remotes
        .iter()
        .find(|remote| remote.name == "origin")
        .expect("origin is listed");
    assert_eq!(
        origin.fetch_url.as_deref(),
        Some("https://example.com/origin.git")
    );
    // Only the configuration-derived fields were refreshed: a full reload would
    // have replaced these Arcs with a new walk.
    assert!(
        Arc::ptr_eq(&before.refs, &after.refs),
        "a config edit reloaded refs"
    );
    assert!(
        Arc::ptr_eq(&before.commit_index, &after.commit_index),
        "a config edit reloaded history"
    );
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
    assert!(
        snapshot.remotes.is_empty(),
        "the test repository has no remotes"
    );
    assert!(snapshot.status.staged.is_empty() && snapshot.status.unstaged.is_empty());
}

#[test]
fn request_diff_with_threads_context_lines_and_whitespace_to_the_worker() {
    let repo = TestRepo::new();
    let base: String = (1..=40).map(|index| format!("line {index}\n")).collect();
    repo.write("a.txt", &base);
    repo.write("b.txt", &base);
    repo.commit_all("base");
    let harness = Harness::open(repo);
    harness.wait_for_complete(1);

    let edited = base
        .replace("line 10\n", "line ten\n")
        .replace("line 30\n", "line thirty\n");
    harness.repo.write("a.txt", &edited);

    for context_lines in [0u32, 1, 3, 10] {
        let ticket = harness.session.request_diff_with(
            DiffRequest::Unstaged {
                path: "a.txt".into(),
            },
            DiffOptions {
                context_lines,
                ignore_whitespace: false,
            },
        );
        let views = harness.diff_ready_views(ticket, 1, TIMEOUT);
        let diff = &views[0].diff;
        if context_lines < 10 {
            assert_eq!(diff.hunks.len(), 2, "context {context_lines}");
            for hunk in &diff.hunks {
                assert_eq!(hunk.lines.len(), 2 + 2 * context_lines as usize);
            }
        } else {
            // Ten lines of context reach across the 19 common lines and merge
            // both edits into one hunk.
            assert_eq!(diff.hunks.len(), 1, "context {context_lines}");
            assert_eq!(diff.hunks[0].lines.len(), 9 + 2 + 19 + 2 + 10);
        }
    }

    // `b.txt` keeps the real edits out of the way: only line 20 is reindented,
    // which a whitespace-insensitive request must hide entirely.
    let reindented = base.replace("line 20\n", "    line 20   \n");
    harness.repo.write("b.txt", &reindented);

    let folded = harness.session.request_diff_with(
        DiffRequest::Unstaged {
            path: "b.txt".into(),
        },
        DiffOptions {
            context_lines: 3,
            ignore_whitespace: true,
        },
    );
    let views = harness.diff_ready_views(folded, 1, TIMEOUT);
    assert!(
        views[0].diff.hunks.is_empty(),
        "a whitespace-only change produced hunks"
    );

    // With a real edit next to it, only the real lines survive, still carrying
    // the file's text.
    let both = reindented.replace("line 30\n", "line thirty\n");
    harness.repo.write("b.txt", &both);
    let folded = harness.session.request_diff_with(
        DiffRequest::Unstaged {
            path: "b.txt".into(),
        },
        DiffOptions {
            context_lines: 3,
            ignore_whitespace: true,
        },
    );
    let views = harness.diff_ready_views(folded, 1, TIMEOUT);
    let hunks = &views[0].diff.hunks;
    assert_eq!(hunks.len(), 1, "only the line 30 edit is real");
    let changed: Vec<&str> = hunks[0]
        .lines
        .iter()
        .filter(|line| line.kind != LineKind::Context)
        .map(|line| line.content.as_str())
        .collect();
    assert_eq!(changed, vec!["line 30", "line thirty"]);

    let strict = harness.session.request_diff(DiffRequest::Unstaged {
        path: "b.txt".into(),
    });
    let views = harness.diff_ready_views(strict, 1, TIMEOUT);
    assert_eq!(
        views[0].diff.hunks.len(),
        2,
        "request_diff must keep the default behaviour"
    );
}

#[test]
fn snapshot_lists_remotes_that_have_no_fetched_branches() {
    let repo = TestRepo::new();
    repo.git(&["remote", "add", "origin", "https://example.com/origin.git"]);
    repo.git(&[
        "remote",
        "add",
        "--no-tags",
        "mirror",
        "https://example.com/mirror.git",
    ]);
    repo.git(&[
        "remote",
        "set-url",
        "--push",
        "mirror",
        "ssh://example.com/mirror.git",
    ]);
    let harness = Harness::open(repo);

    let snapshot = harness.wait_until(TIMEOUT, |snapshot| !snapshot.remotes.is_empty());
    // Sorted by name even though `origin` was added first.
    assert_eq!(
        snapshot
            .remotes
            .iter()
            .map(|remote| remote.name.as_str())
            .collect::<Vec<_>>(),
        vec!["mirror", "origin"]
    );
    let mirror = &snapshot.remotes[0];
    assert_eq!(
        mirror.fetch_url.as_deref(),
        Some("https://example.com/mirror.git")
    );
    assert_eq!(
        mirror.push_url.as_deref(),
        Some("ssh://example.com/mirror.git")
    );
    let origin = &snapshot.remotes[1];
    assert_eq!(
        origin.fetch_url.as_deref(),
        Some("https://example.com/origin.git")
    );
    assert_eq!(
        origin.push_url, None,
        "a remote whose push URL matches its fetch URL reports no separate push URL"
    );
    // Neither remote was fetched, which is exactly why the snapshot must carry
    // the configuration rather than the remote-tracking branches.
    assert!(
        snapshot
            .refs
            .iter()
            .all(|reference| reference.kind != RefKind::RemoteBranch),
        "the test repository should have no remote-tracking branches"
    );
}

#[test]
fn a_missing_git_executable_fails_writes_without_panicking() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.commit_all("base");
    let missing = repo.path().join("no-such-git-binary");
    let harness = Harness::open_with(
        repo,
        SessionOptions {
            git_executable: Some(missing),
        },
    );
    harness.wait_for_complete(1);
    harness.warm_up_watcher();

    harness.repo.write("a.txt", "two\n");
    harness.wait_for_status(|status| status.unstaged.iter().any(|change| change.path == "a.txt"));

    let id = harness
        .session
        .run(Operation::StagePaths(vec!["a.txt".into()]));
    let outcome = harness.wait_for_operation(id);
    let error = outcome
        .error
        .expect("a write with a missing git executable must fail");
    assert!(!error.trim().is_empty(), "the failure has no message");
    // The spawn never reached git, so the command log shows the failed attempt
    // without an exit code.
    assert!(
        outcome
            .commands
            .iter()
            .any(|record| record.exit_code.is_none()),
        "the spawn failure was not logged: {error}"
    );
    // Reads run through gix, so the index is untouched and the session stays
    // usable after the failed write.
    let snapshot = harness.session.snapshot();
    assert!(snapshot.busy.is_none());
    assert!(snapshot.status.staged.is_empty());
    assert!(
        snapshot
            .status
            .unstaged
            .iter()
            .any(|change| change.path == "a.txt")
    );
}

#[test]
fn default_session_options_stage_through_git_on_path() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.commit_all("base");
    let harness = Harness::open_with(repo, SessionOptions::default());
    harness.wait_for_complete(1);

    harness.repo.write("a.txt", "two\n");
    let id = harness
        .session
        .run(Operation::StagePaths(vec!["a.txt".into()]));
    let outcome = harness.wait_for_operation(id);
    assert!(
        outcome.error.is_none(),
        "staging failed with the default options: {:?}",
        outcome.error
    );
    harness.wait_for_status(|status| status.staged.iter().any(|change| change.path == "a.txt"));
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

#[test]
fn history_scope_walks_only_the_chosen_refs_and_all_restores_everything() {
    let repo = TestRepo::new();
    repo.write("base.txt", "base\n");
    repo.commit_all("base");
    let base = repo.git(&["rev-parse", "HEAD"]).trim().to_string();

    repo.git(&["checkout", "-q", "-b", "feature"]);
    repo.write("feature.txt", "feature\n");
    repo.commit_all("feature only");
    let feature = repo.git(&["rev-parse", "HEAD"]).trim().to_string();

    repo.git(&["checkout", "-q", "main"]);
    repo.write("main.txt", "main\n");
    repo.commit_all("main only");
    let main = repo.git(&["rev-parse", "HEAD"]).trim().to_string();

    let harness = Harness::open(repo);
    assert_eq!(harness.session.history_scope(), HistoryScope::All);

    let loaded_ids = |snapshot: &RepoSnapshot| {
        (0..snapshot.commits.len())
            .map(|row| snapshot.commits.get(row).expect("row").0.id.to_string())
            .collect::<Vec<_>>()
    };
    let all = harness.wait_for_complete(3);
    let mut all_ids = loaded_ids(&all);
    all_ids.sort();
    let mut expected = vec![base.clone(), feature.clone(), main.clone()];
    expected.sort();
    assert_eq!(all_ids, expected, "All walks every branch");

    harness
        .session
        .set_history_scope(HistoryScope::Refs(vec!["refs/heads/feature".into()]));
    let scoped = harness.wait_until(TIMEOUT, |snapshot| {
        snapshot.commits.complete && snapshot.commits.len() == 2
    });
    let mut scoped_ids = loaded_ids(&scoped);
    scoped_ids.sort();
    let mut expected = vec![base.clone(), feature.clone()];
    expected.sort();
    assert_eq!(
        scoped_ids, expected,
        "the scope excludes the other branch's commit"
    );

    // A commit id is a valid starting point as well. Waiting on a newer
    // generation separates this load from the still-published scoped one.
    let before = harness.session.snapshot().generation;
    harness
        .session
        .set_history_scope(HistoryScope::Refs(vec![main.clone()]));
    let by_id = harness.wait_until(TIMEOUT, |snapshot| {
        snapshot.generation > before && snapshot.commits.complete && snapshot.commits.len() == 2
    });
    assert!(loaded_ids(&by_id).contains(&main));
    assert!(!loaded_ids(&by_id).contains(&feature));

    harness.session.set_history_scope(HistoryScope::All);
    let restored = harness.wait_for_complete(3);
    let mut restored_ids = loaded_ids(&restored);
    restored_ids.sort();
    let mut expected = vec![base, feature, main];
    expected.sort();
    assert_eq!(restored_ids, expected, "All restores the full history");
}

#[test]
fn request_blame_serves_large_file_through_git_cli() {
    let repo = TestRepo::new();
    let mut body = String::new();
    for i in 0..2_500 {
        body.push_str(&format!("line {i:04}: padding for large blame file\n"));
    }
    repo.write("big.txt", &body);
    repo.commit_all("initial");
    let first: strata_core::Oid = repo.git(&["rev-parse", "HEAD"]).trim().parse().unwrap();

    let updated = body.replacen("line 0005:", "line 0005 changed:", 1);
    repo.write("big.txt", &updated);
    repo.commit_all("edit");
    let second: strata_core::Oid = repo.git(&["rev-parse", "HEAD"]).trim().parse().unwrap();

    let harness = Harness::open(repo);
    let ticket = harness.session.request_blame(second, "big.txt");

    let deadline = Instant::now() + TIMEOUT;
    let mut saw_blame_command = false;
    let blame = loop {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for BlameReady");
        match harness.events.recv_timeout(deadline - now) {
            Ok(SessionEvent::BlameReady { ticket: got, blame }) if got == ticket => break blame,
            Ok(SessionEvent::QueryFailed { ticket: got, error }) if got == ticket => {
                panic!("blame failed: {error}");
            }
            Ok(SessionEvent::CommandLogged(record))
                if record.args.first().map(String::as_str) == Some("blame") =>
            {
                saw_blame_command = true;
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("session event channel closed"),
        }
    };

    assert!(saw_blame_command, "large file blame should use git CLI");
    assert_eq!(blame.lines.len(), 2_500);
    assert_eq!(blame.hunks.len(), 3);
    assert_eq!(blame.hunks[0].commit, first);
    assert_eq!(blame.hunks[1].commit, second);
    assert_eq!(blame.hunks[1].start, 6);
    assert_eq!(blame.hunks[2].commit, first);
}

#[cfg(unix)]
#[test]
fn cancelling_session_blame_kills_in_flight_git_process() {
    use std::os::unix::fs::PermissionsExt;

    let repo = TestRepo::new();
    let large = "x".repeat(70 * 1024);
    repo.write("big.txt", &format!("{large}\n"));
    repo.commit_all("big");
    let head: strata_core::Oid = repo.git(&["rev-parse", "HEAD"]).trim().parse().unwrap();

    let pid_file = repo.path().join("blame.pid");
    let script = repo.path().join("mock-git");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\necho $$ > \"{}\"\nexec sleep 60\n",
            pid_file.display()
        ),
    )
    .expect("write mock git");
    let mut perms = fs::metadata(&script).expect("meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("chmod");

    let harness = Harness::open_with(
        repo,
        SessionOptions {
            git_executable: Some(script),
        },
    );
    let ticket = harness.session.request_blame(head, "big.txt");

    let start = Instant::now();
    while !pid_file.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "mock git never started"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid: u32 = fs::read_to_string(&pid_file)
        .expect("read pid")
        .trim()
        .parse()
        .expect("pid");

    harness.session.cancel_query(ticket);

    // Wait for the CommandLogged event that records the killed child process,
    // which proves the blame worker already exited and reaped it.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let now = Instant::now();
        assert!(
            now < deadline,
            "timed out waiting for cancelled blame to stop"
        );
        match harness.events.recv_timeout(deadline - now) {
            Ok(SessionEvent::BlameReady { ticket: got, .. })
            | Ok(SessionEvent::QueryFailed { ticket: got, .. })
                if got == ticket =>
            {
                panic!("cancelled blame ticket must not emit BlameReady or QueryFailed");
            }
            Ok(SessionEvent::CommandLogged(record))
                if record.args.first().map(String::as_str) == Some("blame") =>
            {
                assert_eq!(record.exit_code, None);
                break;
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("session event channel closed"),
        }
    }

    let alive = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .expect("kill -0")
        .success();
    assert!(!alive, "git child {pid} should have been killed");
}

#[test]
fn request_blame_streams_progress_before_ready_on_large_history() {
    use std::collections::{HashMap, HashSet};

    // Build 600 commits (> GIX_MAX_DEPTH = 256) each adding one unique line so
    // `git blame --incremental` emits 600 hunks (> PROGRESS_BATCH_HUNKS = 500)
    // and is guaranteed to emit at least one BlameProgress with hunks before
    // BlameReady.
    let repo = TestRepo::new();
    let commit_count = 600usize;
    let mut stream = String::new();
    let mut file_body = String::new();
    for index in 0..commit_count {
        file_body.push_str(&format!("line {index:04}\n"));
        let msg = format!("commit {index}");
        stream.push_str(&format!(
            "blob\nmark :{mark}\ndata {len}\n{file_body}",
            mark = index + 1,
            len = file_body.len()
        ));
        stream.push_str("commit refs/heads/main\n");
        stream.push_str(&format!(
            "committer Strata Test <strata@example.com> {} +0000\n",
            1_700_000_000 + index
        ));
        stream.push_str(&format!("data {}\n{msg}\n", msg.len()));
        stream.push_str(&format!(
            "M 100644 :{mark} history.txt\n\n",
            mark = index + 1
        ));
    }
    repo.git_with_input(&["fast-import", "--quiet"], &stream);
    repo.git(&["reset", "--hard", "HEAD"]);
    let head: strata_core::Oid = repo.git(&["rev-parse", "HEAD"]).trim().parse().unwrap();

    // Also add a tiny shallow file in a second repo? No, let's check shallow file
    // via a separate small repo too:
    let shallow_repo = TestRepo::new();
    shallow_repo.write("small.txt", "one\ntwo\n");
    shallow_repo.commit_all("small");
    let shallow_head: strata_core::Oid = shallow_repo
        .git(&["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    let shallow_harness = Harness::open(shallow_repo);
    let shallow_ticket = shallow_harness
        .session
        .request_blame(shallow_head, "small.txt");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for shallow BlameReady");
        match shallow_harness.events.recv_timeout(deadline - now) {
            Ok(SessionEvent::BlameProgress { ticket: got, .. }) if got == shallow_ticket => {
                panic!("gix shallow blame path should emit only BlameReady");
            }
            Ok(SessionEvent::BlameReady { ticket: got, blame }) if got == shallow_ticket => {
                assert_eq!(blame.lines, ["one", "two"]);
                break;
            }
            Ok(SessionEvent::QueryFailed { ticket: got, error }) if got == shallow_ticket => {
                panic!("shallow blame failed: {error}");
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("session event channel closed"),
        }
    }

    let harness = Harness::open(repo);
    let ticket = harness.session.request_blame(head, "history.txt");

    let deadline = Instant::now() + TIMEOUT;
    let mut saw_lines = false;
    let mut progress_hunk_events = 0usize;
    let mut progress_hunks = Vec::new();
    let mut seen_commits = HashSet::new();

    let final_blame = loop {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for BlameReady");
        match harness.events.recv_timeout(deadline - now) {
            Ok(SessionEvent::BlameProgress {
                ticket: got,
                lines,
                hunks,
                commits,
            }) if got == ticket => {
                if let Some(lines) = lines {
                    assert!(!saw_lines, "lines should be emitted once");
                    assert_eq!(lines.len(), commit_count);
                    saw_lines = true;
                }
                for oid in commits.keys() {
                    assert!(
                        seen_commits.insert(*oid),
                        "BlameProgress should only emit newly seen commit metadata"
                    );
                }
                if !hunks.is_empty() {
                    progress_hunk_events += 1;
                    for hunk in &hunks {
                        assert!(
                            seen_commits.contains(&hunk.commit),
                            "commit metadata for hunk must arrive with or before the hunk"
                        );
                    }
                    progress_hunks.extend(hunks);
                }
            }
            Ok(SessionEvent::BlameReady { ticket: got, blame }) if got == ticket => break blame,
            Ok(SessionEvent::QueryFailed { ticket: got, error }) if got == ticket => {
                panic!("blame failed: {error}");
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("session event channel closed"),
        }
    };

    assert!(saw_lines, "expected early file lines in BlameProgress");
    assert!(
        progress_hunk_events >= 1,
        "expected at least one BlameProgress with hunks before BlameReady"
    );

    // Every line attributed in progress events must match the final Blame.
    let mut final_by_line = HashMap::new();
    for hunk in &final_blame.hunks {
        for offset in 0..hunk.len {
            final_by_line.insert(
                hunk.start + offset,
                (
                    hunk.commit,
                    hunk.orig_path.as_str(),
                    hunk.orig_start + offset,
                ),
            );
        }
    }
    assert_eq!(final_by_line.len(), commit_count);
    for hunk in &progress_hunks {
        for offset in 0..hunk.len {
            let line = hunk.start + offset;
            let expected = final_by_line
                .get(&line)
                .expect("progress hunk line exists in final blame");
            assert_eq!(
                (
                    hunk.commit,
                    hunk.orig_path.as_str(),
                    hunk.orig_start + offset
                ),
                *expected,
                "progress hunk line {line} must match final blame"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn cancelling_mid_stream_blame_stops_events() {
    use std::os::unix::fs::PermissionsExt;

    let repo = TestRepo::new();
    let mut body = String::new();
    for i in 0..600 {
        body.push_str(&format!(
            "line {i:04}: padding to exceed 64 KiB threshold {:080}\n",
            i
        ));
    }
    repo.write("big.txt", &body);
    repo.commit_all("big");
    let head: strata_core::Oid = repo.git(&["rev-parse", "HEAD"]).trim().parse().unwrap();

    let pid_file = repo.path().join("midstream.pid");
    let script = repo.path().join("mock-git-stream");
    // Emit 500 valid incremental hunks (triggering one BlameProgress batch),
    // write PID, then sleep so cancellation happens mid-stream.
    let mut script_body = String::from("#!/bin/sh\n");
    script_body.push_str(&format!("echo $$ > \"{}\"\n", pid_file.display()));
    script_body.push_str(&format!(
        "cat <<'EOF'\n{head} 1 1 1\nauthor Strata Test\nauthor-mail <strata@example.com>\nauthor-time 1700000000\nauthor-tz +0000\ncommitter Strata Test\ncommitter-mail <strata@example.com>\ncommitter-time 1700000000\ncommitter-tz +0000\nsummary big\nfilename big.txt\n"
    ));
    for line in 2..=500 {
        script_body.push_str(&format!("{head} {line} {line} 1\nfilename big.txt\n"));
    }
    script_body.push_str("EOF\nexec sleep 60\n");

    fs::write(&script, script_body).expect("write mock git");
    let mut perms = fs::metadata(&script).expect("meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("chmod");

    let harness = Harness::open_with(
        repo,
        SessionOptions {
            git_executable: Some(script),
        },
    );
    let ticket = harness.session.request_blame(head, "big.txt");

    // Wait for both early lines and the first hunk batch before cancelling.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_lines = false;
    let mut saw_hunks = false;
    while !(saw_lines && saw_hunks) {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for mid-stream progress");
        match harness.events.recv_timeout(deadline - now) {
            Ok(SessionEvent::BlameProgress {
                ticket: got,
                lines,
                hunks,
                ..
            }) if got == ticket => {
                if lines.is_some() {
                    saw_lines = true;
                }
                if !hunks.is_empty() {
                    saw_hunks = true;
                }
            }
            Ok(SessionEvent::BlameReady { ticket: got, .. })
            | Ok(SessionEvent::QueryFailed { ticket: got, .. })
                if got == ticket =>
            {
                panic!("mock git should still be sleeping mid-stream");
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("session event channel closed"),
        }
    }

    harness.session.cancel_query(ticket);

    // Wait for the killed git command to be logged, then confirm no further
    // blame events arrive for this ticket.
    let stop_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let now = Instant::now();
        assert!(
            now < stop_deadline,
            "timed out waiting for cancelled blame to stop"
        );
        match harness.events.recv_timeout(stop_deadline - now) {
            Ok(SessionEvent::BlameProgress { ticket: got, .. })
            | Ok(SessionEvent::BlameReady { ticket: got, .. })
            | Ok(SessionEvent::QueryFailed { ticket: got, .. })
                if got == ticket =>
            {
                panic!("cancelled blame ticket must stop emitting events");
            }
            Ok(SessionEvent::CommandLogged(record))
                if record.args.first().map(String::as_str) == Some("blame") =>
            {
                assert_eq!(record.exit_code, None);
                break;
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("session event channel closed"),
        }
    }

    let settle = Instant::now() + Duration::from_millis(150);
    while Instant::now() < settle {
        match harness
            .events
            .recv_timeout(settle.saturating_duration_since(Instant::now()))
        {
            Ok(SessionEvent::BlameProgress { ticket: got, .. })
            | Ok(SessionEvent::BlameReady { ticket: got, .. })
            | Ok(SessionEvent::QueryFailed { ticket: got, .. })
                if got == ticket =>
            {
                panic!("no blame events should arrive after cancellation");
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    let pid: u32 = fs::read_to_string(&pid_file)
        .expect("read pid")
        .trim()
        .parse()
        .expect("pid");
    let alive = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .expect("kill -0")
        .success();
    assert!(!alive, "git child {pid} should have been killed");
}

#[test]
#[ignore = "benchmarks streaming Session::request_blame on STRATA_BENCH_REPO"]
fn bench_streaming_blame_on_repo() {
    let repo_path = std::env::var("STRATA_BENCH_REPO")
        .expect("set STRATA_BENCH_REPO to a repository path to benchmark");
    let file_path = std::env::var("STRATA_BENCH_PATH").unwrap_or_else(|_| "MAINTAINERS".into());

    let repo = Repo::open(&repo_path).expect("open benchmark repo");
    let head = match repo.head().expect("head") {
        HeadInfo::Branch { target, .. } => target.expect("head target"),
        HeadInfo::Detached { target } => target,
    };

    let (tx, rx) = mpsc::channel();
    let session = Session::open(&repo_path, move |event| {
        let _ = tx.send(event);
    })
    .expect("open session");

    let start = Instant::now();
    let ticket = session.request_blame(head, file_path.clone());
    let mut first_lines_at = None;
    let mut first_hunks_at = None;
    let mut progress_batches = 0usize;
    let mut total_lines = 0usize;

    let deadline = Instant::now() + Duration::from_secs(120);
    let final_blame = loop {
        let now = Instant::now();
        assert!(now < deadline, "timed out benchmarking streaming blame");
        match rx.recv_timeout(deadline - now) {
            Ok(SessionEvent::BlameProgress {
                ticket: got,
                lines,
                hunks,
                ..
            }) if got == ticket => {
                if let Some(lines) = lines {
                    first_lines_at.get_or_insert_with(|| start.elapsed());
                    total_lines = lines.len();
                }
                if !hunks.is_empty() {
                    first_hunks_at.get_or_insert_with(|| start.elapsed());
                    progress_batches += 1;
                }
            }
            Ok(SessionEvent::BlameReady { ticket: got, blame }) if got == ticket => break blame,
            Ok(SessionEvent::QueryFailed { ticket: got, error }) if got == ticket => {
                panic!("benchmark blame failed: {error}");
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("session channel closed"),
        }
    };
    let completed_at = start.elapsed();

    eprintln!(
        "streaming blame({file_path}): lines={} hunks={} commits={} batches={progress_batches} first_lines={first_lines_at:?} first_hunks={first_hunks_at:?} completion={completed_at:?}",
        total_lines.max(final_blame.lines.len()),
        final_blame.hunks.len(),
        final_blame.commits.len(),
    );
}
