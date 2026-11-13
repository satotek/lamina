//! Throwaway repositories for tests that need real git behaviour.
//!
//! Every command runs with its own global config file, an author and a
//! committer fixed by the environment, and no system config, so a test can
//! never change its result because of the machine it runs on.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::Oid;
use crate::git_cli::Git;
use crate::repo::Repo;

/// A fresh repository in a temporary directory.
pub(crate) struct TestRepo {
    dir: tempfile::TempDir,
    config: PathBuf,
}

/// The author and committer every fixture commit uses.
pub(crate) const AUTHOR_NAME: &str = "Strata Test";
pub(crate) const AUTHOR_EMAIL: &str = "strata@example.invalid";
/// A fixed timestamp keeps commit ids and blame ordering reproducible.
const TIME: &str = "1700000000 +0000";

impl TestRepo {
    pub(crate) fn new() -> Self {
        let dir = tempfile::TempDir::new().expect("temporary directory");
        let config = dir.path().join("gitconfig");
        std::fs::write(
            &config,
            "[user]\n\tname = Strata Test\n\temail = strata@example.invalid\n\
             [commit]\n\tgpgsign = false\n\
             [tag]\n\tgpgsign = false\n\
             [core]\n\tautocrlf = false\n\
             [init]\n\tdefaultBranch = main\n",
        )
        .expect("write test git config");

        let repo = Self { dir, config };
        repo.git(&["init", "-q"]);
        repo
    }

    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Write a file relative to the worktree, creating parent directories.
    pub(crate) fn write(&self, path: &str, contents: &str) {
        let path = self.path().join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent directory");
        }
        std::fs::write(path, contents).expect("write file");
    }

    /// Run git and require success, returning its stdout as text.
    pub(crate) fn git(&self, args: &[&str]) -> String {
        let output = self.git_output(args);
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Run git and return the raw result, for commands a test expects to fail.
    pub(crate) fn git_output(&self, args: &[&str]) -> Output {
        self.command("git").args(args).output().expect("run git")
    }

    /// Stage everything and commit with `message`, returning the new commit.
    pub(crate) fn commit(&self, message: &str) -> Oid {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
        self.head()
    }

    /// The commit `HEAD` points at.
    pub(crate) fn head(&self) -> Oid {
        self.rev("HEAD")
    }

    pub(crate) fn rev(&self, spec: &str) -> Oid {
        self.git(&["rev-parse", spec])
            .trim()
            .parse()
            .expect("valid object id")
    }

    /// The commits in `range`, oldest first, like `git rev-list --reverse`.
    pub(crate) fn rev_list(&self, range: &str) -> Vec<Oid> {
        self.git(&["rev-list", "--reverse", range])
            .lines()
            .map(|line| line.trim().parse().expect("valid object id"))
            .collect()
    }

    pub(crate) fn open(&self) -> Repo {
        Repo::open(self.path()).expect("open test repository")
    }

    /// A `Git` handle that runs with the same isolated environment as the
    /// fixture's own commands.
    pub(crate) fn git_handle(&self) -> Git {
        Git::new(self.path().to_path_buf())
            .with_env("GIT_CONFIG_GLOBAL", &self.config)
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
            .with_env("GIT_AUTHOR_NAME", AUTHOR_NAME)
            .with_env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
            .with_env("GIT_COMMITTER_NAME", AUTHOR_NAME)
            .with_env("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL)
            .with_env("GIT_AUTHOR_DATE", TIME)
            .with_env("GIT_COMMITTER_DATE", TIME)
    }

    /// A `git` command with the fixture environment applied.
    pub(crate) fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(self.path())
            .env("GIT_CONFIG_GLOBAL", &self.config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", AUTHOR_NAME)
            .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
            .env("GIT_COMMITTER_NAME", AUTHOR_NAME)
            .env("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL)
            .env("GIT_AUTHOR_DATE", TIME)
            .env("GIT_COMMITTER_DATE", TIME)
            .env("GIT_TERMINAL_PROMPT", "0")
            // A test that runs inside a git hook or another checkout must not
            // inherit a repository location.
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE");
        command
    }
}
