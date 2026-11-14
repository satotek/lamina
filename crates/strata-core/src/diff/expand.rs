//! Revealing the unchanged lines a diff leaves out.
//!
//! A diff only carries the context lines around each change. The blobs it was
//! computed from still hold everything else, so a view can show more of a file
//! without running the diff again: [`gaps`] names the hidden runs between the
//! hunks, and [`gap_view`] reads the lines of one run out of the two sides.

use std::collections::BTreeMap;

use super::{DiffLine, FileDiff, LineKind};

/// How many lines one press of a gap bar's arrow reveals.
pub const REVEAL_STEP: u32 = 20;

/// The largest blob a view reveals in full. Beyond it the view shows a notice
/// instead of building rows for a file that would stall a frame.
pub const MAX_EXPAND_BYTES: usize = 1024 * 1024;

/// A run of unchanged lines the diff leaves out.
///
/// A diff has one gap before each hunk and one after the last one. Adjacent
/// hunks (or a hunk touching a file edge) leave a gap with `len == 0`, so a
/// view can index the gaps by position without renumbering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    /// 1-based number of the first hidden line on the old side. For an empty
    /// gap it is the line the run would start at.
    pub old_start: u32,
    /// 1-based number of the first hidden line on the new side.
    pub new_start: u32,
    /// Hidden lines. Unchanged text is common to both sides, so one count
    /// describes both.
    pub len: u32,
}

/// The gaps of `diff`, indexed by the hunk they precede; the last entry
/// follows the last hunk.
pub fn gaps(diff: &FileDiff, old_lines: u32, new_lines: u32) -> Vec<Gap> {
    let mut gaps = Vec::with_capacity(diff.hunks.len() + 1);
    let mut old_end = 0u32;
    let mut new_end = 0u32;
    for hunk in &diff.hunks {
        let old_begin = side_start(hunk.old_start, hunk.old_len);
        let new_begin = side_start(hunk.new_start, hunk.new_len);
        gaps.push(Gap {
            old_start: old_end + 1,
            new_start: new_end + 1,
            len: old_begin
                .saturating_sub(old_end)
                .min(new_begin.saturating_sub(new_end)),
        });
        old_end = old_begin.saturating_add(hunk.old_len).min(old_lines);
        new_end = new_begin.saturating_add(hunk.new_len).min(new_lines);
    }
    gaps.push(Gap {
        old_start: old_end + 1,
        new_start: new_end + 1,
        len: old_lines
            .saturating_sub(old_end)
            .min(new_lines.saturating_sub(new_end)),
    });
    gaps
}

/// The 0-based index a hunk starts at. A zero-length range's header already
/// names the line it follows, which is that index.
fn side_start(start: u32, len: u32) -> u32 {
    if len == 0 {
        start
    } else {
        start.saturating_sub(1)
    }
}

/// Lines in `text`, counting a final line that has no terminator.
pub fn line_count(text: &str) -> u32 {
    let newlines = text.bytes().filter(|byte| *byte == b'\n').count();
    let tail = usize::from(!text.is_empty() && !text.ends_with('\n'));
    (newlines + tail).min(u32::MAX as usize) as u32
}

/// Whether both sides are small enough to reveal in full.
pub fn within_expand_cap(old: &str, new: &str) -> bool {
    old.len() <= MAX_EXPAND_BYTES && new.len() <= MAX_EXPAND_BYTES
}

/// How many lines of one gap are shown above and below its bar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reveal {
    /// Lines shown just after the previous hunk (or the file start).
    pub above: u32,
    /// Lines shown just before the next hunk (or the file end).
    pub below: u32,
}

impl Reveal {
    /// The lines still hidden between the two shown parts.
    pub fn hidden(self, len: u32) -> u32 {
        len.saturating_sub(self.above).saturating_sub(self.below)
    }
}

/// What one press on a gap bar asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapAction {
    /// Reveal the next lines above the bar.
    Above,
    /// Reveal the next lines below the bar.
    Below,
    /// Reveal the whole run, however long it is.
    All,
}

/// Which gaps the user opened, for the diff on screen.
///
/// Indexed like [`gaps`]. The view owns one and clears it when the selection
/// changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expansion {
    whole_file: bool,
    revealed: BTreeMap<usize, Part>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Part {
    above: u32,
    below: u32,
    all: bool,
}

impl Expansion {
    /// True while every gap is shown in full, the whole-file view.
    pub fn is_whole_file(&self) -> bool {
        self.whole_file
    }

    /// Switches the whole-file view on or off. The per-gap reveals stay, so
    /// turning it off returns to what the user had opened before.
    pub fn set_whole_file(&mut self, whole: bool) {
        self.whole_file = whole;
    }

    /// Forgets every reveal, for another file or another diff.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Reveals more of `gap`, whose hidden run is `len` lines long.
    pub fn expand(&mut self, gap: usize, action: GapAction, len: u32) {
        if self.whole_file || len == 0 {
            return;
        }
        let part = self.revealed.entry(gap).or_default();
        if part.all {
            return;
        }
        match action {
            GapAction::All => part.all = true,
            // The two revealed ends never overlap: the lines another side
            // already shows stay where they are, and this click takes what is
            // left.
            GapAction::Above => {
                part.above = part
                    .above
                    .saturating_add(REVEAL_STEP)
                    .min(len.saturating_sub(part.below));
            }
            GapAction::Below => {
                part.below = part
                    .below
                    .saturating_add(REVEAL_STEP)
                    .min(len.saturating_sub(part.above));
            }
        }
    }

    /// How much of `gap` to show, clamped to its length.
    pub fn reveal(&self, gap: usize, len: u32) -> Reveal {
        if len == 0 {
            return Reveal::default();
        }
        if self.whole_file {
            return Reveal {
                above: len,
                below: 0,
            };
        }
        match self.revealed.get(&gap) {
            Some(part) if part.all => Reveal {
                above: len,
                below: 0,
            },
            Some(part) => {
                let above = part.above.min(len);
                Reveal {
                    above,
                    below: part.below.min(len - above),
                }
            }
            None => Reveal::default(),
        }
    }
}

/// The lines of one gap a [`Reveal`] shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GapView {
    /// Lines revealed above the bar, in file order.
    pub above: Vec<DiffLine>,
    /// Lines revealed below the bar, in file order.
    pub below: Vec<DiffLine>,
    /// Lines still hidden between them; zero means no bar is drawn.
    pub hidden: u32,
}

/// Reads the shown lines of `gap` from the two blobs.
pub fn gap_view(old: &str, new: &str, gap: &Gap, reveal: Reveal) -> GapView {
    let above = reveal.above.min(gap.len);
    let below = reveal.below.min(gap.len - above);
    GapView {
        above: context_lines(old, new, gap, 0, above),
        below: context_lines(old, new, gap, gap.len - below, below),
        hidden: gap.len - above - below,
    }
}

/// `count` context lines of `gap`, starting `offset` lines into it.
fn context_lines(old: &str, new: &str, gap: &Gap, offset: u32, count: u32) -> Vec<DiffLine> {
    if count == 0 {
        return Vec::new();
    }
    let old_lines = slice(old, gap.old_start + offset, count);
    let new_lines = slice(new, gap.new_start + offset, count);
    (0..count)
        .map(|index| {
            let content = old_lines
                .get(index as usize)
                .or_else(|| new_lines.get(index as usize))
                .map_or("", |line| line.0);
            let no_newline = old_lines.get(index as usize).is_some_and(|line| line.1)
                || new_lines.get(index as usize).is_some_and(|line| line.1);
            DiffLine {
                kind: LineKind::Context,
                old_lineno: Some(gap.old_start + offset + index),
                new_lineno: Some(gap.new_start + offset + index),
                content: content.to_owned(),
                no_newline_at_eof: no_newline,
                emphasis: Vec::new(),
            }
        })
        .collect()
}

/// `count` lines of `text` from 1-based line `first`, without terminators.
///
/// The second value is true for a final line the file ends without a newline.
fn slice(text: &str, first: u32, count: u32) -> Vec<(&str, bool)> {
    let start = first.saturating_sub(1) as usize;
    text.split_inclusive('\n')
        .enumerate()
        .skip(start)
        .take(count as usize)
        .map(|(_, raw)| {
            let no_newline = !raw.ends_with('\n');
            let content = raw.strip_suffix('\n').unwrap_or(raw);
            let content = content.strip_suffix('\r').unwrap_or(content);
            (content, no_newline)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffOptions, Hunk, diff_text};

    fn text(lines: usize, changes: &[(usize, &str)]) -> String {
        (1..=lines)
            .map(|number| {
                changes.iter().find(|(at, _)| *at == number).map_or_else(
                    || format!("line {number}\n"),
                    |(_, text)| format!("{text}\n"),
                )
            })
            .collect()
    }

    fn diff_of(old: &str, new: &str, context_lines: u32) -> FileDiff {
        diff_text(
            old.as_bytes(),
            new.as_bytes(),
            &DiffOptions {
                context_lines,
                ..DiffOptions::default()
            },
        )
    }

    fn gap_line(line: &DiffLine) -> (Option<u32>, Option<u32>, &str) {
        (line.old_lineno, line.new_lineno, line.content.as_str())
    }

    #[test]
    fn gaps_between_hunks_carry_line_numbers_on_both_sides() {
        let old = text(15, &[(2, "two"), (13, "thirteen")]);
        let new = text(15, &[(2, "TWO"), (13, "THIRTEEN")]);
        let diff = diff_of(&old, &new, 3);
        assert_eq!(diff.hunks.len(), 2);

        let gaps = gaps(&diff, line_count(&old), line_count(&new));
        assert_eq!(
            gaps,
            vec![
                Gap {
                    old_start: 1,
                    new_start: 1,
                    len: 0
                },
                Gap {
                    old_start: 6,
                    new_start: 6,
                    len: 4
                },
                Gap {
                    old_start: 16,
                    new_start: 16,
                    len: 0
                },
            ]
        );

        let view = gap_view(&old, &new, &gaps[1], Reveal { above: 2, below: 1 });
        assert_eq!(
            view.above.iter().map(gap_line).collect::<Vec<_>>(),
            vec![(Some(6), Some(6), "line 6"), (Some(7), Some(7), "line 7")]
        );
        assert_eq!(
            view.below.iter().map(gap_line).collect::<Vec<_>>(),
            vec![(Some(9), Some(9), "line 9")]
        );
        assert_eq!(view.hidden, 1);
    }

    #[test]
    fn the_first_and_last_gaps_reach_the_file_edges() {
        let old = text(20, &[(5, "five"), (10, "ten")]);
        let new = text(20, &[(5, "FIVE"), (10, "TEN")]);
        let diff = diff_of(&old, &new, 1);
        assert_eq!(diff.hunks.len(), 2);

        let gaps = gaps(&diff, line_count(&old), line_count(&new));
        assert_eq!(gaps[0].old_start, 1);
        assert_eq!(gaps[0].len, 3, "lines 1–3 are above the first hunk");
        assert_eq!(gaps[1].len, 2, "lines 7 and 8 sit between the hunks");
        assert_eq!(gaps[2].old_start, 12);
        assert_eq!(gaps[2].len, 9, "lines 12–20 are below the last hunk");

        let last = gap_view(
            &old,
            &new,
            &gaps[2],
            Reveal {
                above: 0,
                below: u32::MAX,
            },
        );
        assert_eq!(last.above.len() + last.below.len(), 9);
        assert_eq!(last.hidden, 0);
        assert_eq!(
            gap_line(last.below.last().unwrap()),
            (Some(20), Some(20), "line 20")
        );
    }

    #[test]
    fn adjacent_hunks_leave_an_empty_gap() {
        // `build_hunk` never produces this, but a hand-built diff (or a future
        // grouping rule) can; the view must not draw a bar for it.
        let diff = FileDiff {
            hunks: vec![
                Hunk {
                    old_start: 1,
                    old_len: 2,
                    new_start: 1,
                    new_len: 2,
                    lines: Vec::new(),
                },
                Hunk {
                    old_start: 3,
                    old_len: 1,
                    new_start: 3,
                    new_len: 1,
                    lines: Vec::new(),
                },
            ],
            binary: false,
        };
        let gaps = gaps(&diff, 5, 5);
        assert_eq!(gaps.len(), 3);
        assert_eq!(gaps[1].len, 0);
        let view = gap_view(
            "a\nb\nc\nd\ne\n",
            "a\nb\nc\nd\ne\n",
            &gaps[1],
            Reveal::default(),
        );
        assert!(view.above.is_empty() && view.below.is_empty());
        assert_eq!(view.hidden, 0);
    }

    #[test]
    fn a_file_without_a_trailing_newline_keeps_the_flag() {
        let old = "a\nb\nc";
        let new = "X\nb\nc";
        let diff = diff_of(old, new, 1);
        let gaps = gaps(&diff, line_count(old), line_count(new));
        let view = gap_view(
            old,
            new,
            &gaps[1],
            Reveal {
                above: u32::MAX,
                below: 0,
            },
        );
        assert_eq!(view.above.len(), 1);
        assert_eq!(
            gap_line(&view.above[0]),
            (Some(3), Some(3), "c"),
            "the last line has its number on both sides"
        );
        assert!(view.above[0].no_newline_at_eof);
    }

    #[test]
    fn crlf_lines_lose_their_carriage_return_but_keep_their_numbers() {
        let old = "a\r\nb\r\nc\r\nd\r\n";
        let new = "a\r\nB\r\nc\r\nd\r\n";
        let diff = diff_of(old, new, 0);
        assert_eq!(diff.hunks.len(), 1);
        let gaps = gaps(&diff, line_count(old), line_count(new));
        assert_eq!(gaps[0].len, 1);
        assert_eq!(gaps[1].len, 2);

        let view = gap_view(
            old,
            new,
            &gaps[1],
            Reveal {
                above: u32::MAX,
                below: 0,
            },
        );
        assert_eq!(
            view.above.iter().map(gap_line).collect::<Vec<_>>(),
            vec![(Some(3), Some(3), "c"), (Some(4), Some(4), "d")]
        );
    }

    #[test]
    fn revealing_clamps_to_the_gap_and_both_ends_never_overlap() {
        let mut expansion = Expansion::default();
        let len = 30;

        expansion.expand(0, GapAction::Above, len);
        assert_eq!(
            expansion.reveal(0, len),
            Reveal {
                above: REVEAL_STEP,
                below: 0
            }
        );

        expansion.expand(0, GapAction::Below, len);
        assert_eq!(
            expansion.reveal(0, len),
            Reveal {
                above: REVEAL_STEP,
                below: len - REVEAL_STEP
            },
            "expanding below keeps the lines above and takes only what is left"
        );

        expansion.expand(0, GapAction::Above, len);
        assert_eq!(
            expansion.reveal(0, len),
            Reveal {
                above: REVEAL_STEP,
                below: len - REVEAL_STEP
            },
            "a fully shown gap has nothing left for either arrow"
        );

        expansion.expand(0, GapAction::All, len);
        assert_eq!(
            expansion.reveal(0, len),
            Reveal {
                above: len,
                below: 0
            }
        );
        assert_eq!(expansion.reveal(0, len).hidden(len), 0);

        // Another gap is untouched, and a gap shorter than one step cannot
        // reveal past its own length.
        assert_eq!(expansion.reveal(1, 5), Reveal::default());
        expansion.expand(1, GapAction::Above, 5);
        assert_eq!(expansion.reveal(1, 5), Reveal { above: 5, below: 0 });
    }

    #[test]
    fn whole_file_reveals_every_gap_and_reset_forgets_everything() {
        let mut expansion = Expansion::default();
        expansion.expand(2, GapAction::Above, 100);
        expansion.set_whole_file(true);
        assert!(expansion.is_whole_file());
        assert_eq!(
            expansion.reveal(2, 100),
            Reveal {
                above: 100,
                below: 0
            }
        );
        expansion.expand(3, GapAction::Below, 100);
        assert_eq!(
            expansion.reveal(3, 100),
            Reveal {
                above: 100,
                below: 0
            },
            "whole-file already shows every line"
        );

        expansion.set_whole_file(false);
        assert_eq!(
            expansion.reveal(2, 100),
            Reveal {
                above: REVEAL_STEP,
                below: 0
            },
            "the per-gap reveal is back"
        );

        expansion.reset();
        assert!(!expansion.is_whole_file());
        assert_eq!(expansion.reveal(2, 100), Reveal::default());
    }

    #[test]
    fn line_counts_include_an_unterminated_last_line() {
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("\n"), 1);
        assert_eq!(line_count("a"), 1);
        assert_eq!(line_count("a\n"), 1);
        assert_eq!(line_count("a\nb"), 2);
        assert_eq!(line_count("a\r\nb\r\n"), 2);
    }
}
