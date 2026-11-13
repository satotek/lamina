//! The diff pane: a virtualised list of unified-diff rows.
//!
//! `Hunk`/`DiffLine` come from `strata-core`; this module only flattens them
//! into uniform-height rows and paints line numbers, `+`/`-` backgrounds and
//! word-level emphasis ranges.

use std::ops::Range;

use gpui::{AnyElement, Div, Hsla, IntoElement, ParentElement, SharedString, Styled, div, px};
use gpui_component::theme::Theme;
use strata_core::diff::{DiffLine, FileDiff, LineKind};
use strata_core::highlight::{HighlightKind, LineHighlights};

pub const ROW_HEIGHT: f32 = 20.0;
const GUTTER_WIDTH: f32 = 46.0;

/// A flat row in the diff list. Hunk headers share the line height so the list
/// can stay a `uniform_list`.
pub enum Row {
    HunkHeader {
        old_start: u32,
        old_len: u32,
        new_start: u32,
        new_len: u32,
    },
    Line(DiffLine),
}

pub fn flatten(diff: &FileDiff) -> Vec<Row> {
    let mut rows = Vec::with_capacity(diff.hunks.iter().map(|h| h.lines.len() + 1).sum());
    for hunk in &diff.hunks {
        rows.push(Row::HunkHeader {
            old_start: hunk.old_start,
            old_len: hunk.old_len,
            new_start: hunk.new_start,
            new_len: hunk.new_len,
        });
        rows.extend(hunk.lines.iter().cloned().map(Row::Line));
    }
    rows
}

#[derive(Clone, Copy)]
pub struct DiffColors {
    pub text: Hsla,
    pub muted: Hsla,
    pub border: Hsla,
    pub added: Hsla,
    pub removed: Hsla,
    pub emphasis_added: Hsla,
    pub emphasis_removed: Hsla,
    pub header: Hsla,
    /// One colour per [`HighlightKind`], indexed by its discriminant.
    syntax: [Hsla; 16],
}

impl DiffColors {
    pub fn from_theme(theme: &Theme) -> Self {
        let dark = theme.is_dark();
        let (red, green, blue, yellow, magenta, cyan) = if dark {
            (
                theme.red,
                theme.green,
                theme.blue,
                theme.yellow,
                theme.magenta,
                theme.cyan,
            )
        } else {
            (
                theme.red_light,
                theme.green_light,
                theme.blue_light,
                theme.yellow_light,
                theme.magenta_light,
                theme.cyan_light,
            )
        };
        // The order matches the `HighlightKind` variants; a comment on the first
        // one alone would rot, so keep this list beside the enum's definition.
        let syntax = [
            magenta,                // Keyword
            green,                  // String
            theme.muted_foreground, // Comment
            blue,                   // Function
            yellow,                 // Type
            yellow,                 // Constant
            cyan,                   // Number
            theme.foreground,       // Variable
            blue,                   // Property
            theme.muted_foreground, // Operator
            theme.muted_foreground, // Punctuation
            magenta,                // Attribute
            red,                    // Tag
            blue,                   // Label
            red,                    // Escape
            cyan,                   // Embedded
        ];
        Self {
            text: theme.foreground,
            muted: theme.muted_foreground,
            border: theme.border,
            // Low alpha fills keep the diff readable in both appearances while
            // still making added and removed runs obvious.
            added: theme.success.opacity(if dark { 0.16 } else { 0.12 }),
            removed: theme.danger.opacity(if dark { 0.16 } else { 0.12 }),
            emphasis_added: theme.success.opacity(if dark { 0.42 } else { 0.30 }),
            emphasis_removed: theme.danger.opacity(if dark { 0.42 } else { 0.30 }),
            header: theme.muted.opacity(if dark { 0.55 } else { 0.45 }),
            syntax,
        }
    }

    pub fn syntax(&self, kind: HighlightKind) -> Hsla {
        self.syntax[kind as usize]
    }
}

pub fn render_row(
    row: &Row,
    colors: &DiffColors,
    mono: &SharedString,
    highlights: Option<&LineHighlights>,
) -> AnyElement {
    match row {
        Row::HunkHeader {
            old_start,
            old_len,
            new_start,
            new_len,
        } => div()
            .flex()
            .items_center()
            .h(px(ROW_HEIGHT))
            .w_full()
            .bg(colors.header)
            .border_t_1()
            .border_color(colors.border)
            .text_size(px(11.))
            .text_color(colors.muted)
            .child(div().pl_2().child(format!(
                "@@ -{old_start},{old_len} +{new_start},{new_len} @@"
            )))
            .into_any_element(),
        Row::Line(line) => {
            let (row_bg, marker, emphasis_bg) = match line.kind {
                LineKind::Added => (Some(colors.added), "+", colors.emphasis_added),
                LineKind::Removed => (Some(colors.removed), "-", colors.emphasis_removed),
                LineKind::Context => (None, " ", colors.emphasis_added),
            };

            let mut content = div().flex().flex_none().whitespace_nowrap();
            for piece in segments(&line.content, &line.emphasis, highlights) {
                let mut span = div().child(SharedString::from(piece.text));
                if let Some(kind) = piece.kind {
                    span = span.text_color(colors.syntax(kind));
                }
                if piece.emphasised {
                    span = span.bg(emphasis_bg).rounded_sm();
                }
                content = content.child(span);
            }

            let mut row = div()
                .flex()
                .items_center()
                .h(px(ROW_HEIGHT))
                .w_full()
                .font_family(mono.clone())
                .text_size(px(12.))
                .text_color(colors.text);
            if let Some(background) = row_bg {
                row = row.bg(background);
            }

            row.child(gutter(line.old_lineno, colors))
                .child(gutter(line.new_lineno, colors))
                .child(
                    div()
                        .w(px(14.))
                        .flex_none()
                        .text_color(colors.muted)
                        .child(marker),
                )
                .child(content)
                .into_any_element()
        }
    }
}

fn gutter(number: Option<u32>, colors: &DiffColors) -> Div {
    let mut gutter = div()
        .flex()
        .justify_end()
        .w(px(GUTTER_WIDTH))
        .flex_none()
        .pr_2()
        .text_color(colors.muted);
    if let Some(number) = number {
        gutter = gutter.child(format!("{number}"));
    }
    gutter
}

/// A run of a line with one syntax colour and/or emphasis.
#[derive(Debug, PartialEq, Eq)]
struct Piece<'a> {
    text: &'a str,
    kind: Option<HighlightKind>,
    emphasised: bool,
}

/// Splits a line into runs by syntax spans and emphasis ranges.
///
/// Both inputs are byte offsets into the same line and may overlap: a changed
/// token inside a changed line gets both a syntax colour and the emphasis
/// background. Only boundaries from either source matter, so every resulting
/// run is uniform. Ranges that fall outside the line are clamped to character
/// boundaries, which keeps a bad range from panicking the renderer.
fn segments<'a>(
    content: &'a str,
    emphasis: &[Range<usize>],
    highlights: Option<&LineHighlights>,
) -> Vec<Piece<'a>> {
    let mut boundaries = vec![0, content.len()];
    for range in emphasis {
        boundaries.push(clamp_boundary(content, range.start));
        boundaries.push(clamp_boundary(content, range.end));
    }
    if let Some(highlights) = highlights {
        for (range, _) in highlights {
            boundaries.push(clamp_boundary(content, range.start));
            boundaries.push(clamp_boundary(content, range.end));
        }
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    // Merge adjacent runs so a line without highlights is still one span
    // instead of one span per boundary.
    let mut runs: Vec<(usize, usize, Option<HighlightKind>, bool)> = Vec::new();
    for pair in boundaries.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        if start >= end {
            continue;
        }
        let kind = highlights.and_then(|spans| {
            spans
                .iter()
                .find(|(range, _)| range.start <= start && end <= range.end)
                .map(|(_, kind)| *kind)
        });
        let emphasised = emphasis
            .iter()
            .any(|range| range.start <= start && end <= range.end);

        match runs.last_mut() {
            Some(last) if last.2 == kind && last.3 == emphasised => last.1 = end,
            _ => runs.push((start, end, kind, emphasised)),
        }
    }

    runs.into_iter()
        .map(|(start, end, kind, emphasised)| Piece {
            text: &content[start..end],
            kind,
            emphasised,
        })
        .collect()
}

fn clamp_boundary(content: &str, index: usize) -> usize {
    let mut index = index.min(content.len());
    while index > 0 && !content.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake;

    fn plain<'a>(pieces: &'a [Piece<'a>]) -> Vec<(&'a str, Option<HighlightKind>, bool)> {
        pieces
            .iter()
            .map(|piece| (piece.text, piece.kind, piece.emphasised))
            .collect()
    }

    fn rebuild(pieces: &[Piece<'_>]) -> String {
        pieces.iter().map(|piece| piece.text).collect()
    }

    #[test]
    fn rows_cover_every_line() {
        let diff = fake::sample_diff(1);
        let total: usize = diff.hunks.iter().map(|hunk| hunk.lines.len()).sum();
        assert_eq!(flatten(&diff).len(), total + diff.hunks.len());
    }

    #[test]
    fn emphasis_is_split_out_of_plain_text() {
        let pieces = segments("abcdef", &[Range { start: 2, end: 4 }], None);
        assert_eq!(
            plain(&pieces),
            vec![("ab", None, false), ("cd", None, true), ("ef", None, false)]
        );
    }

    #[test]
    fn overlapping_ranges_are_merged() {
        let pieces = segments(
            "abcdef",
            &[Range { start: 1, end: 3 }, Range { start: 2, end: 5 }],
            None,
        );
        assert_eq!(
            plain(&pieces),
            vec![("a", None, false), ("bcde", None, true), ("f", None, false)]
        );
    }

    #[test]
    fn highlights_colour_tokens_without_losing_text() {
        let highlights = vec![
            (Range { start: 0, end: 3 }, HighlightKind::Keyword),
            (Range { start: 4, end: 5 }, HighlightKind::Variable),
        ];
        let pieces = segments("let x = 1;", &[], Some(&highlights));

        assert_eq!(rebuild(&pieces), "let x = 1;");
        assert_eq!(
            plain(&pieces),
            vec![
                ("let", Some(HighlightKind::Keyword), false),
                (" ", None, false),
                ("x", Some(HighlightKind::Variable), false),
                (" = 1;", None, false),
            ]
        );
    }

    #[test]
    fn syntax_and_emphasis_overlap_on_one_token() {
        let highlights = vec![(Range { start: 0, end: 3 }, HighlightKind::Keyword)];
        let pieces = segments(
            "let value",
            &[Range { start: 0, end: 3 }],
            Some(&highlights),
        );

        assert_eq!(rebuild(&pieces), "let value");
        assert_eq!(
            plain(&pieces),
            vec![
                ("let", Some(HighlightKind::Keyword), true),
                (" value", None, false),
            ]
        );
    }

    #[test]
    fn out_of_range_spans_are_clamped_to_char_boundaries() {
        let highlights = vec![
            (Range { start: 0, end: 900 }, HighlightKind::String),
            (Range { start: 90, end: 95 }, HighlightKind::Number),
        ];
        let pieces = segments("héllo", &[Range { start: 3, end: 999 }], Some(&highlights));

        assert_eq!(rebuild(&pieces), "héllo");
        for piece in &pieces {
            assert!(!piece.text.is_empty());
        }
        assert!(pieces.iter().any(|piece| piece.emphasised));
    }

    #[test]
    fn no_highlights_keeps_one_span_per_emphasis_run() {
        let pieces = segments("unchanged", &[], None);
        assert_eq!(plain(&pieces), vec![("unchanged", None, false)]);
    }
}
