//! Line attribution: which commit introduced each line of a file.
//!
//! Short histories and small files use `gix-blame` in-process, avoiding the
//! cost of spawning a subprocess. A shallow clone always takes the `git` path:
//! `gix-blame` follows parent commits that the clone does not contain, whereas
//! `git blame` stops at the shallow boundary and attributes the remaining lines
//! to the boundary commit.
//!
//! Reads in Strata normally use `gix`, with `blame` on deep histories or large
//! files as a deliberate, measured exception that falls back to `git blame
//! --incremental` via the read-only `git_cli` runner:
//! - On Linux (1.5M commits): `MAINTAINERS` (~935 KiB, 31k lines, 22k touching
//!   commits) did not finish in >6m in debug and took 47.30s in release with
//!   `gix` (even with commit-graph), whereas `git blame --incremental` finished
//!   in 17.84s; `kernel/sched/core.c` (~296 KiB, 11k lines, 2k touching
//!   commits) took 7.53s in release with `gix` (5.95s with a 32 MiB object
//!   cache; 2.59s without rename tracking, which breaks rename following) vs
//!   1.45s with `git blame --incremental`; a small shallow file (`README`, 168
//!   lines, 5 commits) took 0.15s with `gix` vs 0.16s with `git`.
//! - `gix-blame` walks whole-tree diffs on modifying commits (and in debug
//!   builds re-tokenizes both blobs on every step) and offers no cancellation
//!   hook, whereas `git blame --incremental` uses commit-graph/bloom filters
//!   and can be stopped immediately by killing the child process.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use gix::bstr::{BStr, ByteSlice};

use super::Repo;
use super::log::Shallow;
use crate::git_cli::Git;
use crate::{Error, Oid, Result};

/// Beyond this first-parent depth or blob size, `gix-blame` is consistently
/// slower than spawning `git blame --incremental` and cannot be cancelled.
const GIX_MAX_DEPTH: usize = 256;
const GIX_MAX_BLOB_BYTES: usize = 64 * 1024;
const OBJECT_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// Blame of one file at one commit.
///
/// Hunks are sorted by `start` and together cover every line exactly once, so
/// a viewer can walk them in order and draw `len` lines per hunk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Blame {
    pub hunks: Vec<BlameHunk>,
    /// Author, time and summary per commit referenced by `hunks`.
    pub commits: HashMap<Oid, BlameCommit>,
    /// The blamed file's content, one entry per line, without terminators.
    pub lines: Vec<String>,
}

/// One run of consecutive lines that share an origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameHunk {
    /// The commit that introduced these lines in the source file.
    pub commit: Oid,
    /// Path the lines had in `commit`; differs from the blamed path after a
    /// rename, which is what lets a viewer follow the file through history.
    pub orig_path: String,
    /// First line in `orig_path`, 1-based.
    pub orig_start: u32,
    /// First line in the blamed file, 1-based.
    pub start: u32,
    /// Number of lines in the hunk.
    pub len: u32,
}

/// What the UI needs from a blamed line's commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameCommit {
    pub author_name: String,
    pub author_email: String,
    /// Author time in seconds since the Unix epoch.
    pub author_time: i64,
    /// First line of the commit message.
    pub summary: String,
}

pub(crate) struct BlameProgressUpdate {
    pub(crate) lines: Option<Arc<Vec<String>>>,
    pub(crate) hunks: Vec<BlameHunk>,
    pub(crate) commits: HashMap<Oid, BlameCommit>,
}

impl Repo {
    /// Attribute every line of `path` at `commit` to the commit that last
    /// changed it, following renames of the file itself.
    ///
    /// A path that does not exist at `commit` is an error: blame of a missing
    /// file is more likely a stale selection than an empty answer.
    pub fn blame(&self, commit: Oid, path: &str) -> Result<Blame> {
        Ok(self
            .blame_inner(commit, path, None, &|| false, None)?
            .expect("uncancelled blame returns a result"))
    }

    /// Like [`blame`](Self::blame), but accepts a session [`Git`] handle (for
    /// configured executable and command logging) and stops early if
    /// `cancelled()` becomes true, returning `Ok(None)`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn blame_cancellable(
        &self,
        commit: Oid,
        path: &str,
        git: &Git,
        cancelled: &(impl Fn() -> bool + Send + Sync),
    ) -> Result<Option<Blame>> {
        self.blame_inner(commit, path, Some(git), cancelled, None)
    }

    /// Like [`blame_cancellable`](Self::blame_cancellable), but emits early file
    /// lines and batched hunks on the `git blame --incremental` fallback path.
    pub(crate) fn blame_cancellable_streaming(
        &self,
        commit: Oid,
        path: &str,
        git: &Git,
        cancelled: &(impl Fn() -> bool + Send + Sync),
        on_progress: &mut dyn FnMut(BlameProgressUpdate),
    ) -> Result<Option<Blame>> {
        self.blame_inner(commit, path, Some(git), cancelled, Some(on_progress))
    }

    fn blame_inner(
        &self,
        commit: Oid,
        path: &str,
        git: Option<&Git>,
        cancelled: &(impl Fn() -> bool + Send + Sync),
        mut on_progress: Option<&mut dyn FnMut(BlameProgressUpdate)>,
    ) -> Result<Option<Blame>> {
        if cancelled() {
            return Ok(None);
        }
        let mut repo = self.local();
        let blob_data = {
            let tree = repo
                .find_commit(commit)
                .map_err(Error::gix)?
                .tree()
                .map_err(Error::gix)?;
            let entry = tree
                .lookup_entry_by_path(path)
                .map_err(Error::gix)?
                .ok_or_else(|| Error::other(format!("{path} does not exist at commit {commit}")))?;
            if !entry.mode().is_blob_or_symlink() {
                return Err(Error::other(format!(
                    "{path} is not a file at commit {commit}"
                )));
            }
            entry.object().map_err(Error::gix)?.into_blob().take_data()
        };

        // A pointer's lines belong to LFS metadata, not to the user's file;
        // blaming them would attribute `version` and `oid` to a commit.
        if crate::lfs::LfsPointer::parse(&blob_data).is_some() {
            return Ok(Some(Blame::default()));
        }

        let lines = split_lines(&blob_data);
        if lines.is_empty() {
            return Ok(Some(Blame::default()));
        }
        if cancelled() {
            return Ok(None);
        }

        let shallow = Shallow::load(&repo)?;
        let use_git = !shallow.is_empty()
            || blob_data.len() > GIX_MAX_BLOB_BYTES
            || exceeds_depth(&repo, commit, GIX_MAX_DEPTH);
        if use_git {
            let owned_git;
            let git = match git {
                Some(git) => git,
                None => {
                    owned_git = self.default_git();
                    &owned_git
                }
            };
            if let Some(emit) = on_progress.as_deref_mut() {
                if cancelled() {
                    return Ok(None);
                }
                emit(BlameProgressUpdate {
                    lines: Some(Arc::new(lines.clone())),
                    hunks: Vec::new(),
                    commits: HashMap::new(),
                });
            }
            let Some(git_blame) =
                git.blame_incremental_streaming(commit, path, cancelled, |batch| {
                    if let Some(emit) = on_progress.as_deref_mut() {
                        emit(BlameProgressUpdate {
                            lines: None,
                            hunks: batch.hunks,
                            commits: batch.commits,
                        });
                    }
                })?
            else {
                return Ok(None);
            };
            return Ok(Some(Blame {
                hunks: git_blame.hunks,
                commits: git_blame.commits,
                lines,
            }));
        }

        repo.object_cache_size_if_unset(OBJECT_CACHE_BYTES);
        let outcome = repo
            .blame_file(
                BStr::new(path),
                commit,
                gix::repository::blame_file::Options {
                    // Rename tracking is what makes a moved file keep its history
                    // instead of blaming the rename commit for every line.
                    rewrites: Some(gix::diff::Rewrites::default()),
                    ..Default::default()
                },
            )
            .map_err(Error::gix)?;
        if cancelled() {
            return Ok(None);
        }

        let mut commits = HashMap::new();
        let mut hunks = Vec::with_capacity(outcome.entries.len());
        for entry in outcome.entries {
            if let std::collections::hash_map::Entry::Vacant(slot) = commits.entry(entry.commit_id)
            {
                slot.insert(commit_metadata(&repo, entry.commit_id)?);
            }
            hunks.push(BlameHunk {
                commit: entry.commit_id,
                orig_path: entry
                    .source_file_name
                    .map(|path| path.to_str_lossy().into_owned())
                    .unwrap_or_else(|| path.to_owned()),
                orig_start: entry.start_in_source_file.saturating_add(1),
                start: entry.start_in_blamed_file.saturating_add(1),
                len: entry.len.get(),
            });
        }
        hunks.sort_by_key(|hunk| hunk.start);

        Ok(Some(Blame {
            hunks,
            commits,
            lines,
        }))
    }

    fn default_git(&self) -> Git {
        let dir = self
            .workdir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.git_dir().to_path_buf());
        Git::new(dir)
    }
}

/// True when `start` has more than `limit` first-parent ancestors.
fn exceeds_depth(repo: &gix::Repository, start: Oid, limit: usize) -> bool {
    let graph = repo.commit_graph_if_enabled().ok().flatten();
    let mut current = start;
    for _ in 0..limit {
        let next = if let Some(graph) = &graph
            && let Some(commit) = graph.commit_by_id(current)
        {
            commit
                .iter_parents()
                .next()
                .and_then(|pos| pos.ok().map(|p| graph.commit_at(p).id().to_owned()))
        } else {
            let Ok(commit) = repo.find_commit(current) else {
                return false;
            };
            commit.parent_ids().next().map(|id| id.detach())
        };
        let Some(parent) = next else {
            return false;
        };
        current = parent;
    }
    true
}

fn commit_metadata(repo: &gix::Repository, id: Oid) -> Result<BlameCommit> {
    let commit = repo.find_commit(id).map_err(Error::gix)?;
    let author = commit.author().map_err(Error::gix)?;
    let summary = first_line(commit.message_raw_sloppy());
    Ok(BlameCommit {
        author_name: author.name.to_str_lossy().trim().to_owned(),
        author_email: author.email.to_str_lossy().trim().to_owned(),
        // A malformed date must not hide the attribution; git treats it as 0 too.
        author_time: author.time().map_or(0, |time| time.seconds),
        summary,
    })
}

/// The first line of a commit message, decoded lossily like the log view.
fn first_line(message: &gix::bstr::BStr) -> String {
    let line = message
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    String::from_utf8_lossy(line.strip_suffix(b"\r").unwrap_or(line)).into_owned()
}

/// Split file content the way the diff tokenizer does: one entry per line,
/// including a final unterminated one, with the terminator removed.
fn split_lines(blob: &[u8]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut rest = blob;
    while !rest.is_empty() {
        let end = rest.find_byte(b'\n').map_or(rest.len(), |index| index + 1);
        let (line, remaining) = rest.split_at(end);
        rest = remaining;
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        // A CRLF checkout should not show a stray carriage return at the end
        // of every line in a blame view.
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        lines.push(String::from_utf8_lossy(line).into_owned());
    }
    lines
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::test_support::{AUTHOR_EMAIL, AUTHOR_NAME, TestRepo, shallow_clone_fixture};

    /// Commit per 1-based line, read from the hunks.
    fn line_commits(blame: &Blame) -> Vec<Oid> {
        let mut commits = vec![None; blame.lines.len()];
        for hunk in &blame.hunks {
            for line in hunk.start..hunk.start + hunk.len {
                let slot = commits
                    .get_mut(line as usize - 1)
                    .expect("hunk within the file");
                *slot = Some(hunk.commit);
            }
        }
        commits
            .into_iter()
            .map(|id| id.expect("every line covered"))
            .collect()
    }

    /// The same attribution from `git blame --line-porcelain`, so the test
    /// checks against git rather than only against our own expectation.
    fn git_line_commits(repo: &TestRepo, commit: Oid, path: &str) -> Vec<Oid> {
        let output = repo.git(&["blame", "--line-porcelain", &commit.to_string(), "--", path]);
        let mut commits = HashMap::new();
        for line in output.lines() {
            let mut fields = line.split(' ');
            let Some(sha) = fields.next() else { continue };
            if sha.len() != 40 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            let mut numbers = fields.map(|field| field.parse::<u32>());
            let Some(_orig) = numbers.next().and_then(std::result::Result::ok) else {
                continue;
            };
            let Some(result) = numbers.next().and_then(std::result::Result::ok) else {
                continue;
            };
            commits.insert(result, sha.parse::<Oid>().expect("hex object id"));
        }
        (1..=commits.len() as u32)
            .map(|line| *commits.get(&line).expect("git blamed every line"))
            .collect()
    }

    fn assert_hunks_cover(blame: &Blame) {
        let mut next = 1;
        for hunk in &blame.hunks {
            assert_eq!(hunk.start, next, "hunks must be sorted and contiguous");
            assert!(hunk.len > 0);
            next += hunk.len;
        }
        assert_eq!(
            next as usize,
            blame.lines.len() + 1,
            "hunks must cover exactly the file"
        );
    }

    #[test]
    fn attributes_lines_to_the_commit_that_changed_them() {
        let repo = TestRepo::new();
        repo.write("notes.txt", "one\ntwo\nthree\n");
        let first = repo.commit("first");
        repo.write("notes.txt", "one\nTWO\nthree\n");
        let second = repo.commit("second");
        repo.write("notes.txt", "one\nTWO\nthree\nfour\n");
        let third = repo.commit("third");

        let blame = repo.open().blame(third, "notes.txt").expect("blame");
        assert_eq!(blame.lines, ["one", "TWO", "three", "four"]);
        assert_eq!(
            line_commits(&blame),
            [first, second, first, third],
            "each line belongs to the commit that last changed it"
        );
        assert_eq!(
            git_line_commits(&repo, third, "notes.txt"),
            line_commits(&blame)
        );
        assert_hunks_cover(&blame);

        let metadata = &blame.commits[&second];
        assert_eq!(metadata.author_name, AUTHOR_NAME);
        assert_eq!(metadata.author_email, AUTHOR_EMAIL);
        assert_eq!(metadata.summary, "second");
        assert_eq!(blame.hunks.len(), 4);
        assert!(blame.hunks.iter().all(|hunk| hunk.orig_path == "notes.txt"));
    }

    #[test]
    fn follows_a_rename_and_reports_the_original_path() {
        let repo = TestRepo::new();
        repo.write("old-name.txt", "alpha\nbeta\ngamma\n");
        let first = repo.commit("create");
        repo.write("old-name.txt", "alpha\nBETA\ngamma\n");
        let second = repo.commit("edit");
        repo.git(&["mv", "old-name.txt", "new-name.txt"]);
        let renamed = repo.commit("rename");

        let blame = repo.open().blame(renamed, "new-name.txt").expect("blame");
        assert_eq!(blame.lines, ["alpha", "BETA", "gamma"]);
        assert_eq!(line_commits(&blame), [first, second, first]);
        assert_eq!(
            git_line_commits(&repo, renamed, "new-name.txt"),
            line_commits(&blame)
        );
        assert_hunks_cover(&blame);
        assert!(
            blame
                .hunks
                .iter()
                .all(|hunk| hunk.orig_path == "old-name.txt"),
            "lines keep the path they had when they were written: {:?}",
            blame.hunks
        );
        assert!(
            blame.hunks.iter().all(|hunk| hunk.commit != renamed),
            "the rename commit introduced no line"
        );
    }

    #[test]
    fn shallow_clone_blame_stops_at_the_boundary() {
        let (_source, clone) = shallow_clone_fixture(&["--depth", "2"]);
        let head = clone.head();
        let boundary = clone.rev("HEAD^1");
        let blame = clone.open().blame(head, "dir/new.txt").expect("blame");

        assert_eq!(blame.lines, ["one", "TWO", "three", "four", "five"]);
        assert_hunks_cover(&blame);
        assert!(
            blame
                .hunks
                .iter()
                .all(|hunk| hunk.commit == head || hunk.commit == boundary),
            "lines below the boundary belong to the boundary commit: {:?}",
            blame.hunks
        );
        assert_eq!(
            line_commits(&blame),
            git_line_commits(&clone, head, "dir/new.txt"),
            "the fallback must agree with git blame"
        );
    }

    #[test]
    fn missing_path_is_an_error() {
        let repo = TestRepo::new();
        repo.write("present.txt", "text\n");
        let commit = repo.commit("only");
        let error = repo
            .open()
            .blame(commit, "absent.txt")
            .expect_err("missing path");
        assert!(error.to_string().contains("does not exist"), "{error}");
    }

    #[test]
    fn git_blame_incremental_matches_gix_including_renames_and_metadata() {
        let repo = TestRepo::new();
        repo.write("old-name.txt", "alpha\nbeta\ngamma\n");
        let first = repo.commit("create");
        repo.write("old-name.txt", "alpha\nBETA\ngamma\n");
        let second = repo.commit("edit");
        repo.git(&["mv", "old-name.txt", "new-name.txt"]);
        let renamed = repo.commit("rename");

        let opened = repo.open();
        let gix_blame = opened.blame(renamed, "new-name.txt").expect("gix blame");
        let git = repo.git_handle();
        let blob_lines = split_lines(b"alpha\nBETA\ngamma\n");
        let result = git
            .blame_incremental(renamed, "new-name.txt", &|| false)
            .expect("git blame")
            .expect("not cancelled");
        let git_blame = Blame {
            hunks: result.hunks,
            commits: result.commits,
            lines: blob_lines,
        };

        assert_hunks_cover(&git_blame);
        assert_eq!(line_commits(&git_blame), [first, second, first]);
        assert_eq!(git_blame, gix_blame);
    }

    #[test]
    fn git_blame_incremental_handles_special_filenames_and_line_endings() {
        let repo = TestRepo::new();
        repo.write("a b.txt", "one\r\ntwo\r\n");
        repo.write("café.txt", "no-trailing-newline");
        repo.write("empty.txt", "");
        let commit = repo.commit("special");

        let git = repo.git_handle();
        let opened = repo.open();

        for (path, expected_lines) in [
            ("a b.txt", vec!["one", "two"]),
            ("café.txt", vec!["no-trailing-newline"]),
        ] {
            let gix_blame = opened.blame(commit, path).expect("gix blame");
            let result = git
                .blame_incremental(commit, path, &|| false)
                .expect("git blame")
                .expect("not cancelled");
            let git_blame = Blame {
                hunks: result.hunks,
                commits: result.commits,
                lines: gix_blame.lines.clone(),
            };
            assert_eq!(git_blame.lines, expected_lines);
            assert_hunks_cover(&git_blame);
            assert_eq!(git_blame, gix_blame);
        }

        let empty = opened.blame(commit, "empty.txt").expect("empty");
        assert!(empty.lines.is_empty());
        assert!(empty.hunks.is_empty());
        assert!(empty.commits.is_empty());
    }

    #[test]
    fn falls_back_to_git_for_large_files_and_deep_histories() {
        // Large file (> GIX_MAX_BLOB_BYTES) triggers the git CLI path.
        let repo = TestRepo::new();
        let mut body = String::new();
        for i in 0..2_500 {
            body.push_str(&format!(
                "line {i:04}: padding to exceed 64 KiB threshold\n"
            ));
        }
        assert!(body.len() > GIX_MAX_BLOB_BYTES);
        repo.write("large.txt", &body);
        let first = repo.commit("initial large");
        let modified = body.replacen("line 0010:", "line 0010 edited:", 1);
        repo.write("large.txt", &modified);
        let second = repo.commit("edit line 10");

        let git = repo.git_handle();
        let blame = repo
            .open()
            .blame_cancellable(second, "large.txt", &git, &|| false)
            .expect("blame")
            .expect("not cancelled");
        assert_hunks_cover(&blame);
        let line_ids = line_commits(&blame);
        assert_eq!(line_ids[0], first);
        assert_eq!(line_ids[10], second);
        assert_eq!(line_ids[11], first);
        assert_eq!(git.log().last().expect("git blame logged").args[0], "blame");

        // Deep history (> GIX_MAX_DEPTH) also triggers the git CLI path even on a tiny file.
        let deep = TestRepo::new();
        let total = GIX_MAX_DEPTH + 10;
        let mut stream = String::new();
        stream.push_str("blob\nmark :1\ndata 8\nalpha\n1\n\n");
        stream.push_str("blob\nmark :2\ndata 8\nalpha\n2\n\n");
        for index in 0..total {
            let msg = format!("c{index}");
            let blob = if index + 1 == total { ":2" } else { ":1" };
            stream.push_str("commit refs/heads/main\n");
            stream.push_str(&format!(
                "committer {AUTHOR_NAME} <{AUTHOR_EMAIL}> {} +0000\n",
                1_700_000_000 + index
            ));
            stream.push_str(&format!("data {}\n{}\n", msg.len(), msg));
            stream.push_str(&format!("M 100644 {blob} tiny.txt\n\n"));
        }
        let mut child = deep
            .command("git")
            .args(["fast-import", "--quiet"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("fast-import");
        std::io::Write::write_all(child.stdin.as_mut().expect("stdin"), stream.as_bytes())
            .expect("write stream");
        assert!(child.wait().expect("wait").success());
        deep.git(&["reset", "--hard", "HEAD"]);

        let head = deep.head();
        let commits = deep.rev_list("HEAD");
        assert_eq!(commits.len(), total);
        let deep_git = deep.git_handle();
        let deep_blame = deep
            .open()
            .blame_cancellable(head, "tiny.txt", &deep_git, &|| false)
            .expect("deep blame")
            .expect("not cancelled");
        assert_hunks_cover(&deep_blame);
        assert_eq!(line_commits(&deep_blame), [commits[0], commits[total - 1]]);
        assert_eq!(
            deep_git.log().last().expect("git blame logged").args[0],
            "blame"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancelling_blame_kills_the_git_child_process() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        let repo = TestRepo::new();
        let large = "x".repeat(GIX_MAX_BLOB_BYTES + 1024);
        repo.write("big.txt", &format!("{large}\n"));
        let commit = repo.commit("big");

        let pid_file = repo.path().join("child.pid");
        let script = repo.path().join("slow-git");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > \"{}\"\nexec sleep 60\n",
                pid_file.display()
            ),
        )
        .expect("write mock git");
        let mut perms = std::fs::metadata(&script).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).expect("chmod");

        let git = repo.git_handle().with_git_binary(&script);
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_flag = Arc::clone(&cancelled);
        let opened = repo.open();

        let watcher = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !pid_file.exists() {
                assert!(Instant::now() < deadline, "mock git never started");
                std::thread::sleep(Duration::from_millis(10));
            }
            let pid_text = std::fs::read_to_string(&pid_file).expect("read pid");
            cancelled_flag.store(true, Ordering::SeqCst);
            pid_text.trim().parse::<u32>().expect("pid")
        });

        let start = Instant::now();
        let result = opened
            .blame_cancellable(commit, "big.txt", &git, &|| {
                cancelled.load(Ordering::SeqCst)
            })
            .expect("cancelled blame does not error");
        let elapsed = start.elapsed();
        let pid = watcher.join().expect("watcher join");

        assert!(result.is_none(), "cancelled blame must return None");
        assert!(
            elapsed < Duration::from_secs(5),
            "cancellation should kill child promptly, took {elapsed:?}"
        );
        // Verify the child process itself was terminated rather than left sleeping.
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .expect("kill -0")
            .success();
        assert!(
            !alive,
            "child process {pid} should be dead after cancellation"
        );
    }

    #[test]
    #[ignore = "blames STRATA_BENCH_REPO; run explicitly with --ignored"]
    fn bench_blame_on_repo() {
        let repo_path = std::env::var("STRATA_BENCH_REPO")
            .expect("set STRATA_BENCH_REPO to a repository path to benchmark");
        let file_path =
            std::env::var("STRATA_BENCH_PATH").unwrap_or_else(|_| "kernel/sched/core.c".into());
        let repo = Repo::open(&repo_path).expect("open benchmark repo");
        let head = match repo.head().expect("head") {
            crate::HeadInfo::Branch { target, .. } => target.expect("head target"),
            crate::HeadInfo::Detached { target } => target,
        };
        let start = std::time::Instant::now();
        let blame = repo.blame(head, &file_path).expect("blame");
        let elapsed = start.elapsed();
        assert_hunks_cover(&blame);
        assert!(
            blame
                .hunks
                .iter()
                .all(|hunk| blame.commits.contains_key(&hunk.commit)),
            "every hunk commit must have metadata"
        );
        eprintln!(
            "Repo::blame({file_path}) finished in {elapsed:?}: {} lines, {} hunks, {} commits",
            blame.lines.len(),
            blame.hunks.len(),
            blame.commits.len(),
        );
    }
}
