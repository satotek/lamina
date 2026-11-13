//! Integration tests for repository state, conflicted paths and the merge
//! operations, against real repositories and real git invocations.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use strata_core::git_cli::Git;
use strata_core::repo::Repo;
use strata_core::session::{
    ConflictSide, Operation, OperationId, RepoSnapshot, Session, SessionEvent,
};
use strata_core::{
    ChangeKind, ConflictEntry, ConflictKind, ConflictSides, HeadInfo, Oid, RepoState,
};
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
        // below, so the settings a merge or commit cannot work without are
        // pinned in the repository config.
        repo.git(&["config", "user.name", "Strata Test"]);
        repo.git(&["config", "user.email", "strata@example.com"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
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
        self.git(&["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("parse HEAD")
    }

    fn commit_all(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
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

/// `main` and `side` both change `conflict.txt`, so merging conflicts.
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

/// One conflict per kind of unmerged stage combination:
/// `both.txt` both modified, `gone.txt` deleted on `side`, `added.txt` added
/// on both branches.
fn three_way_conflict_repo() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("both.txt", "base\n");
    repo.write("gone.txt", "base\n");
    repo.commit_all("base");

    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("both.txt", "theirs\n");
    repo.git(&["rm", "-q", "gone.txt"]);
    repo.write("added.txt", "theirs\n");
    repo.commit_all("side");

    repo.git(&["checkout", "-q", "main"]);
    repo.write("both.txt", "ours\n");
    repo.write("gone.txt", "ours\n");
    repo.write("added.txt", "ours\n");
    repo.commit_all("ours");

    let merge = repo.git_output(&["merge", "side"]);
    assert!(!merge.status.success(), "the merge should conflict");
    repo
}

/// What `git status --porcelain=v2` reports for the unmerged paths.
fn conflicts_from_git(repo: &TestRepo) -> Vec<ConflictEntry> {
    let output = repo.git(&["status", "--porcelain=v2"]);
    let mut conflicts: Vec<ConflictEntry> = output
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("u ")?;
            // `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>`; the path
            // is the last field and may contain spaces.
            let fields: Vec<&str> = rest.splitn(10, ' ').collect();
            let kind = match *fields.first()? {
                "DD" => ConflictKind::BothDeleted,
                "AU" => ConflictKind::AddedByUs,
                "UD" => ConflictKind::DeletedByThem,
                "UA" => ConflictKind::AddedByThem,
                "DU" => ConflictKind::DeletedByUs,
                "AA" => ConflictKind::BothAdded,
                "UU" => ConflictKind::BothModified,
                _ => return None,
            };
            Some(ConflictEntry {
                path: fields.get(9)?.to_string(),
                kind,
            })
        })
        .collect();
    conflicts.sort_by(|a, b| a.path.cmp(&b.path));
    conflicts
}

fn conflict(path: &str, kind: ConflictKind) -> ConflictEntry {
    ConflictEntry {
        path: path.to_owned(),
        kind,
    }
}

#[test]
fn state_reports_merge_cherry_pick_and_rebase() {
    let repo = TestRepo::new();
    repo.write("a.txt", "base\n");
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("a.txt", "side\n");
    repo.commit_all("side");
    let side = repo.git(&["rev-parse", "side"]).trim().to_owned();
    repo.git(&["checkout", "-q", "main"]);
    repo.write("a.txt", "main\n");
    repo.commit_all("main");

    assert_eq!(repo.open().state().expect("state"), RepoState::Normal);

    // A merge that stops on a conflict leaves MERGE_HEAD behind.
    let merge = repo.git_output(&["merge", "side"]);
    assert!(!merge.status.success(), "the merge should conflict");
    assert_eq!(repo.open().state().expect("state"), RepoState::Merging);
    repo.git(&["merge", "--abort"]);
    assert_eq!(repo.open().state().expect("state"), RepoState::Normal);

    // A cherry-pick of the same change conflicts and leaves CHERRY_PICK_HEAD.
    let pick = repo.git_output(&["cherry-pick", &side]);
    assert!(!pick.status.success(), "the cherry-pick should conflict");
    assert_eq!(
        repo.open().state().expect("state"),
        RepoState::CherryPicking
    );
    repo.git(&["cherry-pick", "--abort"]);
    assert_eq!(repo.open().state().expect("state"), RepoState::Normal);

    // Rebasing onto the other side stops in the middle as well.
    let rebase = repo.git_output(&["rebase", "side"]);
    assert!(!rebase.status.success(), "the rebase should conflict");
    assert_eq!(repo.open().state().expect("state"), RepoState::Rebasing);
    repo.git(&["rebase", "--abort"]);
    assert_eq!(repo.open().state().expect("state"), RepoState::Normal);
}

#[test]
fn conflicts_match_git_for_every_stage_combination() {
    let repo = three_way_conflict_repo();

    let expected = vec![
        conflict("added.txt", ConflictKind::BothAdded),
        conflict("both.txt", ConflictKind::BothModified),
        conflict("gone.txt", ConflictKind::DeletedByThem),
    ];
    assert_eq!(
        repo.open().conflicts().expect("conflicts"),
        expected,
        "unmerged paths, sorted by path"
    );
    assert_eq!(
        expected,
        conflicts_from_git(&repo),
        "the kinds must be the ones `git status` reports"
    );
}

#[test]
fn conflict_sides_reads_the_index_stages() {
    let repo = three_way_conflict_repo();
    let open = repo.open();

    let both = open.conflict_sides("both.txt").expect("sides");
    assert_eq!(both.base.as_deref(), Some(b"base\n".as_slice()));
    assert_eq!(both.ours.as_deref(), Some(b"ours\n".as_slice()));
    assert_eq!(both.theirs.as_deref(), Some(b"theirs\n".as_slice()));

    let gone = open.conflict_sides("gone.txt").expect("sides");
    assert_eq!(gone.base.as_deref(), Some(b"base\n".as_slice()));
    assert_eq!(gone.ours.as_deref(), Some(b"ours\n".as_slice()));
    assert_eq!(
        gone.theirs, None,
        "the side that deleted the file has no blob"
    );

    let added = open.conflict_sides("added.txt").expect("sides");
    assert_eq!(added.base, None, "a file added on both sides has no base");
    assert_eq!(added.ours.as_deref(), Some(b"ours\n".as_slice()));
    assert_eq!(added.theirs.as_deref(), Some(b"theirs\n".as_slice()));

    assert_eq!(
        open.conflict_sides("missing.txt").expect("sides"),
        ConflictSides::default()
    );
}

#[test]
fn taking_a_side_stages_it_or_removes_the_deleted_path() {
    let repo = three_way_conflict_repo();
    let git = repo.git_handle();

    git.checkout_side("both.txt", true).expect("take ours");
    assert_eq!(repo.read("both.txt"), "ours\n");
    assert_eq!(
        repo.open().conflict_sides("both.txt").expect("sides"),
        ConflictSides::default(),
        "the path must be staged with a single stage-0 entry"
    );

    // `gone.txt` was deleted on `side`, so that side has no blob to check out.
    git.checkout_side("gone.txt", false)
        .expect("take the deleted side");
    assert!(!repo.path().join("gone.txt").exists());
    assert_eq!(
        repo.git(&["ls-files", "--stage", "--", "gone.txt"]).trim(),
        ""
    );

    let remaining: Vec<String> = repo
        .open()
        .conflicts()
        .expect("conflicts")
        .into_iter()
        .map(|entry| entry.path)
        .collect();
    assert_eq!(remaining, vec!["added.txt".to_owned()]);
}

#[test]
fn fast_forward_merge_moves_head_and_no_ff_records_a_merge_commit() {
    let repo = TestRepo::new();
    repo.write("a.txt", "base\n");
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("b.txt", "side\n");
    repo.commit_all("side");
    let side = repo.head();
    repo.git(&["checkout", "-q", "main"]);

    let git = repo.git_handle();
    git.merge("side", false).expect("fast-forward merge");
    assert_eq!(repo.head(), side, "a fast-forward moves HEAD to the branch");
    assert_eq!(repo.git(&["rev-list", "--count", "HEAD"]).trim(), "2");

    // A second branch can still be merged with --no-ff, which records a merge
    // commit even though HEAD could have been moved forward.
    repo.git(&["checkout", "-q", "-b", "other"]);
    repo.write("c.txt", "other\n");
    repo.commit_all("other");
    let other = repo.head();
    repo.git(&["checkout", "-q", "main"]);

    git.merge("other", true).expect("no-ff merge");
    assert_ne!(repo.head(), other);
    let parents = repo.git(&["rev-list", "--parents", "-n", "1", "HEAD"]);
    assert_eq!(
        parents.split_whitespace().count(),
        3,
        "a no-ff merge has two parents: {parents}"
    );
    assert_eq!(repo.git(&["rev-list", "--count", "HEAD"]).trim(), "4");
}

#[test]
fn a_merge_that_cannot_start_is_still_an_error() {
    let repo = TestRepo::new();
    repo.write("a.txt", "base\n");
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("a.txt", "side\n");
    repo.commit_all("side");
    repo.git(&["checkout", "-q", "main"]);
    // git refuses to merge over a local change, exits 1 and leaves no merge
    // state; only a started merge may be reported as success.
    repo.write("a.txt", "local\n");

    let error = repo
        .git_handle()
        .merge("side", false)
        .expect_err("a refused merge is an error");
    assert!(
        error.to_string().contains("local changes"),
        "unexpected error: {error}"
    );
    assert_eq!(repo.open().state().expect("state"), RepoState::Normal);
}

#[test]
fn mergetool_resolves_the_path_with_the_configured_tool() {
    let repo = conflict_repo();
    let merge = repo.git_output(&["merge", "side"]);
    assert!(!merge.status.success(), "the merge should conflict");

    // A non-interactive tool that takes the other side, configured the way a
    // user would configure theirs.
    repo.git(&["config", "merge.tool", "strata-test"]);
    repo.git(&[
        "config",
        "mergetool.strata-test.cmd",
        "cp \"$REMOTE\" \"$MERGED\"",
    ]);
    repo.git(&["config", "mergetool.strata-test.trustExitCode", "true"]);

    let git = repo.git_handle();
    git.mergetool("conflict.txt").expect("run mergetool");
    assert_eq!(repo.read("conflict.txt"), "theirs\n");
    assert!(repo.open().conflicts().expect("conflicts").is_empty());
    assert!(
        repo.git(&["ls-files", "--stage", "--", "conflict.txt"])
            .contains(" 0\tconflict.txt"),
        "mergetool must stage the resolved path"
    );

    git.merge_continue().expect("finish the merge");
    assert_eq!(repo.open().state().expect("state"), RepoState::Normal);
    let parents = repo.git(&["rev-list", "--parents", "-n", "1", "HEAD"]);
    assert_eq!(parents.split_whitespace().count(), 3);
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
}

#[test]
fn session_merge_conflict_then_resolve_ours_and_continue() {
    let repo = conflict_repo();
    let harness = Harness::open(repo);
    // The walk covers every ref, so it holds `base`, `side` and `main`.
    harness.wait_for_complete(3);

    let id = harness.session.run(Operation::Merge {
        branch: "side".into(),
        no_ff: false,
    });
    assert_eq!(
        harness.wait_for_operation(id),
        None,
        "a merge that stops on conflicts is not an operation failure"
    );

    let merging = harness.wait_until(|snapshot| {
        snapshot.state == RepoState::Merging && !snapshot.conflicts.is_empty()
    });
    assert_eq!(
        merging.conflicts.as_ref(),
        &vec![conflict("conflict.txt", ConflictKind::BothModified)]
    );
    assert!(
        merging
            .status
            .unstaged
            .iter()
            .any(|change| change.path == "conflict.txt" && change.kind == ChangeKind::Conflicted),
        "status does not show the conflict: {:?}",
        merging.status
    );

    let id = harness.session.run(Operation::ResolveConflict {
        path: "conflict.txt".into(),
        side: ConflictSide::Ours,
    });
    assert_eq!(harness.wait_for_operation(id), None);
    harness.wait_until(|snapshot| {
        snapshot.conflicts.is_empty() && snapshot.state == RepoState::Merging
    });
    assert_eq!(harness.repo.read("conflict.txt"), "ours\n");
    assert_eq!(
        harness
            .session
            .repo()
            .file_in_index("conflict.txt")
            .expect("index")
            .as_deref(),
        Some(b"ours\n".as_slice())
    );

    let id = harness.session.run(Operation::MergeContinue);
    assert_eq!(harness.wait_for_operation(id), None);
    // The history walk and the status refresh race with each other, so wait for
    // both halves of the finished state.
    let done = harness.wait_until(|snapshot| {
        snapshot.commits.complete
            && snapshot.commits.len() == 4
            && snapshot.state == RepoState::Normal
            && snapshot.conflicts.is_empty()
    });
    assert_eq!(done.state, RepoState::Normal);
    assert!(done.conflicts.is_empty());
    let (head, _) = done.commits.get(0).expect("merge commit");
    assert_eq!(
        head.parents.len(),
        2,
        "the merge commit has one parent per side"
    );
    assert_eq!(
        done.head,
        Some(HeadInfo::Branch {
            short_name: "main".into(),
            target: Some(head.id),
        })
    );
    // git agrees with the walk and with the resolved content.
    assert_eq!(
        harness.repo.git(&["rev-list", "--count", "HEAD"]).trim(),
        "4"
    );
    let parents = harness
        .repo
        .git(&["rev-list", "--parents", "-n", "1", "HEAD"]);
    assert_eq!(parents.split_whitespace().count(), 3);
    assert_eq!(harness.repo.read("conflict.txt"), "ours\n");
}

#[test]
fn session_merge_abort_restores_the_pre_merge_head() {
    let repo = conflict_repo();
    let before = repo.head();
    let harness = Harness::open(repo);
    // The walk covers every ref, so it holds `base`, `side` and `main`.
    harness.wait_for_complete(3);

    let id = harness.session.run(Operation::Merge {
        branch: "side".into(),
        no_ff: false,
    });
    assert_eq!(harness.wait_for_operation(id), None);
    harness.wait_until(|snapshot| snapshot.state == RepoState::Merging);

    let id = harness.session.run(Operation::MergeAbort);
    assert_eq!(harness.wait_for_operation(id), None);
    let aborted = harness.wait_until(|snapshot| {
        snapshot.state == RepoState::Normal && snapshot.conflicts.is_empty()
    });
    assert_eq!(harness.repo.head(), before, "abort must restore HEAD");
    assert_eq!(harness.repo.read("conflict.txt"), "ours\n");
    assert_eq!(
        harness.repo.git(&["rev-list", "--count", "HEAD"]).trim(),
        "2"
    );
    assert_eq!(
        aborted.head,
        Some(HeadInfo::Branch {
            short_name: "main".into(),
            target: Some(before),
        })
    );
}
