//! Integration tests for pulling through `Git`.
//!
//! A pull that stops on conflicts leaves a merge or rebase the user resolves,
//! so it is a normal outcome rather than a failed operation; a pull that fails
//! for any other reason must still be an error. Every workspace is a work tree
//! plus a bare remote and a second clone, run with an isolated git config the
//! same way the other core tests do.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use strata_core::RepoState;
use strata_core::git_cli::Git;
use strata_core::repo::Repo;
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().expect("root temp dir");
        let config = root.path().join("gitconfig");
        fs::write(&config, "").expect("empty git config");
        let fixture = Self { root, config };
        fixture.git(
            fixture.root.path(),
            &["init", "-q", "--bare", "-b", "main", "remote.git"],
        );
        fs::create_dir_all(fixture.work()).expect("work dir");
        fixture.git(&fixture.work(), &["init", "-q", "-b", "main"]);
        fixture
    }

    fn work(&self) -> PathBuf {
        self.root.path().join("work")
    }

    fn remote_arg(&self) -> String {
        self.root
            .path()
            .join("remote.git")
            .to_str()
            .expect("utf-8 path")
            .to_owned()
    }

    fn command(&self, cwd: &Path) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(cwd)
            .env("GIT_CONFIG_GLOBAL", &self.config)
            .env("GIT_CONFIG_SYSTEM", &self.config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("XDG_CONFIG_HOME", self.root.path())
            .env("GIT_AUTHOR_NAME", "Strata Test")
            .env("GIT_AUTHOR_EMAIL", "strata@example.com")
            .env("GIT_COMMITTER_NAME", "Strata Test")
            .env("GIT_COMMITTER_EMAIL", "strata@example.com")
            .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE");
        command
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self.command(cwd).args(args).output().expect("spawn git");
        assert!(
            output.status.success(),
            "git {args:?} in {} failed: {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn write(&self, dir: &Path, name: &str, contents: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(path, contents).expect("write file");
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
        let other = self.root.path().join("other");
        if !other.exists() {
            self.git(
                self.root.path(),
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
        Git::new(self.work())
            .with_env("GIT_CONFIG_GLOBAL", &self.config)
            .with_env("GIT_CONFIG_SYSTEM", &self.config)
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
            .with_env("XDG_CONFIG_HOME", self.root.path())
            .with_env("GIT_AUTHOR_NAME", "Strata Test")
            .with_env("GIT_AUTHOR_EMAIL", "strata@example.com")
            .with_env("GIT_COMMITTER_NAME", "Strata Test")
            .with_env("GIT_COMMITTER_EMAIL", "strata@example.com")
            .with_env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
            .with_env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
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
