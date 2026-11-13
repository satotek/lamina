//! Line attribution: which commit introduced each line of a file.
//!
//! This uses gix's blame implementation rather than `git blame --porcelain`.
//! `gix-blame` is already a dependency of the pinned `gix`, follows renames
//! through `Options::rewrites`, and returns exactly the hunk model this crate
//! wants (`start_in_blamed_file`, `start_in_source_file`, `source_file_name`).
//! Shelling out would add a second process per request and would have to
//! re-parse the porcelain format, without improving on rename handling.
//!
//! Blame reads history until every line is attributed, so a large file can
//! take a while; callers run it on the session's blame worker, never on the
//! thread that serves the rest of the UI.

use std::collections::HashMap;

use gix::bstr::{BStr, ByteSlice};

use super::Repo;
use crate::{Error, Oid, Result};

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

impl Repo {
    /// Attribute every line of `path` at `commit` to the commit that last
    /// changed it, following renames of the file itself.
    ///
    /// A path that does not exist at `commit` is an error: blame of a missing
    /// file is more likely a stale selection than an empty answer.
    pub fn blame(&self, commit: Oid, path: &str) -> Result<Blame> {
        let repo = self.local();
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

        let lines = split_lines(&outcome.blob);
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

        Ok(Blame {
            hunks,
            commits,
            lines,
        })
    }
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
    use crate::test_support::{AUTHOR_EMAIL, AUTHOR_NAME, TestRepo};

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
}
