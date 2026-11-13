//! The diff pane: a virtualised list of unified-diff rows.
//!
//! `Hunk`/`DiffLine` come from `strata-core`; this module only flattens them
//! into uniform-height rows and paints line numbers, `+`/`-` backgrounds and
//! word-level emphasis ranges.

use std::ops::Range;

use gpui::{AnyElement, Div, Hsla, IntoElement, ParentElement, SharedString, Styled, div, px};
use gpui_component::theme::Theme;
use strata_core::diff::{DiffLine, FileDiff, LineKind};

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
}

impl DiffColors {
    pub fn from_theme(theme: &Theme) -> Self {
        let dark = theme.is_dark();
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
        }
    }
}

pub fn render_row(row: &Row, colors: &DiffColors, mono: &SharedString) -> AnyElement {
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
            for (part, emphasised) in emphasise(&line.content, &line.emphasis) {
                let mut span = div().child(SharedString::from(part));
                if emphasised {
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

/// Splits a line into plain and emphasised runs. Ranges are byte offsets and
/// may arrive out of order or overlapping, so they are merged first.
fn emphasise<'a>(content: &'a str, emphasis: &[Range<usize>]) -> Vec<(&'a str, bool)> {
    if emphasis.is_empty() {
        return vec![(content, false)];
    }

    let mut ranges: Vec<Range<usize>> = emphasis
        .iter()
        .map(|range| clamp_to_boundaries(content, range.clone()))
        .filter(|range| range.start < range.end)
        .collect();
    ranges.sort_by_key(|range| range.start);

    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }

    let mut parts = Vec::with_capacity(merged.len() * 2 + 1);
    let mut cursor = 0;
    for range in merged {
        if cursor < range.start {
            parts.push((&content[cursor..range.start], false));
        }
        parts.push((&content[range.start..range.end], true));
        cursor = range.end;
    }
    if cursor < content.len() {
        parts.push((&content[cursor..], false));
    }
    parts
}

fn clamp_to_boundaries(content: &str, range: Range<usize>) -> Range<usize> {
    clamp_boundary(content, range.start)..clamp_boundary(content, range.end)
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

    #[test]
    fn rows_cover_every_line() {
        let diff = fake::sample_diff(1);
        let total: usize = diff.hunks.iter().map(|hunk| hunk.lines.len()).sum();
        assert_eq!(flatten(&diff).len(), total + diff.hunks.len());
    }

    #[test]
    fn emphasis_is_split_out_of_plain_text() {
        let parts = emphasise("abcdef", &[Range { start: 2, end: 4 }]);
        assert_eq!(parts, vec![("ab", false), ("cd", true), ("ef", false)]);
    }

    #[test]
    fn overlapping_ranges_are_merged() {
        let parts = emphasise(
            "abcdef",
            &[Range { start: 1, end: 3 }, Range { start: 2, end: 5 }],
        );
        assert_eq!(parts, vec![("a", false), ("bcde", true), ("f", false)]);
    }
}
