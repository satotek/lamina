//! Integration tests for pulling through `Git`.
//!
//! A pull that stops on conflicts leaves a merge or rebase the user resolves,
//! so it is a normal outcome rather than a failed operation; a pull that fails
//! for any other reason must still be an error. Every workspace is a work tree
//! plus a bare remote and a second clone, run with an isolated git config the
//! same way the other core tests do.

use std::path::{Path, PathBuf};

use lamina_core::RepoState;
use lamina_core::git_cli::Git;
use lamina_core::repo::Repo;
use lamina_core::test_support::TestRepo;

/// A work tree, its bare remote and a second clone, all inside one fixture.
struct Fixture {
    repo: TestRepo,
}

impl Fixture {
    fn new() -> Self {
        let repo = TestRepo::new();
        repo.git_in(
            repo.temp_root(),
            &["init", "-q", "--bare", "-b", "main", "remote.git"],
        );
        let work = repo.scratch_dir("work");
        repo.git_in(&work, &["init", "-q", "-b", "main"]);
        Self { repo }
    }

    fn work(&self) -> PathBuf {
        self.repo.temp_path("work")
    }

    fn remote_arg(&self) -> String {
        self.repo
            .temp_path("remote.git")
            .to_str()
            .expect("utf-8 path")
            .to_owned()
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        self.repo.git_in(cwd, args)
    }

    fn write(&self, dir: &Path, name: &str, contents: &str) {
        self.repo.write_in(dir, name, contents);
    }

    fn commit_all(&self, dir: &Path, message: &str) {
        self.git(dir, &["add", "-A"]);
        self.git(dir, &["commit", "-q", "-m", message]);
    }

    /// The work tree tracked by a bare remote, the same way the tests in
    /// `upstream.rs` build one.
    fn tracking() -> Self {
        let fixture = Self::new();
        let work = fixture.work();
        fixture.write(&work, "a.txt", "base\n");
        fixture.commit_all(&work, "base");
        fixture.git(&work, &["remote", "add", "origin", &fixture.remote_arg()]);
        fixture.git(&work, &["push", "-q", "-u", "origin", "main"]);
        fixture
    }

    /// Commit in the second clone and push it, moving the upstream.
    fn push_from_other(&self, name: &str, contents: &str) {
        let other = self.repo.temp_path("other");
        if !other.exists() {
            self.git(
                self.repo.temp_root(),
                &["clone", "-q", &self.remote_arg(), "other"],
            );
        }
        self.write(&other, name, contents);
        self.commit_all(&other, "theirs");
        self.git(&other, &["push", "-q", "origin", "main"]);
    }

    /// The same repository through the production write path, isolated from
    /// the host's git config.
    fn git_handle(&self) -> Git {
        self.repo.git_handle_in(self.work())
    }

    fn state(&self) -> RepoState {
        Repo::open(self.work())
            .expect("open work tree")
            .state()
            .expect("state")
    }

    fn status(&self) -> String {
        self.git(&self.work(), &["status", "--porcelain=v2"])
    }

    fn git_dir(&self, name: &str) -> PathBuf {
        self.work().join(".git").join(name)
    }
}

/// Both sides change the same line, so pulling the upstream commit conflicts.
fn conflicting_upstream() -> Fixture {
    let fixture = Fixture::tracking();
    fixture.push_from_other("a.txt", "theirs\n");
    fixture.write(&fixture.work(), "a.txt", "ours\n");
    fixture.commit_all(&fixture.work(), "ours");
    fixture
}

#[test]
fn pull_that_stops_on_a_merge_conflict_is_not_an_error() {
    let fixture = conflicting_upstream();
    let git = fixture.git_handle();

    git.pull(Some(false))
        .expect("a merge that stops on conflicts is not an operation failure");

    assert_eq!(
        fixture.state(),
        RepoState::Merging,
        "the repository must be left mid-merge for the user to resolve"
    );
    assert!(
        fixture.git_dir("MERGE_HEAD").is_file(),
        "MERGE_HEAD records the merge git started"
    );
    let status = fixture.status();
    assert!(
        status.contains("u UU "),
        "git must agree about the conflict: {status}"
    );
    assert_eq!(
        fixture
            .git(&fixture.work(), &["log", "-1", "--format=%s"])
            .trim(),
        "ours"
    );
}

#[test]
fn pull_that_stops_on_a_rebase_conflict_is_not_an_error() {
    let fixture = conflicting_upstream();
    let git = fixture.git_handle();

    git.pull_with_progress(Some(true), |_, _| {})
        .expect("a rebase that stops on conflicts is not an operation failure");

    assert_eq!(
        fixture.state(),
        RepoState::Rebasing,
        "the repository must be left mid-rebase for the user to resolve"
    );
    let status = fixture.status();
    assert!(
        status.contains("u UU "),
        "git must agree about the conflict: {status}"
    );
}

#[test]
fn pull_that_cannot_fast_forward_is_still_an_error() {
    let fixture = Fixture::tracking();
    let work = fixture.work();
    // `--no-rebase` would override this setting, so the pull keeps the user's
    // configured behaviour (`rebase: None`), which is ff-only here.
    fixture.git(&work, &["config", "pull.ff", "only"]);
    // Both sides move, so only a merge could reconcile them and ff-only must
    // refuse.
    fixture.push_from_other("a.txt", "theirs\n");
    fixture.write(&work, "a.txt", "ours\n");
    fixture.commit_all(&work, "ours");
    let before = fixture.git(&work, &["rev-parse", "HEAD"]);

    let error = fixture
        .git_handle()
        .pull(None)
        .expect_err("a refused fast-forward is a real failure");
    assert!(
        error.to_string().contains("fast-forward"),
        "unexpected error: {error}"
    );

    assert_eq!(fixture.state(), RepoState::Normal);
    assert!(
        !fixture.git_dir("MERGE_HEAD").is_file(),
        "a refused pull must not start a merge"
    );
    assert_eq!(
        fixture.git(&work, &["rev-parse", "HEAD"]).trim(),
        before.trim(),
        "a refused pull must not move the branch"
    );
    assert_eq!(fixture.status(), "");
}
