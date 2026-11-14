//! Builds unified-diff patches from a selection of lines, for
//! `git apply --cached` (stage), `--cached -R` (unstage) and `-R` (discard).
//! Owned by the patch task.

use std::collections::BTreeSet;

use crate::diff::{FileDiff, LineKind, RawFileDiff};

/// Where a patch is applied. The same selection means different things
/// depending on which pair of trees the diff came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PatchMode {
    /// Diff between index and worktree, applied with `git apply --cached`.
    Stage,
    /// Diff between HEAD and index, applied with `git apply --cached --reverse`.
    Unstage,
    /// Diff between index and worktree, applied with `git apply --reverse`.
    Discard,
}

impl PatchMode {
    /// Arguments for `git apply`, leaving out the patch itself.
    pub fn apply_args(self) -> &'static [&'static str] {
        match self {
            Self::Stage => &["--cached"],
            Self::Unstage => &["--cached", "--reverse"],
            Self::Discard => &["--reverse"],
        }
    }

    /// True when the patch is fed to `git apply --reverse`, which swaps the
    /// roles of the two sides and therefore of added and removed lines.
    pub fn is_reverse(self) -> bool {
        matches!(self, Self::Unstage | Self::Discard)
    }
}

/// Line terminator of the file the diff belongs to.
///
/// [`DiffLine::content`] never contains the terminator, so the builder has to
/// be told which one to put back; reading the file is not its job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
        }
    }
}

/// Lines of one file diff that the user picked.
///
/// Only added and removed lines can be selected; a selected context line has
/// no meaning and is ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineSelection {
    lines: BTreeSet<(usize, usize)>,
}

impl LineSelection {
    /// Build from `(hunk index, line index within `Hunk::lines`)` pairs.
    pub fn new(lines: impl IntoIterator<Item = (usize, usize)>) -> Self {
        Self {
            lines: lines.into_iter().collect(),
        }
    }

    /// Every added and removed line of one hunk.
    pub fn all_of_hunk(diff: &FileDiff, hunk_index: usize) -> Self {
        let mut selection = Self::default();
        let Some(hunk) = diff.hunks.get(hunk_index) else {
            return selection;
        };
        for (line_index, line) in hunk.lines.iter().enumerate() {
            if line.kind != LineKind::Context {
                selection.lines.insert((hunk_index, line_index));
            }
        }
        selection
    }

    /// Every added and removed line of the whole file diff.
    pub fn all(diff: &FileDiff) -> Self {
        let mut selection = Self::default();
        for (hunk_index, hunk) in diff.hunks.iter().enumerate() {
            for (line_index, line) in hunk.lines.iter().enumerate() {
                if line.kind != LineKind::Context {
                    selection.lines.insert((hunk_index, line_index));
                }
            }
        }
        selection
    }

    pub fn contains(&self, hunk_index: usize, line_index: usize) -> bool {
        self.lines.contains(&(hunk_index, line_index))
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.lines.iter().copied()
    }
}

impl FromIterator<(usize, usize)> for LineSelection {
    fn from_iter<T: IntoIterator<Item = (usize, usize)>>(iter: T) -> Self {
        Self::new(iter)
    }
}

/// One file of a diff, plus the bits `FileDiff` does not carry.
///
/// Renames are not represented: git needs `rename from`/`rename to` headers to
/// apply them, so callers should treat a rename as a deletion plus an addition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatchFile<'a> {
    /// Path of the `+++` side, repository-relative and `/`-separated.
    pub path: &'a str,
    /// Path of the `---` side. `None` marks a file that is new in this diff, so
    /// the old side is `/dev/null`.
    pub old_path: Option<&'a str>,
    pub diff: &'a FileDiff,
    pub line_ending: LineEnding,
    /// The file is gone on the new side. Deletion is all or nothing, so a
    /// partial selection yields no patch.
    pub is_deleted: bool,
}

impl<'a> PatchFile<'a> {
    /// A regular modification that keeps its path and uses LF endings.
    pub fn new(path: &'a str, diff: &'a FileDiff) -> Self {
        Self {
            path,
            old_path: Some(path),
            diff,
            line_ending: LineEnding::Lf,
            is_deleted: false,
        }
    }
}

/// Builds the patch for `selection`, or `None` when it would be empty.
///
/// In every mode the side `git apply` reads keeps the coordinates of the
/// original diff: the old side when the patch is applied forward, the new side
/// when it is reversed. The other side is renumbered to follow the lines that
/// survived, so the hunk headers stay consistent.
pub fn build_patch(
    file: &PatchFile<'_>,
    selection: &LineSelection,
    mode: PatchMode,
) -> Option<String> {
    let hunks = file.diff.hunks.iter().map(|hunk| HunkView {
        old_start: hunk.old_start,
        old_len: hunk.old_len,
        new_start: hunk.new_start,
        lines: hunk
            .lines
            .iter()
            .map(|line| LineView {
                kind: line.kind,
                content: line.content.as_bytes(),
                no_newline_at_eof: line.no_newline_at_eof,
            })
            .collect(),
    });
    let meta = PatchMeta {
        path: file.path,
        old_path: file.old_path,
        line_ending: file.line_ending,
        is_deleted: file.is_deleted,
    };
    build_patch_inner(hunks, meta, selection, mode)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// The diff of a file whose lines still hold their original bytes.
///
/// Hunk staging uses this instead of [`PatchFile`]: decoding a legacy-encoded
/// file for display must not change the bytes `git apply` writes back.
pub(crate) struct BytePatchFile<'a> {
    pub path: &'a str,
    pub old_path: Option<&'a str>,
    pub diff: &'a RawFileDiff<'a>,
    pub line_ending: LineEnding,
    pub is_deleted: bool,
}

/// [`build_patch`] for a diff built with [`crate::diff::diff_bytes`].
pub(crate) fn build_patch_bytes(
    file: &BytePatchFile<'_>,
    selection: &LineSelection,
    mode: PatchMode,
) -> Option<Vec<u8>> {
    let hunks = file.diff.hunks.iter().map(|hunk| HunkView {
        old_start: hunk.old_start,
        old_len: hunk.old_len,
        new_start: hunk.new_start,
        lines: hunk
            .lines
            .iter()
            .map(|line| LineView {
                kind: line.kind,
                content: line.content,
                no_newline_at_eof: line.no_newline_at_eof,
            })
            .collect(),
    });
    let meta = PatchMeta {
        path: file.path,
        old_path: file.old_path,
        line_ending: file.line_ending,
        is_deleted: file.is_deleted,
    };
    build_patch_inner(hunks, meta, selection, mode)
}

struct LineView<'a> {
    kind: LineKind,
    content: &'a [u8],
    no_newline_at_eof: bool,
}

struct HunkView<'a> {
    old_start: u32,
    old_len: u32,
    new_start: u32,
    lines: Vec<LineView<'a>>,
}

struct PatchMeta<'a> {
    path: &'a str,
    old_path: Option<&'a str>,
    line_ending: LineEnding,
    is_deleted: bool,
}

fn build_patch_inner<'a>(
    hunks: impl Iterator<Item = HunkView<'a>>,
    meta: PatchMeta<'_>,
    selection: &LineSelection,
    mode: PatchMode,
) -> Option<Vec<u8>> {
    let reverse = mode.is_reverse();
    let eol = meta.line_ending.as_str().as_bytes();
    let mut out_hunks: Vec<OutHunk> = Vec::new();
    // How far a hunk's `+++` position moved against the original diff, summed
    // over the hunks emitted so far. Only forward patches need it: a staged
    // hunk that dropped additions makes the index shorter than the worktree,
    // so every later hunk moves up.
    let mut shift: i64 = 0;
    let mut any_selected = false;

    for (hunk_index, hunk) in hunks.enumerate() {
        let mut lines: Vec<(u8, &LineView<'_>)> = Vec::new();
        for (line_index, line) in hunk.lines.iter().enumerate() {
            let selected = selection.contains(hunk_index, line_index);
            any_selected |= selected;
            let prefix = match line.kind {
                LineKind::Context => b' ',
                LineKind::Added if selected => b'+',
                // Unselected additions stay in the index, so they become
                // context of a reverse patch.
                LineKind::Added if reverse => b' ',
                LineKind::Added => continue,
                LineKind::Removed if selected => b'-',
                // Unselected removals stay in the index, so they become
                // context of a forward patch.
                LineKind::Removed if reverse => continue,
                LineKind::Removed => b' ',
            };
            lines.push((prefix, line));
        }

        if !lines.iter().any(|(prefix, _)| *prefix != b' ') {
            continue;
        }

        let old_len = side_len(&lines, b'+');
        let new_len = side_len(&lines, b'-');

        let (old_start, new_start) = if reverse {
            // The `+++` side is the pre-image, so it keeps its numbers. The
            // `---` side is only a post-image here; a hunk without old lines
            // that gained some (unselected additions became context) therefore
            // starts at the first line instead of the "line before" zero.
            let old_start = if hunk.old_len == 0 && old_len > 0 {
                hunk.old_start + 1
            } else {
                hunk.old_start
            };
            (old_start, hunk.new_start)
        } else {
            let anchor = if hunk.old_len == 0 {
                i64::from(hunk.old_start) + 1
            } else {
                i64::from(hunk.old_start)
            } + shift;
            let new_start = if new_len == 0 { anchor - 1 } else { anchor };
            (hunk.old_start, new_start.max(0) as u32)
        };

        let mut body = Vec::new();
        let last = lines.len() - 1;
        for (index, (prefix, line)) in lines.iter().enumerate() {
            body.push(*prefix);
            body.extend_from_slice(line.content);
            if line.no_newline_at_eof && index == last {
                body.push(b'\n');
                body.extend_from_slice(b"\\ No newline at end of file\n");
            } else {
                body.extend_from_slice(eol);
            }
        }

        if !reverse {
            shift += i64::from(new_len) - i64::from(hunk.old_len);
        }
        out_hunks.push(OutHunk {
            old_start,
            old_len,
            new_start,
            new_len,
            body,
        });
    }

    if out_hunks.is_empty() || !any_selected {
        return None;
    }
    if meta.is_deleted
        && !out_hunks
            .iter()
            .all(|hunk| hunk.old_len > 0 && hunk.new_len == 0)
    {
        return None;
    }

    // A new file only keeps `/dev/null` on the old side while that side stays
    // empty; unstaging part of a new file needs the index content as context.
    let old_is_dev_null = meta.old_path.is_none() && out_hunks.iter().all(|h| h.old_len == 0);
    let old_path = meta.old_path.unwrap_or(meta.path);

    let mut patch = Vec::new();
    if old_is_dev_null && !reverse {
        patch.extend_from_slice(b"new file mode 100644\n");
    } else if meta.is_deleted {
        patch.extend_from_slice(b"deleted file mode 100644\n");
    }
    if old_is_dev_null {
        patch.extend_from_slice(b"--- /dev/null\n");
    } else {
        patch.extend_from_slice(format!("--- a/{old_path}\n").as_bytes());
    }
    if meta.is_deleted {
        patch.extend_from_slice(b"+++ /dev/null\n");
    } else {
        patch.extend_from_slice(format!("+++ b/{}\n", meta.path).as_bytes());
    }
    for hunk in out_hunks {
        patch.extend_from_slice(
            format!(
                "@@ -{} +{} @@\n",
                range(hunk.old_start, hunk.old_len),
                range(hunk.new_start, hunk.new_len)
            )
            .as_bytes(),
        );
        patch.extend_from_slice(&hunk.body);
    }
    Some(patch)
}

/// `start,len`, or just `start` when the range holds one line, as git writes it.
fn range(start: u32, len: u32) -> String {
    if len == 1 {
        start.to_string()
    } else {
        format!("{start},{len}")
    }
}

struct OutHunk {
    old_start: u32,
    old_len: u32,
    new_start: u32,
    new_len: u32,
    body: Vec<u8>,
}

/// Number of emitted lines on one side of the patch: every line except the
/// ones that only exist on the opposite side.
fn side_len<T>(lines: &[(u8, T)], opposite: u8) -> u32 {
    lines
        .iter()
        .filter(|(prefix, _)| *prefix != opposite)
        .count() as u32
}
