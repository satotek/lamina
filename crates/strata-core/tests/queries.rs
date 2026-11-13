//! Integration tests for path-limited history and session queries.
//!
//! Path history is checked against `git log`, because the contract is the git
//! semantics, not this implementation's own expectations.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use strata_core::repo::Repo;
use strata_core::session::{CommitQuery, QueryTicket, RepoSnapshot, Session, SessionEvent};
use tempfile::TempDir;

const BASE_TIME: i64 = 1_700_000_000;
const TIMEOUT: Duration = Duration::from_secs(60);

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

    fn path(&self) -> &Path {
        &self.repo
    }

    fn git(&self, args: &[&str]) -> String {
        self.run(&self.repo, args, BASE_TIME, "")
    }

    fn git_dated(&self, date: i64, args: &[&str]) -> String {
        self.run(&self.repo, args, date, "")
    }

    fn git_input(&self, args: &[&str], input: &str) -> String {
        self.run(&self.repo, args, BASE_TIME, input)
    }

    fn run(&self, dir: &Path, args: &[&str], date: i64, input: &str) -> String {
        let mut command = self.command(dir, args, date);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("git is runnable");
        child
            .stdin
            .as_mut()
            .expect("piped stdin")
            .write_all(input.as_bytes())
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

    /// `git` with the fixture's isolated environment, for a command whose
    /// failure is part of the check.
    fn command(&self, dir: &Path, args: &[&str], date: i64) -> Command {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", &self.empty_config)
            .env("GIT_CONFIG_SYSTEM", &self.empty_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Strata Author")
            .env("GIT_AUTHOR_EMAIL", "author@example.com")
            .env("GIT_COMMITTER_NAME", "Strata Committer")
            .env("GIT_COMMITTER_EMAIL", "committer@example.com")
            .env("GIT_AUTHOR_DATE", format!("@{date} +0000"))
            .env("GIT_COMMITTER_DATE", format!("@{date} +0000"))
            .env("GIT_TERMINAL_PROMPT", "0");
        command
    }

    fn ref_exists(&self, name: &str) -> bool {
        self.command(
            &self.repo,
            &["show-ref", "--verify", "--quiet", name],
            BASE_TIME,
        )
        .status()
        .expect("git is runnable")
        .success()
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

    fn remove(&self, file: &str, date: i64, message: &str) -> String {
        self.git(&["rm", "--quiet", "--", file]);
        self.git_dated(date, &["commit", "--quiet", "-m", message]);
        self.head()
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    fn open(&self) -> Repo {
        Repo::open(self.path()).expect("open repo")
    }

    fn history(&self, path: &str) -> Vec<String> {
        self.open()
            .path_history(path)
            .expect("path history")
            .map(|id| id.expect("commit id").to_string())
            .collect()
    }

    /// The reference this module documents: full history without rename
    /// following, over the same refs the walk uses, in the same `--date-order`
    /// the commit walk produces.
    fn git_history(&self, path: &str) -> Vec<String> {
        let mut args = vec![
            "log",
            "--full-history",
            "--date-order",
            "--format=%H",
            "HEAD",
            "--branches",
            "--remotes",
            "--tags",
        ];
        if self.ref_exists("refs/stash") {
            args.push("refs/stash");
        }
        args.push("--");
        args.push(path);
        self.git(&args).lines().map(str::to_string).collect()
    }

    fn write_commit_graph(&self) {
        self.git(&["commit-graph", "write", "--reachable"]);
    }

    /// Builds `commits` linear commits that each change `file.txt`, with
    /// `git fast-import`; a loop of `git commit` would dominate the test.
    fn fast_import(&self, commits: usize) {
        let mut stream = String::new();
        for index in 0..commits {
            stream.push_str(&format!(
                "blob\nmark :{mark}\ndata 6\n{index:06}\n",
                mark = index + 1
            ));
            let message = format!("commit {index}");
            stream.push_str("commit refs/heads/main\n");
            stream.push_str(&format!(
                "committer Strata Test <strata@example.com> {} +0000\n",
                BASE_TIME + index as i64
            ));
            stream.push_str(&format!("data {}\n{}\n", message.len(), message));
            stream.push_str(&format!("M 100644 :{} file.txt\n\n", index + 1));
        }
        self.git_input(&["fast-import", "--quiet"], &stream);
        // fast-import writes refs only; the index and worktree are still those
        // of the unborn branch.
        self.git(&["reset", "--hard", "HEAD"]);
    }
}

struct Harness {
    // Session first: it owns threads and a watcher that must stop before the
    // temp repository disappears.
    session: Session,
    events: Receiver<SessionEvent>,
    /// Owns the temp directory; never read, only kept alive.
    _fixture: Fixture,
}

impl Harness {
    fn open(fixture: Fixture, commits: usize) -> Self {
        let (tx, rx) = mpsc::channel();
        let session = Session::open(fixture.path(), move |event| {
            let _ = tx.send(event);
        })
        .expect("open session");
        let harness = Self {
            session,
            events: rx,
            _fixture: fixture,
        };
        harness.wait_for_complete(commits);
        harness
    }

    fn wait_until(
        &self,
        mut predicate: impl FnMut(&RepoSnapshot) -> bool,
    ) -> std::sync::Arc<RepoSnapshot> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let snapshot = self.session.snapshot();
            if predicate(&snapshot) {
                return snapshot;
            }
            let now = Instant::now();
            assert!(now < deadline, "timed out waiting for a snapshot");
            match self.events.recv_timeout(deadline - now) {
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => panic!("session event channel closed"),
            }
        }
    }

    fn wait_for_complete(&self, commits: usize) -> std::sync::Arc<RepoSnapshot> {
        self.wait_until(|snapshot| snapshot.commits.complete && snapshot.commits.len() == commits)
    }

    /// Rows and generation of a `QueryReady` for `ticket`.
    fn wait_for_query(&self, ticket: QueryTicket) -> (u64, Vec<usize>) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let now = Instant::now();
            assert!(now < deadline, "timed out waiting for {ticket:?}");
            match self.events.recv_timeout(deadline - now) {
                Ok(SessionEvent::QueryReady {
                    ticket: got,
                    generation,
                    rows,
                }) if got == ticket => return (generation, rows.as_ref().clone()),
                Ok(SessionEvent::QueryFailed { ticket: got, error }) if got == ticket => {
                    panic!("query {ticket:?} failed: {error}")
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => panic!("session event channel closed"),
            }
        }
    }

    /// Query events for `ticket` arriving within `window`, for the negative
    /// assertions ("this ticket emits nothing").
    fn query_events(&self, ticket: QueryTicket, window: Duration) -> Vec<SessionEvent> {
        let deadline = Instant::now() + window;
        let mut events = Vec::new();
        loop {
            let now = Instant::now();
            if now >= deadline {
                return events;
            }
            match self.events.recv_timeout(deadline - now) {
                Ok(event) => {
                    let matches = match &event {
                        SessionEvent::QueryReady { ticket: got, .. }
                        | SessionEvent::QueryFailed { ticket: got, .. } => *got == ticket,
                        _ => false,
                    };
                    if matches {
                        events.push(event);
                    }
                }
                Err(RecvTimeoutError::Timeout) => return events,
                Err(RecvTimeoutError::Disconnected) => return events,
            }
        }
    }

    fn summaries(&self, rows: &[usize]) -> Vec<String> {
        let snapshot = self.session.snapshot();
        rows.iter()
            .map(|&row| {
                snapshot
                    .commits
                    .get(row)
                    .unwrap_or_else(|| panic!("row {row} is loaded"))
                    .0
                    .summary
                    .clone()
            })
            .collect()
    }
}

fn search(text: &str) -> CommitQuery {
    CommitQuery {
        text: text.to_string(),
        in_summary: true,
        in_author: false,
        in_hash: false,
    }
}

#[test]
fn path_history_reports_only_commits_that_changed_the_file() {
    let fixture = Fixture::new();
    fixture.commit("other.txt", "0\n", BASE_TIME + 100, "root");

    // Ten commits; the file is added in the third, changed in the sixth and
    // ninth, and untouched by the rest.
    let mut changed = Vec::new();
    for index in 0..9 {
        let date = BASE_TIME + 200 + index;
        let message = format!("commit {index}");
        let id = match index {
            2 => {
                fixture.write("src/main.rs", "0\n");
                fixture.commit_paths(&["src/main.rs"], date, &message)
            }
            5 | 8 => fixture.commit("src/main.rs", &format!("{index}\n"), date, &message),
            _ => fixture.commit("other.txt", &format!("other {index}\n"), date, &message),
        };
        if matches!(index, 2 | 5 | 8) {
            changed.push(id);
        }
    }

    let expected: Vec<String> = changed.into_iter().rev().collect();
    assert_eq!(expected.len(), 3, "the file changes in 3 of the 10 commits");
    assert_eq!(fixture.history("src/main.rs"), expected);
    assert_eq!(
        fixture.history("src/main.rs"),
        fixture.git_history("src/main.rs")
    );
}

#[test]
fn path_history_includes_a_merge_that_changed_the_file() {
    let fixture = Fixture::new();
    fixture.write("app.txt", "0\n");
    fixture.write("feature.txt", "0\n");
    // Both files exist from the root commit, so the branches only own what they
    // change and the merge is comparable to both parents.
    let base = fixture.commit_paths(&["app.txt", "feature.txt"], BASE_TIME + 100, "base");

    fixture.git(&["checkout", "--quiet", "-b", "feature"]);
    // Both branches converge on the same feature.txt, so after the merge that
    // file is TREESAME to every parent and no history filter reports the merge.
    fixture.write("app.txt", "2\n");
    fixture.write("feature.txt", "1\n");
    let feature = fixture.commit_paths(
        &["app.txt", "feature.txt"],
        BASE_TIME + 200,
        "feature changes app and feature.txt",
    );

    fixture.git(&["checkout", "--quiet", "main"]);
    let main = fixture.commit(
        "feature.txt",
        "1\n",
        BASE_TIME + 300,
        "main changes feature.txt",
    );
    fixture.git_dated(
        BASE_TIME + 400,
        &[
            "merge",
            "--quiet",
            "--no-ff",
            "-m",
            "merge feature",
            "feature",
        ],
    );
    let merge = fixture.head();

    // The merge takes the feature branch's version, so it differs from its
    // first parent and both `--full-history` and this walk report it.
    assert_eq!(
        fixture.history("app.txt"),
        vec![merge.clone(), feature.clone(), base.clone()]
    );
    assert_eq!(fixture.history("app.txt"), fixture.git_history("app.txt"));
    // feature.txt is unchanged by the merge on both sides, so the merge is
    // TREESAME to every parent and neither side lists it.
    assert!(!fixture.history("feature.txt").contains(&merge));
    assert_eq!(
        fixture.history("feature.txt"),
        fixture.git_history("feature.txt")
    );
    assert_eq!(fixture.history("feature.txt"), vec![main, feature, base]);
}

#[test]
fn path_history_skips_a_merge_treesame_to_its_first_parent() {
    let fixture = Fixture::new();
    fixture.write("app.txt", "0\n");
    fixture.write("side.txt", "0\n");
    let base = fixture.commit_paths(&["app.txt", "side.txt"], BASE_TIME + 100, "base");

    fixture.git(&["checkout", "--quiet", "-b", "side"]);
    let side = fixture.commit("side.txt", "1\n", BASE_TIME + 200, "side changes side.txt");

    fixture.git(&["checkout", "--quiet", "main"]);
    let main = fixture.commit("app.txt", "2\n", BASE_TIME + 300, "main changes app");
    fixture.git_dated(
        BASE_TIME + 400,
        &["merge", "--quiet", "--no-ff", "-m", "merge side", "side"],
    );
    let merge = fixture.head();

    // The merge keeps main's `app.txt`, so it is TREESAME to its first parent
    // and this walk (which only compares the first parent) omits it.
    assert_eq!(fixture.history("app.txt"), vec![main.clone(), base.clone()]);
    // `--full-history` keeps merges unless they are TREESAME to *all* parents,
    // so it lists this merge because it differs from `side`. The difference is
    // intentional: only the first parent is compared and history is not
    // simplified by following a TREESAME parent.
    let reference = fixture.git_history("app.txt");
    assert!(
        reference.contains(&merge),
        "git --full-history keeps the merge"
    );
    assert!(
        !reference.contains(&side),
        "the side commit did not touch app.txt"
    );
    assert_eq!(reference.first(), Some(&merge));
}

#[test]
fn path_history_for_a_directory_covers_everything_below_it() {
    let fixture = Fixture::new();
    fixture.write("root.txt", "0\n");
    fixture.write("dir/one.txt", "0\n");
    fixture.write("dir/sub/three.txt", "0\n");
    let root = fixture.commit_paths(
        &["root.txt", "dir/one.txt", "dir/sub/three.txt"],
        BASE_TIME + 100,
        "root",
    );

    let one = fixture.commit("dir/one.txt", "1\n", BASE_TIME + 200, "dir/one");
    fixture.commit("root.txt", "2\n", BASE_TIME + 300, "root again");
    let three = fixture.commit("dir/sub/three.txt", "1\n", BASE_TIME + 400, "dir/sub/three");

    assert_eq!(
        fixture.history("dir"),
        vec![three.clone(), one.clone(), root.clone()]
    );
    assert_eq!(fixture.history("dir"), fixture.git_history("dir"));
    assert_eq!(
        fixture.history("dir/sub"),
        vec![three.clone(), root.clone()]
    );
    assert_eq!(fixture.history("dir/sub"), fixture.git_history("dir/sub"));
    // Trailing and doubled slashes name the same tree.
    assert_eq!(
        fixture.history("dir/"),
        vec![three.clone(), one.clone(), root.clone()]
    );
    assert_eq!(fixture.history("dir//sub/"), vec![three, root]);
}

#[test]
fn path_history_reports_deletion_and_readdition() {
    let fixture = Fixture::new();
    fixture.write("keep.txt", "0\n");
    let added = fixture.commit("file.txt", "one\n", BASE_TIME + 100, "add file");
    fixture.commit("keep.txt", "1\n", BASE_TIME + 200, "unrelated");
    let deleted = fixture.remove("file.txt", BASE_TIME + 300, "delete file");
    fixture.commit("keep.txt", "2\n", BASE_TIME + 400, "unrelated again");
    let readded = fixture.commit("file.txt", "two\n", BASE_TIME + 500, "re-add file");

    assert_eq!(
        fixture.history("file.txt"),
        vec![readded.clone(), deleted.clone(), added.clone()]
    );
    assert_eq!(fixture.history("file.txt"), fixture.git_history("file.txt"));
}

#[test]
fn path_history_for_an_unknown_path_is_empty() {
    let fixture = Fixture::new();
    fixture.commit("file.txt", "one\n", BASE_TIME + 100, "first");
    fixture.commit("dir/file.txt", "two\n", BASE_TIME + 200, "second");

    assert!(fixture.history("never/existed.txt").is_empty());
    assert!(fixture.history("file.txt/nested").is_empty());
    assert!(fixture.history("dir/other").is_empty());
    // A file is not a directory prefix: only the path itself matches.
    assert_eq!(
        fixture.history("file.txt").len(),
        1,
        "the root commit added the file, the later one touched dir/file.txt"
    );
}

#[test]
fn path_history_matches_git_log_across_branches() {
    let fixture = Fixture::new();
    fixture.write("shared.txt", "0\n");
    fixture.write("main.txt", "0\n");
    fixture.write("topic.txt", "0\n");
    let root = fixture.commit("shared.txt", "1\n", BASE_TIME + 100, "root");

    fixture.git(&["checkout", "--quiet", "-b", "topic"]);
    let topic_one = fixture.commit("shared.txt", "2\n", BASE_TIME + 200, "topic changes shared");
    let topic_two = fixture.commit("topic.txt", "1\n", BASE_TIME + 300, "topic only");
    let topic_three = fixture.commit(
        "shared.txt",
        "3\n",
        BASE_TIME + 400,
        "topic changes shared again",
    );

    fixture.git(&["checkout", "--quiet", "main"]);
    let main_one = fixture.commit("main.txt", "1\n", BASE_TIME + 250, "main only");
    fixture.git_dated(
        BASE_TIME + 500,
        &["merge", "--quiet", "--no-ff", "-m", "merge topic", "topic"],
    );
    let merge = fixture.head();

    let commits = fixture.history("shared.txt");
    assert_eq!(commits, fixture.git_history("shared.txt"));
    assert_eq!(commits, vec![merge.clone(), topic_three, topic_one, root]);
    assert!(
        !commits.contains(&topic_two),
        "a commit that left the path alone is not listed"
    );
    assert!(!commits.contains(&main_one));
}

#[test]
fn path_history_uses_the_commit_graph() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "0\n");
    let first = fixture.commit("a.txt", "1\n", BASE_TIME + 100, "first");
    fixture.commit("b.txt", "0\n", BASE_TIME + 200, "second");
    let third = fixture.commit("a.txt", "2\n", BASE_TIME + 300, "third");
    fixture.write_commit_graph();

    assert_eq!(fixture.history("a.txt"), vec![third, first]);
    assert_eq!(fixture.history("a.txt"), fixture.git_history("a.txt"));
}

#[test]
fn session_search_matches_summary_author_and_hash() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "0\n");
    fixture.commit("a.txt", "1\n", BASE_TIME + 100, "Add parser");
    fixture.write("b.txt", "0\n");
    fixture.commit("b.txt", "1\n", BASE_TIME + 200, "fix PARSER bug");
    fixture.write("c.txt", "0\n");
    // A commit whose author is distinctive, through `--author`.
    fixture.git(&["add", "--", "c.txt"]);
    fixture.git_dated(
        BASE_TIME + 300,
        &[
            "commit",
            "--quiet",
            "--author=Ada Lovelace <ada@example.com>",
            "-m",
            "docs only",
        ],
    );
    let docs = fixture.head();

    let harness = Harness::open(fixture, 3);
    let snapshot = harness.session.snapshot();

    let by_summary = harness.session.search_commits(search("parser"));
    let (generation, rows) = harness.wait_for_query(by_summary);
    assert_eq!(generation, snapshot.generation);
    assert_eq!(
        harness.summaries(&rows),
        vec!["fix PARSER bug", "Add parser"]
    );

    let by_author = harness.session.search_commits(CommitQuery {
        text: "ada@example".to_string(),
        in_summary: false,
        in_author: true,
        in_hash: false,
    });
    let (_, rows) = harness.wait_for_query(by_author);
    assert_eq!(harness.summaries(&rows), vec!["docs only"]);

    let short_hash = &docs[..7];
    let by_hash = harness.session.search_commits(CommitQuery {
        text: short_hash.to_string(),
        in_summary: false,
        in_author: false,
        in_hash: true,
    });
    let (_, rows) = harness.wait_for_query(by_hash);
    assert_eq!(rows.len(), 1);
    assert_eq!(harness.summaries(&rows), vec!["docs only"]);

    // Below four characters a hash-shaped text is not treated as an id.
    let too_short = harness.session.search_commits(CommitQuery {
        text: docs[..3].to_string(),
        in_summary: false,
        in_author: false,
        in_hash: true,
    });
    let (_, rows) = harness.wait_for_query(too_short);
    assert!(rows.is_empty());
}

#[test]
fn session_path_history_returns_rows_of_the_snapshot() {
    let fixture = Fixture::new();
    fixture.write("deep/nested/file.txt", "0\n");
    let root = fixture.commit("deep/nested/file.txt", "1\n", BASE_TIME + 100, "root");
    fixture.commit("other.txt", "0\n", BASE_TIME + 200, "unrelated");
    let second = fixture.commit(
        "deep/nested/file.txt",
        "2\n",
        BASE_TIME + 300,
        "change nested",
    );

    let harness = Harness::open(fixture, 3);
    let snapshot = harness.session.snapshot();

    let ticket = harness.session.path_history("deep".to_string());
    let (generation, rows) = harness.wait_for_query(ticket);
    assert_eq!(generation, snapshot.generation);
    assert_eq!(harness.summaries(&rows), vec!["change nested", "root"]);
    for (row, id) in rows.iter().zip([&second, &root]) {
        let (commit, _) = snapshot.commits.get(*row).expect("row is loaded");
        assert_eq!(&commit.id.to_string(), id);
    }

    let empty = harness.session.path_history("no/such/path".to_string());
    let (_, rows) = harness.wait_for_query(empty);
    assert!(rows.is_empty(), "an unknown path answers with no rows");
}

#[test]
fn session_cancelled_query_emits_nothing() {
    let fixture = Fixture::new();
    fixture.fast_import(5_000);
    let harness = Harness::open(fixture, 5_000);

    // Occupy the single worker with a path walk over the whole history, then
    // queue a search and cancel it while it is still queued.
    let busy = harness.session.path_history("file.txt".to_string());
    let cancelled = harness.session.search_commits(search("commit"));
    harness.session.cancel_query(cancelled);

    let (_, rows) = harness.wait_for_query(busy);
    assert_eq!(rows.len(), 5_000, "the walk covers every commit");
    assert!(
        harness
            .query_events(cancelled, Duration::from_millis(300))
            .is_empty(),
        "a cancelled query must not answer"
    );

    // The worker is still alive and serves later queries.
    let after = harness.session.search_commits(search("commit 4999"));
    let (_, rows) = harness.wait_for_query(after);
    assert_eq!(harness.summaries(&rows), vec!["commit 4999"]);
}

/// A path that never matches keeps the walk busy for the whole history, so the
/// abandon signal has to reach the walk itself, not just the row loop.
#[test]
fn cancelled_path_query_frees_the_worker_and_drop_does_not_wait() {
    let fixture = Fixture::new();
    // fast-import leaves history without a commit-graph, so the walk really has
    // to visit every commit before it can answer an empty path.
    fixture.fast_import(20_000);
    let harness = Harness::open(fixture, 20_000);

    // How long the walk that has to be abandoned takes.
    let start = Instant::now();
    let baseline = harness.session.path_history("no/such/path".to_string());
    harness.wait_for_query(baseline);
    let full_walk = start.elapsed();
    assert!(
        full_walk > Duration::from_millis(50),
        "the fixture must make a full walk observable, took {full_walk:?}"
    );

    // Let the worker pick up a second walk of the same shape, then cancel it.
    let cancelled = harness.session.path_history("other/such/path".to_string());
    std::thread::sleep(full_walk / 4);
    harness.session.cancel_query(cancelled);

    // A search only reads the published snapshot, so it can only be slow if
    // the worker is still walking the cancelled path.
    let start = Instant::now();
    let after = harness.session.search_commits(search("commit 19999"));
    harness.wait_for_query(after);
    let waited = start.elapsed();
    assert!(
        waited < full_walk / 2,
        "the worker took {waited:?} to answer after a cancel; a full walk is {full_walk:?}"
    );
    assert!(
        harness
            .query_events(cancelled, Duration::from_millis(200))
            .is_empty(),
        "a cancelled path query must not answer"
    );

    // Dropping the session must not wait for the walk either.
    harness.session.path_history("third/such/path".to_string());
    std::thread::sleep(full_walk / 4);
    let start = Instant::now();
    drop(harness);
    let dropped = start.elapsed();
    assert!(
        dropped < full_walk / 2,
        "session drop waited {dropped:?} for a walk; a full walk is {full_walk:?}"
    );
}

/// 100k commits, each changing the file: the slowest shape for a single path.
#[test]
#[ignore = "perf: builds a 100k-commit repository"]
fn path_history_is_fast_on_100k_commits() {
    let fixture = Fixture::new();
    fixture.fast_import(100_000);
    fixture.write_commit_graph();

    let repo = fixture.open();
    let start = Instant::now();
    let ids: Vec<_> = repo
        .path_history("file.txt")
        .expect("path history")
        .map(|id| id.expect("id"))
        .collect();
    let elapsed = start.elapsed();

    assert_eq!(ids.len(), 100_000, "every commit changed the file");
    eprintln!("path_history over 100k commits: {elapsed:?}");
    assert!(elapsed < Duration::from_secs(1), "took {elapsed:?}");
}
