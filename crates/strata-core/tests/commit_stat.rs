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
use strata_core::{ChangeKind, CommitStat, Oid};
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
fn commit_stat_counts_a_rename_as_a_deletion_and_an_addition() {
    let repo = TestRepo::new();
    repo.write("old.txt", "one\ntwo\nthree\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "root"]);

    // A rename that also edits the file: without rename detection the old path
    // is deleted in full and the new one added in full.
    repo.write("new.txt", "one\ntwo\nthree\nfour\n");
    fs::remove_file(repo.path().join("old.txt")).expect("remove old file");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "rename and edit"]);
    let commit = repo.head();

    let stat = stat_of(&repo, commit);
    assert_matches_git(&repo, commit, &stat);
    let paths: Vec<&str> = stat.files.iter().map(|file| file.path.as_str()).collect();
    assert_eq!(paths, ["new.txt", "old.txt"]);
}

#[test]
fn a_stat_after_the_file_list_reuses_the_rename_aware_diff() {
    let repo = TestRepo::new();
    repo.write("old.txt", "one\ntwo\nthree\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "root"]);

    repo.write("new.txt", "one\ntwo\nthree\nfour\n");
    fs::remove_file(repo.path().join("old.txt")).expect("remove old file");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "rename and edit"]);
    let commit = repo.head();

    // The UI asks for the file list and the line counts of one commit. The
    // cached list detects the rename, while the counts still have to match
    // `--no-renames`.
    let handle = repo.open();
    let changes = handle.commit_changes(commit).expect("commit changes");
    assert!(
        changes
            .iter()
            .any(|change| change.kind == ChangeKind::Renamed),
        "the file list should detect the rename: {changes:?}"
    );
    let stat = handle.commit_stat(commit).expect("commit stat");
    assert_matches_git(&repo, commit, &stat);
}

#[test]
fn commit_stat_skips_a_file_too_large_to_count() {
    let repo = TestRepo::new();
    repo.write("small.txt", "one\n");
    // Counting this would cost more than the commit view is worth; it is
    // reported like a binary file instead of being diffed in full.
    let mut large = String::new();
    let line = "0123456789abcdef0123456789abcdef\n";
    while large.len() <= 4 * 1024 * 1024 {
        large.push_str(line);
    }
    repo.write("large.txt", &large);
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "large file"]);
    let commit = repo.head();

    let stat = stat_of(&repo, commit);
    let large = stat
        .files
        .iter()
        .find(|file| file.path == "large.txt")
        .expect("large file in stat");
    assert!(large.binary, "a file over the stat limit is flagged");
    assert_eq!((large.added, large.removed), (0, 0));
    let small = stat
        .files
        .iter()
        .find(|file| file.path == "small.txt")
        .expect("small file in stat");
    assert!(!small.binary);
    assert_eq!((small.added, small.removed), (1, 0));
}

/// One commit touching 3000 files with half of every file rewritten: the shape
/// of a huge merge's file list, where counting must not render every line.
#[test]
#[ignore = "perf: builds a 3000-file commit"]
fn commit_stat_on_3000_files_is_fast() {
    let repo = TestRepo::new();
    for index in 0..3000 {
        let mut body = String::new();
        for line in 0..100 {
            body.push_str(&format!(
                "line {line} of file {index} original content with some words\n"
            ));
        }
        repo.write(&format!("src/file{index:04}.txt"), &body);
    }
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "base"]);

    for index in 0..3000 {
        let mut body = String::new();
        for line in 0..100 {
            if line % 2 == 0 {
                body.push_str(&format!(
                    "line {line} of file {index} changed content with other words\n"
                ));
            } else {
                body.push_str(&format!(
                    "line {line} of file {index} original content with some words\n"
                ));
            }
        }
        repo.write(&format!("src/file{index:04}.txt"), &body);
    }
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "touch 3000 files"]);
    let commit = repo.head();

    let start = Instant::now();
    let stat = stat_of(&repo, commit);
    let elapsed = start.elapsed();
    eprintln!("commit_stat over 3000 changed files: {elapsed:?}");
    assert_eq!(stat.files.len(), 3000);
    assert_eq!((stat.added, stat.removed), (150_000, 150_000));

    // Debug builds are for coverage, not for the timing contract.
    let limit = if cfg!(debug_assertions) {
        Duration::from_secs(10)
    } else {
        Duration::from_millis(500)
    };
    assert!(
        elapsed < limit,
        "stat took {elapsed:?}, expected under {limit:?}"
    );
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

#[test]
fn parallel_commit_stat_and_blob_cache_match_git_across_mixed_edge_cases() {
    let repo = TestRepo::new();

    // Create enough files (>= MIN_PARALLEL_STAT_FILES = 64) to exercise chunked
    // parallel workers alongside binaries, renames, empty-blob transitions, and
    // files > 4 MiB.
    for i in 0..80 {
        let content = format!(
            "// header line 1\n// header line 2\nbody line A of {i}\nbody line B of {i}\n// footer 1\n// footer 2\n"
        );
        repo.write(&format!("mod/file_{i:03}.rs"), &content);
    }
    for i in 0..10 {
        repo.write(
            &format!("rename/src_{i:02}.txt"),
            &format!("prefix\nline {i} a\nline {i} b\nline {i} c\nline {i} d\nsuffix\n"),
        );
    }
    repo.write_bytes("bin/old.dat", &[0, 1, 2, 3, 4]);
    repo.write("to_empty.txt", "alpha\nbeta\n");
    repo.write("from_empty.txt", "");

    let mut huge_old = String::new();
    let unit = "0123456789abcdef0123456789abcdef0123456789abcdef\n";
    while huge_old.len() <= 4 * 1024 * 1024 + 256 {
        huge_old.push_str(unit);
    }
    repo.write("huge_mod.txt", &huge_old);
    repo.write("huge_renamed_old.txt", &huge_old);
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "base"]);

    // Modify the 80 text files across different edit shapes (prefix/suffix/middle/EOF newline).
    for i in 0..80 {
        let content = match i % 4 {
            0 => format!(
                "// SPDX added\n// header line 1\n// header line 2\nbody line A of {i}\nbody line B of {i}\n// footer 1\n// footer 2\n"
            ),
            1 => format!(
                "// header line 1\n// header line 2\nbody changed {i}\n// footer 1\n// footer 2\n"
            ),
            2 => format!(
                "// header line 1\n// header line 2\nbody line A of {i}\nbody line B of {i}\n// footer 1\n// footer 2"
            ),
            _ => format!(
                "// header line 1\n// header line 2\nbody line A of {i}\nbody line B of {i}\n// footer 1\n// footer 2\nadded\n"
            ),
        };
        repo.write(&format!("mod/file_{i:03}.rs"), &content);
    }
    for i in 0..10 {
        fs::remove_file(repo.path().join(format!("rename/src_{i:02}.txt"))).expect("rm rename src");
        repo.write(
            &format!("rename/dst_{i:02}.txt"),
            &format!(
                "prefix\nline {i} a\nline {i} b\nline {i} c\nline {i} d\nline {i} e\nsuffix\n"
            ),
        );
    }
    repo.write_bytes("bin/new.dat", &[0, 9, 8, 7]);
    repo.write("to_empty.txt", "");
    repo.write("from_empty.txt", "now populated\nline 2\n");
    repo.write("huge_mod.txt", &format!("{huge_old}extra\n"));
    fs::remove_file(repo.path().join("huge_renamed_old.txt")).expect("rm huge old");
    repo.write(
        "huge_renamed_new.txt",
        &format!("{huge_old}renamed extra\n"),
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "mixed parallel commit"]);
    let commit = repo.head();

    // Order 1: stat then changes.
    let h1 = repo.open();
    let stat1 = h1.commit_stat(commit).expect("stat1");
    let changes1 = h1.commit_changes(commit).expect("changes1");

    // Order 2: changes then stat.
    let h2 = repo.open();
    let changes2 = h2.commit_changes(commit).expect("changes2");
    let stat2 = h2.commit_stat(commit).expect("stat2");

    assert_eq!(stat1, stat2);
    assert_eq!(changes1, changes2);
    assert!(
        stat1
            .files
            .iter()
            .filter(|f| f.path.starts_with("huge_"))
            .all(|f| f.binary && f.added == 0 && f.removed == 0)
    );
    // Huge file rename must still be paired by commit_changes despite being flagged in stat.
    let huge_change = changes1
        .iter()
        .find(|c| c.path == "huge_renamed_new.txt")
        .expect("huge rename in changes");
    assert_eq!(huge_change.kind, ChangeKind::Renamed);
    assert_eq!(
        huge_change.old_path.as_deref(),
        Some("huge_renamed_old.txt")
    );

    // Verify all non-huge files match `git show --numstat --no-renames` verbatim.
    let git_expected = repo.git_numstat(commit);
    let actual_non_huge: BTreeMap<String, (u64, u64, bool)> = stat1
        .files
        .iter()
        .filter(|f| !f.path.starts_with("huge_"))
        .map(|f| (f.path.clone(), (f.added, f.removed, f.binary)))
        .collect();
    let expected_non_huge: BTreeMap<String, (u64, u64, bool)> = git_expected
        .into_iter()
        .filter(|(p, _)| !p.starts_with("huge_"))
        .collect();
    assert_eq!(actual_non_huge, expected_non_huge);
}
