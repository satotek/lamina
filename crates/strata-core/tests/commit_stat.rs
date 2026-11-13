//! Integration tests for commit-wide line statistics, checked against
//! `git show --numstat` because the git semantics are the contract.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use strata_core::repo::Repo;
use strata_core::session::{Session, SessionEvent};
use strata_core::{CommitStat, Oid};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(60);

struct TestRepo {
    dir: TempDir,
    /// Holds the empty config file; kept alive for the lifetime of the test.
    _env: TempDir,
    config: PathBuf,
}

impl TestRepo {
    fn new() -> Self {
        let dir = TempDir::new().expect("repo temp dir");
        let env = TempDir::new().expect("env temp dir");
        let config = env.path().join("gitconfig");
        fs::write(&config, "").expect("write empty config");

        let repo = TestRepo {
            dir,
            _env: env,
            config,
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn open(&self) -> Repo {
        Repo::open(self.path()).expect("open repository")
    }

    fn write(&self, relative: &str, contents: &str) {
        self.write_bytes(relative, contents.as_bytes());
    }

    fn write_bytes(&self, relative: &str, contents: &[u8]) {
        let path = self.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(path, contents).expect("write file");
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command
            .current_dir(self.path())
            .args(args)
            .env("GIT_CONFIG_GLOBAL", &self.config)
            .env("GIT_CONFIG_SYSTEM", &self.config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Strata Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Strata Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00 +0000")
            .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00 +0000")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE");
        command
    }

    fn git(&self, args: &[&str]) -> String {
        let output = self.command(args).output().expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("git stdout is UTF-8")
    }

    fn head(&self) -> Oid {
        self.git(&["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("parse HEAD")
    }

    /// `git show --numstat` as (path -> (added, removed, binary)), the
    /// reference for a commit against its first parent (or the empty tree).
    fn git_numstat(&self, commit: Oid) -> BTreeMap<String, (u64, u64, bool)> {
        let output = self.git(&[
            "show",
            "--numstat",
            "--format=",
            "--no-renames",
            &commit.to_string(),
        ]);
        output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let mut fields = line.splitn(3, '\t');
                let added = fields.next().expect("added");
                let removed = fields.next().expect("removed");
                let path = fields.next().expect("path").to_owned();
                match (added.parse(), removed.parse()) {
                    (Ok(added), Ok(removed)) => (path, (added, removed, false)),
                    // git prints `-` for a binary file.
                    _ => (path, (0, 0, true)),
                }
            })
            .collect()
    }
}

fn stat_of(repo: &TestRepo, commit: Oid) -> CommitStat {
    repo.open().commit_stat(commit).expect("commit stat")
}

fn assert_matches_git(repo: &TestRepo, commit: Oid, stat: &CommitStat) {
    let expected = repo.git_numstat(commit);
    let actual: BTreeMap<String, (u64, u64, bool)> = stat
        .files
        .iter()
        .map(|file| (file.path.clone(), (file.added, file.removed, file.binary)))
        .collect();
    assert_eq!(actual, expected, "per-file counts");

    let added: u64 = expected.values().map(|(added, _, _)| added).sum();
    let removed: u64 = expected.values().map(|(_, removed, _)| removed).sum();
    assert_eq!((stat.added, stat.removed), (added, removed), "totals");
}

#[test]
fn commit_stat_counts_every_added_line_of_a_root_commit() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\ntwo\n");
    repo.write("dir/b.txt", "x\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "root"]);
    let commit = repo.head();

    let stat = stat_of(&repo, commit);
    assert_matches_git(&repo, commit, &stat);
    assert_eq!((stat.added, stat.removed), (3, 0));
    assert_eq!(stat.files.len(), 2);
    assert!(stat.files.iter().all(|file| !file.binary));
}

#[test]
fn commit_stat_compares_against_the_first_parent() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\ntwo\n");
    repo.write("dir/b.txt", "x\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "root"]);

    // A modification, a deletion and an addition in one commit.
    repo.write("a.txt", "one\nthree\n");
    fs::remove_file(repo.path().join("dir/b.txt")).expect("remove file");
    repo.write("c.txt", "new\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "second"]);
    let commit = repo.head();

    let stat = stat_of(&repo, commit);
    assert_matches_git(&repo, commit, &stat);
    assert_eq!((stat.added, stat.removed), (2, 2));
    let paths: Vec<&str> = stat.files.iter().map(|file| file.path.as_str()).collect();
    assert_eq!(paths, ["a.txt", "c.txt", "dir/b.txt"], "sorted by path");
}

#[test]
fn commit_stat_flags_a_binary_file_without_counting_lines() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "root"]);

    repo.write_bytes("bin.dat", &[0, 1, 2, 3, 0]);
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "binary"]);
    let commit = repo.head();

    let stat = stat_of(&repo, commit);
    assert_matches_git(&repo, commit, &stat);
    let binary = stat
        .files
        .iter()
        .find(|file| file.path == "bin.dat")
        .expect("binary file in stat");
    assert!(binary.binary);
    assert_eq!((binary.added, binary.removed), (0, 0));
    assert_eq!((stat.added, stat.removed), (0, 0), "binary adds no lines");
}

#[test]
fn session_request_commit_stat_answers_with_the_same_counts() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "root"]);
    repo.write("a.txt", "one\ntwo\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "second"]);
    let commit = repo.head();

    let expected = repo.open().commit_stat(commit).expect("commit stat");
    let (tx, rx) = mpsc::channel();
    // Declared after `repo`, so the session's threads stop before the temp
    // repository disappears.
    let session = Session::open(repo.path(), move |event| {
        let _ = tx.send(event);
    })
    .expect("open session");
    let ticket = session.request_commit_stat(commit);

    let deadline = Instant::now() + TIMEOUT;
    let stat = loop {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for {ticket:?}");
        match rx.recv_timeout(deadline - now) {
            Ok(SessionEvent::CommitStatReady { ticket: got, stat }) if got == ticket => break stat,
            Ok(SessionEvent::QueryFailed { ticket: got, error }) if got == ticket => {
                panic!("commit stat {ticket:?} failed: {error}")
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("session event channel closed"),
        }
    };
    assert_eq!(*stat, expected);
}
