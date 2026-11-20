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
#[cfg_attr(not(test), allow(dead_code))]
pub struct TestRepo {
    _dir: tempfile::TempDir,
    /// The worktree root. A clone lives in a subdirectory so the fixture's
    /// config file does not make the clone destination non-empty.
    root: PathBuf,
    config: PathBuf,
}

/// The author and committer every fixture commit uses.
#[cfg_attr(not(test), allow(dead_code))]
pub const AUTHOR_NAME: &str = "Lamina Test";
#[cfg_attr(not(test), allow(dead_code))]
pub const AUTHOR_EMAIL: &str = "lamina@example.invalid";
/// A fixed timestamp keeps commit ids and blame ordering reproducible.
const TIME: &str = "1700000000 +0000";

#[cfg_attr(not(test), allow(dead_code))]
impl TestRepo {
    pub fn new() -> Self {
        let repo = Self::with_dir(tempfile::TempDir::new().expect("temporary directory"));
        repo.git(&["init", "-q"]);
        repo
    }

    /// A repository in a fresh temporary directory, cloned from `source` with
    /// `args`. The source is a local `file://` URL so the clone needs no network.
    pub fn clone_from(source: &TestRepo, args: &[&str]) -> Self {
        let repo = Self::with_dir(tempfile::TempDir::new().expect("temporary directory"));
        let url = format!("file://{}", source.path().display());
        let mut argv = vec!["clone"];
        argv.extend_from_slice(args);
        argv.push(&url);
        argv.push(".");
        repo.git(&argv);
        repo
    }

    /// The fixture environment, before either `git init` or `git clone`.
    fn with_dir(dir: tempfile::TempDir) -> Self {
        let root = dir.path().join("repo");
        std::fs::create_dir(&root).expect("create repository directory");
        Self::with_root(dir, root)
    }

    fn with_root(dir: tempfile::TempDir, root: PathBuf) -> Self {
        let config = dir.path().join("gitconfig");
        std::fs::write(
            &config,
            "[user]\n\tname = Lamina Test\n\temail = lamina@example.invalid\n\
             [commit]\n\tgpgsign = false\n\
             [tag]\n\tgpgsign = false\n\
             [core]\n\tautocrlf = false\n\
             [init]\n\tdefaultBranch = main\n",
        )
        .expect("write test git config");

        Self {
            _dir: dir,
            root,
            config,
        }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Write a file relative to the worktree, creating parent directories.
    pub fn write(&self, path: &str, contents: &str) {
        self.write_bytes(path, contents.as_bytes());
    }

    /// Write raw bytes relative to the worktree, for encodings other than
    /// UTF-8 and for paths that are not text.
    pub fn write_bytes(&self, path: impl AsRef<Path>, contents: &[u8]) {
        let path = self.path().join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent directory");
        }
        std::fs::write(path, contents).expect("write file");
    }

    /// Run git and require success, returning its stdout as text.
    pub fn git(&self, args: &[&str]) -> String {
        let output = self.git_output(args);
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Run git and return the raw result, for commands a test expects to fail.
    pub fn git_output(&self, args: &[&str]) -> Output {
        self.command("git").args(args).output().expect("run git")
    }

    /// Stage everything and commit with `message`, returning the new commit.
    pub fn commit(&self, message: &str) -> Oid {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
        self.head()
    }

    /// Stage everything and commit as `name <email>`, for a repository whose
    /// history should carry a non-ASCII author.
    pub fn commit_as(&self, name: &str, email: &str, message: &str) -> Oid {
        self.git(&["add", "-A"]);
        let output = self
            .command("git")
            .args(["commit", "-q", "-m", message])
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_COMMITTER_NAME", name)
            .env("GIT_COMMITTER_EMAIL", email)
            .output()
            .expect("run git commit");
        assert!(
            output.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        self.head()
    }

    /// The commit `HEAD` points at.
    pub fn head(&self) -> Oid {
        self.rev("HEAD")
    }

    pub fn rev(&self, spec: &str) -> Oid {
        self.git(&["rev-parse", spec])
            .trim()
            .parse()
            .expect("valid object id")
    }

    /// The commits in `range`, oldest first, like `git rev-list --reverse`.
    pub fn rev_list(&self, range: &str) -> Vec<Oid> {
        self.git(&["rev-list", "--reverse", range])
            .lines()
            .map(|line| line.trim().parse().expect("valid object id"))
            .collect()
    }

    pub fn open(&self) -> Repo {
        Repo::open(self.path()).expect("open test repository")
    }

    /// A `Git` handle that runs with the same isolated environment as the
    /// fixture's own commands.
    pub fn git_handle(&self) -> Git {
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
    pub fn command(&self, program: &str) -> Command {
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

impl Default for TestRepo {
    fn default() -> Self {
        Self::new()
    }
}

/// A path written the way it belongs in a git config value.
///
/// Backslash is an escape character in that format, so a native Windows path
/// written raw makes the whole file unreadable. Git stores such paths with
/// forward slashes itself, which every platform accepts.
#[cfg_attr(not(test), allow(dead_code))]
pub fn config_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// A source repository whose HEAD renames a file after a merge, and a clone of
/// it made with `args`.
///
/// With `["--depth", "2"]` the clone keeps the rename commit and the merge
/// below it, which makes the merge the shallow boundary: neither of its two
/// parents is in the clone.
#[cfg_attr(not(test), allow(dead_code))]
pub fn shallow_clone_fixture(args: &[&str]) -> (TestRepo, TestRepo) {
    let source = TestRepo::new();
    source.write("base.txt", "base\n");
    source.commit("base");
    source.write("dir/old.txt", "one\ntwo\nthree\n");
    source.write("keep.txt", "keep\n");
    source.commit("add files");
    source.write("dir/old.txt", "one\nTWO\nthree\n");
    source.commit("edit file");
    source.write("dir/old.txt", "one\nTWO\nthree\nfour\n");
    source.commit("edit again");
    source.git(&["checkout", "-q", "-b", "topic"]);
    source.write("topic.txt", "topic\n");
    source.commit("topic commit");
    source.git(&["checkout", "-q", "main"]);
    source.git(&["merge", "-q", "--no-ff", "-m", "merge topic", "topic"]);
    source.git(&["mv", "dir/old.txt", "dir/new.txt"]);
    source.write("dir/new.txt", "one\nTWO\nthree\nfour\nfive\n");
    source.commit("rename and edit");

    let clone = TestRepo::clone_from(&source, args);
    (source, clone)
}
