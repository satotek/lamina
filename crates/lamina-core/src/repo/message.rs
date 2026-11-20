//! Commit messages and the message prepared for an in-progress merge.
//!
//! Both reads exist to prefill an editor: `commit_message` for "amend", and
//! `merge_message` for the commit that finishes a merge or a squashed merge.

use std::io::ErrorKind;

use gix::bstr::ByteSlice;

use super::Repo;
use crate::{Error, Oid, Result};

/// `MERGE_MSG` lives in the per-worktree git dir, so a linked worktree must read
/// its own file instead of the main one's.
const MERGE_MSG: &str = "MERGE_MSG";

/// `SQUASH_MSG` is what `git merge --squash` leaves behind; it also lives in the
/// per-worktree git dir.
const SQUASH_MSG: &str = "SQUASH_MSG";

impl Repo {
    /// The full message of `commit` (summary and body) as stored in the object,
    /// without trimming.
    ///
    /// Nothing is stripped, including trailing whitespace lines: the caller
    /// prefills an editor with this, and git applies `commit.cleanup` when the
    /// message is committed. `git log -1 --format=%B` prints the same text with
    /// exactly one `\n` appended, so strip one trailing newline from its output
    /// when comparing the two.
    ///
    /// The `encoding` header is ignored: gix parses it but has no decoder, and
    /// this crate carries no transcoder for the encodings a commit can name. Bytes
    /// that are not valid UTF-8 therefore come back with U+FFFD replacement
    /// characters rather than being converted from the declared encoding.
    pub fn commit_message(&self, commit: Oid) -> Result<String> {
        let repo = self.local();
        let commit = repo.find_commit(commit).map_err(Error::gix)?;
        // `message_raw` decodes the object headers up to the message, which is
        // where a truncated or corrupt object shows up as an error.
        let message = commit.message_raw().map_err(Error::gix)?;
        Ok(message.to_str_lossy().into_owned())
    }

    /// The message git has prepared for a merge or squashed merge that is still
    /// in progress, or `None` when neither is.
    ///
    /// `MERGE_MSG` is read first and `SQUASH_MSG` only when `MERGE_MSG` is
    /// missing or holds nothing but comments: a conflicted `git merge --squash`
    /// leaves both, with `MERGE_MSG` carrying just the conflict summary and
    /// `SQUASH_MSG` carrying the log that becomes the message.
    ///
    /// The result is the cleaned message, i.e. what `git commit` stores with
    /// `commit.cleanup=strip` (its default when the message is edited): lines
    /// starting with `core.commentChar` are dropped, trailing whitespace on each
    /// line is removed, leading and trailing blank lines are dropped, runs of
    /// blank lines collapse to one, and the result ends with a single `\n`.
    /// `None` also means the file exists but nothing is left after cleaning,
    /// because an empty prefill is no more useful than no prefill.
    ///
    /// `git commit --no-edit` uses the `whitespace` cleanup instead and keeps
    /// comment lines, so its commit message contains the conflict summary block;
    /// this read removes it on purpose.
    pub fn merge_message(&self) -> Result<Option<String>> {
        let comment = self.comment_prefix();
        for name in [MERGE_MSG, SQUASH_MSG] {
            let Some(raw) = self.read_git_dir_file(name)? else {
                continue;
            };
            let message = clean_message(&raw, &comment);
            if !message.is_empty() {
                return Ok(Some(message));
            }
        }
        Ok(None)
    }

    /// A file in this worktree's private git dir; `None` when it does not exist.
    fn read_git_dir_file(&self, name: &str) -> Result<Option<Vec<u8>>> {
        match std::fs::read(self.git_dir().join(name)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
            Err(err) => Err(Error::Io(err)),
        }
    }

    /// The prefix that marks a comment line in a message.
    ///
    /// `core.commentChar = auto` asks git to pick a character that does not clash
    /// with the message. A generated message such as `MERGE_MSG` is written
    /// before that choice is made, with git's default `#`, so `auto` is read as
    /// `#`; once git does pick another character it records it in
    /// `core.commentChar`, and that value is used instead.
    fn comment_prefix(&self) -> Vec<u8> {
        let repo = self.local();
        let config = repo.config_snapshot();
        let Some(value) = config.string("core.commentChar") else {
            return vec![b'#'];
        };
        let value = value.as_bstr();
        if value.is_empty() || value == b"auto".as_bstr() {
            return vec![b'#'];
        }
        // git accepts one character, which may be multi-byte; the prefix is that
        // character's bytes.
        value
            .chars()
            .next()
            .map(|ch| ch.to_string().into_bytes())
            .unwrap_or_else(|| vec![b'#'])
    }
}

/// Apply git's `strip` cleanup to `raw`, decoding the leftovers lossily.
fn clean_message(raw: &[u8], comment: &[u8]) -> String {
    let mut lines: Vec<&[u8]> = Vec::new();
    for line in raw.split(|byte| *byte == b'\n') {
        if line.starts_with(comment) {
            continue;
        }
        // git's `cleanup` trims every byte `isspace` matches, which includes the
        // `\r` of a CRLF message.
        let mut end = line.len();
        while end > 0 && is_space(line[end - 1]) {
            end -= 1;
        }
        lines.push(&line[..end]);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    let first = lines
        .iter()
        .position(|line| !line.is_empty())
        .unwrap_or(lines.len());

    let mut cleaned = Vec::new();
    let mut blank_before = false;
    for line in &lines[first..] {
        if line.is_empty() {
            blank_before = true;
            continue;
        }
        if blank_before {
            cleaned.push(b'\n');
            blank_before = false;
        }
        cleaned.extend_from_slice(line);
        cleaned.push(b'\n');
    }
    cleaned.to_str_lossy().into_owned()
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(raw: &str) -> String {
        clean_message(raw.as_bytes(), b"#")
    }

    #[test]
    fn strip_cleanup_drops_comments_and_blank_lines() {
        assert_eq!(clean("subject\n"), "subject\n");
        assert_eq!(clean("subject"), "subject\n");
        assert_eq!(clean("\n\n\nsubject\n"), "subject\n");
        assert_eq!(clean("subject\n\n\n\nbody\n\n\n"), "subject\n\nbody\n");
        assert_eq!(clean("subject   \nbody\t\n"), "subject\nbody\n");
        assert_eq!(clean("subject\n# comment\nbody\n"), "subject\nbody\n");
        assert_eq!(clean("subject\n   \n\t\nbody\n"), "subject\n\nbody\n");
        assert_eq!(clean("# only comments\n#\n"), "");
        assert_eq!(clean(""), "");
    }

    #[test]
    fn a_different_comment_prefix_only_drops_matching_lines() {
        assert_eq!(
            clean_message(b"subject\n; gone\n# kept\n", b";"),
            "subject\n# kept\n"
        );
    }
}
