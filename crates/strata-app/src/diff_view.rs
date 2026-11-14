//! The diff pane: diff rows rendered with the user's display settings.
//!
//! `Hunk`/`DiffLine` come from `strata-core`; this module flattens them into
//! rows and paints line numbers, `+`/`-` backgrounds, word-level emphasis and
//! syntax colours. Every pane that shows a diff goes through [`diff_list`], so
//! the diff preferences in [`crate::settings`] (unified or side-by-side,
//! wrapping, syntax colours, visible whitespace, tab width and the code font)
//! apply everywhere at once.
//!
//! Each hunk is drawn as a card: the header row is its rounded top edge, the
//! line rows continue its sides and the hunk's last line closes it. The gap
//! above a card lives in the header row's top padding, so the list stays a
//! `uniform_list` of fixed-height rows.
//!
//! Nothing here reads git: the panes hand over rows and the [`DiffView`] they
//! came from.

use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, BorrowAppContext as _, Div, Global, HighlightStyle, Hsla, IntoElement,
    ListAlignment, ListOffset, ListState, ParentElement, SharedString, Styled, StyledText,
    UniformListScrollHandle, Window, div, list, prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_component::scroll::Scrollbar;
use gpui_component::theme::Theme;
use strata_core::diff::{
    DiffLine, Expansion, FileDiff, Gap, GapAction, LineKind, REVEAL_STEP, gap_view,
    within_expand_cap,
};
use strata_core::highlight::{HighlightKind, LineHighlights};
use strata_core::session::DiffView;

use crate::scrollbar::WithScrollbar as _;
use crate::settings::{DiffLayout, Settings};
use crate::style::{CODE_FONT_MAX, CODE_FONT_MIN, Design};

/// Height of one diff row at the default code font size.
pub const ROW_HEIGHT: f32 = 20.0;
const GUTTER_WIDTH: f32 = 46.0;
const PICK_WIDTH: f32 = 14.0;
const MARKER_WIDTH: f32 = 14.0;
/// How far a hunk card is inset from the pane's edges.
const CARD_INSET: f32 = 8.0;
/// The empty band above a card. It is drawn inside the hunk header row,
/// because every row of a `uniform_list` is the same fixed height.
const CARD_GAP: f32 = 8.0;

/// The card edges a drawn row draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowRole {
    /// The hunk header: the card's rounded top edge. The row also carries the
    /// gap above the card in its top padding.
    Header,
    /// A line row that continues the card's sides.
    Middle,
    /// The hunk's last line row: the card's bottom edge and rounded corners.
    Tail,
}

/// The role of a row in the unified stream.
///
/// Every hunk starts with its header, so a line row that is followed by
/// anything but another line row closes its card.
fn row_role(rows: &[Row], index: usize) -> Option<RowRole> {
    match rows.get(index)? {
        Row::HunkHeader { .. } => Some(RowRole::Header),
        // A gap bar and the lines revealed around it are outside the cards,
        // like the gap above a header.
        Row::Gap { .. } | Row::Context(_) => None,
        Row::Line(_) => Some(if matches!(rows.get(index + 1), Some(Row::Line(_))) {
            RowRole::Middle
        } else {
            RowRole::Tail
        }),
    }
}

/// The role of an item in the side-by-side stream, which pairs the same rows
/// into one list item per change row.
fn split_role(pairs: &[SplitRow], index: usize) -> Option<RowRole> {
    match pairs.get(index)? {
        SplitRow::Header { .. } => Some(RowRole::Header),
        SplitRow::Gap { .. } => None,
        SplitRow::Lines { .. } => Some(
            if matches!(pairs.get(index + 1), Some(SplitRow::Lines { .. })) {
                RowRole::Middle
            } else {
                RowRole::Tail
            },
        ),
    }
}

/// The old and new side of a side-by-side diff.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Old,
    New,
}

/// A flat row in the diff list. Hunk headers share the line height so an
/// unwrapped list can stay a `uniform_list`.
pub enum Row {
    HunkHeader {
        old_start: u32,
        old_len: u32,
        new_start: u32,
        new_len: u32,
    },
    Line(DiffLine),
    /// A context line revealed out of a gap. It is not part of a hunk card, so
    /// it is drawn without the card's edges.
    Context(DiffLine),
    /// The still-hidden part of a gap, drawn as a slim bar that offers to show
    /// more of it. `gap` indexes [`DiffView::gaps`].
    Gap {
        gap: usize,
        hidden: u32,
    },
}

/// What a gap bar's buttons do for one diff.
pub type GapHandler = Rc<dyn Fn(usize, GapAction, &mut Window, &mut App)>;

/// How much of a diff's hidden context is on screen, for one file of one diff.
///
/// A pane owns one and resets it when the selection changes, so every file
/// starts collapsed again. The rows are rebuilt from the diff the view already
/// holds: revealing context never runs the diff a second time.
#[derive(Clone, Default)]
pub struct DiffContext {
    expansion: Expansion,
    notice: Option<String>,
    /// Bumped whenever the rows the context describes change, so a rebuild
    /// that finishes late cannot replace a newer one.
    revision: u64,
}

/// Shown when a file is past the cap instead of building rows for all of it.
fn too_large() -> String {
    t!("diff.too_large").to_string()
}

impl DiffContext {
    /// Back to no revealed gap and no whole-file view.
    pub fn reset(&mut self) {
        let revision = self.next_revision();
        *self = Self {
            revision,
            ..Self::default()
        };
    }

    fn next_revision(&self) -> u64 {
        self.revision.wrapping_add(1)
    }

    /// Identifies the expansion a rebuild was started for.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_whole_file(&self) -> bool {
        self.expansion.is_whole_file()
    }

    /// The raw reveals, for a pane that builds its own rows.
    pub fn expansion(&self) -> &Expansion {
        &self.expansion
    }

    /// Why the last whole-file or reveal-all request was refused, if it was.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Applies one press on a gap bar; true when the rows must be rebuilt.
    pub fn expand(&mut self, gap: usize, action: GapAction, view: &DiffView) -> bool {
        let Some(run) = view.gaps.get(gap).filter(|gap| gap.len > 0) else {
            return false;
        };
        if action == GapAction::All && !within_expand_cap(&view.old_text, &view.new_text) {
            self.notice = Some(too_large());
            return false;
        }
        self.notice = None;
        self.expansion.expand(gap, action, run.len);
        self.revision = self.next_revision();
        true
    }

    /// Switches the whole-file view; true when the rows must be rebuilt.
    ///
    /// A file past the cap keeps its diff and shows [`Self::notice`] instead:
    /// building every row of a multi-megabyte file would stall a frame.
    pub fn toggle_whole_file(&mut self, view: &DiffView) -> bool {
        if self.expansion.is_whole_file() {
            self.expansion.set_whole_file(false);
            self.notice = None;
            self.revision = self.next_revision();
            return true;
        }
        if !within_expand_cap(&view.old_text, &view.new_text) {
            self.notice = Some(too_large());
            return false;
        }
        self.expansion.set_whole_file(true);
        self.notice = None;
        self.revision = self.next_revision();
        true
    }

    /// The rows this expansion shows for `view`.
    pub fn rows(&self, view: &DiffView) -> Vec<Row> {
        flatten_expanded(
            &view.diff,
            &view.old_text,
            &view.new_text,
            &view.gaps,
            &self.expansion,
        )
    }
}

pub fn flatten(diff: &FileDiff) -> Vec<Row> {
    flatten_expanded(diff, "", "", &[], &Expansion::default())
}

/// The diff's rows with the expanded context of `expansion` filled in.
///
/// `gaps` is [`DiffView::gaps`]: one entry before each hunk and one after the
/// last one.
pub fn flatten_expanded(
    diff: &FileDiff,
    old: &str,
    new: &str,
    gaps: &[Gap],
    expansion: &Expansion,
) -> Vec<Row> {
    let mut rows = Vec::with_capacity(diff.hunks.iter().map(|h| h.lines.len() + 1).sum());
    for (index, hunk) in diff.hunks.iter().enumerate() {
        rows.extend(gap_rows(old, new, gaps.get(index), index, expansion));
        rows.push(Row::HunkHeader {
            old_start: hunk.old_start,
            old_len: hunk.old_len,
            new_start: hunk.new_start,
            new_len: hunk.new_len,
        });
        rows.extend(hunk.lines.iter().cloned().map(Row::Line));
    }
    rows.extend(gap_rows(
        old,
        new,
        gaps.get(diff.hunks.len()),
        diff.hunks.len(),
        expansion,
    ));
    rows
}

/// The rows one gap contributes: the lines revealed above the bar, the bar
/// itself when lines remain hidden, then the lines revealed below it.
pub fn gap_rows(
    old: &str,
    new: &str,
    gap: Option<&Gap>,
    index: usize,
    expansion: &Expansion,
) -> Vec<Row> {
    let Some(gap) = gap.filter(|gap| gap.len > 0) else {
        return Vec::new();
    };
    let view = gap_view(old, new, gap, expansion.reveal(index, gap.len));
    let mut rows = Vec::with_capacity(view.above.len() + view.below.len() + 1);
    rows.extend(view.above.into_iter().map(Row::Context));
    if view.hidden > 0 {
        rows.push(Row::Gap {
            gap: index,
            hidden: view.hidden,
        });
    }
    rows.extend(view.below.into_iter().map(Row::Context));
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
    /// A gap bar, which spans both columns.
    Gap {
        row: usize,
    },
}

/// Aligns the unified row stream into side-by-side rows, one per hunk.
///
/// Side-by-side is shorter than the unified stream whenever a hunk removes
/// more lines than it adds or the other way round, so [`diff_list`] asks for
/// one list item per pair rather than per unified row.
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
            Row::Gap { .. } => {
                flush_block(&mut removed, &mut added, &mut out);
                out.push(SplitRow::Gap { row: index });
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
            Row::Context(_) => {
                flush_block(&mut removed, &mut added, &mut out);
                out.push(SplitRow::Lines {
                    old: Some(index),
                    new: Some(index),
                });
            }
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
    /// The band behind an added or removed line.
    pub added: Hsla,
    pub removed: Hsla,
    /// The stronger band behind the changed words inside that line.
    pub emphasis_added: Hsla,
    pub emphasis_removed: Hsla,
    /// The band behind a hunk header.
    pub header: Hsla,
    /// Line numbers and the `+`/`-` marker column.
    pub gutter: Hsla,
    /// The `·` and `→` markers that reveal whitespace.
    pub whitespace: Hsla,
    /// Accent for the lines picked for a partial stage or discard.
    pub selection: Hsla,
    /// One colour per [`HighlightKind`], indexed by its discriminant.
    syntax: [Hsla; 16],
}

impl DiffColors {
    /// The diff's colours, from the app's design palette.
    ///
    /// Only the colour half of [`Design`] is read: the sizes and flags it also
    /// carries never reach a diff row, so the defaults are enough to resolve
    /// the light or dark table from `theme`. Panes that already hold a
    /// [`Design`] can call [`Self::from_design`] instead.
    pub fn from_theme(theme: &Theme) -> Self {
        Self::from_design(&Design::new(theme, &Settings::default()))
    }

    pub fn from_design(design: &Design) -> Self {
        // The order matches the `HighlightKind` variants; a comment on the
        // first one alone would rot, so keep this list beside the enum's
        // definition.
        let syntax = [
            design.syntax(HighlightKind::Keyword),
            design.syntax(HighlightKind::String),
            design.syntax(HighlightKind::Comment),
            design.syntax(HighlightKind::Function),
            design.syntax(HighlightKind::Type),
            design.syntax(HighlightKind::Constant),
            design.syntax(HighlightKind::Number),
            design.syntax(HighlightKind::Variable),
            design.syntax(HighlightKind::Property),
            design.syntax(HighlightKind::Operator),
            design.syntax(HighlightKind::Punctuation),
            design.syntax(HighlightKind::Attribute),
            design.syntax(HighlightKind::Tag),
            design.syntax(HighlightKind::Label),
            design.syntax(HighlightKind::Escape),
            design.syntax(HighlightKind::Embedded),
        ];
        Self {
            text: design.text,
            muted: design.muted,
            border: design.border,
            added: design.diff_added_bg,
            removed: design.diff_removed_bg,
            emphasis_added: design.diff_added_word,
            emphasis_removed: design.diff_removed_word,
            header: design.diff_hunk_header,
            gutter: design.diff_gutter_fg,
            whitespace: design.diff_whitespace,
            selection: design.accent,
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
    /// Keeps context revealed out of a gap aligned with the hunk cards around
    /// it. The staging pane draws its rows flush and leaves this off.
    pub inset: bool,
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
            font_size: settings
                .appearance
                .code_font_size
                .clamp(CODE_FONT_MIN, CODE_FONT_MAX),
            // The settings page offers 1–8 columns. A hand-edited file may ask
            // for more, and there is no reason to refuse it, so only zero (which
            // has no tab stop) is ruled out here.
            tab_width: usize::from(settings.appearance.tab_width.clamp(1, 16)),
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

    /// The code font, for a pane that draws rows of its own (the conflict view).
    pub fn mono(&self) -> &SharedString {
        &self.mono
    }

    /// The code font size, for a pane that draws rows of its own.
    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    /// Whether syntax spans are painted, for a pane that builds its own runs.
    pub fn shows_syntax(&self) -> bool {
        self.syntax
    }

    /// What changes how a wrapped line breaks: the font, its size, the tab
    /// width and whether whitespace markers take a tab's place. A list that
    /// cached wrapped heights has to measure again when any of them moves.
    fn fingerprint(&self) -> u64 {
        let mut hash = FNV_OFFSET;
        hash_bytes(&mut hash, self.mono.as_bytes());
        hash_bytes(&mut hash, &self.font_size.to_bits().to_le_bytes());
        hash_bytes(&mut hash, &(self.tab_width as u64).to_le_bytes());
        hash_bytes(&mut hash, &[u8::from(self.show_whitespace)]);
        hash
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
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
    unified_row(row, colors, style, highlights, options, false, None)
}

/// The diff pane's list of rows.
///
/// Unwrapped rows all share [`DiffStyle::row_height`], so they stay in the
/// caller's `uniform_list`. A wrapped row is as tall as its text needs, which
/// only gpui's general [`list`] can measure; that list keeps its own state per
/// pane here and remeasures when the rows, the width or the font change.
pub struct DiffList<'a> {
    pub id: &'static str,
    pub rows: Rc<Vec<Row>>,
    pub view: Option<Arc<DiffView>>,
    pub colors: DiffColors,
    pub mono: SharedString,
    pub scroll: &'a UniformListScrollHandle,
    /// Handles the buttons of the hidden-lines bars; a pane that has no
    /// expansion state (the synthetic benchmark) leaves it out.
    pub on_gap: Option<GapHandler>,
}

pub fn diff_list(list: DiffList<'_>, cx: &mut App) -> AnyElement {
    let DiffList {
        id,
        rows,
        view,
        colors,
        mono,
        scroll,
        on_gap,
    } = list;
    let style = DiffStyle::from_settings(cx, &mono);
    let (count, pairs) = list_items(&rows, style.split);
    let source = Rows {
        rows,
        pairs,
        view,
        colors,
        style,
        on_gap,
    };

    if source.style.wrap {
        return wrapped_list(id, source, count, cx);
    }

    uniform_list(id, count, move |range: Range<usize>, _window, _cx| {
        range
            .map(|index| source.element(index, false))
            .collect::<Vec<_>>()
    })
    .track_scroll(scroll)
    .with_scrollbar(scroll)
    .flex_1()
    .min_h_0()
    .into_any_element()
}

/// The rows of one diff and everything they need to be drawn, shared by the
/// items of whichever list the layout asks for.
struct Rows {
    rows: Rc<Vec<Row>>,
    pairs: Option<Rc<[SplitRow]>>,
    view: Option<Arc<DiffView>>,
    colors: DiffColors,
    style: DiffStyle,
    on_gap: Option<GapHandler>,
}

impl Rows {
    fn element(&self, index: usize, wrap: bool) -> AnyElement {
        let Self {
            rows,
            pairs,
            view,
            colors,
            style,
            on_gap,
        } = self;
        if let Some(pairs) = pairs.as_deref() {
            return match pairs.get(index) {
                Some(pair) => split_row(
                    *pair,
                    rows,
                    colors,
                    style,
                    view.as_deref(),
                    wrap,
                    split_role(pairs, index),
                    on_gap.as_ref(),
                ),
                // The item count is the pairing's length, so this only happens
                // for an index from a stale count.
                None => div().h(px(style.row_height())).w_full().into_any_element(),
            };
        }

        let Some(row) = rows.get(index) else {
            return div().into_any_element();
        };
        if let Row::Gap { gap, hidden } = row {
            return gap_bar(*gap, *hidden, colors, style, on_gap.as_ref());
        }
        let highlights = line_highlights(row, view.as_deref(), style.syntax);
        unified_row(
            row,
            colors,
            style,
            highlights,
            RowOptions {
                inset: true,
                ..RowOptions::default()
            },
            wrap,
            row_role(rows, index),
        )
    }
}

/// What one frame draws: the number of list items and, in side-by-side, the
/// pairing that maps each item to its two rows.
///
/// The item count is the number of drawn rows; using the unified count in
/// side-by-side mode leaves the list with blank rows at the end.
fn list_items(rows: &[Row], split: bool) -> (usize, Option<Rc<[SplitRow]>>) {
    if split {
        let pairs: Rc<[SplitRow]> = Rc::from(split_rows(rows));
        (pairs.len(), Some(pairs))
    } else {
        (rows.len(), None)
    }
}

/// The wrapped diff as a [`list`], which measures each row and only lays out
/// the ones near the viewport.
fn wrapped_list(id: &'static str, rows: Rows, count: usize, cx: &mut App) -> AnyElement {
    let state = wrapped_state(
        id,
        count,
        rows_fingerprint(&rows.rows),
        rows.style.fingerprint(),
        &rows.style,
        cx,
    );

    // `list` is the scroll container itself and has no children, so the
    // overlay scrollbar goes beside it in a positioned container rather than
    // through [`WithScrollbar`], which takes a `uniform_list` handle.
    div()
        .relative()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .child(
            list(state.clone(), move |index, _window, _cx| {
                rows.element(index, true)
            })
            .size_full(),
        )
        .child(Scrollbar::vertical(&state).id(id))
        .into_any_element()
}

/// One wrapped list per pane.
///
/// The panes hold a `UniformListScrollHandle`, which a measured list cannot
/// use, so the wrapped list's state lives here, keyed by the pane's list id.
#[derive(Default)]
struct WrappedLists(HashMap<&'static str, WrappedList>);

impl Global for WrappedLists {}

struct WrappedList {
    state: ListState,
    /// The item count the state was built for; a layout change makes it stale.
    items: usize,
    /// Fingerprints of the rows and of the style the cached heights belong to.
    content: u64,
    style: u64,
}

impl WrappedList {
    fn new(items: usize, content: u64, style: u64, row_height: f32) -> Self {
        let state = ListState::new(items, ListAlignment::Top, px(WRAP_OVERDRAW))
            .with_uniform_item_height(px(row_height));
        Self {
            state,
            items,
            content,
            style,
        }
    }

    /// Brings the state up to date with this frame.
    ///
    /// gpui only re-measures the rows it paints, so a row that is off screen
    /// keeps the height it had when it was last seen. New rows, another font
    /// or another tab width therefore invalidate every cached height, while
    /// the scroll position the user had stays put.
    fn sync(&mut self, items: usize, content: u64, style: u64, row_height: f32) {
        if self.items != items {
            let top = self.state.logical_scroll_top();
            self.state.reset_with_uniform_height(items, px(row_height));
            self.items = items;
            self.state.scroll_to(ListOffset {
                item_ix: top.item_ix.min(items),
                offset_in_item: top.offset_in_item,
            });
        }
        if self.content != content || self.style != style {
            self.content = content;
            self.style = style;
            self.state.remeasure();
        }
    }
}

fn wrapped_state(
    id: &'static str,
    items: usize,
    content: u64,
    style: u64,
    diff_style: &DiffStyle,
    cx: &mut App,
) -> ListState {
    if !cx.has_global::<WrappedLists>() {
        cx.set_global(WrappedLists::default());
    }
    cx.update_global::<WrappedLists, _>(|lists, _| {
        let entry = lists
            .0
            .entry(id)
            .or_insert_with(|| WrappedList::new(items, content, style, diff_style.row_height()));
        entry.sync(items, content, style, diff_style.row_height());
        entry.state.clone()
    })
}

/// How far past the viewport the wrapped list measures rows, so scrolling does
/// not reveal an unmeasured row at the edge.
const WRAP_OVERDRAW: f32 = 240.0;

/// How many rows apart the wrapped list samples a diff to tell it apart from
/// the one it measured before; a prime keeps the sample off hunk boundaries.
const FINGERPRINT_STEP: usize = 61;

/// A cheap identity for the rows behind a wrapped list.
///
/// The state has to know when its cached heights describe another diff. Hashing
/// every line per frame would cost more than it saves, so the row count, a
/// sample of the rows and the last row are enough to see a change.
fn rows_fingerprint(rows: &[Row]) -> u64 {
    let mut hash = FNV_OFFSET;
    hash_bytes(&mut hash, &(rows.len() as u64).to_le_bytes());
    for row in rows.iter().step_by(FINGERPRINT_STEP) {
        hash_row(&mut hash, row);
    }
    if let Some(last) = rows.last() {
        hash_row(&mut hash, last);
    }
    hash
}

fn hash_row(hash: &mut u64, row: &Row) {
    match row {
        Row::HunkHeader {
            old_start,
            old_len,
            new_start,
            new_len,
        } => {
            for number in [*old_start, *old_len, *new_start, *new_len] {
                hash_bytes(hash, &number.to_le_bytes());
            }
        }
        Row::Line(line) | Row::Context(line) => {
            hash_bytes(hash, &[row_kind(line.kind)]);
            hash_bytes(hash, &(line.content.len() as u64).to_le_bytes());
            // A long line's tail rarely decides whether two diffs differ, and
            // hashing it in full would make the fingerprint scale with the file.
            let bytes = line.content.as_bytes();
            hash_bytes(hash, &bytes[..bytes.len().min(48)]);
        }
        Row::Gap { gap, hidden } => {
            hash_bytes(hash, &(*gap as u64).to_le_bytes());
            hash_bytes(hash, &hidden.to_le_bytes());
        }
    }
}

fn row_kind(kind: LineKind) -> u8 {
    match kind {
        LineKind::Context => 0,
        LineKind::Added => 1,
        LineKind::Removed => 2,
    }
}

/// The syntax spans of the line a row shows, from the view that produced it.
fn line_highlights<'a>(
    row: &Row,
    view: Option<&'a DiffView>,
    syntax: bool,
) -> Option<&'a LineHighlights> {
    let view = view.filter(|_| syntax)?;
    let (Row::Line(line) | Row::Context(line)) = row else {
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
    role: Option<RowRole>,
) -> AnyElement {
    match row {
        Row::HunkHeader {
            old_start,
            old_len,
            new_start,
            new_len,
        } => hunk_header((*old_start, *old_len, *new_start, *new_len), colors, style),
        Row::Gap { gap, hidden } => gap_bar(*gap, *hidden, colors, style, None),
        Row::Line(line) | Row::Context(line) => {
            let picked = options.pick == Some(true);
            let mut outer = div()
                .flex()
                .min_h(px(style.row_height()))
                .w_full()
                .font_family(style.mono.clone())
                .text_size(px(style.font_size))
                .text_color(colors.text);
            // Context revealed out of a gap has no card, but it sits among the
            // cards and must keep their gutter alignment.
            if role.is_some() || (options.inset && matches!(row, Row::Context(_))) {
                outer = outer.pl(px(CARD_INSET)).pr(px(CARD_INSET));
            }
            outer = if wrap {
                outer.items_start()
            } else {
                outer.items_center().h(px(style.row_height()))
            };

            // The tint and the gutters live in this box, inside the card's
            // border. Without a role (the staging pane) it is the whole row.
            let mut body = div().flex().flex_1().min_w_0().w_full();
            body = if wrap {
                body.items_start().min_h(px(style.row_height()))
            } else {
                body.items_center().h_full()
            };

            match role {
                Some(role) => {
                    body = body.border_l_1().border_r_1().border_color(colors.border);
                    if role == RowRole::Tail {
                        body = body.border_b_1().rounded_b_sm();
                    }
                    // The pick accent stays inside the card so the card's own
                    // left border runs unbroken along the hunk.
                    body = body.child(div().flex_none().w(px(2.0)).self_stretch().bg(if picked {
                        colors.selection
                    } else {
                        Hsla::transparent_black()
                    }));
                }
                None => {
                    body = body.border_l_2().border_color(if picked {
                        colors.selection
                    } else {
                        Hsla::transparent_black()
                    });
                }
            }

            if let Some(background) = line_background(line, colors) {
                body = body.bg(background);
            } else if picked {
                body = body.bg(colors.selection.opacity(0.14));
            }

            outer
                .child(
                    body.child(
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
                            .text_color(colors.gutter)
                            .child(marker(line.kind)),
                    )
                    .child(line_content(line, colors, style, highlights, wrap)),
                )
                .into_any_element()
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one call per side-by-side item; a struct would only rename the fields"
)]
fn split_row(
    pair: SplitRow,
    rows: &[Row],
    colors: &DiffColors,
    style: &DiffStyle,
    view: Option<&DiffView>,
    wrap: bool,
    role: Option<RowRole>,
    on_gap: Option<&GapHandler>,
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
        SplitRow::Gap { row } => match rows.get(row) {
            Some(Row::Gap { gap, hidden }) => gap_bar(*gap, *hidden, colors, style, on_gap),
            _ => div().into_any_element(),
        },
        SplitRow::Lines { old, new } => {
            let old_line = old.and_then(|index| line_at(rows, index));
            let new_line = new.and_then(|index| line_at(rows, index));
            let old_highlights =
                old_line.and_then(|line| side_highlights(view, Side::Old, line, style.syntax));
            let new_highlights =
                new_line.and_then(|line| side_highlights(view, Side::New, line, style.syntax));
            let tail = role == Some(RowRole::Tail);
            // Lines revealed out of a gap are not in a hunk card: they keep
            // the gutter and divider but none of the card's outline.
            let loose = old.is_some_and(|index| matches!(rows.get(index), Some(Row::Context(_))));

            let mut outer = div()
                .flex()
                .w_full()
                .min_h(px(style.row_height()))
                .font_family(style.mono.clone())
                .text_size(px(style.font_size))
                .text_color(colors.text)
                .pl(px(CARD_INSET))
                .pr(px(CARD_INSET));
            outer = if wrap {
                outer.items_start()
            } else {
                outer.items_center().h(px(style.row_height()))
            };

            let mut card = div().flex().flex_1().min_w_0().w_full();
            if !loose {
                card = card.border_l_1().border_r_1().border_color(colors.border);
            }
            if tail {
                card = card.border_b_1().rounded_b_sm();
            }
            card = if wrap {
                card.items_start()
            } else {
                card.items_center().h_full()
            };

            outer
                .child(
                    card.child(side_cell(
                        old_line,
                        Side::Old,
                        colors,
                        style,
                        old_highlights,
                        wrap,
                        tail,
                    ))
                    .child(side_cell(
                        new_line,
                        Side::New,
                        colors,
                        style,
                        new_highlights,
                        wrap,
                        tail,
                    )),
                )
                .into_any_element()
        }
    }
}

fn line_at(rows: &[Row], index: usize) -> Option<&DiffLine> {
    match rows.get(index) {
        Some(Row::Line(line) | Row::Context(line)) => Some(line),
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
    tail: bool,
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
        .border_l_2()
        .border_color(match side {
            // The divider between the columns, drawn on the new side only.
            Side::Old => Hsla::transparent_black(),
            Side::New => colors.border,
        });
    cell = if wrap {
        cell.items_start().min_h(px(style.row_height()))
    } else {
        cell.items_center().h_full()
    };
    if tail {
        // The tint is painted by the cell, so the card's rounded bottom
        // corners have to round the cell's own background too.
        cell = match side {
            Side::Old => cell.rounded_bl_sm(),
            Side::New => cell.rounded_br_sm(),
        };
    }
    if let Some(background) = background {
        cell = cell.bg(background);
    }

    cell.child(gutter(number, colors))
        .child(
            div()
                .w(px(MARKER_WIDTH))
                .flex_none()
                .text_color(colors.gutter)
                .child(marker),
        )
        .when_some(line, |cell, line| {
            cell.child(line_content(line, colors, style, highlights, wrap))
        })
}

/// The slim bar that stands in for the lines a gap still hides.
///
/// It sits between two hunk cards (or at a file edge), so it is drawn on the
/// pane's background rather than in a card.
pub(crate) fn gap_bar(
    gap: usize,
    hidden: u32,
    colors: &DiffColors,
    style: &DiffStyle,
    on_gap: Option<&GapHandler>,
) -> AnyElement {
    let mut row = div()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(8.0))
        .h(px(style.row_height()))
        .w_full()
        .bg(colors.header)
        .border_t_1()
        .border_b_1()
        .border_color(colors.border)
        .font_family(style.mono.clone())
        .text_size(px((style.font_size - 1.0).max(9.0)))
        .text_color(colors.muted)
        .child(if hidden == 1 {
            t!("diff.hidden.one", count = hidden).to_string()
        } else {
            t!("diff.hidden.other", count = hidden).to_string()
        });
    if let Some(on_gap) = on_gap {
        for (id, label, action, tooltip) in [
            (
                "gap-above",
                format!("↑ {REVEAL_STEP}"),
                GapAction::Above,
                t!("diff.reveal_more_above").to_string(),
            ),
            (
                "gap-below",
                format!("↓ {REVEAL_STEP}"),
                GapAction::Below,
                t!("diff.reveal_more_below").to_string(),
            ),
            (
                "gap-all",
                "All".to_owned(),
                GapAction::All,
                t!("diff.reveal_gap").to_string(),
            ),
        ] {
            let on_gap = Rc::clone(on_gap);
            row = row.child(
                crate::ui::button((id, gap), label)
                    .size(crate::ui::ButtonSize::Tiny)
                    .ghost()
                    .tooltip(tooltip)
                    .on_click(move |_event, window, cx| on_gap(gap, action, window, cx)),
            );
        }
    }
    row.into_any_element()
}

fn hunk_header(
    (old_start, old_len, new_start, new_len): (u32, u32, u32, u32),
    colors: &DiffColors,
    style: &DiffStyle,
) -> AnyElement {
    let text = format!("@@ -{old_start},{old_len} +{new_start},{new_len} @@");
    let band = style.row_height() - CARD_GAP;

    div()
        .flex()
        .flex_col()
        .h(px(style.row_height()))
        .w_full()
        .pl(px(CARD_INSET))
        .pr(px(CARD_INSET))
        .pt(px(CARD_GAP))
        .child(
            div()
                .flex()
                .items_center()
                .flex_1()
                .w_full()
                .bg(colors.header)
                .border_t_1()
                .border_l_1()
                .border_r_1()
                .border_color(colors.border)
                .rounded_t_sm()
                .font_family(style.mono.clone())
                .text_size(px((style.font_size - 1.0).max(9.0)))
                .line_height(px((band - 1.0).max(8.0)))
                .text_color(colors.muted)
                .child(div().pl_2().child(text))
                // Reserved for per-hunk actions.
                .child(div().flex_1()),
        )
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
    let text = styled(drawn, emphasis_bg, colors, style);

    let mut content = div()
        .font_family(style.mono.clone())
        .text_size(px(style.font_size))
        .text_color(colors.text);
    content = if wrap {
        content.flex_1().min_w_0().whitespace_normal()
    } else {
        content.flex_none().whitespace_nowrap()
    };
    content.child(text)
}

/// One line of code drawn with the diff's display settings: the tab width,
/// visible whitespace and syntax colours.
///
/// The diff rows use this for their line text after [`line_content`] has added
/// emphasis; a pane that shows code without changed ranges (the conflict view)
/// calls it directly so both honour the same settings.
pub fn styled_line(
    content: &str,
    style: &DiffStyle,
    highlights: Option<&LineHighlights>,
    colors: &DiffColors,
) -> StyledText {
    let drawn = DrawnLine {
        text: content.to_owned(),
        emphasis: Vec::new(),
        highlights: highlights
            .filter(|_| style.syntax)
            .map(|spans| spans.to_vec()),
    };
    let drawn = rewrite(drawn, style);
    styled(drawn, colors.emphasis_added, colors, style)
}

/// One styled text run per colour or emphasis change.
///
/// A single text element rather than a row of spans is what lets a long line
/// wrap: spans are blocks, and a wrapped block stacks instead of flowing.
fn styled(
    drawn: DrawnLine,
    emphasis_bg: Hsla,
    colors: &DiffColors,
    style: &DiffStyle,
) -> StyledText {
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
                    color: Some(if is_marker { colors.whitespace } else { colour }),
                    background_color: piece.emphasised.then_some(emphasis_bg),
                    ..HighlightStyle::default()
                },
            ));
        }
    }
    StyledText::new(drawn.text).with_highlights(runs)
}

/// One line as it is drawn: the rewritten text, its emphasis ranges and the
/// syntax spans that still apply, all in offsets into `text`.
struct DrawnLine {
    text: String,
    emphasis: Vec<Range<usize>>,
    highlights: Option<Vec<(Range<usize>, HighlightKind)>>,
}

fn display(line: &DiffLine, style: &DiffStyle, highlights: Option<&LineHighlights>) -> DrawnLine {
    let drawn = DrawnLine {
        text: line.content.clone(),
        emphasis: line.emphasis.clone(),
        highlights: highlights.cloned(),
    };
    rewrite(drawn, style)
}

/// Expands tabs and marks whitespace in `drawn`, remapping its ranges.
fn rewrite(drawn: DrawnLine, style: &DiffStyle) -> DrawnLine {
    if !style.show_whitespace && !drawn.text.contains('\t') {
        return drawn;
    }

    let (text, offsets) = display_text(&drawn.text, style.tab_width, style.show_whitespace);
    let emphasis = drawn
        .emphasis
        .iter()
        .map(|range| remap(range, &offsets))
        .collect();
    let highlights = drawn.highlights.as_ref().map(|spans| {
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
        .text_color(colors.gutter);
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
    fn roles_close_each_hunk_on_its_last_line() {
        let rows = vec![
            header(),
            line(LineKind::Context, Some(1), Some(1), "a"),
            line(LineKind::Removed, Some(2), None, "b"),
            line(LineKind::Added, None, Some(2), "B"),
            header(),
            line(LineKind::Added, None, Some(9), "only"),
        ];

        let roles: Vec<_> = (0..rows.len())
            .map(|index| row_role(&rows, index))
            .collect();
        assert_eq!(
            roles,
            vec![
                Some(RowRole::Header),
                Some(RowRole::Middle),
                Some(RowRole::Middle),
                Some(RowRole::Tail),
                Some(RowRole::Header),
                Some(RowRole::Tail),
            ]
        );
        assert_eq!(row_role(&rows, rows.len()), None);
    }

    #[test]
    fn split_roles_close_each_hunk_on_its_last_pair() {
        let rows = vec![
            header(),
            line(LineKind::Removed, Some(1), None, "a"),
            line(LineKind::Added, None, Some(1), "A"),
            line(LineKind::Context, Some(2), Some(2), "b"),
            header(),
            line(LineKind::Added, None, Some(9), "only"),
        ];
        let pairs = split_rows(&rows);

        let roles: Vec<_> = (0..pairs.len())
            .map(|index| split_role(&pairs, index))
            .collect();
        assert_eq!(
            roles,
            vec![
                Some(RowRole::Header),
                Some(RowRole::Middle),
                Some(RowRole::Tail),
                Some(RowRole::Header),
                Some(RowRole::Tail),
            ]
        );
        assert_eq!(split_role(&pairs, pairs.len()), None);
    }

    #[test]
    fn list_items_are_the_rows_each_layout_draws() {
        let rows = vec![
            header(),
            line(LineKind::Removed, Some(1), None, "a"),
            line(LineKind::Removed, Some(2), None, "b"),
            line(LineKind::Added, None, Some(1), "A"),
        ];

        let (unified, pairs) = list_items(&rows, false);
        assert_eq!(unified, rows.len(), "unified draws one item per row");
        assert!(pairs.is_none());

        let (split, pairs) = list_items(&rows, true);
        let pairs = pairs.expect("side-by-side pairs its rows");
        assert_eq!(split, pairs.len());
        assert_eq!(split, 3, "one hunk header and the two paired rows");
        assert!(
            split < rows.len(),
            "the blank tail rows side-by-side used to leave are gone"
        );
    }

    #[test]
    fn the_last_split_item_is_a_drawn_row() {
        // Three removals against one addition: two rows have an empty side,
        // but every item the list asks for is still painted as a row.
        let rows = vec![
            header(),
            line(LineKind::Removed, Some(1), None, "a"),
            line(LineKind::Removed, Some(2), None, "b"),
            line(LineKind::Removed, Some(3), None, "c"),
            line(LineKind::Added, None, Some(1), "A"),
            line(LineKind::Context, Some(4), Some(2), "d"),
        ];
        let (count, pairs) = list_items(&rows, true);
        let pairs = pairs.expect("side-by-side pairs its rows");
        assert_eq!(count, pairs.len());
        assert_eq!(
            pairs.last(),
            Some(&SplitRow::Lines {
                old: Some(5),
                new: Some(5)
            }),
            "the list ends on the last context row, not on an empty item"
        );
    }

    #[test]
    fn the_wrapped_list_sees_row_changes() {
        let rows = Rc::new(vec![line(LineKind::Context, Some(1), Some(1), "alpha")]);
        assert_eq!(rows_fingerprint(&rows), rows_fingerprint(&rows));

        let other = Rc::new(vec![line(LineKind::Context, Some(1), Some(1), "beta")]);
        assert_ne!(
            rows_fingerprint(&rows),
            rows_fingerprint(&other),
            "same length, different text"
        );

        let longer = Rc::new(vec![
            line(LineKind::Context, Some(1), Some(1), "alpha"),
            line(LineKind::Context, Some(2), Some(2), "next"),
        ]);
        assert_ne!(rows_fingerprint(&rows), rows_fingerprint(&longer));
    }

    #[test]
    fn the_wrapped_list_sees_font_and_tab_changes() {
        let style = DiffStyle {
            mono: SharedString::default(),
            font_size: 12.5,
            tab_width: 4,
            split: false,
            wrap: true,
            syntax: true,
            show_whitespace: false,
        };
        let key = style.fingerprint();
        assert_eq!(key, style.clone().fingerprint());

        assert_ne!(
            key,
            DiffStyle {
                font_size: 18.0,
                ..style.clone()
            }
            .fingerprint(),
            "a larger font wraps sooner"
        );
        assert_ne!(
            key,
            DiffStyle {
                tab_width: 9,
                ..style.clone()
            }
            .fingerprint(),
            "a wider tab pushes text further"
        );
        assert_ne!(
            key,
            DiffStyle {
                show_whitespace: true,
                ..style.clone()
            }
            .fingerprint(),
            "markers take the width a tab used to"
        );
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

    #[test]
    fn diff_colours_come_from_the_design_palette() {
        let design = crate::style::Design::default();
        let colors = DiffColors::from_design(&design);

        assert_eq!(colors.text, design.text);
        assert_eq!(colors.muted, design.muted);
        assert_eq!(colors.border, design.border);
        assert_eq!(colors.added, design.diff_added_bg);
        assert_eq!(colors.removed, design.diff_removed_bg);
        assert_eq!(colors.emphasis_added, design.diff_added_word);
        assert_eq!(colors.emphasis_removed, design.diff_removed_word);
        assert_eq!(colors.header, design.diff_hunk_header);
        assert_eq!(colors.gutter, design.diff_gutter_fg);
        assert_eq!(colors.whitespace, design.diff_whitespace);
        assert_eq!(colors.selection, design.accent);

        for kind in [
            HighlightKind::Keyword,
            HighlightKind::String,
            HighlightKind::Comment,
            HighlightKind::Function,
            HighlightKind::Type,
            HighlightKind::Constant,
            HighlightKind::Number,
            HighlightKind::Variable,
            HighlightKind::Property,
            HighlightKind::Operator,
            HighlightKind::Punctuation,
            HighlightKind::Attribute,
            HighlightKind::Tag,
            HighlightKind::Label,
            HighlightKind::Escape,
            HighlightKind::Embedded,
        ] {
            assert_eq!(
                colors.syntax(kind),
                design.syntax(kind),
                "{kind:?} must keep the design's colour"
            );
        }
    }

    #[test]
    fn diff_colours_follow_the_theme() {
        let light = DiffColors::from_theme(&Theme::default());
        let mut dark_theme = Theme::default();
        dark_theme.mode = gpui_component::ThemeMode::Dark;
        let dark = DiffColors::from_theme(&dark_theme);

        assert_ne!(light.added, dark.added);
        assert_ne!(light.removed, dark.removed);
        assert_ne!(light.header, dark.header);
        assert_ne!(
            light.syntax(HighlightKind::String),
            dark.syntax(HighlightKind::String)
        );
    }

    // -----------------------------------------------------------------------
    // Context expansion and the whole-file view.
    // -----------------------------------------------------------------------

    use strata_core::diff::{DiffOptions, gaps, line_count, within_expand_cap};
    use strata_core::session::DiffRequest;

    fn diff_between(old: &str, new: &str, context_lines: u32) -> FileDiff {
        strata_core::diff::diff_text(
            old.as_bytes(),
            new.as_bytes(),
            &DiffOptions {
                context_lines,
                ..DiffOptions::default()
            },
        )
    }

    fn numbered(lines: usize) -> String {
        (1..=lines).map(|line| format!("line {line}\n")).collect()
    }

    fn view_of(old: &str, new: &str) -> DiffView {
        let diff = diff_between(old, new, 3);
        let gaps = gaps(&diff, line_count(old), line_count(new));
        DiffView {
            request: DiffRequest::Staged {
                path: "a.txt".into(),
            },
            diff,
            old_highlights: Arc::new(Vec::new()),
            new_highlights: Arc::new(Vec::new()),
            old_text: Arc::from(old),
            new_text: Arc::from(new),
            old_encoding: strata_core::encoding::TextEncoding::UTF_8,
            new_encoding: strata_core::encoding::TextEncoding::UTF_8,
            gaps: Arc::from(gaps),
            lfs: None,
            image: None,
        }
    }

    fn drawn_lines(rows: &[Row]) -> Vec<&DiffLine> {
        rows.iter()
            .filter_map(|row| match row {
                Row::Line(line) | Row::Context(line) => Some(line),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_gap_bar_sits_between_the_revealed_lines() {
        let old = numbered(60);
        let new = old
            .replace("line 1\n", "FIRST\n")
            .replace("line 60\n", "LAST\n");
        let view = view_of(&old, &new);
        let mut context = DiffContext::default();
        assert!(context.expand(1, GapAction::Above, &view));

        let rows = context.rows(&view);
        let bars: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| matches!(row, Row::Gap { .. }).then_some(index))
            .collect();
        assert_eq!(bars.len(), 1, "one bar for the one gap with much to show");

        let bar = bars[0];
        assert!(
            matches!(rows.get(bar), Some(Row::Gap { gap: 1, hidden: 32 })),
            "52 lines sit between the two hunks, 20 of them now above the bar"
        );
        let above = drawn_lines(&rows[..bar]);
        let revealed = &above[above.len() - REVEAL_STEP as usize..];
        assert_eq!(revealed.first().and_then(|line| line.new_lineno), Some(5));
        assert_eq!(revealed.last().and_then(|line| line.old_lineno), Some(24));
        assert!(revealed.iter().all(|line| line.kind == LineKind::Context));
        assert_eq!(
            drawn_lines(&rows[bar + 1..])
                .first()
                .and_then(|line| line.new_lineno),
            Some(57),
            "nothing is revealed below the bar yet, so the next hunk follows it"
        );
    }

    #[test]
    fn whole_file_is_the_same_as_a_huge_context_diff() {
        let old = numbered(40);
        let new = old
            .replace("line 9\n", "NINE\n")
            .replace("line 30\n", "THIRTY\n");
        let view = view_of(&old, &new);

        let mut context = DiffContext::default();
        assert!(context.toggle_whole_file(&view));
        assert!(context.is_whole_file());
        let rows = context.rows(&view);
        let expanded = drawn_lines(&rows);

        // The same file compared with more context than it has lines: one hunk
        // carrying every line, in the same order.
        let huge = diff_between(&old, &new, u32::MAX);
        let expected: Vec<&DiffLine> = huge.hunks.iter().flat_map(|hunk| &hunk.lines).collect();
        assert_eq!(expanded, expected);

        // The unified rows are a header followed by that stream; the list can
        // switch to side-by-side without losing a line.
        let (count, pairs) = list_items(&rows, true);
        assert!(count > 0);
        assert_eq!(pairs.expect("side-by-side pairs").len(), count);
    }

    #[test]
    fn expanding_below_reveals_the_end_of_the_gap() {
        let old = numbered(60);
        let new = old
            .replace("line 1\n", "FIRST\n")
            .replace("line 60\n", "LAST\n");
        let view = view_of(&old, &new);
        let mut context = DiffContext::default();
        context.expand(1, GapAction::Below, &view);

        let rows = context.rows(&view);
        let bar = rows
            .iter()
            .position(|row| matches!(row, Row::Gap { .. }))
            .expect("most of the gap is still hidden");
        let below: Vec<Option<u32>> = drawn_lines(&rows[bar + 1..])
            .into_iter()
            .map(|line| line.new_lineno)
            .collect();
        assert_eq!(below.first().copied().flatten(), Some(37));
        assert_eq!(
            below.get(REVEAL_STEP as usize - 1).copied().flatten(),
            Some(56),
            "twenty lines are revealed just before the next hunk"
        );

        // Revealing all of it drops the bar and keeps both ends.
        assert!(context.expand(1, GapAction::All, &view));
        let rows = context.rows(&view);
        assert!(!rows.iter().any(|row| matches!(row, Row::Gap { .. })));
        let numbers: Vec<Option<u32>> = drawn_lines(&rows)
            .into_iter()
            .filter(|line| line.kind == LineKind::Context)
            .map(|line| line.new_lineno)
            .collect();
        assert!(numbers.contains(&Some(5)));
        assert!(numbers.contains(&Some(56)));
    }

    #[test]
    fn toggle_whole_file_leaves_a_too_large_file_alone() {
        let big = "x".repeat(strata_core::diff::MAX_EXPAND_BYTES + 1);
        let view = DiffView {
            request: DiffRequest::Staged {
                path: "a.txt".into(),
            },
            diff: FileDiff::default(),
            old_highlights: Arc::new(Vec::new()),
            new_highlights: Arc::new(Vec::new()),
            lfs: None,
            old_text: Arc::from(big.clone()),
            new_text: Arc::from("a\n"),
            old_encoding: strata_core::encoding::TextEncoding::UTF_8,
            new_encoding: strata_core::encoding::TextEncoding::UTF_8,
            gaps: Arc::from(vec![strata_core::diff::Gap {
                old_start: 1,
                new_start: 1,
                len: 1,
            }]),
            image: None,
        };
        assert!(!within_expand_cap(&view.old_text, &view.new_text));

        let mut context = DiffContext::default();
        assert!(
            !context.toggle_whole_file(&view),
            "the rows must not be rebuilt for a file past the cap"
        );
        assert!(!context.is_whole_file());
        assert!(context.notice().is_some());

        assert!(!context.expand(0, GapAction::All, &view));
        assert!(context.notice().is_some());

        // Twenty lines at a time stays cheap whatever the file size.
        assert!(context.expand(0, GapAction::Above, &view));
        assert!(context.notice().is_none());
    }
}
