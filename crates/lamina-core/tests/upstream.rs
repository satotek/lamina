//! Integration tests for `Repo::upstreams` and streamed fetch progress.
//!
//! Every test builds a workspace (work tree + bare remote + a second clone)
//! inside one temp dir and runs `git` with an isolated config, the same way the
//! other core tests do. Counts are checked against `git rev-list
//! --left-right --count`, which is the external reference for ahead/behind.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lamina_core::UpstreamInfo;
use lamina_core::git_cli::Git;
use lamina_core::repo::Repo;
use lamina_core::session::{Operation, Session, SessionEvent};
use lamina_core::test_support::TestRepo;

const TIMEOUT: Duration = Duration::from_secs(60);

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

    fn temp_root(&self) -> &Path {
        self.repo.temp_root()
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

    fn git_with_input(&self, cwd: &Path, args: &[&str], input: &str) -> String {
        self.repo.git_input_in(cwd, args, input)
    }

    fn write(&self, dir: &Path, name: &str, contents: &str) {
        self.repo.write_in(dir, name, contents);
    }

    fn commit_all(&self, dir: &Path, message: &str) {
        self.git(dir, &["add", "-A"]);
        self.git(dir, &["commit", "-q", "-m", message]);
    }

    fn config_value(&self, dir: &Path, key: &str, value: &str) {
        self.git(dir, &["config", key, value]);
    }

    /// The production write path for the work tree, isolated from the host's
    /// git config.
    fn git_handle(&self, cwd: &Path) -> Git {
        self.repo.git_handle_in(cwd)
    }
}

/// Commits `count` files with one commit each and returns nothing; the messages
/// only need to be distinct.
fn commit_many(fixture: &Fixture, dir: &Path, prefix: &str, count: usize) {
    for index in 0..count {
        fixture.write(dir, &format!("{prefix}-{index}.txt"), &format!("{index}\n"));
        fixture.commit_all(dir, &format!("{prefix} {index}"));
    }
}

/// A work tree whose branches cover every tracking state, plus a second clone
/// that moves the remote side.
struct Scenario {
    fixture: Fixture,
}

fn scenario() -> Scenario {
    let fixture = Fixture::new();
    let work = fixture.work();

    fixture.write(&work, "base.txt", "base\n");
    fixture.commit_all(&work, "base");
    fixture.git(&work, &["remote", "add", "origin", &fixture.remote_arg()]);
    fixture.git(&work, &["push", "-q", "-u", "origin", "main"]);
    // Remote branches that start at the same commit as `main`.
    fixture.git(
        &work,
        &[
            "push",
            "-q",
            "origin",
            "main:sync",
            "main:behind",
            "main:diverged",
            "main:gone",
            "main:other",
        ],
    );
    fixture.git(&work, &["fetch", "-q", "origin"]);

    // Tracking branches that never move locally.
    for name in ["sync", "behind", "diverged", "gone"] {
        fixture.git(
            &work,
            &["branch", "--track", name, &format!("origin/{name}")],
        );
    }
    // Upstream with a different name than the local branch.
    fixture.git(&work, &["branch", "--track", "feature", "origin/other"]);
    // No upstream configuration at all.
    fixture.git(&work, &["branch", "local-only", "main"]);

    // Two commits only the local side has.
    fixture.git(
        &work,
        &["switch", "-q", "-c", "ahead", "--track", "origin/main"],
    );
    commit_many(&fixture, &work, "ahead", 2);

    // One local commit against two remote commits.
    fixture.git(&work, &["switch", "-q", "diverged"]);
    commit_many(&fixture, &work, "local-diverged", 1);
    fixture.git(&work, &["switch", "-q", "main"]);

    // The second clone advances and deletes branches on the remote.
    let other = fixture.temp_root().join("other");
    fixture.git(
        fixture.temp_root(),
        &["clone", "-q", &fixture.remote_arg(), "other"],
    );
    fixture.git(&other, &["switch", "-q", "behind"]);
    commit_many(&fixture, &other, "remote-behind", 3);
    fixture.git(&other, &["push", "-q", "origin", "behind"]);
    fixture.git(&other, &["switch", "-q", "diverged"]);
    commit_many(&fixture, &other, "remote-diverged", 2);
    fixture.git(&other, &["push", "-q", "origin", "diverged"]);
    fixture.git(&other, &["switch", "-q", "other"]);
    commit_many(&fixture, &other, "remote-other", 1);
    fixture.git(&other, &["push", "-q", "origin", "other"]);
    fixture.git(&other, &["push", "-q", "origin", "--delete", "gone"]);

    fixture.git(&work, &["fetch", "-q", "--prune", "origin"]);
    Scenario { fixture }
}

fn read_upstreams(scenario: &Scenario) -> std::collections::HashMap<String, UpstreamInfo> {
    Repo::open(scenario.fixture.work())
        .expect("open work tree")
        .upstreams()
        .expect("upstreams")
}

/// `git rev-list --left-right --count left...right`: the first number counts
/// commits only `left` has, the second only `right`.
fn git_ahead_behind(scenario: &Scenario, left: &str, right: &str) -> (u32, u32) {
    let output = scenario.fixture.git(
        &scenario.fixture.work(),
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{left}...{right}"),
        ],
    );
    let mut fields = output.split_whitespace();
    (
        fields.next().expect("ahead count").parse().expect("number"),
        fields
            .next()
            .expect("behind count")
            .parse()
            .expect("number"),
    )
}

#[test]
fn ahead_behind_matches_git_for_every_tracking_state() {
    let scenario = scenario();
    let upstreams = read_upstreams(&scenario);

    let cases = [
        // in sync
        ("sync", "origin/sync", 0, 0),
        // ahead only
        ("ahead", "origin/main", 2, 0),
        // behind only
        ("behind", "origin/behind", 0, 3),
        // diverged
        ("diverged", "origin/diverged", 1, 2),
        // the checked-out branch
        ("main", "origin/main", 0, 0),
    ];
    for (branch, upstream, ahead, behind) in cases {
        assert_eq!(
            upstreams.get(branch),
            Some(&UpstreamInfo {
                name: upstream.to_owned(),
                ahead: Some(ahead),
                behind: Some(behind),
                gone: false,
            }),
            "{branch} vs {upstream}"
        );
        assert_eq!(
            (ahead, behind),
            git_ahead_behind(&scenario, branch, upstream),
            "{branch} vs {upstream} disagrees with git"
        );
    }

    // The same counts must come out of the commit-graph-accelerated path.
    scenario.fixture.git(
        &scenario.fixture.work(),
        &["commit-graph", "write", "--reachable"],
    );
    let from_graph = read_upstreams(&scenario);
    for (branch, upstream, ahead, behind) in cases {
        assert_eq!(
            from_graph.get(branch),
            Some(&UpstreamInfo {
                name: upstream.to_owned(),
                ahead: Some(ahead),
                behind: Some(behind),
                gone: false,
            }),
            "{branch} with a commit-graph"
        );
    }
}

#[test]
fn a_branch_without_upstream_configuration_is_absent() {
    let scenario = scenario();
    let upstreams = read_upstreams(&scenario);

    assert!(
        !upstreams.contains_key("local-only"),
        "a branch with no upstream must not be reported: {upstreams:?}"
    );
    assert!(upstreams.contains_key("main"), "configured branch missing");
}

#[test]
fn upstream_configured_after_opening_is_seen() {
    let fixture = Fixture::new();
    let work = fixture.work();
    fixture.write(&work, "a.txt", "a\n");
    fixture.commit_all(&work, "base");
    fixture.git(&work, &["remote", "add", "origin", &fixture.remote_arg()]);
    fixture.git(&work, &["push", "-q", "-u", "origin", "main"]);
    fixture.git(&work, &["checkout", "-q", "-b", "topic"]);
    fixture.write(&work, "b.txt", "b\n");
    fixture.commit_all(&work, "topic");

    // A session keeps one `Repo` across operations, and `git push
    // --set-upstream` writes `branch.topic.*` after that handle was opened.
    let repo = Repo::open(&work).expect("open work tree");
    assert!(
        !repo.upstreams().expect("upstreams").contains_key("topic"),
        "topic has no upstream yet"
    );
    fixture.git(
        &work,
        &["branch", "--set-upstream-to", "origin/main", "topic"],
    );
    assert_eq!(
        repo.upstreams().expect("upstreams").get("topic"),
        Some(&UpstreamInfo {
            name: "origin/main".to_owned(),
            ahead: Some(1),
            behind: Some(0),
            gone: false,
        })
    );
}

#[test]
fn a_local_branch_can_be_the_upstream() {
    let fixture = Fixture::new();
    let work = fixture.work();
    fixture.write(&work, "a.txt", "a\n");
    fixture.commit_all(&work, "base");
    fixture.git(&work, &["branch", "base2"]);
    fixture.git(&work, &["checkout", "-q", "-b", "topic"]);
    fixture.write(&work, "b.txt", "b\n");
    fixture.commit_all(&work, "topic");
    fixture.git(&work, &["branch", "--set-upstream-to", "base2", "topic"]);

    let upstreams = Repo::open(&work)
        .expect("open work tree")
        .upstreams()
        .expect("upstreams");
    assert_eq!(
        upstreams.get("topic"),
        Some(&UpstreamInfo {
            name: "base2".to_owned(),
            ahead: Some(1),
            behind: Some(0),
            gone: false,
        })
    );
}

#[test]
fn a_pruned_remote_branch_marks_the_upstream_gone() {
    let scenario = scenario();
    let upstreams = read_upstreams(&scenario);

    assert_eq!(
        upstreams.get("gone"),
        Some(&UpstreamInfo {
            name: "origin/gone".to_owned(),
            ahead: None,
            behind: None,
            gone: true,
        })
    );
    // The configuration is what marks it, so the branch keeps its name.
    let config = scenario.fixture.git(
        &scenario.fixture.work(),
        &["config", "--get", "branch.gone.merge"],
    );
    assert_eq!(config.trim(), "refs/heads/gone");
}

#[test]
fn a_branch_can_track_a_differently_named_remote_branch() {
    let scenario = scenario();
    let upstreams = read_upstreams(&scenario);

    assert_eq!(
        upstreams.get("feature"),
        Some(&UpstreamInfo {
            name: "origin/other".to_owned(),
            ahead: Some(0),
            behind: Some(1),
            gone: false,
        })
    );
    assert_eq!(
        (0, 1),
        git_ahead_behind(&scenario, "feature", "origin/other")
    );
}

#[test]
fn fetch_emits_operation_progress() {
    let fixture = Fixture::new();
    let work = fixture.work();
    fixture.write(&work, "base.txt", "base\n");
    fixture.commit_all(&work, "base");
    fixture.git(&work, &["remote", "add", "origin", &fixture.remote_arg()]);
    fixture.git(&work, &["push", "-q", "-u", "origin", "main"]);

    // New remote commits give the fetch something to report.
    let other = fixture.temp_root().join("other");
    fixture.git(
        fixture.temp_root(),
        &["clone", "-q", &fixture.remote_arg(), "other"],
    );
    commit_many(&fixture, &other, "remote", 5);
    fixture.git(&other, &["push", "-q", "origin", "main"]);

    let (tx, rx) = mpsc::channel();
    let session = Session::open(fixture.work(), move |event| {
        let _ = tx.send(event);
    })
    .expect("open session");

    // Let the first load land so the operation queue is not racing the walk.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        assert!(Instant::now() < deadline, "no snapshot arrived");
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(SessionEvent::SnapshotUpdated { generation }) if generation > 0 => break,
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("session closed"),
        }
    }

    let id = session.run(Operation::Fetch {
        remote: Some("origin".into()),
        prune: false,
    });

    let mut progress: Vec<(String, Option<u8>)> = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for fetch {id:?}");
        match rx.recv_timeout(deadline - now) {
            Ok(SessionEvent::OperationProgress {
                id: got,
                text,
                percent,
            }) if got == id => progress.push((text, percent)),
            Ok(SessionEvent::OperationFinished { id: got, error }) if got == id => {
                assert!(error.is_none(), "fetch failed: {error:?}");
                break;
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("session event channel closed"),
        }
    }
    drop(session);

    // Git always finishes a fetching run with the ref-update summary, and the
    // reporter must deliver that last line even when the throttle just emitted.
    // A tiny repository may not print the `Receiving objects` counters at all,
    // so percentage parsing itself is covered by the unit tests next to it.
    assert!(
        !progress.is_empty(),
        "no OperationProgress event was emitted"
    );
    let (last_text, _) = progress.last().expect("progress");
    assert!(
        last_text.contains("origin/main"),
        "the final progress line should be git's ref summary, got {progress:?}"
    );
    assert!(
        progress
            .iter()
            .filter_map(|(_, percent)| *percent)
            .all(|percent| percent <= 100),
        "an out-of-range percentage was emitted: {progress:?}"
    );
}

/// The streaming variants exist because the plain ones cannot report progress;
/// they must stay byte-for-byte equivalent in what they ask git to do.
#[test]
fn progress_variants_build_the_same_commands() {
    use lamina_core::git_cli::Git;

    let fixture = Fixture::new();
    let work = fixture.work();
    fixture.write(&work, "a.txt", "a\n");
    fixture.commit_all(&work, "base");
    fixture.git(&work, &["remote", "add", "origin", &fixture.remote_arg()]);
    fixture.git(&work, &["push", "-q", "-u", "origin", "main"]);

    let git = fixture.git_handle(&work);
    let args = |git: &Git| -> Vec<Vec<String>> {
        git.log().iter().map(|record| record.args.clone()).collect()
    };

    let _ = git.fetch(Some("origin"), true);
    let _ = git.fetch_unshallow();
    let _ = git.pull(Some(true));
    let _ = git.push(Some("origin"), Some("main"), false, true);
    let plain = args(&git);

    git.clear_log();
    let _ = git.fetch_with_progress(Some("origin"), true, |_, _| {});
    let _ = git.fetch_unshallow_with_progress(|_, _| {});
    let _ = git.pull_with_progress(Some(true), |_, _| {});
    let _ = git.push_with_progress(Some("origin"), Some("main"), false, true, |_, _| {});
    let streaming = args(&git);

    assert_eq!(plain, streaming);
}

/// One thousand branches on a 100k-commit history would still be interactive if
/// the per-branch walk only covered the divergent commits; this guards the
/// merge-base-bounded walk against a regression to full-history counting.
#[test]
#[ignore = "perf: builds a 100k-commit repository, run explicitly"]
fn upstreams_stay_fast_with_fifty_branches() {
    let fixture = Fixture::new();
    let work = fixture.work();

    let commits = 100_000usize;
    let mut stream = String::new();
    stream.push_str("blob\nmark :1\ndata 3\nhi\n\n");
    for index in 0..commits {
        let message = format!("commit {index}");
        stream.push_str("commit refs/heads/main\n");
        stream.push_str(&format!(
            "committer Lamina Test <lamina@example.com> {} +0000\n",
            1_700_000_000 + index as i64
        ));
        stream.push_str(&format!("data {}\n{}\n", message.len(), message));
        stream.push_str("M 100644 :1 file.txt\n\n");
    }
    fixture.git_with_input(&work, &["fast-import", "--quiet"], &stream);
    fixture.git(&work, &["remote", "add", "origin", &fixture.remote_arg()]);

    // Fifty tracking branches: even ones are 5 commits ahead, odd ones 3 behind.
    let branch_count = 50usize;
    let mut expected = Vec::with_capacity(branch_count);
    for index in 0..branch_count {
        let base = format!("main~{}", index * 20);
        let local = fixture.git(&work, &["rev-parse", &base]).trim().to_owned();
        let (upstream, ahead, behind) = if index % 2 == 0 {
            (
                fixture
                    .git(&work, &["rev-parse", &format!("main~{}", index * 20 + 5)])
                    .trim()
                    .to_owned(),
                5,
                0,
            )
        } else {
            (
                fixture
                    .git(&work, &["rev-parse", &format!("main~{}", index * 20 - 3)])
                    .trim()
                    .to_owned(),
                0,
                3,
            )
        };
        let branch = format!("b{index}");
        fixture.git(
            &work,
            &["update-ref", &format!("refs/heads/{branch}"), &local],
        );
        fixture.git(
            &work,
            &[
                "update-ref",
                &format!("refs/remotes/origin/{branch}"),
                &upstream,
            ],
        );
        let remote_key = format!("branch.{branch}.remote");
        let merge_key = format!("branch.{branch}.merge");
        let merge_ref = format!("refs/heads/{branch}");
        fixture.config_value(&work, &remote_key, "origin");
        fixture.config_value(&work, &merge_key, &merge_ref);
        expected.push((branch, ahead, behind));
    }
    fixture.git(&work, &["commit-graph", "write", "--reachable"]);

    let repo = Repo::open(&work).expect("open work tree");
    let started = Instant::now();
    let upstreams = repo.upstreams().expect("upstreams");
    let elapsed = started.elapsed();
    eprintln!("upstreams(): {branch_count} branches on {commits} commits took {elapsed:?}",);

    assert_eq!(upstreams.len(), branch_count);
    for (branch, ahead, behind) in &expected {
        assert_eq!(
            upstreams.get(branch),
            Some(&UpstreamInfo {
                name: format!("origin/{branch}"),
                ahead: Some(*ahead),
                behind: Some(*behind),
                gone: false,
            }),
            "{branch}"
        );
    }

    // A regression to walking both full histories would take far longer; the
    // generous bound keeps slow machines from turning this into a flaky test.
    assert!(
        elapsed < Duration::from_secs(5),
        "upstreams() took {elapsed:?} for {branch_count} branches"
    );
}
