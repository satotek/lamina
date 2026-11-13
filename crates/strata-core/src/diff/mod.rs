//! Text diff (imara-diff) with intra-line emphasis. Owned by the diff task.

mod emphasis;
mod lines;

#[cfg(test)]
mod tests;

use std::fmt::Write as _;
use std::ops::Range;

use imara_diff::{Algorithm, Diff, InternedInput};

use lines::Line;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
    /// Line text without the trailing newline.
    pub content: String,
    /// True when this line is the last line of its side and has no trailing newline.
    pub no_newline_at_eof: bool,
    /// Byte ranges within `content` that changed (word-level emphasis).
    pub emphasis: Vec<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// 1-based, as in a unified diff header.
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileDiff {
    pub hunks: Vec<Hunk>,
    pub binary: bool,
}

#[derive(Debug, Clone)]
pub struct DiffOptions {
    pub context_lines: u32,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self { context_lines: 3 }
    }
}

/// A changed range of lines, both sides 0-based and end-exclusive.
struct Change {
    old: Range<usize>,
    new: Range<usize>,
}

pub fn diff_text(old: &[u8], new: &[u8], opts: &DiffOptions) -> FileDiff {
    if lines::is_binary(old) || lines::is_binary(new) {
        return FileDiff {
            hunks: Vec::new(),
            binary: true,
        };
    }

    let old = String::from_utf8_lossy(old);
    let new = String::from_utf8_lossy(new);
    let old_text: &str = &old;
    let new_text: &str = &new;
    let old_lines = lines::split(old_text);
    let new_lines = lines::split(new_text);

    // imara's `&str` source tokenises whole lines including their terminator,
    // which is what makes CRLF/LF and a dropped final newline show up.
    let input = InternedInput::new(old_text, new_text);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    let changes: Vec<Change> = diff
        .hunks()
        .map(|hunk| Change {
            old: hunk.before.start as usize..hunk.before.end as usize,
            new: hunk.after.start as usize..hunk.after.end as usize,
        })
        .collect();

    let context = opts.context_lines as usize;
    let mut hunks = Vec::new();
    for (first, last) in group_changes(&changes, context) {
        let mut hunk = build_hunk(&old_lines, &new_lines, &changes, first, last, context);
        emphasis::emphasize(&mut hunk.lines);
        hunks.push(hunk);
    }

    FileDiff {
        hunks,
        binary: false,
    }
}

/// Groups changes the way `git diff` does: changes stay in one hunk when the
/// common lines between them fit twice into the context, otherwise the context
/// would overlap and the hunks would run into each other.
fn group_changes(changes: &[Change], context: usize) -> Vec<(usize, usize)> {
    if changes.is_empty() {
        return Vec::new();
    }
    let max_gap = context.saturating_mul(2);
    let mut groups = Vec::new();
    let mut first = 0;
    for idx in 1..changes.len() {
        let gap_old = changes[idx].old.start - changes[idx - 1].old.end;
        let gap_new = changes[idx].new.start - changes[idx - 1].new.end;
        if gap_old.min(gap_new) > max_gap {
            groups.push((first, idx - 1));
            first = idx;
        }
    }
    groups.push((first, changes.len() - 1));
    groups
}

fn build_hunk(
    old: &[Line<'_>],
    new: &[Line<'_>],
    changes: &[Change],
    first: usize,
    last: usize,
    context: usize,
) -> Hunk {
    let ctx_before = context
        .min(changes[first].old.start)
        .min(changes[first].new.start);
    let ctx_after = context
        .min(old.len() - changes[last].old.end)
        .min(new.len() - changes[last].new.end);

    let old_start = changes[first].old.start - ctx_before;
    let new_start = changes[first].new.start - ctx_before;
    let old_len = changes[last].old.end + ctx_after - old_start;
    let new_len = changes[last].new.end + ctx_after - new_start;

    let mut lines = Vec::with_capacity(old_len + new_len);
    let mut old_pos = old_start;
    let mut new_pos = new_start;

    for _ in 0..ctx_before {
        lines.push(context_line(old, old_pos, new, new_pos));
        old_pos += 1;
        new_pos += 1;
    }
    for change in &changes[first..=last] {
        while old_pos < change.old.start {
            lines.push(context_line(old, old_pos, new, new_pos));
            old_pos += 1;
            new_pos += 1;
        }
        for idx in change.old.clone() {
            lines.push(DiffLine {
                kind: LineKind::Removed,
                old_lineno: Some(idx as u32 + 1),
                new_lineno: None,
                content: old[idx].content.to_string(),
                no_newline_at_eof: old[idx].no_newline,
                emphasis: Vec::new(),
            });
        }
        for idx in change.new.clone() {
            lines.push(DiffLine {
                kind: LineKind::Added,
                old_lineno: None,
                new_lineno: Some(idx as u32 + 1),
                content: new[idx].content.to_string(),
                no_newline_at_eof: new[idx].no_newline,
                emphasis: Vec::new(),
            });
        }
        old_pos = change.old.end;
        new_pos = change.new.end;
    }
    for _ in 0..ctx_after {
        lines.push(context_line(old, old_pos, new, new_pos));
        old_pos += 1;
        new_pos += 1;
    }

    Hunk {
        old_start: header_start(old_start, old_len),
        old_len: old_len as u32,
        new_start: header_start(new_start, new_len),
        new_len: new_len as u32,
        lines,
    }
}

/// Unified headers count lines from 1, except for an empty range: there the
/// start is the line number the range would follow ("the line before"), which
/// is exactly the 0-based index of the insertion point.
fn header_start(index: usize, len: usize) -> u32 {
    if len == 0 {
        index as u32
    } else {
        index as u32 + 1
    }
}

fn context_line(old: &[Line<'_>], old_pos: usize, new: &[Line<'_>], new_pos: usize) -> DiffLine {
    debug_assert_eq!(
        old[old_pos].raw, new[new_pos].raw,
        "context must be identical on both sides"
    );
    DiffLine {
        kind: LineKind::Context,
        old_lineno: Some(old_pos as u32 + 1),
        new_lineno: Some(new_pos as u32 + 1),
        content: old[old_pos].content.to_string(),
        no_newline_at_eof: old[old_pos].no_newline || new[new_pos].no_newline,
        emphasis: Vec::new(),
    }
}

/// Render a diff in the format `git diff` and `patch` understand.
pub fn to_unified(diff: &FileDiff, old_path: &str, new_path: &str) -> String {
    if diff.binary {
        return format!("Binary files {old_path} and {new_path} differ\n");
    }
    if diff.hunks.is_empty() {
        // `git diff` prints nothing at all when the files are identical.
        return String::new();
    }

    let mut out = String::new();
    let _ = writeln!(out, "--- {old_path}");
    let _ = writeln!(out, "+++ {new_path}");
    for hunk in &diff.hunks {
        out.push_str("@@ -");
        write_range(&mut out, hunk.old_start, hunk.old_len);
        out.push_str(" +");
        write_range(&mut out, hunk.new_start, hunk.new_len);
        out.push_str(" @@\n");
        for line in &hunk.lines {
            out.push(match line.kind {
                LineKind::Context => ' ',
                LineKind::Added => '+',
                LineKind::Removed => '-',
            });
            out.push_str(&line.content);
            out.push('\n');
            if line.no_newline_at_eof {
                out.push_str("\\ No newline at end of file\n");
            }
        }
    }
    out
}

/// `git` writes a count only when it is not 1.
fn write_range(out: &mut String, start: u32, len: u32) {
    if len == 1 {
        let _ = write!(out, "{start}");
    } else {
        let _ = write!(out, "{start},{len}");
    }
}
