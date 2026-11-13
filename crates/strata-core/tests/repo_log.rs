//! Integration tests for HEAD, refs and the commit walk.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use strata_core::repo::Repo;
use strata_core::{HeadInfo, RefInfo, RefKind, RemoteInfo};
use tempfile::TempDir;

const BASE_TIME: i64 = 1_700_000_000;

/// A repository in a temp directory driven by the `git` CLI with an isolated
/// configuration, so nothing depends on the machine's git setup.
struct Fixture {
    tmp: TempDir,
    repo: PathBuf,
    empty_config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().expect("temp dir");
        let repo = tmp.path().join("repo");
        let empty_config = tmp.path().join("empty-gitconfig");
        std::fs::write(&empty_config, b"").expect("empty config");
        std::fs::create_dir_all(&repo).expect("repo dir");

        let fixture = Self {
            tmp,
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
        self.git_at(&self.repo, args)
    }

    fn git_at(&self, dir: &Path, args: &[&str]) -> String {
        self.run(dir, args, BASE_TIME)
    }

    fn git_dated(&self, date: i64, args: &[&str]) -> String {
        self.run(&self.repo, args, date)
    }

    fn run(&self, dir: &Path, args: &[&str], date: i64) -> String {
        self.run_with_input(dir, args, date, "")
    }

    fn run_with_input(&self, dir: &Path, args: &[&str], date: i64, input: &str) -> String {
        use std::io::Write;
        use std::process::Stdio;

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

    fn git_input(&self, args: &[&str], input: &str) -> String {
        self.run_with_input(&self.repo, args, BASE_TIME, input)
    }

    fn commit(&self, file: &str, content: &str, date: i64, message: &str) -> String {
        let path = self.repo.join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("file parent");
        }
        std::fs::write(&path, content).expect("write file");
        self.git(&["add", "--", file]);
        self.git_dated(date, &["commit", "--quiet", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    fn open(&self) -> Repo {
        Repo::open(self.path()).expect("open repo")
    }

    fn walk_ids(&self) -> Vec<String> {
        self.open()
            .walk_commits()
            .expect("walk")
            .map(|commit| commit.expect("commit").id.to_string())
            .collect()
    }

    /// The reference this module documents: HEAD, branches, remote-tracking
    /// branches, tags and the stash, in the `--date-order` the walk produces.
    /// Private namespaces such as `refs/notes/` are deliberately absent.
    fn git_log_date_order(&self) -> Vec<String> {
        let mut args = vec![
            "log",
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
        self.git(&args).lines().map(str::to_string).collect()
    }
}

fn kinds_and_names(refs: &[RefInfo]) -> Vec<(RefKind, String)> {
    refs.iter().map(|r| (r.kind, r.full_name.clone())).collect()
}

#[test]
fn unborn_head_has_no_target() {
    let fixture = Fixture::new();
    let repo = fixture.open();

    assert_eq!(
        repo.head().expect("head"),
        HeadInfo::Branch {
            short_name: "main".to_string(),
            target: None,
        }
    );
    assert!(repo.refs().expect("refs").is_empty());
    assert_eq!(repo.walk_commits().expect("walk").count(), 0);
}

#[test]
fn linear_history_walks_newest_first() {
    let fixture = Fixture::new();
    let first = fixture.commit("file.txt", "one\n", BASE_TIME + 100, "first");
    let second = fixture.commit("file.txt", "two\n", BASE_TIME + 200, "second\n\nbody text");
    let third = fixture.commit("file.txt", "three\n", BASE_TIME + 300, "third");

    let repo = fixture.open();
    assert_eq!(
        repo.head().expect("head"),
        HeadInfo::Branch {
            short_name: "main".to_string(),
            target: Some(third.parse().expect("oid")),
        }
    );

    let commits: Vec<_> = repo
        .walk_commits()
        .expect("walk")
        .map(|commit| commit.expect("commit"))
        .collect();
    let ids: Vec<String> = commits.iter().map(|c| c.id.to_string()).collect();
    assert_eq!(ids, vec![third.clone(), second.clone(), first.clone()]);

    let middle = &commits[1];
    assert_eq!(middle.summary, "second", "summary is only the first line");
    assert_eq!(middle.author_name, "Strata Author");
    assert_eq!(middle.author_email, "author@example.com");
    assert_eq!(middle.author_time, BASE_TIME + 200);
    assert_eq!(middle.committer_time, BASE_TIME + 200);
    assert_eq!(middle.parents.len(), 1);
    assert_eq!(middle.parents[0].to_string(), first);
}

#[test]
fn refs_cover_branches_and_peeled_tags() {
    let fixture = Fixture::new();
    let first = fixture.commit("file.txt", "one\n", BASE_TIME + 100, "first");
    let second = fixture.commit("file.txt", "two\n", BASE_TIME + 200, "second");

    fixture.git(&["branch", "feature", &first]);
    fixture.git(&["tag", "light", &first]);
    fixture.git(&["tag", "-a", "annotated", "-m", "annotated tag", &second]);

    // A commit that is only reachable through a tag must still be walked.
    fixture.git(&["checkout", "--quiet", "-b", "side", &first]);
    let side = fixture.commit("side.txt", "side\n", BASE_TIME + 250, "side only");
    fixture.git(&["tag", "-a", "side-tag", "-m", "side tag"]);
    fixture.git(&["checkout", "--quiet", "main"]);
    fixture.git(&["branch", "-D", "side"]);

    let repo = fixture.open();
    let refs = repo.refs().expect("refs");
    assert_eq!(
        kinds_and_names(&refs),
        vec![
            (RefKind::LocalBranch, "refs/heads/feature".to_string()),
            (RefKind::LocalBranch, "refs/heads/main".to_string()),
            (RefKind::Tag, "refs/tags/annotated".to_string()),
            (RefKind::Tag, "refs/tags/light".to_string()),
            (RefKind::Tag, "refs/tags/side-tag".to_string()),
        ]
    );

    let tagged = |name: &str| {
        refs.iter()
            .find(|r| r.full_name == name)
            .unwrap_or_else(|| panic!("{name} is listed"))
            .target
            .to_string()
    };
    assert_eq!(tagged("refs/tags/light"), first);
    // An annotated tag must be peeled to the commit, not left as a tag object.
    assert_eq!(tagged("refs/tags/annotated"), second);
    assert_eq!(tagged("refs/tags/side-tag"), side);

    let walked = fixture.walk_ids();
    assert!(
        walked.contains(&side),
        "tag-only commit is part of the walk"
    );
    assert_eq!(walked, fixture.git_log_date_order());
}

#[test]
fn merge_is_walked_children_first() {
    let fixture = Fixture::new();
    let base = fixture.commit("main.txt", "base\n", BASE_TIME + 100, "base");

    fixture.git(&["checkout", "--quiet", "-b", "feature"]);
    let feature = fixture.commit("feature.txt", "feature\n", BASE_TIME + 200, "feature work");

    fixture.git(&["checkout", "--quiet", "main"]);
    let main = fixture.commit("main.txt", "main\n", BASE_TIME + 300, "main work");
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

    let ids = fixture.walk_ids();
    assert_eq!(ids, fixture.git_log_date_order());
    assert_eq!(ids, vec![merge.clone(), main, feature, base]);

    let merge_commit = fixture
        .open()
        .walk_commits()
        .expect("walk")
        .next()
        .expect("at least one commit")
        .expect("commit");
    assert_eq!(merge_commit.id.to_string(), merge);
    assert_eq!(merge_commit.parents.len(), 2);
}

#[test]
fn date_order_keeps_children_before_newer_parents() {
    let fixture = Fixture::new();
    // Two branches whose commit dates interleave and include children that are
    // older than their parents, so a plain date sort would violate topology.
    fixture.commit("main.txt", "c1\n", BASE_TIME + 1000, "c1");
    fixture.git(&["checkout", "--quiet", "-b", "feature"]);
    fixture.commit("feature.txt", "f1\n", BASE_TIME + 2000, "f1");
    fixture.commit("feature.txt", "f2\n", BASE_TIME + 1100, "f2");

    fixture.git(&["checkout", "--quiet", "main"]);
    fixture.commit("main.txt", "c2\n", BASE_TIME + 3000, "c2");
    fixture.commit("main.txt", "c3\n", BASE_TIME + 1200, "c3");
    fixture.git_dated(
        BASE_TIME + 4000,
        &[
            "merge",
            "--quiet",
            "--no-ff",
            "-m",
            "merge feature",
            "feature",
        ],
    );

    let expected = fixture.git_log_date_order();
    assert!(expected.len() >= 6);
    assert_eq!(fixture.walk_ids(), expected);

    // Sanity-check that the test data really required topological sorting.
    let plain_dates = fixture.git(&["log", "--all", "--format=%ct"]);
    let dates: Vec<i64> = plain_dates
        .lines()
        .map(|d| d.parse().expect("date"))
        .collect();
    let mut sorted = dates.clone();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    assert_ne!(dates, sorted, "dates are not already newest-first");
}

#[test]
fn detached_head_reports_its_commit() {
    let fixture = Fixture::new();
    fixture.commit("file.txt", "one\n", BASE_TIME + 100, "first");
    let second = fixture.commit("file.txt", "two\n", BASE_TIME + 200, "second");
    fixture.commit("file.txt", "three\n", BASE_TIME + 300, "third");

    fixture.git(&["checkout", "--quiet", "--detach", "HEAD~1"]);

    assert_eq!(
        fixture.open().head().expect("head"),
        HeadInfo::Detached {
            target: second.parse().expect("oid"),
        }
    );
    assert_eq!(fixture.walk_ids(), fixture.git_log_date_order());
}

#[test]
fn remote_tracking_branches_are_listed_without_head_alias() {
    let fixture = Fixture::new();
    let first = fixture.commit("file.txt", "one\n", BASE_TIME + 100, "first");
    let second = fixture.commit("file.txt", "two\n", BASE_TIME + 200, "second");
    fixture.git(&["branch", "topic", &first]);

    let clone = fixture.tmp.path().join("clone");
    fixture.git_at(
        fixture.tmp.path(),
        &[
            "clone",
            "--quiet",
            fixture.repo.to_str().expect("utf-8 path"),
            clone.to_str().expect("utf-8 path"),
        ],
    );
    // Make the alias and the packed-refs code path explicit instead of relying
    // on whatever `clone` happens to write.
    fixture.git_at(
        &clone,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    fixture.git_at(&clone, &["pack-refs", "--all"]);

    let refs = Repo::open(&clone)
        .expect("open clone")
        .refs()
        .expect("refs");
    let remote: Vec<_> = refs
        .iter()
        .filter(|r| r.kind == RefKind::RemoteBranch)
        .map(|r| {
            (
                r.full_name.clone(),
                r.short_name.clone(),
                r.target.to_string(),
            )
        })
        .collect();
    assert_eq!(
        remote,
        vec![
            (
                "refs/remotes/origin/main".to_string(),
                "origin/main".to_string(),
                second,
            ),
            (
                "refs/remotes/origin/topic".to_string(),
                "origin/topic".to_string(),
                first,
            ),
        ],
        "origin/HEAD is a symbolic alias and must be skipped"
    );
}

#[test]
fn many_refs_walk_matches_git_log() {
    let fixture = Fixture::new();
    let mut commits = Vec::new();
    for index in 0..40 {
        commits.push(fixture.commit(
            "file.txt",
            &format!("{index}\n"),
            BASE_TIME + 100 + index,
            &format!("commit {index}"),
        ));
    }
    fixture.git(&[
        "tag",
        "-a",
        "annotated-base",
        "-m",
        "annotated",
        &commits[0],
    ]);

    // Many refs share a handful of targets, so the walk has to deduplicate ref
    // targets before checking objects; one git process creates them all.
    let mut refs = String::new();
    for index in 0..2000 {
        let target = &commits[index % commits.len()];
        refs.push_str(&format!("create refs/tags/many-{index:04} {target}\n"));
    }
    fixture.git_input(&["update-ref", "--stdin"], &refs);

    let expected = fixture.git_log_date_order();
    assert_eq!(fixture.walk_ids(), expected, "loose refs");

    // packed-refs records the peeled target of every annotated tag, and a ref
    // without one is known to point at a non-tag object.
    fixture.git(&["pack-refs", "--all"]);
    assert_eq!(fixture.walk_ids(), expected, "packed refs");

    let listed = fixture.open().refs().expect("refs");
    assert_eq!(
        listed.iter().filter(|r| r.kind == RefKind::Tag).count(),
        2001,
        "the annotated tag and every lightweight tag are listed"
    );
}

#[test]
fn private_namespace_refs_are_not_walked() {
    let fixture = Fixture::new();
    let first = fixture.commit("file.txt", "one\n", BASE_TIME + 100, "first");
    let second = fixture.commit("file.txt", "two\n", BASE_TIME + 200, "second");

    // A commit reachable only from a private ref: a walk that includes private
    // namespaces would list it, and its lane would have no ref badge.
    fixture.git(&["checkout", "--quiet", "--detach", &first]);
    let hidden = fixture.commit(
        "hidden/only.txt",
        "hidden\n",
        BASE_TIME + 150,
        "hidden only",
    );
    fixture.git(&["checkout", "--quiet", "main"]);
    fixture.git(&["update-ref", "refs/custom/x", &hidden]);
    for name in ["refs/prefetch/main", "refs/bisect/bad"] {
        fixture.git(&["update-ref", name, &hidden]);
    }
    fixture.git(&["update-ref", "refs/original/heads/main", &first]);
    fixture.git(&["notes", "add", "-m", "a note", &second]);

    let walked = fixture.walk_ids();
    assert!(
        !walked.contains(&hidden),
        "a commit only reachable from refs/custom is not history"
    );
    assert_eq!(walked, vec![second.clone(), first.clone()]);
    assert_eq!(walked, fixture.git_log_date_order());

    // Path history reuses the walk, so the private commit's path is not there.
    let repo = fixture.open();
    let history: Vec<String> = repo
        .path_history("hidden/only.txt")
        .expect("path history")
        .map(|id| id.expect("commit id").to_string())
        .collect();
    assert!(
        history.is_empty(),
        "a path only touched by a private-ref commit has no history"
    );

    let listed = fixture.open().refs().expect("refs");
    assert_eq!(
        kinds_and_names(&listed),
        vec![(RefKind::LocalBranch, "refs/heads/main".to_string())],
        "private namespaces are not listed as refs either"
    );
}

#[test]
fn stash_is_walked() {
    let fixture = Fixture::new();
    fixture.commit("file.txt", "base\n", BASE_TIME + 100, "base");
    std::fs::write(fixture.path().join("file.txt"), "stashed\n").expect("write");
    fixture.git_dated(
        BASE_TIME + 200,
        &["stash", "push", "--quiet", "-m", "work in progress"],
    );
    let stash = fixture.git(&["rev-parse", "refs/stash"]);

    let walked = fixture.walk_ids();
    assert!(
        walked.contains(&stash),
        "the stash tip must be part of the history"
    );
    assert_eq!(walked, fixture.git_log_date_order());
}

#[test]
fn commit_graph_walk_matches_git_log() {
    let fixture = Fixture::new();
    let base = fixture.commit("main.txt", "base\n", BASE_TIME + 1000, "base");

    fixture.git(&["checkout", "--quiet", "-b", "feature"]);
    fixture.commit("feature.txt", "f1\n", BASE_TIME + 2000, "f1");
    fixture.commit("feature.txt", "f2\n", BASE_TIME + 1100, "f2");

    fixture.git(&["checkout", "--quiet", "main"]);
    fixture.commit("main.txt", "c2\n", BASE_TIME + 3000, "c2");
    fixture.commit("main.txt", "c3\n", BASE_TIME + 1200, "c3");
    fixture.git_dated(
        BASE_TIME + 4000,
        &[
            "merge",
            "--quiet",
            "--no-ff",
            "-m",
            "merge feature",
            "feature",
        ],
    );
    fixture.git(&["tag", "at-base", &base]);

    fixture.git(&["commit-graph", "write", "--reachable", "--split"]);
    assert_eq!(
        fixture.walk_ids(),
        fixture.git_log_date_order(),
        "split commit-graph"
    );

    // Commits written after the graph are missing from it, which is what a
    // repository looks like between fetches; both code paths must agree.
    fixture.commit("main.txt", "late\n", BASE_TIME + 1500, "late");
    fixture.git(&["checkout", "--quiet", "-b", "side", &base]);
    fixture.commit("side.txt", "side\n", BASE_TIME + 1600, "side");
    fixture.git(&["checkout", "--quiet", "main"]);
    assert_eq!(
        fixture.walk_ids(),
        fixture.git_log_date_order(),
        "history newer than the commit-graph"
    );
}

#[test]
fn non_commit_refs_are_ignored() {
    let fixture = Fixture::new();
    fixture.commit("file.txt", "one\n", BASE_TIME + 100, "first");
    fixture.commit("file.txt", "two\n", BASE_TIME + 200, "second");

    let blob = fixture.git_input(&["hash-object", "-w", "--stdin"], "raw\n");
    let tree = fixture.git(&["rev-parse", "HEAD^{tree}"]);
    fixture.git(&["tag", "blob-tag", &blob]);
    fixture.git(&["tag", "tree-tag", &tree]);
    fixture.git(&["tag", "-a", "annotated-blob", "-m", "a blob", &blob]);
    fixture.git(&["pack-refs", "--all"]);

    let refs = fixture.open().refs().expect("refs");
    assert_eq!(
        kinds_and_names(&refs),
        vec![(RefKind::LocalBranch, "refs/heads/main".to_string())],
        "refs that do not resolve to a commit are not listed"
    );
    assert_eq!(fixture.walk_ids(), fixture.git_log_date_order());
}

#[test]
#[ignore = "walks STRATA_BENCH_REPO; run explicitly with --ignored"]
fn bench_walk_commits_per_second() {
    let path = std::env::var("STRATA_BENCH_REPO")
        .expect("set STRATA_BENCH_REPO to a repository path to benchmark");
    let repo = Repo::open(path).expect("open benchmark repo");

    let start = Instant::now();
    let mut count = 0usize;
    for commit in repo.walk_commits().expect("walk") {
        commit.expect("commit");
        count += 1;
    }
    let elapsed = start.elapsed();

    println!(
        "walked {count} commits in {elapsed:?} ({:.0} commits/sec)",
        count as f64 / elapsed.as_secs_f64()
    );
}

#[test]
fn parents_lists_first_parent_first() {
    let fixture = Fixture::new();
    let base = fixture.commit("main.txt", "base\n", BASE_TIME + 100, "base");
    fixture.git(&["checkout", "--quiet", "-b", "feature"]);
    let feature = fixture.commit("feature.txt", "feature\n", BASE_TIME + 200, "feature");
    fixture.git(&["checkout", "--quiet", "main"]);
    let main = fixture.commit("main.txt", "main\n", BASE_TIME + 300, "main");
    fixture.git_dated(
        BASE_TIME + 400,
        &["merge", "--quiet", "--no-ff", "-m", "merge", "feature"],
    );
    let merge = fixture.head();

    let repo = fixture.open();
    let parents: Vec<String> = repo
        .parents(merge.parse().expect("oid"))
        .expect("parents")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(parents, vec![main, feature]);
    assert!(
        repo.parents(base.parse().expect("oid"))
            .expect("root")
            .is_empty()
    );
}

#[test]
fn stashes_are_listed_newest_first() {
    let fixture = Fixture::new();
    fixture.commit("file.txt", "base\n", BASE_TIME + 100, "base");
    assert!(fixture.open().stashes().expect("no stash ref").is_empty());

    for (date, content, message) in [(200, "one\n", "first"), (300, "two\n", "second")] {
        std::fs::write(fixture.path().join("file.txt"), content).expect("write");
        fixture.git_dated(
            BASE_TIME + date,
            &["stash", "push", "--quiet", "-m", message],
        );
    }

    let stashes = fixture.open().stashes().expect("stashes");
    let listed: Vec<(usize, String)> = stashes
        .iter()
        .map(|stash| (stash.index, stash.message.clone()))
        .collect();
    assert_eq!(
        listed,
        vec![
            (0, "On main: second".to_string()),
            (1, "On main: first".to_string()),
        ]
    );
    assert_eq!(
        stashes[0].id.to_string(),
        fixture.git(&["rev-parse", "stash@{0}"]).trim()
    );
}

#[test]
fn remotes_are_sorted_by_name_and_keep_their_urls() {
    let fixture = Fixture::new();
    fixture.git(&["remote", "add", "zeta", "https://example.com/zeta.git"]);
    // Added second, so a listing in configuration order would put it second.
    fixture.git(&["remote", "add", "alpha", "https://example.com/alpha.git"]);
    fixture.git(&[
        "remote",
        "set-url",
        "--push",
        "alpha",
        "ssh://example.com/alpha.git",
    ]);

    assert_eq!(
        fixture.open().remotes().expect("remotes"),
        vec![
            RemoteInfo {
                name: "alpha".into(),
                fetch_url: Some("https://example.com/alpha.git".into()),
                push_url: Some("ssh://example.com/alpha.git".into()),
            },
            RemoteInfo {
                name: "zeta".into(),
                fetch_url: Some("https://example.com/zeta.git".into()),
                push_url: None,
            },
        ]
    );
}

#[test]
fn a_remote_without_fetched_branches_is_still_listed() {
    let fixture = Fixture::new();
    fixture.commit("a.txt", "one\n", BASE_TIME, "base");
    fixture.git_at(
        fixture.tmp.path(),
        &["init", "--quiet", "--bare", "origin.git"],
    );
    let bare = fixture.tmp.path().join("origin.git");
    let bare_url = bare.to_str().expect("utf-8 path").to_string();
    fixture.git(&["remote", "add", "origin", &bare_url]);
    fixture.git(&["push", "--quiet", "--set-upstream", "origin", "main"]);
    // Configured but never fetched: it has no remote-tracking branches at all.
    fixture.git(&["remote", "add", "spare", "https://example.com/spare.git"]);

    assert!(
        fixture.ref_exists("refs/remotes/origin/main"),
        "the first remote should have a tracking branch"
    );
    assert!(!fixture.ref_exists("refs/remotes/spare/main"));

    let remotes = fixture.open().remotes().expect("remotes");
    let names: Vec<&str> = remotes.iter().map(|remote| remote.name.as_str()).collect();
    assert_eq!(names, vec!["origin", "spare"]);
    assert_eq!(remotes[0].fetch_url.as_deref(), Some(bare_url.as_str()));
    assert_eq!(
        remotes[0].push_url, None,
        "pushes use the fetch URL unless a push URL is configured"
    );
    assert_eq!(
        remotes[1].fetch_url.as_deref(),
        Some("https://example.com/spare.git")
    );
}

#[test]
fn a_repository_without_remotes_lists_none() {
    let fixture = Fixture::new();
    fixture.commit("a.txt", "one\n", BASE_TIME, "base");
    assert!(fixture.open().remotes().expect("remotes").is_empty());
}
