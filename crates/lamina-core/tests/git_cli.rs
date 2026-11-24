//! Integration tests for the `git` CLI write layer.
//!
//! Every repository lives in a `TempDir` and runs with an empty global/system
//! config plus a fixed identity, so the machine's git settings cannot leak in.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use lamina_core::Error;
use lamina_core::git_cli::{
    ApplyMode, CommandLog, CommitOptions, Git, MAX_CAPTURE_BYTES, MIN_GIT_VERSION,
};
use lamina_core::test_support::TestRepo;

/// A fixture root with an isolated environment. A test's repositories live
/// beside the fixture's own worktree so a clone and a bare remote each get a
/// directory of their own.
struct Fixture {
    fixture: TestRepo,
}

impl Fixture {
    fn new() -> Self {
        Self {
            // The commit test asserts the identity git records.
            fixture: TestRepo::with_identity("Test Author", "test@example.com"),
        }
    }

    fn path(&self) -> &Path {
        self.fixture.path()
    }

    fn git_in(&self, workdir: impl AsRef<Path>) -> Git {
        self.fixture.git_handle_in(workdir)
    }

    /// A fresh repository on branch `main` with no commits (unborn HEAD).
    fn repo(&self) -> Git {
        let dir = self.fixture.scratch_dir("work");
        let git = self.git_in(&dir);
        git.run(&["init", "-b", "main"], None).expect("git init");
        git
    }

    fn bare_remote(&self) -> (Git, PathBuf) {
        let dir = self.fixture.scratch_dir("remote.git");
        let git = self.git_in(&dir);
        git.run(&["init", "--bare", "-b", "main", "."], None)
            .expect("git init --bare");
        (git, dir)
    }

    fn clone_from(&self, remote: &Path, name: &str) -> Git {
        let dir = self.fixture.scratch_dir(name);
        let git = self.git_in(&dir);
        git.run(&["clone", remote.to_str().unwrap(), "."], None)
            .expect("git clone");
        git
    }
}

fn write_file(git: &Git, name: &str, contents: &str) {
    let path = git.workdir().join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent dir");
    }
    fs::write(path, contents).expect("write file");
}

fn read_file(git: &Git, name: &str) -> String {
    fs::read_to_string(git.workdir().join(name)).expect("read file")
}

fn stdout(git: &Git, args: &[&str]) -> String {
    String::from_utf8(git.run(args, None).expect("git command").stdout).expect("utf-8 output")
}

fn status(git: &Git) -> String {
    stdout(git, &["status", "--porcelain"])
}

fn head_oid(git: &Git) -> String {
    stdout(git, &["rev-parse", "HEAD"]).trim().to_owned()
}

fn subject(git: &Git) -> String {
    stdout(git, &["log", "-1", "--format=%s"]).trim().to_owned()
}

fn commit_file(git: &Git, name: &str, contents: &str, message: &str) {
    write_file(git, name, contents);
    git.stage_paths(&[name]).expect("stage");
    git.commit(message, CommitOptions::default())
        .expect("commit");
}

#[test]
fn reports_and_checks_git_version() {
    let fixture = Fixture::new();
    let git = fixture.repo();

    let version = git.version().expect("git version");
    assert!(
        version >= MIN_GIT_VERSION,
        "tests need git {MIN_GIT_VERSION:?}, found {version:?}"
    );
    git.ensure_supported_version().expect("supported git");
}

#[test]
fn stage_unstage_and_discard_paths() {
    let fixture = Fixture::new();
    let git = fixture.repo();
    commit_file(&git, "tracked.txt", "one\n", "initial");

    write_file(&git, "tracked.txt", "two\n");
    write_file(&git, "nested/new.txt", "new\n");
    git.stage_paths(&["tracked.txt", "nested/new.txt"])
        .expect("stage");
    let staged = status(&git);
    assert!(staged.contains("M  tracked.txt"), "{staged}");
    assert!(staged.contains("A  nested/new.txt"), "{staged}");

    git.unstage_paths(&["tracked.txt"]).expect("unstage");
    let unstaged = status(&git);
    assert!(unstaged.contains(" M tracked.txt"), "{unstaged}");
    assert!(unstaged.contains("A  nested/new.txt"), "{unstaged}");

    git.discard_paths(&["tracked.txt"]).expect("discard");
    assert_eq!(read_file(&git, "tracked.txt"), "one\n");

    git.unstage_paths(&["nested/new.txt"]).expect("unstage");
    git.delete_untracked(&["nested"]).expect("delete untracked");
    assert!(!git.workdir().join("nested").exists());
    assert!(status(&git).trim().is_empty(), "{}", status(&git));
}

#[test]
fn unstage_works_before_the_first_commit() {
    let fixture = Fixture::new();
    let git = fixture.repo();

    write_file(&git, "first.txt", "one\n");
    git.stage_paths(&["first.txt"]).expect("stage");
    assert!(status(&git).contains("A  first.txt"));

    // `git restore --staged` cannot resolve HEAD here, so this exercises the
    // `git rm --cached` fallback.
    git.unstage_paths(&["first.txt"]).expect("unstage unborn");
    assert_eq!(status(&git).trim(), "?? first.txt");
}

#[test]
fn long_path_lists_use_pathspec_files() {
    let fixture = Fixture::new();
    let git = fixture.repo();
    commit_file(&git, "keep.txt", "keep\n", "initial");

    let mut names = Vec::new();
    for index in 0..120 {
        let name = format!("files/{index:03}.txt");
        write_file(&git, &name, "x\n");
        names.push(name);
    }
    git.stage_paths(&names).expect("stage many");
    let staged = stdout(&git, &["diff", "--cached", "--name-only"]);
    assert_eq!(staged.lines().count(), 120);

    git.unstage_paths(&names).expect("unstage many");
    assert!(
        stdout(&git, &["diff", "--cached", "--name-only"])
            .trim()
            .is_empty()
    );

    // `git clean` has no pathspec-from-file, so this also covers the chunking
    // path used for long untracked lists.
    let untracked: Vec<String> = (0..120).map(|index| format!("u{index:03}.txt")).collect();
    for name in &untracked {
        write_file(&git, name, "");
    }
    git.delete_untracked(&untracked).expect("clean many");
    for name in &untracked {
        assert!(!git.workdir().join(name).exists(), "{name} survived");
    }
}

#[test]
fn apply_patch_stages_unstages_and_discards() {
    let fixture = Fixture::new();
    let git = fixture.repo();
    commit_file(&git, "file.txt", "one\n", "initial");

    write_file(&git, "file.txt", "two\n");
    let patch = stdout(&git, &["diff", "--", "file.txt"]);
    assert!(patch.contains("-one"), "{patch}");

    git.apply_patch(&patch, ApplyMode::Stage)
        .expect("stage patch");
    assert!(status(&git).contains("M  file.txt"), "{}", status(&git));
    assert_eq!(read_file(&git, "file.txt"), "two\n");

    git.apply_patch(&patch, ApplyMode::Unstage)
        .expect("unstage patch");
    assert!(status(&git).contains(" M file.txt"), "{}", status(&git));

    git.apply_patch(&patch, ApplyMode::Discard)
        .expect("discard patch");
    assert_eq!(read_file(&git, "file.txt"), "one\n");
    assert!(status(&git).trim().is_empty(), "{}", status(&git));
}

#[test]
fn commit_supports_multiline_messages_and_options() {
    let fixture = Fixture::new();
    let git = fixture.repo();
    commit_file(&git, "a.txt", "one\n", "initial");

    write_file(&git, "a.txt", "two\n");
    git.stage_paths(&["a.txt"]).expect("stage");
    let message =
        "Subject with \"double\" and 'single' quotes\n\nBody: it's a test; \"quoted\" again.\n";
    git.commit(message, CommitOptions::default())
        .expect("commit");
    assert_eq!(
        stdout(&git, &["log", "-1", "--format=%B"]).trim_end(),
        message.trim_end()
    );
    assert_eq!(subject(&git), "Subject with \"double\" and 'single' quotes");
    assert_eq!(
        stdout(&git, &["log", "-1", "--format=%an"]).trim(),
        "Test Author"
    );

    git.commit(
        "amended subject\n\namended body\n",
        CommitOptions {
            amend: true,
            ..CommitOptions::default()
        },
    )
    .expect("amend");
    assert_eq!(subject(&git), "amended subject");
    assert_eq!(stdout(&git, &["rev-list", "--count", "HEAD"]).trim(), "2");

    let error = git
        .commit("nothing to do\n", CommitOptions::default())
        .expect_err("empty commit without --allow-empty must fail");
    assert!(matches!(error, Error::GitCommand { .. }));

    git.commit(
        "empty commit\n",
        CommitOptions {
            allow_empty: true,
            ..CommitOptions::default()
        },
    )
    .expect("allow empty");
    assert_eq!(subject(&git), "empty commit");

    git.commit(
        "signed off\n",
        CommitOptions {
            allow_empty: true,
            amend: true,
            signoff: true,
        },
    )
    .expect("signoff");
    let body = stdout(&git, &["log", "-1", "--format=%B"]);
    assert!(
        body.contains("Signed-off-by: Test Author <test@example.com>"),
        "{body}"
    );
}

#[test]
fn branch_lifecycle() {
    let fixture = Fixture::new();
    let git = fixture.repo();
    commit_file(&git, "a.txt", "one\n", "initial");

    git.create_branch("feature", Some("main"), false)
        .expect("create branch");
    assert_eq!(
        stdout(&git, &["rev-parse", "refs/heads/feature"]).trim(),
        head_oid(&git)
    );

    git.checkout_branch("feature").expect("checkout feature");
    assert_eq!(
        stdout(&git, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "feature"
    );

    git.create_branch("topic", None, true)
        .expect("create + checkout");
    assert_eq!(
        stdout(&git, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "topic"
    );
    write_file(&git, "topic.txt", "work\n");
    git.stage_paths(&["topic.txt"]).expect("stage");
    git.commit("topic work\n", CommitOptions::default())
        .expect("commit topic");

    git.checkout_branch("main").expect("back to main");
    let error = git
        .delete_branch("topic", false)
        .expect_err("unmerged branch needs -D");
    assert!(matches!(error, Error::GitCommand { .. }));
    git.delete_branch("topic", true).expect("force delete");
    assert!(
        git.run(&["rev-parse", "--verify", "refs/heads/topic"], None)
            .is_err()
    );

    git.delete_branch("feature", false).expect("delete merged");
}

#[test]
fn stash_round_trip() {
    let fixture = Fixture::new();
    let git = fixture.repo();
    commit_file(&git, "a.txt", "one\n", "initial");

    write_file(&git, "a.txt", "modified\n");
    let message = "notes: \"quoted\" and a dash - here";
    git.stash_push(Some(message), false).expect("stash");
    assert_eq!(read_file(&git, "a.txt"), "one\n");

    let entries = git.stash_list().expect("stash list");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].index, 0);
    assert!(entries[0].message.contains(message), "{:?}", entries[0]);
    assert_eq!(entries[0].oid.to_string().len(), 40);

    git.stash_pop(0).expect("pop");
    assert_eq!(read_file(&git, "a.txt"), "modified\n");
    assert!(git.stash_list().expect("list").is_empty());

    write_file(&git, "untracked.txt", "new\n");
    git.stash_push(None, true).expect("stash untracked");
    assert!(!git.workdir().join("untracked.txt").exists());
    assert_eq!(read_file(&git, "a.txt"), "one\n");

    git.stash_apply(0).expect("apply untracked");
    assert_eq!(read_file(&git, "untracked.txt"), "new\n");
    assert_eq!(read_file(&git, "a.txt"), "modified\n");
    assert_eq!(git.stash_list().expect("list").len(), 1);
    git.stash_drop(0).expect("drop");
    assert!(git.stash_list().expect("list").is_empty());
}

#[test]
fn push_fetch_pull_against_local_remote() {
    let fixture = Fixture::new();
    let git = fixture.repo();
    commit_file(&git, "a.txt", "one\n", "initial");

    let (remote_git, remote_path) = fixture.bare_remote();
    git.run(
        &["remote", "add", "origin", remote_path.to_str().unwrap()],
        None,
    )
    .expect("add remote");
    git.push(Some("origin"), Some("main"), true, false)
        .expect("first push");
    assert_eq!(
        stdout(&remote_git, &["rev-parse", "main"]).trim(),
        head_oid(&git)
    );

    // Pushing without explicit remote/branch uses the configured upstream.
    git.push(None, None, false, false).expect("push upstream");
    let error = git
        .push(None, Some("main"), false, false)
        .expect_err("branch without remote is ambiguous");
    assert!(matches!(error, Error::Other(_)));

    let clone = fixture.clone_from(&remote_path, "clone");
    assert_eq!(read_file(&clone, "a.txt"), "one\n");

    commit_file(&git, "a.txt", "two\n", "second");
    git.push(Some("origin"), Some("main"), false, false)
        .expect("second push");
    clone.fetch(Some("origin"), true).expect("fetch one remote");
    assert_eq!(
        stdout(&clone, &["rev-parse", "origin/main"]).trim(),
        head_oid(&git)
    );
    clone.fetch(None, false).expect("fetch all remotes");
    clone.pull(None).expect("fast-forward pull");
    assert_eq!(read_file(&clone, "a.txt"), "two\n");

    // Diverged history: --rebase replays the local commit on top.
    commit_file(&clone, "local.txt", "local\n", "local change");
    commit_file(&git, "a.txt", "remote\n", "remote change");
    git.push(Some("origin"), Some("main"), false, false)
        .expect("third push");
    clone.pull(Some(true)).expect("rebase pull");
    // Rebasing replays the local commit on top of the fetched upstream.
    let subjects = stdout(&clone, &["log", "-2", "--format=%s"]);
    assert_eq!(
        subjects.lines().collect::<Vec<_>>(),
        vec!["local change", "remote change"]
    );
    assert_eq!(read_file(&git, "a.txt"), "remote\n");
    assert_eq!(read_file(&clone, "local.txt"), "local\n");

    // --no-rebase creates a merge commit.
    commit_file(&clone, "local2.txt", "local again\n", "local again");
    commit_file(&git, "a.txt", "remote again\n", "remote again");
    git.push(Some("origin"), Some("main"), false, false)
        .expect("fourth push");
    clone.pull(Some(false)).expect("merge pull");
    let parents = stdout(&clone, &["log", "-1", "--format=%p"]);
    assert_eq!(parents.split_whitespace().count(), 2, "{parents}");

    // A rewritten history is rejected by a normal push but accepted with lease.
    git.commit(
        "rewritten\n",
        CommitOptions {
            amend: true,
            ..CommitOptions::default()
        },
    )
    .expect("amend");
    let error = git
        .push(Some("origin"), Some("main"), false, false)
        .expect_err("non-fast-forward without force");
    assert!(matches!(error, Error::GitCommand { .. }));
    git.push(Some("origin"), Some("main"), false, true)
        .expect("force with lease");
    assert_eq!(
        stdout(&remote_git, &["rev-parse", "main"]).trim(),
        head_oid(&git)
    );

    // Prune drops remote-tracking refs for branches deleted on the remote.
    git.create_branch("to-delete", None, false)
        .expect("create branch");
    git.push(Some("origin"), Some("to-delete"), true, false)
        .expect("push branch");
    git.run(&["push", "origin", "--delete", "to-delete"], None)
        .expect("delete remote branch");
    clone.fetch(Some("origin"), true).expect("pruning fetch");
    assert!(
        clone
            .run(
                &["rev-parse", "--verify", "refs/remotes/origin/to-delete"],
                None
            )
            .is_err()
    );
}

#[test]
fn failures_surface_git_command_errors() {
    let fixture = Fixture::new();
    let git = fixture.repo();
    commit_file(&git, "a.txt", "one\n", "initial");

    let error = git
        .create_branch("bad..name", None, false)
        .expect_err("invalid branch name");
    match error {
        Error::GitCommand { args, code, stderr } => {
            assert!(args.contains("branch"), "{args}");
            assert!(args.contains("bad..name"), "{args}");
            assert_eq!(code, Some(128));
            assert!(stderr.contains("not a valid branch name"), "{stderr}");
        }
        other => panic!("expected GitCommand, got {other:?}"),
    }

    let error = git
        .checkout_branch("missing-branch")
        .expect_err("missing branch");
    assert!(matches!(error, Error::GitCommand { .. }));

    // Names that would be parsed as options never reach git.
    let error = git
        .checkout_branch("--orphan")
        .expect_err("option-like name");
    assert!(matches!(error, Error::Other(_)));
}

#[test]
fn command_log_records_entries_and_notifies_subscriber() {
    let fixture = Fixture::new();
    let git = fixture.repo();
    git.clear_log();

    let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    git.set_log_subscriber(move |record| {
        sink.lock().expect("seen lock").push(record.args.clone());
    });

    git.version().expect("version");
    let log = git.log();
    assert_eq!(log.len(), 1);
    let record = &log[0];
    assert_eq!(record.args, vec!["--version".to_owned()]);
    assert_eq!(record.exit_code, Some(0));
    assert!(record.stdout.contains("git version"), "{}", record.stdout);
    assert!(record.stderr.is_empty());
    assert!(record.duration < Duration::from_secs(120));
    assert!(record.started_at <= SystemTime::now());
    assert_eq!(seen.lock().expect("seen lock").len(), 1);

    let _ = git.create_branch("bad..name", None, false);
    let log = git.log();
    let last = log.last().expect("log entry");
    assert_eq!(last.exit_code, Some(128));
    assert!(
        last.stderr.contains("not a valid branch name"),
        "{}",
        last.stderr
    );

    // Large output is truncated in the log but not in the returned Output.
    let big = "x".repeat(100 * 1024);
    commit_file(&git, "big.txt", &big, "big");
    let shown = git
        .run(&["show", "HEAD:big.txt"], None)
        .expect("show big blob");
    assert!(shown.stdout.len() > MAX_CAPTURE_BYTES);
    let last = git.log().pop().expect("log entry");
    assert_eq!(
        last.args,
        vec!["show".to_owned(), "HEAD:big.txt".to_owned()]
    );
    assert_eq!(last.stdout.len(), MAX_CAPTURE_BYTES);
}

#[test]
fn log_can_be_shared_between_handles() {
    let fixture = Fixture::new();
    let git = fixture.repo();
    git.clear_log();
    let log = Arc::new(CommandLog::new());

    let shared = git.clone().with_log(Arc::clone(&log));
    shared
        .run(&["status", "--porcelain"], None)
        .expect("status");
    assert_eq!(log.len(), 1);
    assert_eq!(log.snapshot()[0].args, vec!["status", "--porcelain"]);
    assert!(git.log().is_empty(), "original handle keeps its own log");
}

#[test]
fn missing_git_binary_is_reported_and_logged() {
    let fixture = Fixture::new();
    let git = fixture.repo();

    let broken =
        Git::new(git.workdir().to_path_buf()).with_git_binary(fixture.path().join("no-such-git"));
    let error = broken
        .run(&["--version"], None)
        .expect_err("missing binary");
    assert!(matches!(error, Error::Io(_)));

    let log = broken.log();
    let last = log.last().expect("spawn failure logged");
    assert_eq!(last.args, vec!["--version".to_owned()]);
    assert_eq!(last.exit_code, None);
    assert!(!last.stderr.is_empty());
}

#[test]
fn run_streaming_reports_progress_lines() {
    let fixture = Fixture::new();
    let git = fixture.repo();
    commit_file(&git, "a.txt", "one\n", "initial");
    let (_, remote_path) = fixture.bare_remote();
    git.run(
        &["remote", "add", "origin", remote_path.to_str().unwrap()],
        None,
    )
    .expect("add remote");

    let mut lines = Vec::new();
    git.run_streaming(
        &["push", "--progress", "--set-upstream", "origin", "main"],
        |line| lines.push(line.text),
    )
    .expect("streaming push");
    assert!(!lines.is_empty(), "push should report progress on stderr");
    assert!(lines.iter().any(|line| line.contains("main")), "{lines:?}");
    assert_eq!(git.log().last().expect("log entry").exit_code, Some(0));

    // stderr is delivered before the failure is returned.
    let mut failure_lines = Vec::new();
    let error = git
        .run_streaming(&["rev-parse", "--verify", "refs/heads/missing"], |line| {
            failure_lines.push(line.text);
        })
        .expect_err("missing ref");
    assert!(matches!(error, Error::GitCommand { .. }));
    assert!(
        failure_lines.iter().any(|line| line.contains("fatal")),
        "{failure_lines:?}"
    );
}
