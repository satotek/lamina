//! Cancellable line attribution via `git blame --incremental`.
//!
//! `Repo::blame` normally uses `gix`, which is fast on shallow histories. On
//! large repositories with deep history or large files, `gix-blame` walks
//! whole-tree diffs per modifying commit and lacks cancellation, whereas
//! `git blame --incremental` streams attributed hunks from git's commit-graph
//! and bloom-filter machinery and can be stopped immediately by killing the
//! child process.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use crate::repo::{BlameCommit, BlameHunk};
use crate::{Error, Oid, Result};

use super::Git;

pub(crate) struct GitBlame {
    pub(crate) hunks: Vec<BlameHunk>,
    pub(crate) commits: HashMap<Oid, BlameCommit>,
}

struct PendingHunk {
    commit: Oid,
    orig_start: u32,
    start: u32,
    len: u32,
    author_name: String,
    author_email: String,
    author_time: i64,
    summary: String,
}

impl Git {
    /// Attributes `path` at `commit` using `git blame --incremental`, returning
    /// sorted, coalesced hunks and commit metadata, or `Ok(None)` if cancelled.
    pub(crate) fn blame_incremental(
        &self,
        commit: Oid,
        path: &str,
        cancelled: &(impl Fn() -> bool + Send + Sync),
    ) -> Result<Option<GitBlame>> {
        let commit_hex = commit.to_string();
        let args = ["blame", "--incremental", &commit_hex, "--", path];

        let mut hunks = Vec::new();
        let mut commits = HashMap::new();
        let mut current: Option<PendingHunk> = None;

        let finished = self.run_stdout_cancellable(&args, cancelled, |raw| {
            let line = raw.strip_suffix(b"\n").unwrap_or(raw);
            let line = line.strip_suffix(b"\r").unwrap_or(line);

            let Some(pending) = current.as_mut() else {
                if line.is_empty() {
                    return Ok(());
                }
                let (oid, orig_start, start, len) = parse_header(line)?;
                current = Some(PendingHunk {
                    commit: oid,
                    orig_start,
                    start,
                    len,
                    author_name: String::new(),
                    author_email: String::new(),
                    author_time: 0,
                    summary: String::new(),
                });
                return Ok(());
            };

            if let Some(rest) = line.strip_prefix(b"filename ") {
                let finished_hunk = current.take().expect("pending hunk present");
                if let Entry::Vacant(slot) = commits.entry(finished_hunk.commit) {
                    slot.insert(BlameCommit {
                        author_name: finished_hunk.author_name,
                        author_email: finished_hunk.author_email,
                        author_time: finished_hunk.author_time,
                        summary: finished_hunk.summary,
                    });
                }
                hunks.push(BlameHunk {
                    commit: finished_hunk.commit,
                    orig_path: unquote_git_path(rest),
                    orig_start: finished_hunk.orig_start,
                    start: finished_hunk.start,
                    len: finished_hunk.len,
                });
            } else if !commits.contains_key(&pending.commit) {
                if let Some(rest) = line.strip_prefix(b"author ") {
                    pending.author_name = String::from_utf8_lossy(rest).trim().to_owned();
                } else if let Some(rest) = line.strip_prefix(b"author-mail ") {
                    let raw_email = String::from_utf8_lossy(rest);
                    let trimmed = raw_email.trim();
                    let email = trimmed
                        .strip_prefix('<')
                        .and_then(|s| s.strip_suffix('>'))
                        .unwrap_or(trimmed)
                        .trim();
                    pending.author_email = email.to_owned();
                } else if let Some(rest) = line.strip_prefix(b"author-time ") {
                    pending.author_time = std::str::from_utf8(rest)
                        .ok()
                        .and_then(|s| s.trim().parse::<i64>().ok())
                        .unwrap_or(0);
                } else if let Some(rest) = line.strip_prefix(b"summary ") {
                    pending.summary = String::from_utf8_lossy(rest).into_owned();
                }
            }
            Ok(())
        })?;

        if !finished {
            return Ok(None);
        }
        if current.is_some() {
            return Err(Error::other("truncated `git blame --incremental` output"));
        }

        hunks.sort_by_key(|hunk| hunk.start);
        coalesce_hunks(&mut hunks);
        Ok(Some(GitBlame { hunks, commits }))
    }
}

fn parse_header(line: &[u8]) -> Result<(Oid, u32, u32, u32)> {
    let mut parts = line.split(|byte| *byte == b' ');
    let parse_u32 = |field: Option<&[u8]>| -> Option<u32> {
        std::str::from_utf8(field?).ok()?.parse::<u32>().ok()
    };
    let oid = parts
        .next()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|hex| hex.parse::<Oid>().ok());
    let orig_start = parse_u32(parts.next());
    let start = parse_u32(parts.next());
    let len = parse_u32(parts.next());
    match (oid, orig_start, start, len) {
        (Some(oid), Some(orig_start), Some(start), Some(len)) if len > 0 => {
            Ok((oid, orig_start, start, len))
        }
        _ => Err(Error::other(format!(
            "unexpected `git blame --incremental` header: {}",
            String::from_utf8_lossy(line)
        ))),
    }
}

/// Merge adjacent hunks that share the same commit, original path, and
/// contiguous source line numbers, matching `gix-blame`'s coalescing rules.
fn coalesce_hunks(hunks: &mut Vec<BlameHunk>) {
    let mut write = 0usize;
    for read in 0..hunks.len() {
        let can_merge = if write > 0 {
            let prev = &hunks[write - 1];
            let curr = &hunks[read];
            prev.commit == curr.commit
                && prev.orig_path == curr.orig_path
                && prev.start.saturating_add(prev.len) == curr.start
                && prev.orig_start.saturating_add(prev.len) == curr.orig_start
        } else {
            false
        };
        if can_merge {
            hunks[write - 1].len = hunks[write - 1].len.saturating_add(hunks[read].len);
        } else {
            if write != read {
                hunks.swap(write, read);
            }
            write += 1;
        }
    }
    hunks.truncate(write);
}

/// Decode a path from git's `filename` line, handling git's C-style quoting
/// (`"..."` with octal UTF-8 escapes and backslash sequences).
fn unquote_git_path(raw: &[u8]) -> String {
    if raw.len() >= 2 && raw.starts_with(b"\"") && raw.ends_with(b"\"") {
        let inner = &raw[1..raw.len() - 1];
        let mut out = Vec::with_capacity(inner.len());
        let mut i = 0;
        while i < inner.len() {
            if inner[i] == b'\\' && i + 1 < inner.len() {
                i += 1;
                match inner[i] {
                    b'a' => out.push(0x07),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0c),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'v' => out.push(0x0b),
                    b'\\' => out.push(b'\\'),
                    b'"' => out.push(b'"'),
                    b'0'..=b'7' => {
                        let mut val = inner[i] - b'0';
                        for _ in 0..2 {
                            if i + 1 < inner.len() && (b'0'..=b'7').contains(&inner[i + 1]) {
                                i += 1;
                                val = val.wrapping_mul(8).wrapping_add(inner[i] - b'0');
                            } else {
                                break;
                            }
                        }
                        out.push(val);
                    }
                    other => out.push(other),
                }
            } else {
                out.push(inner[i]);
            }
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    } else {
        String::from_utf8_lossy(raw).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquotes_git_c_style_paths() {
        assert_eq!(unquote_git_path(b"plain/path.rs"), "plain/path.rs");
        assert_eq!(unquote_git_path(b"a b.txt"), "a b.txt");
        assert_eq!(unquote_git_path(br#""caf\303\251.txt""#), "café.txt");
        assert_eq!(
            unquote_git_path(br#""q\"uote\\test.txt""#),
            r#"q"uote\test.txt"#
        );
    }

    #[test]
    fn coalesces_adjacent_hunks_matching_source_and_path() {
        let oid: Oid = "1111111111111111111111111111111111111111".parse().unwrap();
        let other: Oid = "2222222222222222222222222222222222222222".parse().unwrap();
        let mut hunks = vec![
            BlameHunk {
                commit: oid,
                orig_path: "a.txt".into(),
                orig_start: 1,
                start: 1,
                len: 2,
            },
            BlameHunk {
                commit: oid,
                orig_path: "a.txt".into(),
                orig_start: 3,
                start: 3,
                len: 4,
            },
            // Non-contiguous in orig_start: must NOT merge.
            BlameHunk {
                commit: oid,
                orig_path: "a.txt".into(),
                orig_start: 10,
                start: 7,
                len: 1,
            },
            BlameHunk {
                commit: other,
                orig_path: "a.txt".into(),
                orig_start: 1,
                start: 8,
                len: 2,
            },
        ];
        coalesce_hunks(&mut hunks);
        assert_eq!(hunks.len(), 3);
        assert_eq!(hunks[0].start, 1);
        assert_eq!(hunks[0].len, 6);
        assert_eq!(hunks[1].start, 7);
        assert_eq!(hunks[2].start, 8);
    }
}
