//! Text diff (imara-diff) with intra-line emphasis. Owned by the diff task.

use std::ops::Range;

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

pub fn diff_text(_old: &[u8], _new: &[u8], _opts: &DiffOptions) -> FileDiff {
    todo!("diff task")
}
