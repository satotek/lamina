//! Integration tests for the repository watcher against real git repositories.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use lamina_core::watch::{Changes, RepoWatcher};
use tempfile::TempDir;

/// Generous: inotify, FSEvents and Windows all deliver with different latencies.
const TIMEOUT: Duration = Duration::from_secs(10);
const DEBOUNCE: Duration = Duration::from_millis(100);

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
        std::fs::write(repo.config_file(), "").expect("empty git config");
        repo.git(&["init", "--initial-branch=main"]);
        repo
    }

    fn config_file(&self) -> PathBuf {
        self.config.path().join("gitconfig")
    }

    fn path(&self) -> &Path {
        self.workdir.path()
    }

    fn git_dir(&self) -> PathBuf {
        self.path().join(".git")
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.path().join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(path, contents).expect("write file");
    }

    /// Run git with an isolated configuration so the host's config and hooks
    /// cannot influence the test.
    fn git(&self, args: &[&str]) -> String {
        self.git_in(self.path(), args)
    }

    /// Like [`Self::git`], but with `cwd` as the repository (e.g. a linked
    /// worktree).
    fn git_in(&self, cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", self.config_file())
            .env("GIT_CONFIG_SYSTEM", self.config_file())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("XDG_CONFIG_HOME", self.config.path())
            .env("GIT_AUTHOR_NAME", "Lamina Test")
            .env("GIT_AUTHOR_EMAIL", "lamina@example.com")
            .env("GIT_COMMITTER_NAME", "Lamina Test")
            .env("GIT_COMMITTER_EMAIL", "lamina@example.com")
            .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

struct Harness {
    // Declared first so the watcher stops before the temp dirs are removed.
    _watcher: RepoWatcher,
    rx: Receiver<Changes>,
    repo: TestRepo,
}

#[test]
fn editing_a_file_reports_worktree() {
    let harness = Harness::start(None, &[]);
    harness.repo.write("tracked.txt", "two\n");

    let seen = collect_until(&harness.rx, TIMEOUT, |changes| changes.worktree);
    assert!(
        !seen.is_empty(),
        "no worktree change within {TIMEOUT:?} after editing a file"
    );
    let expected = harness.repo.path().join("tracked.txt");
    assert!(
        seen.iter().any(|changes| changes.paths.contains(&expected)),
        "`{expected:?}` missing from {:?}",
        seen.iter().map(|c| &c.paths).collect::<Vec<_>>()
    );
}

#[test]
fn git_add_reports_index() {
    let harness = Harness::start(None, &[]);
    harness.repo.write("added.txt", "added\n");
    harness.repo.git(&["add", "added.txt"]);

    let seen = collect_until(&harness.rx, TIMEOUT, |changes| changes.index);
    assert!(
        !seen.is_empty(),
        "no index change within {TIMEOUT:?} after `git add`"
    );
}

#[test]
fn git_commit_reports_refs_or_head() {
    let harness = Harness::start(None, &[]);
    harness.repo.write("tracked.txt", "two\n");
    harness.repo.git(&["add", "tracked.txt"]);
    settle(&harness.rx);

    harness.repo.git(&["commit", "-m", "second"]);
    let seen = collect_until(&harness.rx, TIMEOUT, |changes| changes.refs || changes.head);
    assert!(
        !seen.is_empty(),
        "no refs/head change within {TIMEOUT:?} after `git commit`"
    );
}

#[test]
fn git_checkout_b_reports_head() {
    let harness = Harness::start(None, &[]);
    harness.repo.git(&["checkout", "-b", "feature"]);

    let seen = collect_until(&harness.rx, TIMEOUT, |changes| changes.head);
    assert!(
        !seen.is_empty(),
        "no head change within {TIMEOUT:?} after `git checkout -b`"
    );
}

#[test]
fn linked_worktree_with_external_git_dir() {
    let repo = TestRepo::new();
    repo.write("tracked.txt", "one\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-m", "initial"]);

    // A linked worktree's git dir lives under the main repository, outside the
    // worktree itself; the caller resolves it and passes it in.
    let worktree_parent = TempDir::new().expect("worktree parent temp dir");
    let worktree = worktree_parent.path().join("linked");
    let worktree_arg = worktree.to_str().expect("utf-8 temp path");
    repo.git(&["worktree", "add", "-b", "linked", worktree_arg]);
    let git_dir = PathBuf::from(
        repo.git_in(&worktree, &["rev-parse", "--absolute-git-dir"])
            .trim(),
    );
    assert!(
        !git_dir.starts_with(&worktree),
        "expected the git dir outside the worktree, got {git_dir:?}"
    );

    let (tx, rx) = channel();
    let _watcher = RepoWatcher::new(Some(&worktree), &git_dir, DEBOUNCE, move |changes| {
        let _ = tx.send(changes);
    })
    .expect("start watcher");

    std::fs::write(worktree.join("warmup.txt"), "warm\n").expect("write warm-up file");
    assert!(
        !collect_until(&rx, TIMEOUT, |changes| changes.worktree).is_empty(),
        "watcher never reported a worktree change in the linked worktree"
    );
    settle(&rx);

    repo.git_in(&worktree, &["add", "tracked.txt"]);
    let seen = collect_until(&rx, TIMEOUT, |changes| changes.index);
    assert!(
        !seen.is_empty(),
        "`git add` in a linked worktree did not report the external git dir's index"
    );
}

#[test]
fn continuous_stream_is_flushed_every_second() {
    let harness = Harness::start(None, &["stream"]);

    // Keeping the stream alive for well over a second must not starve the
    // callback: the one-second latency cap forces periodic flushes even though
    // the quiet window never settles.
    let started = Instant::now();
    let mut next_write = started;
    let mut callbacks = 0;
    let mut written = 0;
    while started.elapsed() < Duration::from_millis(2500) {
        if Instant::now() >= next_write {
            harness
                .repo
                .write(&format!("stream/{written}.txt"), "continuous\n");
            written += 1;
            next_write += Duration::from_millis(40);
        }
        if harness.rx.try_recv().is_ok() {
            callbacks += 1;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(
        callbacks >= 2,
        "continuous stream produced only {callbacks} refresh(es) in 2.5s"
    );
}

#[test]
fn ignored_directory_produces_no_refresh() {
    let harness = Harness::start(
        Some("target/\nnode_modules/\n"),
        &["target/debug", "node_modules/pkg"],
    );

    for i in 0..20 {
        harness
            .repo
            .write(&format!("target/debug/artifact{i}.bin"), "build output");
        harness
            .repo
            .write(&format!("node_modules/pkg/index{i}.js"), "module");
    }

    // Longer than a couple of debounce windows: a refresh caused by these
    // writes would certainly arrive in time.
    match harness.rx.recv_timeout(DEBOUNCE * 8) {
        Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {}
        Ok(changes) => panic!("ignored writes reported {changes:?}"),
    }
}

#[test]
fn burst_of_writes_is_coalesced() {
    let harness = Harness::start(None, &["burst"]);

    for i in 0..200 {
        harness.repo.write(&format!("burst/file{i}.txt"), "burst\n");
    }

    let seen = collect_quiet(&harness.rx, DEBOUNCE * 3, TIMEOUT);
    assert!(!seen.is_empty(), "200 writes produced no refresh at all");
    assert!(
        seen.len() <= 8,
        "200 writes produced {} refreshes: {:?}",
        seen.len(),
        seen.iter()
            .map(|c| (c.worktree, c.index, c.refs, c.head))
            .collect::<Vec<_>>()
    );
    assert!(
        seen.iter().any(|changes| changes.worktree),
        "no worktree flag in {seen:?}"
    );
}

impl Harness {
    /// Create a repository with one commit, an optional `.gitignore` and a few
    /// empty directories, then start watching it.
    fn start(gitignore: Option<&str>, directories: &[&str]) -> Self {
        let repo = TestRepo::new();
        if let Some(contents) = gitignore {
            repo.write(".gitignore", contents);
        }
        repo.write("tracked.txt", "one\n");
        for directory in directories {
            std::fs::create_dir_all(repo.path().join(directory)).expect("create dir");
        }
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-m", "initial"]);

        let (tx, rx) = channel();
        let watcher = RepoWatcher::new(
            Some(repo.path()),
            &repo.git_dir(),
            DEBOUNCE,
            move |changes| {
                let _ = tx.send(changes);
            },
        )
        .expect("start watcher");

        let harness = Self {
            _watcher: watcher,
            rx,
            repo,
        };
        harness.warm_up();
        harness
    }

    /// Prove the pipeline is live before asserting on a specific action.
    ///
    /// Watch registration can race with the first events on some backends
    /// (FSEvents in particular), so every test pays for one known-good write
    /// and then waits until the stream is quiet again.
    fn warm_up(&self) {
        self.repo.write("warmup.txt", "warm\n");
        let seen = collect_until(&self.rx, TIMEOUT, |changes| changes.worktree);
        assert!(!seen.is_empty(), "watcher never reported the warm-up write");
        settle(&self.rx);
    }
}

/// Collect changes until `predicate` holds or `timeout` elapses.
fn collect_until(
    rx: &Receiver<Changes>,
    timeout: Duration,
    predicate: impl Fn(&Changes) -> bool,
) -> Vec<Changes> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match rx.recv_timeout(deadline - now) {
            Ok(changes) => {
                let matched = predicate(&changes);
                seen.push(changes);
                if matched {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    seen
}

/// Collect changes until the channel has been quiet for `quiet`.
fn collect_quiet(rx: &Receiver<Changes>, quiet: Duration, max: Duration) -> Vec<Changes> {
    let started = Instant::now();
    let mut seen = Vec::new();
    while started.elapsed() < max {
        match rx.recv_timeout(quiet) {
            Ok(changes) => seen.push(changes),
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    seen
}

/// Wait until no further change arrives for a few debounce windows, then make
/// sure nothing stale is left in the channel.
fn settle(rx: &Receiver<Changes>) {
    let _ = collect_quiet(rx, DEBOUNCE * 3, DEBOUNCE * 3);
    while rx.try_recv().is_ok() {}
}
