//! Integration tests for the history operations — cherry-pick, revert, reset,
//! tags, per-region conflict resolution and continue/abort — against real
//! repositories and real git invocations.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use strata_core::git_cli::Git;
use strata_core::repo::Repo;
use strata_core::session::{
    Operation, OperationId, RepoSnapshot, ResetMode, Session, SessionEvent,
};
use strata_core::{ConflictEntry, ConflictKind, HeadInfo, Oid, RefKind, RepoState};
use tempfile::TempDir;

/// Generous: a session test waits for watcher events and history reloads.
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
        // The session runs its git commands without the environment overrides
        // below, so the settings a commit, tag or pick cannot work without are
        // pinned in the repository config.
        repo.git(&["config", "user.name", "Strata Test"]);
        repo.git(&["config", "user.email", "strata@example.com"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo.git(&["config", "tag.gpgsign", "false"]);
        repo.git(&["config", "core.autocrlf", "false"]);
        repo
    }

    fn path(&self) -> &Path {
        self.workdir.path()
    }

    fn config_file(&self) -> PathBuf {
        self.config.path().join("gitconfig")
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

    /// Isolated from the host's git config and hooks.
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
        let output = self.git_output(args);
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn git_output(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("spawn git")
    }

    fn head(&self) -> Oid {
        self.oid("HEAD")
    }

    fn oid(&self, reference: &str) -> Oid {
        self.git(&["rev-parse", reference])
            .trim()
            .parse()
            .expect("parse object id")
    }

    fn commit_all(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }

    /// `git status --porcelain=v2`, the reference the snapshot is checked
    /// against.
    fn status(&self) -> String {
        self.git(&["status", "--porcelain=v2"])
    }

    /// The same repository through the production write path, with the same
    /// isolation so a test never reads the host's git config.
    fn git_handle(&self) -> Git {
        Git::new(self.path().to_path_buf())
            .with_env("GIT_CONFIG_GLOBAL", self.config_file())
            .with_env("GIT_CONFIG_SYSTEM", self.config_file())
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
            .with_env("XDG_CONFIG_HOME", self.config.path())
            .with_env("GIT_AUTHOR_NAME", "Strata Test")
            .with_env("GIT_AUTHOR_EMAIL", "strata@example.com")
            .with_env("GIT_COMMITTER_NAME", "Strata Test")
            .with_env("GIT_COMMITTER_EMAIL", "strata@example.com")
            .with_env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
            .with_env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
    }

    fn open(&self) -> Repo {
        Repo::open(self.path()).expect("open repository")
    }
}

/// `main` and `side` both change `conflict.txt`, so any pick, revert or rebase
/// between them conflicts.
fn conflict_repo() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("conflict.txt", "base\n");
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("conflict.txt", "theirs\n");
    repo.commit_all("theirs");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("conflict.txt", "ours\n");
    repo.commit_all("ours");
    repo
}

fn conflict(path: &str, kind: ConflictKind) -> ConflictEntry {
    ConflictEntry {
        path: path.to_owned(),
        kind,
    }
}

struct Harness {
    // Session first: it owns threads and a watcher that must stop before the
    // temporary repository disappears.
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
    fn wait_until(&self, mut predicate: impl FnMut(&RepoSnapshot) -> bool) -> Arc<RepoSnapshot> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let snapshot = self.session.snapshot();
            if predicate(&snapshot) {
                return snapshot;
            }
            let now = Instant::now();
            assert!(
                now < deadline,
                "timed out waiting for a snapshot (state {:?}, conflicts {:?}, {} commits, complete {}, status {:?})",
                snapshot.state,
                snapshot.conflicts,
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

    fn wait_for_complete(&self, commits: usize) -> Arc<RepoSnapshot> {
        self.wait_until(|snapshot| snapshot.commits.complete && snapshot.commits.len() == commits)
    }

    /// Waits for the operation and returns git's message on failure.
    fn wait_for_operation(&self, id: OperationId) -> Option<String> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let now = Instant::now();
            assert!(now < deadline, "timed out waiting for operation {id:?}");
            match self.events.recv_timeout(deadline - now) {
                Ok(SessionEvent::OperationFinished {
                    id: finished,
                    error,
                }) if finished == id => return error,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => panic!("session event channel closed"),
            }
        }
    }

    fn run(&self, operation: Operation) -> Option<String> {
        let id = self.session.run(operation);
        self.wait_for_operation(id)
    }
}

#[test]
fn cherry_pick_of_a_clean_commit_lands_on_the_current_branch() {
    let repo = TestRepo::new();
    repo.write("a.txt", "base\n");
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("b.txt", "side\n");
    repo.commit_all("side");
    let picked = repo.head();
    repo.git(&["checkout", "-q", "main"]);
    let before = repo.head();

    let harness = Harness::open(repo);
    // `base` and the `side` commit, across both refs.
    harness.wait_for_complete(2);

    assert_eq!(
        harness.run(Operation::CherryPick { commit: picked }),
        None,
        "a clean pick must succeed"
    );

    let done = harness.wait_until(|snapshot| {
        snapshot.commits.complete
            && snapshot.commits.len() == 3
            && snapshot.state == RepoState::Normal
    });
    let head = harness.repo.head();
    assert_ne!(head, before, "the branch must move");
    assert_ne!(head, picked, "a pick records a new commit");
    assert_eq!(
        harness.repo.git(&["log", "-1", "--format=%s"]).trim(),
        "side"
    );
    assert_eq!(harness.repo.git(&["show", "HEAD:b.txt"]).trim(), "side");
    assert_eq!(
        harness.repo.git(&["rev-list", "--count", "HEAD"]).trim(),
        "2"
    );
    assert_eq!(harness.repo.status(), "");
    assert_eq!(
        done.head,
        Some(HeadInfo::Branch {
            short_name: "main".into(),
            target: Some(head),
        })
    );
}

#[test]
fn cherry_pick_conflict_is_not_an_error_and_continues_after_resolving() {
    let repo = conflict_repo();
    let picked = repo.oid("side");
    let harness = Harness::open(repo);
    harness.wait_for_complete(3);

    assert_eq!(
        harness.run(Operation::CherryPick { commit: picked }),
        None,
        "a pick that stops on a conflict is not an operation failure"
    );

    let stopped = harness.wait_until(|snapshot| {
        snapshot.state == RepoState::CherryPicking && !snapshot.conflicts.is_empty()
    });
    assert_eq!(
        stopped.conflicts.as_ref(),
        &vec![conflict("conflict.txt", ConflictKind::BothModified)]
    );
    assert_eq!(
        harness.repo.head(),
        harness.repo.oid("main"),
        "a stopped pick leaves HEAD where it was"
    );
    assert!(
        harness.repo.status().contains("u UU "),
        "git must agree about the conflict: {}",
        harness.repo.status()
    );

    assert_eq!(
        harness.run(Operation::ResolveWithContent {
            path: "conflict.txt".into(),
            content: b"resolved\n".to_vec(),
        }),
        None
    );
    assert_eq!(harness.repo.read("conflict.txt"), "resolved\n");
    harness.wait_until(|snapshot| snapshot.conflicts.is_empty());
    assert_eq!(
        harness
            .session
            .repo()
            .file_in_index("conflict.txt")
            .expect("index")
            .as_deref(),
        Some(b"resolved\n".as_slice()),
        "the resolved content must be staged, clearing the unmerged stages"
    );

    assert_eq!(
        harness.run(Operation::ContinueInProgress),
        None,
        "continuing must not wait for an editor"
    );

    let done = harness.wait_until(|snapshot| {
        snapshot.commits.complete
            && snapshot.commits.len() == 4
            && snapshot.state == RepoState::Normal
            && snapshot.conflicts.is_empty()
    });
    assert_eq!(
        harness.repo.git(&["log", "-1", "--format=%s"]).trim(),
        "theirs"
    );
    assert_eq!(
        harness.repo.git(&["show", "HEAD:conflict.txt"]).trim(),
        "resolved",
        "the commit must carry the content the user resolved"
    );
    assert_eq!(
        harness.repo.status(),
        "",
        "a finished pick must leave nothing behind"
    );
    assert!(done.conflicts.is_empty());
    assert_eq!(done.state, RepoState::Normal);
}

#[test]
fn aborting_a_conflicted_cherry_pick_restores_head_and_the_worktree() {
    let repo = conflict_repo();
    let picked = repo.oid("side");
    let before = repo.head();
    let harness = Harness::open(repo);
    harness.wait_for_complete(3);

    assert_eq!(harness.run(Operation::CherryPick { commit: picked }), None);
    harness.wait_until(|snapshot| snapshot.state == RepoState::CherryPicking);

    assert_eq!(harness.run(Operation::AbortInProgress), None);
    let aborted = harness.wait_until(|snapshot| {
        snapshot.state == RepoState::Normal && snapshot.conflicts.is_empty()
    });

    assert_eq!(harness.repo.head(), before, "abort must restore HEAD");
    assert_eq!(harness.repo.read("conflict.txt"), "ours\n");
    assert_eq!(
        harness.repo.git(&["rev-list", "--count", "HEAD"]).trim(),
        "2"
    );
    assert_eq!(harness.repo.status(), "");
    assert_eq!(
        aborted.head,
        Some(HeadInfo::Branch {
            short_name: "main".into(),
            target: Some(before),
        })
    );
}

#[test]
fn revert_of_a_clean_commit_restores_its_parent() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.commit_all("one");
    repo.write("a.txt", "two\n");
    repo.commit_all("two");
    let reverted = repo.head();

    let harness = Harness::open(repo);
    harness.wait_for_complete(2);

    assert_eq!(
        harness.run(Operation::Revert { commit: reverted }),
        None,
        "a clean revert of a single-parent commit must succeed"
    );

    harness.wait_until(|snapshot| snapshot.commits.complete && snapshot.commits.len() == 3);
    assert_eq!(harness.repo.read("a.txt"), "one\n");
    assert_eq!(
        harness.repo.git(&["log", "-1", "--format=%s"]).trim(),
        "Revert \"two\"",
        "--no-edit must keep git's prepared message"
    );
    assert_eq!(
        harness.repo.git(&["rev-parse", "HEAD^"]).trim(),
        reverted.to_string(),
        "the revert commit sits on top of the reverted one"
    );
    assert_eq!(harness.repo.status(), "");
}

#[test]
fn revert_of_a_merge_commit_uses_the_first_parent() {
    let repo = TestRepo::new();
    repo.write("base.txt", "base\n");
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "feature"]);
    repo.write("feature.txt", "feature\n");
    repo.commit_all("feature");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("main.txt", "main\n");
    repo.commit_all("main");
    repo.git(&["merge", "--no-ff", "-q", "-m", "merge feature", "feature"]);
    let merge = repo.head();

    let harness = Harness::open(repo);
    // `base`, both sides and the merge itself.
    harness.wait_for_complete(4);

    assert_eq!(
        harness.run(Operation::Revert { commit: merge }),
        None,
        "reverting a merge needs `-m 1`, which the wrapper must add"
    );

    harness.wait_until(|snapshot| snapshot.commits.complete && snapshot.commits.len() == 5);
    assert!(
        !harness.repo.path().join("feature.txt").exists(),
        "the change merged from the second parent is reverted"
    );
    assert!(
        harness.repo.path().join("main.txt").exists(),
        "the first parent is the mainline and keeps its change"
    );
    assert_eq!(
        harness.repo.git(&["log", "-1", "--format=%s"]).trim(),
        "Revert \"merge feature\""
    );
    assert_eq!(harness.repo.status(), "");
}

#[test]
fn revert_conflict_stops_and_abort_restores_the_branch() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.commit_all("one");
    repo.write("a.txt", "two\n");
    repo.commit_all("two");
    let reverted = repo.head();
    // Reverting `two` against `three` cannot apply cleanly.
    repo.write("a.txt", "three\n");
    repo.commit_all("three");
    let before = repo.head();

    let harness = Harness::open(repo);
    harness.wait_for_complete(3);

    assert_eq!(
        harness.run(Operation::Revert { commit: reverted }),
        None,
        "a revert that stops on a conflict is not an operation failure"
    );
    let stopped = harness.wait_until(|snapshot| {
        snapshot.state == RepoState::Reverting && !snapshot.conflicts.is_empty()
    });
    assert_eq!(
        stopped.conflicts.as_ref(),
        &vec![conflict("a.txt", ConflictKind::BothModified)]
    );
    assert!(
        harness.repo.status().contains("u UU "),
        "git must agree about the conflict: {}",
        harness.repo.status()
    );

    assert_eq!(harness.run(Operation::AbortInProgress), None);
    harness.wait_until(|snapshot| {
        snapshot.state == RepoState::Normal && snapshot.conflicts.is_empty()
    });
    assert_eq!(harness.repo.head(), before, "abort must restore HEAD");
    assert_eq!(harness.repo.read("a.txt"), "three\n");
    assert_eq!(harness.repo.status(), "");
}

#[test]
fn reset_soft_mixed_and_hard_move_head_index_and_worktree() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.commit_all("one");
    let first = repo.head();
    repo.write("a.txt", "two\n");
    repo.write("b.txt", "new\n");
    repo.commit_all("two");

    let harness = Harness::open(repo);
    harness.wait_for_complete(2);

    // soft: only the branch moves; both changes stay staged.
    assert_eq!(
        harness.run(Operation::Reset {
            target: first,
            mode: ResetMode::Soft,
        }),
        None
    );
    assert_eq!(harness.repo.head(), first);
    assert_eq!(harness.repo.read("a.txt"), "two\n");
    let status = harness.repo.status();
    assert!(
        status.contains("1 M. ") && status.contains("1 A. "),
        "soft must keep the changes staged: {status}"
    );

    // mixed: the index follows the branch, the worktree keeps the changes.
    assert_eq!(
        harness.run(Operation::Reset {
            target: first,
            mode: ResetMode::Mixed,
        }),
        None
    );
    assert_eq!(harness.repo.head(), first);
    assert_eq!(harness.repo.read("a.txt"), "two\n");
    let status = harness.repo.status();
    assert!(
        status.contains("1 .M "),
        "mixed must leave a.txt unstaged: {status}"
    );
    assert!(
        status.contains("? b.txt"),
        "mixed must leave the added file untracked: {status}"
    );

    // hard: index and worktree both follow the branch. Untracked files are
    // outside the index, so git leaves `b.txt` alone.
    assert_eq!(
        harness.run(Operation::Reset {
            target: first,
            mode: ResetMode::Hard,
        }),
        None
    );
    assert_eq!(harness.repo.head(), first);
    assert_eq!(harness.repo.read("a.txt"), "one\n");
    assert_eq!(harness.repo.status(), "? b.txt\n");
}

#[test]
fn tags_are_lightweight_or_annotated_and_can_be_deleted() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.commit_all("one");
    let first = repo.head();
    repo.write("a.txt", "two\n");
    repo.commit_all("two");
    let second = repo.head();

    let harness = Harness::open(repo);
    harness.wait_for_complete(2);

    assert_eq!(
        harness.run(Operation::CreateTag {
            name: "v1".into(),
            target: first,
            message: None,
        }),
        None
    );
    assert_eq!(
        harness.repo.git(&["cat-file", "-t", "refs/tags/v1"]).trim(),
        "commit",
        "a tag without a message is lightweight"
    );
    assert_eq!(
        harness.repo.git(&["rev-parse", "refs/tags/v1"]).trim(),
        first.to_string()
    );

    // Multi-line and starting with `-`: the message travels on stdin, so it
    // never reaches git's option parser.
    let message = "- first line\nsecond line\n";
    assert_eq!(
        harness.run(Operation::CreateTag {
            name: "v2".into(),
            target: second,
            message: Some(message.into()),
        }),
        None
    );
    assert_eq!(
        harness.repo.git(&["cat-file", "-t", "refs/tags/v2"]).trim(),
        "tag",
        "a tag with a message is annotated"
    );
    assert_eq!(
        harness
            .repo
            .git(&["rev-parse", "refs/tags/v2^{commit}"])
            .trim(),
        second.to_string()
    );
    assert_eq!(
        harness
            .repo
            .git(&["tag", "-l", "--format=%(contents)", "v2"])
            .trim_end(),
        message.trim_end()
    );

    // Both tags reach the snapshot as tag refs.
    let tagged = harness.wait_until(|snapshot| {
        ["refs/tags/v1", "refs/tags/v2"].iter().all(|name| {
            snapshot
                .refs
                .iter()
                .any(|reference| reference.full_name == *name)
        })
    });
    assert!(tagged.refs.iter().any(|reference| {
        reference.full_name == "refs/tags/v2"
            && reference.kind == RefKind::Tag
            && reference.target == second
    }));

    // git's own name rules are reported before anything is written.
    let error = harness
        .run(Operation::CreateTag {
            name: "bad..name".into(),
            target: first,
            message: None,
        })
        .expect("an invalid tag name is an error");
    assert!(
        error.contains("not a valid tag name"),
        "unexpected error: {error}"
    );
    // A name git would accept but a command line would read as an option.
    let error = harness
        .run(Operation::CreateTag {
            name: "-v3".into(),
            target: first,
            message: None,
        })
        .expect("an option-like tag name is an error");
    assert!(
        error.contains("must not start with `-`"),
        "unexpected error: {error}"
    );
    assert_eq!(harness.repo.git(&["tag", "-l"]), "v1\nv2\n");

    assert_eq!(
        harness.run(Operation::DeleteTag { name: "v1".into() }),
        None
    );
    assert_eq!(harness.repo.git(&["tag", "-l"]), "v2\n");
    assert_eq!(
        harness
            .repo
            .git_output(&["rev-parse", "--verify", "--quiet", "refs/tags/v1"])
            .status
            .code(),
        Some(1),
        "the deleted tag must be gone"
    );
    harness.wait_until(|snapshot| {
        snapshot
            .refs
            .iter()
            .all(|reference| reference.full_name != "refs/tags/v1")
    });
}

#[test]
fn rebase_conflict_continues_without_an_editor() {
    let repo = conflict_repo();
    let original = repo.oid("side");
    repo.git(&["checkout", "-q", "side"]);
    let rebase = repo.git_output(&["rebase", "main"]);
    assert!(!rebase.status.success(), "the rebase should conflict");
    assert_eq!(repo.open().state().expect("state"), RepoState::Rebasing);

    let harness = Harness::open(repo);
    harness.wait_until(|snapshot| {
        snapshot.state == RepoState::Rebasing && !snapshot.conflicts.is_empty()
    });

    assert_eq!(
        harness.run(Operation::ResolveWithContent {
            path: "conflict.txt".into(),
            content: b"resolved\n".to_vec(),
        }),
        None
    );
    harness.wait_until(|snapshot| snapshot.conflicts.is_empty());

    // `git rebase --continue` opens an editor for the replayed message, so a
    // wrapper without `GIT_EDITOR=true` fails here instead of finishing.
    assert_eq!(
        harness.run(Operation::ContinueInProgress),
        None,
        "continuing a rebase must not need an editor"
    );

    let done = harness.wait_until(|snapshot| {
        snapshot.state == RepoState::Normal && snapshot.conflicts.is_empty()
    });
    let head = harness.repo.head();
    assert_ne!(head, original, "the replayed commit is new");
    assert_eq!(
        harness.repo.git(&["rev-parse", "HEAD^"]).trim(),
        harness.repo.git(&["rev-parse", "main"]).trim(),
        "the replayed commit sits on main"
    );
    assert_eq!(
        harness.repo.git(&["log", "-1", "--format=%s"]).trim(),
        "theirs"
    );
    assert_eq!(
        harness.repo.git(&["show", "HEAD:conflict.txt"]).trim(),
        "resolved"
    );
    assert_eq!(
        harness.repo.git(&["branch", "--show-current"]).trim(),
        "side"
    );
    assert_eq!(harness.repo.status(), "");
    assert_eq!(done.state, RepoState::Normal);
}

#[test]
fn aborting_a_conflicted_rebase_restores_the_branch() {
    let repo = conflict_repo();
    let original = repo.oid("side");
    repo.git(&["checkout", "-q", "side"]);
    let rebase = repo.git_output(&["rebase", "main"]);
    assert!(!rebase.status.success(), "the rebase should conflict");

    let harness = Harness::open(repo);
    harness.wait_until(|snapshot| snapshot.state == RepoState::Rebasing);

    assert_eq!(harness.run(Operation::AbortInProgress), None);
    harness.wait_until(|snapshot| {
        snapshot.state == RepoState::Normal && snapshot.conflicts.is_empty()
    });
    assert_eq!(
        harness.repo.head(),
        original,
        "abort must restore the branch"
    );
    assert_eq!(
        harness.repo.git(&["branch", "--show-current"]).trim(),
        "side"
    );
    assert_eq!(harness.repo.read("conflict.txt"), "theirs\n");
    assert_eq!(harness.repo.status(), "");
}

#[test]
fn resolve_with_content_refuses_paths_outside_the_worktree() {
    let repo = TestRepo::new();
    repo.write("a.txt", "base\n");
    repo.commit_all("base");
    let git_config = repo.read(".git/config");

    let harness = Harness::open(repo);
    harness.wait_for_complete(1);

    let parent_escape = harness
        .repo
        .path()
        .parent()
        .expect("worktree parent")
        .join("strata-resolve-escape.txt");
    let absolute = harness.repo.path().join("strata-resolve-absolute.txt");
    for path in [
        "../strata-resolve-escape.txt".to_owned(),
        absolute.to_string_lossy().into_owned(),
        ".git/config".to_owned(),
    ] {
        let error = harness
            .run(Operation::ResolveWithContent {
                path: path.clone(),
                content: b"x\n".to_vec(),
            })
            .unwrap_or_else(|| panic!("{path} must be rejected"));
        assert!(
            error.contains("worktree") || error.contains(".git"),
            "unexpected error for {path}: {error}"
        );
    }

    assert!(!parent_escape.exists(), "a `..` path must not escape");
    assert!(!absolute.exists(), "an absolute path must not be written");
    assert_eq!(harness.repo.read(".git/config"), git_config);
    assert_eq!(harness.repo.status(), "", "nothing may be staged");
}

#[test]
fn continue_and_abort_report_when_nothing_is_in_progress() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.commit_all("one");

    let harness = Harness::open(repo);
    harness.wait_for_complete(1);

    let error = harness
        .run(Operation::ContinueInProgress)
        .expect("nothing to continue");
    assert!(
        error.contains("nothing to continue"),
        "unexpected error: {error}"
    );

    let error = harness
        .run(Operation::AbortInProgress)
        .expect("nothing to abort");
    assert!(
        error.contains("nothing to abort"),
        "unexpected error: {error}"
    );
}

#[test]
fn bisect_is_aborted_with_reset_and_has_nothing_to_continue() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.commit_all("one");
    repo.write("a.txt", "two\n");
    repo.commit_all("two");
    repo.write("a.txt", "three\n");
    repo.commit_all("three");
    let before = repo.head();

    repo.git(&["bisect", "start", "HEAD", "HEAD~2"]);
    assert_eq!(repo.open().state().expect("state"), RepoState::Bisecting);

    let git = repo.git_handle();
    let error = git
        .continue_in_progress()
        .expect_err("bisect has no continue");
    assert!(
        error.to_string().contains("nothing to continue"),
        "unexpected error: {error}"
    );

    git.abort_in_progress().expect("bisect reset");
    assert_eq!(repo.open().state().expect("state"), RepoState::Normal);
    assert_eq!(repo.head(), before);
    assert_eq!(repo.git(&["branch", "--show-current"]).trim(), "main");
}
