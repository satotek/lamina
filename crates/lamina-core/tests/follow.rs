//! Integration tests for rename following in path-limited history.
//!
//! The walks are checked against `git log --follow`, because the contract is
//! git's rename semantics, not this implementation's own expectations.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use lamina_core::Oid;
use lamina_core::repo::{PathHistoryEntry, Repo};
use tempfile::TempDir;

const BASE_TIME: i64 = 1_700_000_000;

/// A repository in a temp directory driven by the `git` CLI with an isolated
/// configuration, so nothing depends on the machine's git setup.
struct Fixture {
    /// Owns the temp directory; never read, only kept alive.
    _tmp: TempDir,
    repo: PathBuf,
    empty_config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().expect("temp dir");
        let repo = tmp.path().join("repo");
        let empty_config = tmp.path().join("empty-gitconfig");
        fs::write(&empty_config, b"").expect("empty config");
        fs::create_dir_all(&repo).expect("repo dir");

        let fixture = Self {
            _tmp: tmp,
            repo,
            empty_config,
        };
        fixture.git(&["init", "--quiet", "--initial-branch=main"]);
        fixture
    }

    fn git(&self, args: &[&str]) -> String {
        self.run(args, BASE_TIME)
    }

    fn git_dated(&self, date: i64, args: &[&str]) -> String {
        self.run(args, date)
    }

    fn run(&self, args: &[&str], date: i64) -> String {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(&self.repo)
            .env("GIT_CONFIG_GLOBAL", &self.empty_config)
            .env("GIT_CONFIG_SYSTEM", &self.empty_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Lamina Author")
            .env("GIT_AUTHOR_EMAIL", "author@example.com")
            .env("GIT_COMMITTER_NAME", "Lamina Committer")
            .env("GIT_COMMITTER_EMAIL", "committer@example.com")
            .env("GIT_AUTHOR_DATE", format!("@{date} +0000"))
            .env("GIT_COMMITTER_DATE", format!("@{date} +0000"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("git is runnable");
        child
            .stdin
            .as_mut()
            .expect("piped stdin")
            .write_all(b"")
            .expect("write git stdin");
        let output = child.wait_with_output().expect("git finishes");

        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string()
    }

    fn write(&self, file: &str, content: &str) {
        let path = self.repo.join(file);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("file parent");
        }
        fs::write(&path, content).expect("write file");
    }

    fn commit(&self, file: &str, content: &str, date: i64, message: &str) -> String {
        self.write(file, content);
        self.commit_paths(&[file], date, message)
    }

    fn commit_paths(&self, paths: &[&str], date: i64, message: &str) -> String {
        let mut args = vec!["add", "--"];
        args.extend(paths.iter().copied());
        self.git(&args);
        self.git_dated(date, &["commit", "--quiet", "-m", message]);
        self.head()
    }

    /// `git mv` plus whatever else `edit` writes, committed as one change.
    fn rename(
        &self,
        from: &str,
        to: &str,
        date: i64,
        message: &str,
        edit: impl FnOnce(),
    ) -> String {
        self.git(&["mv", "--", from, to]);
        edit();
        self.git(&["add", "--all"]);
        self.git_dated(date, &["commit", "--quiet", "-m", message]);
        self.head()
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    fn open(&self) -> Repo {
        Repo::open(&self.repo).expect("open repo")
    }

    /// The reference: `git log --follow` over the same path, newest first.
    fn git_follow(&self, path: &str) -> Vec<String> {
        self.git(&[
            "log",
            "--follow",
            "--date-order",
            "--format=%H",
            "HEAD",
            "--",
            path,
        ])
        .lines()
        .map(str::to_string)
        .collect()
    }

    fn entries(&self, path: &str) -> Vec<(Oid, String)> {
        self.open()
            .path_history_entries(path)
            .expect("path history")
            .map(|entry| {
                let PathHistoryEntry { commit, path } = entry.expect("history row");
                (commit, path)
            })
            .collect()
    }
}

fn ids(rows: &[(Oid, String)]) -> Vec<String> {
    rows.iter().map(|(id, _)| id.to_string()).collect()
}

#[test]
fn path_history_follows_a_file_across_a_rename() {
    let fixture = Fixture::new();
    // Enough lines that a rename plus a small edit stays above git's 50%
    // similarity threshold.
    let body = "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\n";
    let first = fixture.commit("src/old_name.txt", body, BASE_TIME + 100, "add file");
    let second = fixture.commit(
        "src/old_name.txt",
        &format!("{body}line 7\n"),
        BASE_TIME + 200,
        "edit before rename",
    );
    let third = fixture.rename(
        "src/old_name.txt",
        "src/new_name.txt",
        BASE_TIME + 300,
        "rename file",
        || {},
    );
    let fourth = fixture.commit(
        "src/new_name.txt",
        &format!("{body}line 7\nline 8\n"),
        BASE_TIME + 400,
        "edit after rename",
    );

    let history = fixture.entries("src/new_name.txt");
    assert_eq!(
        ids(&history),
        vec![fourth.clone(), third.clone(), second.clone(), first.clone()],
        "the walk continues past the rename into the old name"
    );
    assert_eq!(
        history
            .iter()
            .map(|(_, path)| path.as_str())
            .collect::<Vec<_>>(),
        vec![
            "src/new_name.txt",
            "src/new_name.txt",
            "src/old_name.txt",
            "src/old_name.txt",
        ],
        "each row carries the name the file had at that commit"
    );
    assert_eq!(ids(&history), fixture.git_follow("src/new_name.txt"));

    // The id-only walk follows the same commits, so existing callers see the
    // richer history without changing their item type.
    let plain: Vec<String> = fixture
        .open()
        .path_history("src/new_name.txt")
        .expect("path history")
        .map(|id| id.expect("commit id").to_string())
        .collect();
    assert_eq!(plain, ids(&history));
}

#[test]
fn path_history_follows_a_rename_committed_with_an_edit() {
    let fixture = Fixture::new();
    let body = "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\n";
    let first = fixture.commit("old.txt", body, BASE_TIME + 100, "add file");
    let second = fixture.commit(
        "old.txt",
        &format!("{body}iota\n"),
        BASE_TIME + 200,
        "edit before rename",
    );
    // Rename and edit in one commit: gix has to compare contents to pair the
    // deletion with the addition.
    let renamed = fixture.rename(
        "old.txt",
        "new.txt",
        BASE_TIME + 300,
        "rename and edit",
        || {
            fixture.write("new.txt", &format!("{body}iota\nkappa\n"));
        },
    );

    let history = fixture.entries("new.txt");
    assert_eq!(
        ids(&history),
        vec![renamed, second, first],
        "the rename commit is reported under its new name"
    );
    assert_eq!(
        history
            .iter()
            .map(|(_, path)| path.as_str())
            .collect::<Vec<_>>(),
        vec!["new.txt", "old.txt", "old.txt"]
    );
    assert_eq!(ids(&history), fixture.git_follow("new.txt"));
}

#[test]
fn path_history_stops_at_a_rewrite_too_different_to_be_a_rename() {
    let fixture = Fixture::new();
    let first = fixture.commit(
        "old.txt",
        "one\ntwo\nthree\nfour\nfive\n",
        BASE_TIME + 100,
        "add file",
    );
    // All content replaced, so neither git nor this walk can pair the deletion
    // with the addition, and history for the new name starts at the rewrite.
    let rewritten = fixture.rename("old.txt", "new.txt", BASE_TIME + 200, "rewrite", || {
        fixture.write(
            "new.txt",
            "completely\ndifferent\nwords\nwithout\nany\noverlap\n",
        );
    });

    let history = fixture.entries("new.txt");
    assert_eq!(ids(&history), vec![rewritten]);
    assert!(!ids(&history).contains(&first));
    assert_eq!(ids(&history), fixture.git_follow("new.txt"));
}
