//! Three-way line merge of a conflicted path's index stages.
//!
//! The merge editor needs what a real three-way merge produces: text both
//! sides agree on, changes only one side made, and conflicting regions with
//! their base, ours and theirs line ranges. gix's blob merge writes the merged
//! file with conflict markers but does not expose those ranges, so the diff3
//! walk here computes them from the three blobs. `merge.conflictStyle` only
//! decides how an unresolved conflict is displayed; the regions are the same in
//! every style.

use std::ops::Range;

use imara_diff::{Algorithm, Diff, InternedInput};

use crate::ConflictSides;

/// How unresolved conflicts are written, from `merge.conflictStyle`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ConflictStyle {
    /// Only the two sides, `<<<<<<<` / `=======` / `>>>>>>>`.
    #[default]
    Merge,
    /// The base between `|||||||` and `=======` as well.
    Diff3,
    /// Like [`Diff3`](Self::Diff3). git reduces the common leading and trailing
    /// lines of the two sides in this style; the regions are already reduced to
    /// the lines the sides disagree about, so the markers here are the same.
    ZealousDiff3,
}

impl ConflictStyle {
    /// The style a configuration value names; unknown values read as git's
    /// default instead of failing a merge over a typo.
    pub fn parse(value: &str) -> Self {
        match value.trim() {
            "diff3" => Self::Diff3,
            "zdiff3" => Self::ZealousDiff3,
            _ => Self::Merge,
        }
    }

    /// Whether the base text is shown inside the conflict markers.
    pub fn shows_base(self) -> bool {
        self != Self::Merge
    }
}

/// The line terminator a text file uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn as_bytes(self) -> &'static [u8] {
        match self {
            Self::Lf => b"\n",
            Self::CrLf => b"\r\n",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
        }
    }

    /// `lines` written with this terminator, ending with one when asked.
    pub fn join(self, lines: &[String], trailing_newline: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            bytes.extend_from_slice(line.as_bytes());
            if trailing_newline || index + 1 < lines.len() {
                bytes.extend_from_slice(self.as_bytes());
            }
        }
        bytes
    }
}

/// A text side split into lines, plus what rebuilding its bytes needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextFile {
    /// Lines without their terminators, in order. A `\r` before a line's `\n`
    /// is part of the terminator, not of the text.
    pub lines: Vec<String>,
    /// The terminator the file starts with. Mixed files are rare; git itself
    /// picks one form when it writes conflict markers.
    pub line_ending: LineEnding,
    /// The file ends with a line terminator.
    pub trailing_newline: bool,
}

impl TextFile {
    /// Splits `bytes` into display lines.
    ///
    /// Non-UTF-8 bytes become replacement characters; [`MergeAnalysis::text`]
    /// is what tells callers not to offer editing for such a side.
    pub fn parse(bytes: &[u8]) -> Self {
        let text = String::from_utf8_lossy(bytes);
        let mut lines = Vec::new();
        let mut line_ending = LineEnding::Lf;
        let mut first = true;
        let mut start = 0;
        while let Some(offset) = text[start..].find('\n') {
            let end = start + offset;
            let line = &text[start..end];
            let (line, crlf) = match line.strip_suffix('\r') {
                Some(line) => (line, true),
                None => (line, false),
            };
            if first {
                line_ending = if crlf {
                    LineEnding::CrLf
                } else {
                    LineEnding::Lf
                };
                first = false;
            }
            lines.push(line.to_owned());
            start = end + 1;
        }
        if start < text.len() {
            // The final line has no terminator, so a trailing `\r` is text.
            lines.push(text[start..].to_owned());
        }
        Self {
            lines,
            line_ending,
            trailing_newline: text.ends_with('\n'),
        }
    }

    /// The lines in `range`, clamped to what the file has.
    pub fn slice(&self, range: &Range<usize>) -> &[String] {
        let start = range.start.min(self.lines.len());
        let end = range.end.min(self.lines.len()).max(start);
        &self.lines[start..end]
    }
}

/// Which side's text a merged segment takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SegmentKind {
    /// Both sides still have the base text.
    Unchanged,
    /// Only ours changed the region, or both made the same change.
    Ours,
    /// Only theirs changed the region.
    Theirs,
    /// Both changed the same region, differently.
    Conflict,
}

/// One stretch of the merge, with the same text located in all three sides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub kind: SegmentKind,
    /// 0-based, end-exclusive line ranges in base, ours and theirs.
    pub base: Range<usize>,
    pub ours: Range<usize>,
    pub theirs: Range<usize>,
}

impl Segment {
    /// The text this segment contributes to an automatic merge.
    fn merged_lines(&self, analysis: &MergeAnalysis) -> Vec<String> {
        match self.kind {
            SegmentKind::Unchanged | SegmentKind::Ours => analysis.ours.slice(&self.ours).to_vec(),
            SegmentKind::Theirs => analysis.theirs.slice(&self.theirs).to_vec(),
            SegmentKind::Conflict => analysis.marker_lines(self),
        }
    }
}

/// The three stages of one conflicted path, merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeAnalysis {
    pub base: TextFile,
    pub ours: TextFile,
    pub theirs: TextFile,
    /// Ordered, non-overlapping segments covering every line of every side.
    pub segments: Vec<Segment>,
    /// From `merge.conflictStyle`; only the display of unresolved conflicts
    /// depends on it.
    pub style: ConflictStyle,
    /// Every present stage is UTF-8 text. A side that is not cannot be edited
    /// line by line without losing bytes, so the editor is not offered.
    pub text: bool,
}

impl MergeAnalysis {
    /// Merges the stages in `sides`.
    pub fn analyze(sides: &ConflictSides, style: ConflictStyle) -> Self {
        let base_bytes = sides.base.as_deref().unwrap_or_default();
        let ours_bytes = sides.ours.as_deref().unwrap_or_default();
        let theirs_bytes = sides.theirs.as_deref().unwrap_or_default();
        let text = [base_bytes, ours_bytes, theirs_bytes]
            .into_iter()
            .all(|bytes| std::str::from_utf8(bytes).is_ok());

        let base = TextFile::parse(base_bytes);
        let ours = TextFile::parse(ours_bytes);
        let theirs = TextFile::parse(theirs_bytes);
        let segments = if text {
            merge_lines(&base.lines, &ours.lines, &theirs.lines)
        } else {
            Vec::new()
        };
        Self {
            base,
            ours,
            theirs,
            segments,
            style,
            text,
        }
    }

    /// The line ending a merged result uses: the one from a side that has
    /// content, so a CRLF checkout is written back as CRLF.
    pub fn line_ending(&self) -> LineEnding {
        if !self.ours.lines.is_empty() {
            self.ours.line_ending
        } else if !self.theirs.lines.is_empty() {
            self.theirs.line_ending
        } else {
            self.base.line_ending
        }
    }

    /// The automatically merged lines, with unresolved conflicts written as
    /// markers in the configured style.
    pub fn merged_lines(&self) -> Vec<String> {
        self.segments
            .iter()
            .flat_map(|segment| segment.merged_lines(self))
            .collect()
    }

    /// The marker block for an unresolved conflict, in the configured style.
    pub fn marker_lines(&self, segment: &Segment) -> Vec<String> {
        let mut lines =
            Vec::with_capacity(segment.ours.len() + segment.theirs.len() + segment.base.len() + 4);
        lines.push("<<<<<<< ours".to_owned());
        lines.extend(self.ours.slice(&segment.ours).iter().cloned());
        if self.style.shows_base() {
            lines.push("||||||| base".to_owned());
            lines.extend(self.base.slice(&segment.base).iter().cloned());
        }
        lines.push("=======".to_owned());
        lines.extend(self.theirs.slice(&segment.theirs).iter().cloned());
        lines.push(">>>>>>> theirs".to_owned());
        lines
    }
}

/// A fingerprint of a file's bytes.
///
/// The merge editor compares the file on disk with what it loaded, so a write
/// cannot silently overwrite an edit made elsewhere. This detects accidental
/// changes, not tampering: it is a content hash, not a signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentHash(u64);

impl ContentHash {
    /// The FNV-1a hash of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut hash = OFFSET;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
        Self(hash)
    }
}

/// A changed run of lines in one side.
#[derive(Debug, Clone)]
struct Change {
    /// 0-based, end-exclusive base range.
    before: Range<usize>,
    /// 0-based, end-exclusive range in the side's own lines.
    after: Range<usize>,
}

fn merge_lines(base: &[String], ours: &[String], theirs: &[String]) -> Vec<Segment> {
    let ours_changes = changes(base, ours);
    let theirs_changes = changes(base, theirs);

    // Union of both sides' changed ranges in base coordinates. A range that
    // touches an insertion or another range from the other side describes one
    // overlapping region; adjacent replacements do not touch each other, so a
    // change to one line by each side still merges cleanly.
    let mut bounds: Vec<Range<usize>> = ours_changes
        .iter()
        .chain(theirs_changes.iter())
        .map(|change| change.before.clone())
        .collect();
    bounds.sort_by_key(|range| (range.start, range.end));

    let mut regions: Vec<Range<usize>> = Vec::with_capacity(bounds.len());
    for range in bounds {
        match regions.last_mut() {
            Some(last)
                if range.start < last.end
                    || (range.start == last.end
                        && (range.start == range.end || last.start == last.end)) =>
            {
                last.end = last.end.max(range.end);
            }
            _ => regions.push(range),
        }
    }

    let mut segments = Vec::with_capacity(regions.len() * 2 + 1);
    let mut cursor = 0;
    for region in regions {
        let end = region.end;
        if region.start > cursor {
            segments.push(unchanged_segment(
                cursor..region.start,
                &ours_changes,
                &theirs_changes,
            ));
        }
        segments.push(change_segment(
            region,
            &ours_changes,
            &theirs_changes,
            ours,
            theirs,
        ));
        cursor = end;
    }
    if cursor < base.len() {
        segments.push(unchanged_segment(
            cursor..base.len(),
            &ours_changes,
            &theirs_changes,
        ));
    }
    segments
}

/// A stretch neither side changed. Insertions on its edges belong to the
/// changed regions before and after it, so they are excluded here.
fn unchanged_segment(
    base: Range<usize>,
    ours_changes: &[Change],
    theirs_changes: &[Change],
) -> Segment {
    Segment {
        kind: SegmentKind::Unchanged,
        ours: map_start(ours_changes, base.start, false)..map_end(ours_changes, base.end, false),
        theirs: map_start(theirs_changes, base.start, false)
            ..map_end(theirs_changes, base.end, false),
        base,
    }
}

/// A region at least one side changed, mapped to both sides' line ranges.
fn change_segment(
    base: Range<usize>,
    ours_changes: &[Change],
    theirs_changes: &[Change],
    ours: &[String],
    theirs: &[String],
) -> Segment {
    let ours_range =
        map_start(ours_changes, base.start, true)..map_end(ours_changes, base.end, true);
    let theirs_range =
        map_start(theirs_changes, base.start, true)..map_end(theirs_changes, base.end, true);
    let ours_changed = region_changed(ours_changes, &base);
    let theirs_changed = region_changed(theirs_changes, &base);
    let kind = match (ours_changed, theirs_changed) {
        (false, false) => SegmentKind::Unchanged,
        (true, false) => SegmentKind::Ours,
        (false, true) => SegmentKind::Theirs,
        (true, true) => {
            if ours[ours_range.clone()] == theirs[theirs_range.clone()] {
                // The same edit on both sides is not a conflict; either side's
                // range holds the merged text.
                SegmentKind::Ours
            } else {
                SegmentKind::Conflict
            }
        }
    };
    Segment {
        kind,
        base,
        ours: ours_range,
        theirs: theirs_range,
    }
}

/// Whether `side` changed any line of `region`.
fn region_changed(changes: &[Change], region: &Range<usize>) -> bool {
    changes
        .iter()
        .any(|change| change.before.start >= region.start && change.before.end <= region.end)
}

/// The line diff of one side against the base, in line coordinates.
fn changes(base: &[String], side: &[String]) -> Vec<Change> {
    if base == side {
        return Vec::new();
    }
    // imara tokenises a `&str` into whole lines including their terminator.
    // Joining without one makes lines with and without a final newline compare
    // equal, which is what `TextFile::trailing_newline` is for.
    let before = base.join("\n");
    let after = side.join("\n");
    let input = InternedInput::new(before.as_str(), after.as_str());
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    diff.hunks()
        .map(|hunk| Change {
            before: hunk.before.start as usize..hunk.before.end as usize,
            after: hunk.after.start as usize..hunk.after.end as usize,
        })
        .collect()
}

/// Where base position `position` starts in a side's lines.
///
/// `include_points` says whether an insertion exactly at `position` belongs to
/// the stretch being mapped. A change region owns insertions on its edges; the
/// unchanged stretch after it does not, because the insertion was already
/// emitted before that stretch.
fn map_start(changes: &[Change], position: usize, include_points: bool) -> usize {
    let mut delta: i64 = 0;
    for change in changes {
        if change.before.start == position {
            if change.before.end == position && !include_points {
                // Consumed by the stretch before this one.
                delta += change.after.len() as i64;
                continue;
            }
            return change.after.start;
        }
        if change.before.start > position {
            break;
        }
        if change.before.end <= position {
            delta += change.after.len() as i64 - change.before.len() as i64;
        } else {
            // Strictly inside a change, which a region boundary cannot be;
            // clamping to its replacement keeps the ranges valid regardless.
            return change.after.start;
        }
    }
    (position as i64 + delta) as usize
}

/// Where base position `position` ends in a side's lines.
fn map_end(changes: &[Change], position: usize, include_points: bool) -> usize {
    let mut delta: i64 = 0;
    for change in changes {
        if change.before.start > position {
            break;
        }
        if change.before.start == position {
            if change.before.end == position && include_points {
                delta += change.after.len() as i64;
            }
            break;
        }
        if change.before.end <= position {
            delta += change.after.len() as i64 - change.before.len() as i64;
        } else {
            return change.after.start;
        }
    }
    (position as i64 + delta) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sides(base: &str, ours: &str, theirs: &str) -> ConflictSides {
        ConflictSides {
            base: Some(base.as_bytes().to_vec()),
            ours: Some(ours.as_bytes().to_vec()),
            theirs: Some(theirs.as_bytes().to_vec()),
        }
    }

    fn kinds(analysis: &MergeAnalysis) -> Vec<SegmentKind> {
        analysis
            .segments
            .iter()
            .map(|segment| segment.kind)
            .collect()
    }

    /// Every side's segments must cover its lines exactly once, in order: a
    /// gap or a repeated line would make the editor drop or duplicate text.
    fn assert_covers(analysis: &MergeAnalysis) {
        let ours: Vec<&Range<usize>> = analysis
            .segments
            .iter()
            .map(|segment| &segment.ours)
            .collect();
        let theirs: Vec<&Range<usize>> = analysis
            .segments
            .iter()
            .map(|segment| &segment.theirs)
            .collect();
        assert_covers_side(&ours, analysis.ours.lines.len(), "ours");
        assert_covers_side(&theirs, analysis.theirs.lines.len(), "theirs");
    }

    fn assert_covers_side(ranges: &[&Range<usize>], len: usize, side: &str) {
        let mut at = 0;
        for range in ranges {
            assert_eq!(range.start, at, "{side} ranges must be contiguous");
            at = range.end;
        }
        assert_eq!(at, len, "{side} ranges must cover the whole side");
    }

    #[test]
    fn non_overlapping_changes_merge_without_conflicts() {
        let sides = sides("a\nb\nc\nd\n", "A\nb\nc\nd\n", "a\nb\nc\nD\n");
        let analysis = MergeAnalysis::analyze(&sides, ConflictStyle::Merge);
        assert_eq!(
            kinds(&analysis),
            [
                SegmentKind::Ours,
                SegmentKind::Unchanged,
                SegmentKind::Theirs
            ]
        );
        assert_eq!(analysis.segments[0].base, 0..1);
        assert_eq!(analysis.segments[0].ours, 0..1);
        assert_eq!(analysis.segments[1].base, 1..3);
        assert_eq!(analysis.segments[2].base, 3..4);
        assert_eq!(
            analysis.merged_lines(),
            ["A", "b", "c", "D"],
            "both sides' changes are kept"
        );
        assert_covers(&analysis);
    }

    #[test]
    fn overlapping_changes_conflict_with_the_base_range() {
        let sides = sides("a\nb\nc\n", "a\nB1\nc\n", "a\nB2\nc\n");
        let analysis = MergeAnalysis::analyze(&sides, ConflictStyle::Diff3);
        assert_eq!(
            kinds(&analysis),
            [
                SegmentKind::Unchanged,
                SegmentKind::Conflict,
                SegmentKind::Unchanged
            ]
        );
        let conflict = &analysis.segments[1];
        assert_eq!(conflict.base, 1..2);
        assert_eq!(conflict.ours, 1..2);
        assert_eq!(conflict.theirs, 1..2);

        let merged = analysis.merged_lines();
        assert_eq!(merged[0], "a");
        assert!(merged.contains(&"<<<<<<< ours".to_owned()));
        assert!(
            merged.contains(&"||||||| base".to_owned()),
            "diff3 shows the base"
        );
        assert!(merged.contains(&"B1".to_owned()));
        assert!(merged.contains(&"B2".to_owned()));
        assert_eq!(merged.last().map(String::as_str), Some("c"));
        assert_covers(&analysis);
    }

    #[test]
    fn merge_style_hides_the_base() {
        let sides = sides("a\nb\nc\n", "a\nB1\nc\n", "a\nB2\nc\n");
        let analysis = MergeAnalysis::analyze(&sides, ConflictStyle::Merge);
        assert!(
            !analysis
                .merged_lines()
                .iter()
                .any(|line| line == "||||||| base")
        );
    }

    #[test]
    fn an_identical_change_on_both_sides_is_not_a_conflict() {
        let sides = sides("a\nb\nc\n", "a\nB\nc\n", "a\nB\nc\n");
        let analysis = MergeAnalysis::analyze(&sides, ConflictStyle::Merge);
        assert_eq!(
            kinds(&analysis),
            [
                SegmentKind::Unchanged,
                SegmentKind::Ours,
                SegmentKind::Unchanged
            ]
        );
        assert_eq!(analysis.merged_lines(), ["a", "B", "c"]);
        assert_covers(&analysis);
    }

    #[test]
    fn a_deletion_is_an_empty_side_range() {
        let sides = sides("a\nb\nc\n", "a\nc\n", "a\nb\nC\n");
        let analysis = MergeAnalysis::analyze(&sides, ConflictStyle::Merge);
        assert_eq!(
            kinds(&analysis),
            [
                SegmentKind::Unchanged,
                SegmentKind::Ours,
                SegmentKind::Theirs
            ]
        );
        assert_eq!(analysis.segments[1].base, 1..2);
        assert!(
            analysis.segments[1].ours.is_empty(),
            "ours deleted the line"
        );
        assert_eq!(analysis.merged_lines(), ["a", "C"]);
        assert_covers(&analysis);
    }

    #[test]
    fn a_file_deleted_on_one_side_is_a_conflict_against_the_edited_other_side() {
        let sides = ConflictSides {
            base: Some(b"a\nb\n".to_vec()),
            ours: None,
            theirs: Some(b"a\nb\nc\n".to_vec()),
        };
        let analysis = MergeAnalysis::analyze(&sides, ConflictStyle::Merge);
        assert_eq!(kinds(&analysis), [SegmentKind::Conflict]);
        assert_eq!(analysis.segments[0].base, 0..2);
        assert_eq!(analysis.segments[0].ours, 0..0, "our side holds no lines");
        assert_eq!(analysis.segments[0].theirs, 0..3);
        assert_covers(&analysis);
    }

    #[test]
    fn both_added_files_conflict_over_the_whole_content() {
        let sides = ConflictSides {
            base: None,
            ours: Some(b"ours\n".to_vec()),
            theirs: Some(b"theirs\n".to_vec()),
        };
        let analysis = MergeAnalysis::analyze(&sides, ConflictStyle::Merge);
        assert_eq!(kinds(&analysis), [SegmentKind::Conflict]);
        assert_eq!(analysis.segments[0].base, 0..0);
        assert_eq!(analysis.segments[0].ours, 0..1);
        assert_eq!(analysis.segments[0].theirs, 0..1);
        assert_covers(&analysis);
    }

    #[test]
    fn an_insertion_beside_a_change_stays_in_that_region() {
        let sides = sides("a\nc\n", "a\nb\nc\n", "a\nC\n");
        let analysis = MergeAnalysis::analyze(&sides, ConflictStyle::Merge);
        assert_eq!(
            kinds(&analysis),
            [SegmentKind::Unchanged, SegmentKind::Conflict]
        );
        let conflict = &analysis.segments[1];
        assert_eq!(conflict.base, 1..2);
        assert_eq!(conflict.ours, 1..3, "the insertion is part of our version");
        assert_eq!(conflict.theirs, 1..2);
        assert_covers(&analysis);
    }

    #[test]
    fn an_insertion_at_the_end_of_a_change_is_merged_once() {
        // Ours edits the last base line; theirs adds a line after it. The
        // insertion belongs to the conflict region, and no later segment may
        // repeat it.
        let sides = sides("a\nb\n", "a\nB\n", "a\nb\nb2\n");
        let analysis = MergeAnalysis::analyze(&sides, ConflictStyle::Merge);
        assert_eq!(
            kinds(&analysis),
            [SegmentKind::Unchanged, SegmentKind::Conflict]
        );
        assert_eq!(analysis.segments[1].base, 1..2);
        assert_eq!(analysis.segments[1].ours, 1..2);
        assert_eq!(analysis.segments[1].theirs, 1..3);
        assert_covers(&analysis);
        assert!(analysis.merged_lines().iter().any(|line| line == "b2"));
    }

    #[test]
    fn the_stable_run_after_a_consumed_insertion_starts_after_it() {
        // Theirs inserts after the line ours edits, and a line follows. The
        // merged insertion must not appear once in the conflict and again in
        // the unchanged tail.
        let sides = sides("a\nb\nc\n", "a\nB\nc\n", "a\nb\nb2\nc\n");
        let analysis = MergeAnalysis::analyze(&sides, ConflictStyle::Merge);
        assert_eq!(
            kinds(&analysis),
            [
                SegmentKind::Unchanged,
                SegmentKind::Conflict,
                SegmentKind::Unchanged
            ]
        );
        let tail = &analysis.segments[2];
        assert_eq!(analysis.ours.slice(&tail.ours), ["c"]);
        assert_eq!(analysis.theirs.slice(&tail.theirs), ["c"]);
        assert_covers(&analysis);
    }

    #[test]
    fn crlf_files_keep_one_crlf_line_ending() {
        let sides = sides(
            "one\r\ntwo\r\nthree\r\n",
            "one\r\nTWO\r\nthree\r\n",
            "one\r\ntwo\r\nTHREE\r\n",
        );
        let analysis = MergeAnalysis::analyze(&sides, ConflictStyle::Merge);
        assert_eq!(analysis.line_ending(), LineEnding::CrLf);
        assert!(analysis.ours.trailing_newline);
        assert_eq!(
            analysis.merged_lines(),
            ["one", "TWO", "THREE"],
            "the lines carry no carriage return"
        );
        assert_eq!(
            analysis.line_ending().join(&analysis.merged_lines(), true),
            b"one\r\nTWO\r\nTHREE\r\n",
            "the merged bytes are CRLF again"
        );
        assert_covers(&analysis);
    }

    #[test]
    fn a_missing_final_newline_is_kept() {
        let sides = sides("a\nb", "a\nB", "a\nb");
        let analysis = MergeAnalysis::analyze(&sides, ConflictStyle::Merge);
        assert!(!analysis.ours.trailing_newline);
        assert_eq!(analysis.merged_lines(), ["a", "B"]);
        assert_eq!(
            analysis.line_ending().join(&analysis.merged_lines(), false),
            b"a\nB"
        );
        assert_covers(&analysis);
    }

    #[test]
    fn a_single_empty_line_stays_one_line() {
        let file = TextFile::parse(b"\n");
        assert_eq!(file.lines, [""]);
        assert!(file.trailing_newline);
        assert_eq!(LineEnding::Lf.join(&file.lines, true), b"\n");
        assert!(TextFile::parse(b"").lines.is_empty());
    }

    #[test]
    fn non_utf8_stages_are_not_offered_as_text() {
        let sides = ConflictSides {
            base: Some(vec![0xff, 0x00]),
            ours: Some(b"a\n".to_vec()),
            theirs: Some(b"b\n".to_vec()),
        };
        let analysis = MergeAnalysis::analyze(&sides, ConflictStyle::Merge);
        assert!(!analysis.text);
        assert!(analysis.segments.is_empty());
    }

    #[test]
    fn content_hashes_distinguish_content_length_and_absence() {
        assert_eq!(ContentHash::of(b"a\n"), ContentHash::of(b"a\n"));
        assert_ne!(ContentHash::of(b"a\n"), ContentHash::of(b"a"));
    }

    #[test]
    fn conflict_styles_parse_like_git_names_them() {
        assert_eq!(ConflictStyle::parse("merge"), ConflictStyle::Merge);
        assert_eq!(ConflictStyle::parse("diff3"), ConflictStyle::Diff3);
        assert_eq!(ConflictStyle::parse("zdiff3"), ConflictStyle::ZealousDiff3);
        assert_eq!(
            ConflictStyle::parse("nonsense"),
            ConflictStyle::Merge,
            "an unknown style falls back to git's default"
        );
    }
}
