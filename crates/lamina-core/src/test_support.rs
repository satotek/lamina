//! Throwaway repositories for tests that need real git behaviour.
//!
//! Every command runs with its own global config file, an author and a
//! committer fixed by the environment, and no system config, so a test can
//! never change its result because of the machine it runs on.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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
    author: (String, String),
    /// Commit timestamp in seconds, so tests about date ordering are
    /// independent of the machine's clock.
    time: i64,
}

/// The author and committer every fixture commit uses.
#[cfg_attr(not(test), allow(dead_code))]
pub const AUTHOR_NAME: &str = "Lamina Test";
#[cfg_attr(not(test), allow(dead_code))]
pub const AUTHOR_EMAIL: &str = "lamina@example.invalid";
/// A fixed timestamp keeps commit ids and blame ordering reproducible.
const DEFAULT_TIME: i64 = 1_700_000_000;

#[cfg_attr(not(test), allow(dead_code))]
impl TestRepo {
    pub fn new() -> Self {
        Self::with_identity(AUTHOR_NAME, AUTHOR_EMAIL)
    }

    /// A repository whose commits use `name <email>` instead of the default
    /// test identity, for tests that check the author a command records.
    pub fn with_identity(name: &str, email: &str) -> Self {
        let repo = Self::build(name, email, DEFAULT_TIME);
        repo.git(&["init", "-q"]);
        repo
    }

    /// A repository with the settings a commit, merge or tag needs pinned in
    /// the repository itself, for code that runs git without the fixture's
    /// environment: a `Session`, which opens its own `Git` handle.
    pub fn committing() -> Self {
        let repo = Self::new();
        repo.configure_local();
        repo
    }

    /// A repository whose commits use `seconds` as their timestamp, for tests
    /// about date ordering or stash creation times.
    pub fn with_time(seconds: i64) -> Self {
        let repo = Self::build(AUTHOR_NAME, AUTHOR_EMAIL, seconds);
        repo.git(&["init", "-q"]);
        repo
    }

    /// A bare repository, for a remote another fixture pushes to or clones.
    pub fn bare() -> Self {
        let repo = Self::build(AUTHOR_NAME, AUTHOR_EMAIL, DEFAULT_TIME);
        repo.git(&["init", "-q", "--bare"]);
        repo
    }

    /// A repository in a fresh temporary directory, cloned from `source` with
    /// `args`. The source is a local `file://` URL so the clone needs no network.
    pub fn clone_from(source: &TestRepo, args: &[&str]) -> Self {
        let repo = Self::build(&source.author.0, &source.author.1, source.time);
        let url = format!("file://{}", source.path().display());
        let mut argv = vec!["clone"];
        argv.extend_from_slice(args);
        argv.push(&url);
        argv.push(".");
        repo.git(&argv);
        repo
    }

    /// Pin the settings a commit, merge or tag cannot work without in the
    /// repository itself, for code that runs git without the fixture's
    /// environment: a `Session`, which opens its own `Git` handle.
    pub fn configure_local(&self) {
        self.git(&["config", "user.name", &self.author.0]);
        self.git(&["config", "user.email", &self.author.1]);
        self.git(&["config", "commit.gpgsign", "false"]);
        self.git(&["config", "tag.gpgsign", "false"]);
        self.git(&["config", "core.autocrlf", "false"]);
    }

    fn build(name: &str, email: &str, time: i64) -> Self {
        let dir = tempfile::TempDir::new().expect("temporary directory");
        let root = dir.path().join("repo");
        std::fs::create_dir(&root).expect("create repository directory");
        let config = dir.path().join("gitconfig");
        std::fs::write(
            &config,
            format!(
                "[user]\n\tname = {name}\n\temail = {email}\n\
                 [commit]\n\tgpgsign = false\n\
                 [tag]\n\tgpgsign = false\n\
                 [core]\n\tautocrlf = false\n\
                 [init]\n\tdefaultBranch = main\n\
                 [protocol \"file\"]\n\tallow = always\n"
            ),
        )
        .expect("write test git config");

        Self {
            _dir: dir,
            root,
            config,
            author: (name.to_owned(), email.to_owned()),
            time,
        }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// The repository's git directory, `.git`, for a test that passes it to a
    /// watcher or reads a state file below it.
    pub fn git_dir(&self) -> PathBuf {
        self.path().join(".git")
    }

    /// The fixture's isolated global config file.
    pub fn config_file(&self) -> &Path {
        &self.config
    }

    /// The fixture's temporary directory, which holds the worktree and the
    /// config file but is not part of the repository.
    pub fn temp_root(&self) -> &Path {
        self._dir.path()
    }

    /// A path beside the worktree, inside the fixture's temporary directory,
    /// for a clone, a bare remote or a linked worktree.
    ///
    /// Nothing created here is inside the worktree, so a watcher test never
    /// mistakes the fixture's own files for a change.
    pub fn temp_path(&self, name: &str) -> PathBuf {
        self.temp_root().join(name)
    }

    /// [`Self::temp_path`], with the directory created.
    pub fn scratch_dir(&self, name: &str) -> PathBuf {
        let path = self.temp_path(name);
        std::fs::create_dir_all(&path).expect("create scratch directory");
        path
    }

    /// Write a file relative to the worktree, creating parent directories.
    pub fn write(&self, path: &str, contents: &str) {
        self.write_bytes(path, contents.as_bytes());
    }

    /// Read a file relative to the worktree.
    pub fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.path().join(path)).expect("read file")
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

    /// Write a file relative to `dir`, which may be outside the worktree (a
    /// clone or a linked worktree), creating parent directories.
    pub fn write_in(&self, dir: impl AsRef<Path>, path: &str, contents: &str) {
        let path = dir.as_ref().join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent directory");
        }
        std::fs::write(path, contents).expect("write file");
    }

    /// Run git and require success, returning its stdout as text.
    pub fn git(&self, args: &[&str]) -> String {
        self.git_in(self.path(), args)
    }

    /// Run git in `cwd`, for clones and linked worktrees that are not the
    /// fixture's own worktree.
    pub fn git_in(&self, cwd: impl AsRef<Path>, args: &[&str]) -> String {
        let output = self.git_output_in(cwd, args);
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Run git and return the raw result, for commands a test expects to fail.
    pub fn git_output(&self, args: &[&str]) -> Output {
        self.git_output_in(self.path(), args)
    }

    /// [`Self::git_output`] in `cwd`.
    pub fn git_output_in(&self, cwd: impl AsRef<Path>, args: &[&str]) -> Output {
        self.command_in("git", cwd)
            .args(args)
            .output()
            .expect("run git")
    }

    /// Run git with `input` on stdin and require success, for `fast-import`,
    /// `hash-object --stdin` and `update-ref --stdin`.
    pub fn git_input(&self, args: &[&str], input: &str) -> String {
        self.git_input_in(self.path(), args, input)
    }

    /// [`Self::git_input`] in `cwd`.
    pub fn git_input_in(&self, cwd: impl AsRef<Path>, args: &[&str], input: &str) -> String {
        let mut child = self
            .command_in("git", cwd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn git");
        child
            .stdin
            .as_mut()
            .expect("git stdin")
            .write_all(input.as_bytes())
            .expect("write git stdin");
        let output = child.wait_with_output().expect("git output");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Run git with `seconds` as the author and committer date, for tests
    /// about history ordering.
    pub fn git_dated(&self, seconds: i64, args: &[&str]) -> String {
        self.git_dated_in(self.path(), seconds, args)
    }

    /// [`Self::git_dated`] in `cwd`.
    pub fn git_dated_in(&self, cwd: impl AsRef<Path>, seconds: i64, args: &[&str]) -> String {
        let date = format!("{seconds} +0000");
        let output = self
            .command_in("git", cwd)
            .args(args)
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Stage everything and commit with `message`, returning the new commit.
    pub fn commit(&self, message: &str) -> Oid {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
        self.head()
    }

    /// Stage everything and commit as `name <email>`, for a repository whose
    /// history should carry another author.
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
        self.git_handle_in(self.path())
    }

    /// [`Self::git_handle`] for another working tree, such as a clone.
    pub fn git_handle_in(&self, workdir: impl AsRef<Path>) -> Git {
        let (name, email) = &self.author;
        Git::new(workdir.as_ref().to_path_buf())
            .with_env("GIT_CONFIG_GLOBAL", &self.config)
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
            .with_env("GIT_AUTHOR_NAME", name)
            .with_env("GIT_AUTHOR_EMAIL", email)
            .with_env("GIT_COMMITTER_NAME", name)
            .with_env("GIT_COMMITTER_EMAIL", email)
            .with_env("GIT_AUTHOR_DATE", self.date_value())
            .with_env("GIT_COMMITTER_DATE", self.date_value())
    }

    /// A `git` command with the fixture environment applied.
    pub fn command(&self, program: &str) -> Command {
        self.command_in(program, self.path())
    }

    /// [`Self::command`] in `cwd`.
    pub fn command_in(&self, program: &str, cwd: impl AsRef<Path>) -> Command {
        let (name, email) = &self.author;
        let mut command = Command::new(program);
        command
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", &self.config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_COMMITTER_NAME", name)
            .env("GIT_COMMITTER_EMAIL", email)
            .env("GIT_AUTHOR_DATE", self.date_value())
            .env("GIT_COMMITTER_DATE", self.date_value())
            .env("GIT_TERMINAL_PROMPT", "0")
            // A test that runs inside a git hook or another checkout must not
            // inherit a repository location.
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE");
        command
    }

    fn date_value(&self) -> OsString {
        format!("{} +0000", self.time).into()
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
