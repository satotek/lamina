//! The diff pane: diff rows rendered with the user's display settings.
//!
//! `Hunk`/`DiffLine` come from `strata-core`; this module flattens them into
//! rows and paints line numbers, `+`/`-` backgrounds, word-level emphasis and
//! syntax colours. Every pane that shows a diff goes through [`diff_list`], so
//! the diff preferences in [`crate::settings`] (unified or side-by-side,
//! wrapping, syntax colours, visible whitespace, tab width and the code font)
//! apply everywhere at once.
//!
//! Nothing here reads git: the panes hand over rows and the [`DiffView`] they
//! came from.

use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, BorrowAppContext as _, Div, Global, HighlightStyle, Hsla,
    InteractiveElement as _, IntoElement, ParentElement, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled, StyledText, UniformListScrollHandle, div,
    prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_component::scroll::ScrollableElement as _;
use gpui_component::theme::Theme;
use strata_core::diff::{DiffLine, FileDiff, LineKind};
use strata_core::highlight::{HighlightKind, LineHighlights};
use strata_core::session::DiffView;

use crate::scrollbar::WithScrollbar as _;
use crate::settings::{DiffLayout, Settings};

/// Height of one diff row at the default code font size.
pub const ROW_HEIGHT: f32 = 20.0;
const GUTTER_WIDTH: f32 = 46.0;
const PICK_WIDTH: f32 = 14.0;
const MARKER_WIDTH: f32 = 14.0;

/// The old and new side of a side-by-side diff.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Old,
    New,
}

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

/// One row of a side-by-side diff: the row indices of its two sides, or the
/// row index of a hunk header that spans both columns.
///
/// A change block pairs its removed and added lines in order; the longer side's
/// extra lines have an empty counterpart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplitRow {
    Header {
        row: usize,
    },
    Lines {
        old: Option<usize>,
        new: Option<usize>,
    },
}

/// Aligns the unified row stream into side-by-side rows, one per hunk.
///
/// Side-by-side is shorter than the unified stream whenever a hunk removes
/// more lines than it adds or the other way round. [`diff_list`] keeps one list
/// item per unified row anyway, so the tail of a split list is empty rather
/// than a second, layout-dependent row count.
fn split_rows(rows: &[Row]) -> Vec<SplitRow> {
    let mut out = Vec::with_capacity(rows.len());
    let mut removed = Vec::new();
    let mut added = Vec::new();

    for (index, row) in rows.iter().enumerate() {
        match row {
            Row::HunkHeader { .. } => {
                flush_block(&mut removed, &mut added, &mut out);
                out.push(SplitRow::Header { row: index });
            }
            Row::Line(line) => match line.kind {
                LineKind::Removed => removed.push(index),
                LineKind::Added => added.push(index),
                LineKind::Context => {
                    flush_block(&mut removed, &mut added, &mut out);
                    out.push(SplitRow::Lines {
                        old: Some(index),
                        new: Some(index),
                    });
                }
            },
        }
    }
    flush_block(&mut removed, &mut added, &mut out);
    out
}

fn flush_block(removed: &mut Vec<usize>, added: &mut Vec<usize>, out: &mut Vec<SplitRow>) {
    for pair in 0..removed.len().max(added.len()) {
        out.push(SplitRow::Lines {
            old: removed.get(pair).copied(),
            new: added.get(pair).copied(),
        });
    }
    removed.clear();
    added.clear();
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
    /// Accent for the lines picked for a partial stage or discard.
    pub selection: Hsla,
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
            selection: theme.primary,
            syntax,
        }
    }

    pub fn syntax(&self, kind: HighlightKind) -> Hsla {
        self.syntax[kind as usize]
    }
}

/// Decoration for one row of a diff that is also a staging surface.
#[derive(Clone, Copy, Default)]
pub struct RowOptions {
    /// `Some(picked)` for a changed line the user can select, `None` for
    /// context lines and hunk headers.
    pub pick: Option<bool>,
}

/// The code font and diff display preferences, read once per frame.
///
/// Every pane builds one from [`Settings::get`] so a change reaches all of them
/// on the next frame; `fallback` is the theme's monospace family, used until
/// the user picks a code font.
#[derive(Clone)]
pub struct DiffStyle {
    mono: SharedString,
    font_size: f32,
    tab_width: usize,
    split: bool,
    wrap: bool,
    syntax: bool,
    show_whitespace: bool,
}

impl DiffStyle {
    pub fn from_settings(cx: &App, fallback: &SharedString) -> Self {
        let settings = Settings::get(cx);
        let chosen = settings
            .appearance
            .code_font
            .as_deref()
            .map(str::trim)
            .filter(|font| !font.is_empty());
        Self {
            mono: chosen.map_or_else(
                || fallback.clone(),
                |font| SharedString::from(font.to_owned()),
            ),
            font_size: settings.appearance.code_font_size.clamp(9.0, 24.0),
            tab_width: usize::from(settings.appearance.tab_width.clamp(1, 8)),
            split: settings.diff.layout == DiffLayout::Split,
            wrap: settings.diff.wrap_lines,
            syntax: settings.diff.syntax_highlighting,
            show_whitespace: settings.diff.show_whitespace,
        }
    }

    /// One line per row, never wrapping.
    ///
    /// The staging pane keeps this: its rows are click targets into a
    /// [`FileDiff`], so a row has to stay the one diff line it shows.
    pub fn one_line_per_row(mut self) -> Self {
        self.split = false;
        self.wrap = false;
        self
    }

    /// Row height for this font size. A list is only uniform if every row
    /// agrees, so panes with their own hunk headers (the staging pane) ask for
    /// this instead of using [`ROW_HEIGHT`] directly.
    pub fn row_height(&self) -> f32 {
        (self.font_size + 7.5).max(ROW_HEIGHT)
    }
}

/// One row of a diff, as the plain (unified) renderer draws it.
pub fn render_row_with(
    row: &Row,
    colors: &DiffColors,
    style: &DiffStyle,
    highlights: Option<&LineHighlights>,
    options: RowOptions,
) -> AnyElement {
    let highlights = if style.syntax { highlights } else { None };
    unified_row(row, colors, style, highlights, options, false)
}

/// The diff pane's list of rows.
///
/// Wrapping makes rows taller than one line, which a `uniform_list` cannot
/// measure, so a wrapped pane is a plain scroll column; everything else keeps
/// the uniform list and the caller's scroll handle.
pub fn diff_list(
    id: &'static str,
    rows: Rc<Vec<Row>>,
    view: Option<Arc<DiffView>>,
    colors: DiffColors,
    mono: SharedString,
    scroll: &UniformListScrollHandle,
    cx: &mut App,
) -> AnyElement {
    let style = DiffStyle::from_settings(cx, &mono);
    let pairs = style.split.then(|| split_rows(&rows));

    if style.wrap {
        let handle = wrapped_scroll(id, cx);
        let mut column = div().flex().flex_col().w_full();
        for index in 0..rows.len() {
            column = column.child(row_element(
                &rows,
                index,
                pairs.as_deref(),
                view.as_deref(),
                &colors,
                &style,
                true,
            ));
        }
        return div()
            .id(id)
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_y_scroll()
            .track_scroll(&handle)
            .vertical_scrollbar(&handle)
            .child(column)
            .into_any_element();
    }

    uniform_list(id, rows.len(), move |range: Range<usize>, _window, _cx| {
        range
            .map(|index| {
                row_element(
                    &rows,
                    index,
                    pairs.as_deref(),
                    view.as_deref(),
                    &colors,
                    &style,
                    false,
                )
            })
            .collect::<Vec<_>>()
    })
    .track_scroll(scroll)
    .with_scrollbar(scroll)
    .flex_1()
    .min_h_0()
    .into_any_element()
}

/// A wrapped pane cannot be a uniform list, and gpui's general list sizes its
/// rows while text wrapping wants a definite width, so a wrapped diff is a
/// plain scroll column. That costs the virtualisation, which is why wrapping is
/// off by default; its scroll offset still has to outlive the frame, and the
/// panes hold a `UniformListScrollHandle` for the unwrapped path, so the wrapped
/// one lives here, one per pane, keyed by the pane's list id.
#[derive(Default)]
struct WrappedScroll(HashMap<&'static str, ScrollHandle>);

impl Global for WrappedScroll {}

fn wrapped_scroll(id: &'static str, cx: &mut App) -> ScrollHandle {
    if !cx.has_global::<WrappedScroll>() {
        cx.set_global(WrappedScroll::default());
    }
    cx.update_global::<WrappedScroll, _>(|scrolls, _| scrolls.0.entry(id).or_default().clone())
}

fn row_element(
    rows: &[Row],
    index: usize,
    pairs: Option<&[SplitRow]>,
    view: Option<&DiffView>,
    colors: &DiffColors,
    style: &DiffStyle,
    wrap: bool,
) -> AnyElement {
    if let Some(pairs) = pairs {
        return match pairs.get(index) {
            Some(pair) => split_row(*pair, rows, colors, style, view, wrap),
            // Side-by-side is shorter than the unified stream; the extra list
            // items stay empty so the item count does not depend on the layout.
            None => div().h(px(style.row_height())).w_full().into_any_element(),
        };
    }

    let Some(row) = rows.get(index) else {
        return div().into_any_element();
    };
    let highlights = line_highlights(row, view, style.syntax);
    unified_row(row, colors, style, highlights, RowOptions::default(), wrap)
}

/// The syntax spans of the line a row shows, from the view that produced it.
fn line_highlights<'a>(
    row: &Row,
    view: Option<&'a DiffView>,
    syntax: bool,
) -> Option<&'a LineHighlights> {
    let view = view.filter(|_| syntax)?;
    let Row::Line(line) = row else {
        return None;
    };
    let (highlights, number) = match line.kind {
        LineKind::Removed => (&view.old_highlights, line.old_lineno),
        LineKind::Added => (&view.new_highlights, line.new_lineno),
        LineKind::Context => (&view.new_highlights, line.new_lineno.or(line.old_lineno)),
    };
    highlights.get(number?.checked_sub(1)? as usize)
}

fn side_highlights<'a>(
    view: Option<&'a DiffView>,
    side: Side,
    line: &DiffLine,
    syntax: bool,
) -> Option<&'a LineHighlights> {
    let view = view.filter(|_| syntax)?;
    let (highlights, number) = match side {
        Side::Old => (&view.old_highlights, line.old_lineno),
        Side::New => (&view.new_highlights, line.new_lineno),
    };
    highlights.get(number?.checked_sub(1)? as usize)
}

fn unified_row(
    row: &Row,
    colors: &DiffColors,
    style: &DiffStyle,
    highlights: Option<&LineHighlights>,
    options: RowOptions,
    wrap: bool,
) -> AnyElement {
    match row {
        Row::HunkHeader {
            old_start,
            old_len,
            new_start,
            new_len,
        } => hunk_header((*old_start, *old_len, *new_start, *new_len), colors, style),
        Row::Line(line) => {
            let picked = options.pick == Some(true);
            let mut row = div()
                .flex()
                .min_h(px(style.row_height()))
                .w_full()
                .border_l_2()
                .border_color(if picked {
                    colors.selection
                } else {
                    Hsla::transparent_black()
                })
                .font_family(style.mono.clone())
                .text_size(px(style.font_size))
                .text_color(colors.text);
            row = if wrap {
                row.items_start()
            } else {
                row.items_center().h(px(style.row_height()))
            };

            if let Some(background) = line_background(line, colors) {
                row = row.bg(background);
            } else if picked {
                row = row.bg(colors.selection.opacity(0.14));
            }

            row.child(
                div()
                    .w(px(PICK_WIDTH))
                    .flex_none()
                    .text_size(px(10.0))
                    .text_color(colors.selection)
                    .child(if picked { "✓" } else { "" }),
            )
            .child(gutter(line.old_lineno, colors))
            .child(gutter(line.new_lineno, colors))
            .child(
                div()
                    .w(px(MARKER_WIDTH))
                    .flex_none()
                    .text_color(colors.muted)
                    .child(marker(line.kind)),
            )
            .child(line_content(line, colors, style, highlights, wrap))
            .into_any_element()
        }
    }
}

fn split_row(
    pair: SplitRow,
    rows: &[Row],
    colors: &DiffColors,
    style: &DiffStyle,
    view: Option<&DiffView>,
    wrap: bool,
) -> AnyElement {
    match pair {
        SplitRow::Header { row } => match rows.get(row) {
            Some(Row::HunkHeader {
                old_start,
                old_len,
                new_start,
                new_len,
            }) => hunk_header((*old_start, *old_len, *new_start, *new_len), colors, style),
            _ => div().into_any_element(),
        },
        SplitRow::Lines { old, new } => {
            let old_line = old.and_then(|index| line_at(rows, index));
            let new_line = new.and_then(|index| line_at(rows, index));
            let old_highlights =
                old_line.and_then(|line| side_highlights(view, Side::Old, line, style.syntax));
            let new_highlights =
                new_line.and_then(|line| side_highlights(view, Side::New, line, style.syntax));
            div()
                .flex()
                .w_full()
                .min_h(px(style.row_height()))
                .font_family(style.mono.clone())
                .text_size(px(style.font_size))
                .text_color(colors.text)
                .child(side_cell(
                    old_line,
                    Side::Old,
                    colors,
                    style,
                    old_highlights,
                    wrap,
                ))
                .child(side_cell(
                    new_line,
                    Side::New,
                    colors,
                    style,
                    new_highlights,
                    wrap,
                ))
                .into_any_element()
        }
    }
}

fn line_at(rows: &[Row], index: usize) -> Option<&DiffLine> {
    match rows.get(index) {
        Some(Row::Line(line)) => Some(line),
        _ => None,
    }
}

/// One column of a side-by-side row: its line number, marker and text.
fn side_cell(
    line: Option<&DiffLine>,
    side: Side,
    colors: &DiffColors,
    style: &DiffStyle,
    highlights: Option<&LineHighlights>,
    wrap: bool,
) -> Div {
    let number = line.and_then(|line| match side {
        Side::Old => line.old_lineno,
        Side::New => line.new_lineno,
    });
    let (background, marker) = match line.map(|line| line.kind) {
        Some(LineKind::Added) => (Some(colors.added), "+"),
        Some(LineKind::Removed) => (Some(colors.removed), "-"),
        Some(LineKind::Context) => (None, " "),
        None => (None, ""),
    };

    let mut cell = div()
        .flex()
        .flex_1()
        .min_w_0()
        .w_full()
        .min_h(px(style.row_height()))
        .border_l_2()
        .border_color(match side {
            // The divider between the columns, drawn on the new side only.
            Side::Old => Hsla::transparent_black(),
            Side::New => colors.border,
        });
    cell = if wrap {
        cell.items_start()
    } else {
        cell.items_center().h(px(style.row_height()))
    };
    if let Some(background) = background {
        cell = cell.bg(background);
    }

    cell.child(gutter(number, colors))
        .child(
            div()
                .w(px(MARKER_WIDTH))
                .flex_none()
                .text_color(colors.muted)
                .child(marker),
        )
        .when_some(line, |cell, line| {
            cell.child(line_content(line, colors, style, highlights, wrap))
        })
}

fn hunk_header(
    (old_start, old_len, new_start, new_len): (u32, u32, u32, u32),
    colors: &DiffColors,
    style: &DiffStyle,
) -> AnyElement {
    div()
        .flex()
        .items_center()
        .h(px(style.row_height()))
        .w_full()
        .bg(colors.header)
        .border_t_1()
        .border_color(colors.border)
        .font_family(style.mono.clone())
        .text_size(px((style.font_size - 1.0).max(9.0)))
        .text_color(colors.muted)
        .child(div().pl_2().child(format!(
            "@@ -{old_start},{old_len} +{new_start},{new_len} @@"
        )))
        .into_any_element()
}

fn line_background(line: &DiffLine, colors: &DiffColors) -> Option<Hsla> {
    match line.kind {
        LineKind::Added => Some(colors.added),
        LineKind::Removed => Some(colors.removed),
        LineKind::Context => None,
    }
}

fn marker(kind: LineKind) -> &'static str {
    match kind {
        LineKind::Added => "+",
        LineKind::Removed => "-",
        LineKind::Context => " ",
    }
}

fn line_content(
    line: &DiffLine,
    colors: &DiffColors,
    style: &DiffStyle,
    highlights: Option<&LineHighlights>,
    wrap: bool,
) -> Div {
    let emphasis_bg = match line.kind {
        LineKind::Added => colors.emphasis_added,
        LineKind::Removed => colors.emphasis_removed,
        LineKind::Context => colors.emphasis_added,
    };
    let drawn = display(line, style, highlights);

    // One styled text run per colour/emphasis change. A single text element
    // rather than a row of spans is what lets a long line wrap: spans are
    // blocks, and a wrapped block stacks instead of flowing.
    let mut runs = Vec::new();
    let mut offset = 0;
    for piece in segments(&drawn.text, &drawn.emphasis, drawn.highlights.as_deref()) {
        let colour = piece.kind.map_or(colors.text, |kind| colors.syntax(kind));
        for (run, is_marker) in split_markers(piece.text, style.show_whitespace) {
            let start = offset;
            offset += run.len();
            runs.push((
                start..offset,
                HighlightStyle {
                    color: Some(if is_marker { colors.muted } else { colour }),
                    background_color: piece.emphasised.then_some(emphasis_bg),
                    ..HighlightStyle::default()
                },
            ));
        }
    }

    let mut content = div()
        .font_family(style.mono.clone())
        .text_size(px(style.font_size))
        .text_color(colors.text);
    content = if wrap {
        content.flex_1().min_w_0().whitespace_normal()
    } else {
        content.flex_none().whitespace_nowrap()
    };
    content.child(StyledText::new(drawn.text).with_highlights(runs))
}

/// One line as it is drawn: the rewritten text, its emphasis ranges and the
/// syntax spans that still apply, all in offsets into `text`.
struct DrawnLine {
    text: String,
    emphasis: Vec<Range<usize>>,
    highlights: Option<Vec<(Range<usize>, HighlightKind)>>,
}

fn display(line: &DiffLine, style: &DiffStyle, highlights: Option<&LineHighlights>) -> DrawnLine {
    if !style.show_whitespace && !line.content.contains('\t') {
        return DrawnLine {
            text: line.content.clone(),
            emphasis: line.emphasis.clone(),
            highlights: highlights.cloned(),
        };
    }

    let (text, offsets) = display_text(&line.content, style.tab_width, style.show_whitespace);
    let emphasis = line
        .emphasis
        .iter()
        .map(|range| remap(range, &offsets))
        .collect();
    let highlights = highlights.map(|spans| {
        spans
            .iter()
            .map(|(range, kind)| (remap(range, &offsets), *kind))
            .collect()
    });
    DrawnLine {
        text,
        emphasis,
        highlights,
    }
}

/// Rewrites a line for display: tabs expand to the next tab stop (or become
/// `→` while whitespace is visible) and spaces become `·`. The second value
/// maps every byte offset in `content` to the offset the same character has in
/// the rewritten text, which is how emphasis and syntax ranges survive.
fn display_text(content: &str, tab_width: usize, show_whitespace: bool) -> (String, Vec<usize>) {
    let tab_width = tab_width.max(1);
    let mut text = String::with_capacity(content.len());
    let mut offsets = vec![0; content.len() + 1];
    let mut column = 0;

    for (index, character) in content.char_indices() {
        for offset in &mut offsets[index..index + character.len_utf8()] {
            *offset = text.len();
        }
        match character {
            '\t' if show_whitespace => {
                text.push('\u{2192}');
                column += 1;
            }
            '\t' => {
                let width = tab_width - column % tab_width;
                text.extend(std::iter::repeat_n(' ', width));
                column += width;
            }
            ' ' if show_whitespace => {
                text.push('\u{00b7}');
                column += 1;
            }
            _ => {
                text.push(character);
                column += 1;
            }
        }
    }
    offsets[content.len()] = text.len();
    (text, offsets)
}

fn remap(range: &Range<usize>, offsets: &[usize]) -> Range<usize> {
    let map = |index: usize| offsets[index.min(offsets.len() - 1)];
    map(range.start)..map(range.end)
}

/// Splits a span into whitespace markers and ordinary text, so the markers can
/// be drawn dimmed. `markers` is false when whitespace is shown as itself.
fn split_markers(text: &str, markers: bool) -> Vec<(&str, bool)> {
    if !markers || text.is_empty() {
        return vec![(text, false)];
    }

    let mut runs = Vec::new();
    let mut start = 0;
    let mut current = None;
    for (index, character) in text.char_indices() {
        let is_marker = matches!(character, '\u{00b7}' | '\u{2192}');
        match current {
            Some(mode) if mode == is_marker => {}
            Some(mode) => {
                runs.push((&text[start..index], mode));
                start = index;
                current = Some(is_marker);
            }
            None => current = Some(is_marker),
        }
    }
    if let Some(mode) = current {
        runs.push((&text[start..], mode));
    }
    runs
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
    highlights: Option<&[(Range<usize>, HighlightKind)]>,
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

    fn line(kind: LineKind, old: Option<u32>, new: Option<u32>, content: &str) -> Row {
        Row::Line(DiffLine {
            kind,
            old_lineno: old,
            new_lineno: new,
            content: content.to_owned(),
            no_newline_at_eof: false,
            emphasis: Vec::new(),
        })
    }

    fn header() -> Row {
        Row::HunkHeader {
            old_start: 1,
            old_len: 2,
            new_start: 1,
            new_len: 2,
        }
    }

    #[test]
    fn rows_cover_every_line() {
        let diff = fake::sample_diff(1);
        let total: usize = diff.hunks.iter().map(|hunk| hunk.lines.len()).sum();
        assert_eq!(flatten(&diff).len(), total + diff.hunks.len());
    }

    #[test]
    fn split_pairs_removed_with_added() {
        let rows = vec![
            header(),
            line(LineKind::Context, Some(1), Some(1), "a"),
            line(LineKind::Removed, Some(2), None, "b"),
            line(LineKind::Removed, Some(3), None, "c"),
            line(LineKind::Added, None, Some(2), "B"),
            line(LineKind::Added, None, Some(3), "C"),
            line(LineKind::Context, Some(4), Some(4), "d"),
        ];
        assert_eq!(
            split_rows(&rows),
            vec![
                SplitRow::Header { row: 0 },
                SplitRow::Lines {
                    old: Some(1),
                    new: Some(1)
                },
                SplitRow::Lines {
                    old: Some(2),
                    new: Some(4)
                },
                SplitRow::Lines {
                    old: Some(3),
                    new: Some(5)
                },
                SplitRow::Lines {
                    old: Some(6),
                    new: Some(6)
                },
            ]
        );
    }

    #[test]
    fn split_pairs_a_longer_side_with_empty_counterparts() {
        // Three removals, one addition: the change block is three rows, the
        // last two with an empty new side.
        let rows = vec![
            header(),
            line(LineKind::Removed, Some(1), None, "a"),
            line(LineKind::Removed, Some(2), None, "b"),
            line(LineKind::Removed, Some(3), None, "c"),
            line(LineKind::Added, None, Some(1), "A"),
        ];
        assert_eq!(
            split_rows(&rows),
            vec![
                SplitRow::Header { row: 0 },
                SplitRow::Lines {
                    old: Some(1),
                    new: Some(4)
                },
                SplitRow::Lines {
                    old: Some(2),
                    new: None
                },
                SplitRow::Lines {
                    old: Some(3),
                    new: None
                },
            ]
        );
    }

    #[test]
    fn split_handles_pure_additions_and_deletions() {
        let added = vec![
            header(),
            line(LineKind::Added, None, Some(1), "a"),
            line(LineKind::Added, None, Some(2), "b"),
        ];
        assert_eq!(
            split_rows(&added),
            vec![
                SplitRow::Header { row: 0 },
                SplitRow::Lines {
                    old: None,
                    new: Some(1)
                },
                SplitRow::Lines {
                    old: None,
                    new: Some(2)
                },
            ]
        );

        let removed = vec![
            header(),
            line(LineKind::Removed, Some(1), None, "a"),
            line(LineKind::Removed, Some(2), None, "b"),
        ];
        assert_eq!(
            split_rows(&removed),
            vec![
                SplitRow::Header { row: 0 },
                SplitRow::Lines {
                    old: Some(1),
                    new: None
                },
                SplitRow::Lines {
                    old: Some(2),
                    new: None
                },
            ]
        );
    }

    #[test]
    fn split_starts_a_new_block_at_each_hunk_header() {
        let rows = vec![
            header(),
            line(LineKind::Removed, Some(1), None, "a"),
            header(),
            line(LineKind::Added, None, Some(1), "b"),
        ];
        assert_eq!(
            split_rows(&rows),
            vec![
                SplitRow::Header { row: 0 },
                SplitRow::Lines {
                    old: Some(1),
                    new: None
                },
                SplitRow::Header { row: 2 },
                SplitRow::Lines {
                    old: None,
                    new: Some(3)
                },
            ]
        );
    }

    #[test]
    fn a_uniform_list_needs_one_row_per_line_and_split_is_shorter() {
        // The split list reuses the unified item count, so it can only be
        // shorter; this is what leaves the tail rows empty.
        let rows = vec![
            header(),
            line(LineKind::Removed, Some(1), None, "a"),
            line(LineKind::Removed, Some(2), None, "b"),
            line(LineKind::Added, None, Some(1), "c"),
        ];
        assert!(split_rows(&rows).len() <= rows.len());
    }

    #[test]
    fn tabs_expand_to_the_next_tab_stop() {
        let (text, offsets) = display_text("a\tb", 4, false);
        assert_eq!(text, "a   b");
        assert_eq!(offsets[2], 4, "the character after the tab moved to 4");
        assert_eq!(
            offsets[1], 1,
            "the tab itself starts after the first column"
        );

        let (text, _) = display_text("\tx", 8, false);
        assert_eq!(text, "        x");
    }

    #[test]
    fn visible_whitespace_replaces_spaces_and_tabs() {
        let (text, offsets) = display_text("a b\tc", 2, true);
        assert_eq!(text, "a\u{00b7}b\u{2192}c");
        assert_eq!(offsets[3], 4, "offsets follow the wider markers");
    }

    #[test]
    fn emphasis_survives_tab_expansion() {
        let mut line = DiffLine {
            kind: LineKind::Added,
            old_lineno: None,
            new_lineno: Some(1),
            content: "a\tb".to_owned(),
            no_newline_at_eof: false,
            emphasis: vec![Range { start: 2, end: 3 }],
        };
        let style = DiffStyle {
            mono: SharedString::default(),
            font_size: 12.5,
            tab_width: 4,
            split: false,
            wrap: false,
            syntax: true,
            show_whitespace: false,
        };
        let drawn = display(&line, &style, None);
        assert_eq!(drawn.text, "a   b");
        assert_eq!(drawn.emphasis, vec![Range { start: 4, end: 5 }]);

        // The emphasis covers exactly the `b`, which is the last run.
        let pieces = segments(&drawn.text, &drawn.emphasis, None);
        assert_eq!(
            plain(&pieces),
            vec![("a   ", None, false), ("b", None, true)]
        );

        line.emphasis.clear();
    }

    #[test]
    fn whitespace_markers_are_split_into_their_own_runs() {
        assert_eq!(
            split_markers("a\u{00b7}b", true),
            vec![("a", false), ("\u{00b7}", true), ("b", false)]
        );
        assert_eq!(split_markers("plain", false), vec![("plain", false)]);
        assert_eq!(
            split_markers("\u{2192}\u{2192}", true),
            vec![("\u{2192}\u{2192}", true)]
        );
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
        let pieces = segments("let x = 1;", &[], Some(highlights.as_slice()));

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
            Some(highlights.as_slice()),
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
        let pieces = segments(
            "héllo",
            &[Range { start: 3, end: 999 }],
            Some(highlights.as_slice()),
        );

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

    #[test]
    fn rows_grow_with_a_larger_code_font() {
        let style = DiffStyle {
            mono: SharedString::default(),
            font_size: 22.0,
            tab_width: 4,
            split: false,
            wrap: false,
            syntax: true,
            show_whitespace: false,
        };
        assert!(style.row_height() > ROW_HEIGHT);
        assert_eq!(
            style.clone().one_line_per_row().row_height(),
            style.row_height()
        );
    }
}
