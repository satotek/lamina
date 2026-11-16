//! Terminal grid element, theme mapping, and scrollbar adapter.
//!
//! Kept in `crate::ui` so painting, font measurement, and palette conversion
//! stay independent of repositories, PTY sessions, and `RepoView`.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    FocusHandle, Font, FontStyle, FontWeight, GlobalElementId, Hsla, InspectorElementId,
    IntoElement, LayoutId, ParentElement as _, Pixels, Point, SharedString, Size,
    StrikethroughStyle, Styled as _, TextAlign, TextRun, UnderlineStyle as GpuiUnderlineStyle,
    Window, div, fill, point, px, relative, size,
};
use gpui_component::scroll::{Scrollbar, ScrollbarHandle};
use strata_term::{
    CellSnapshot, CellWidth, CursorShape, Rgb, ScrollbarSnapshot, TerminalSelection,
    TerminalSnapshot, TerminalTheme, UnderlineStyle,
};

use crate::style::{self, Design};

/// Horizontal padding inside the terminal canvas before column 0.
pub const GRID_PADDING_X: f32 = 8.0;
/// Vertical padding inside the terminal canvas before row 0.
pub const GRID_PADDING_Y: f32 = 6.0;

/// Returns the scaled `(x, y)` canvas padding inside [`TerminalGrid`].
pub fn grid_padding(zoom: f32) -> (f32, f32) {
    (GRID_PADDING_X * zoom, GRID_PADDING_Y * zoom)
}

#[derive(Default)]
struct ScrollbarState {
    viewport_bounds: Bounds<Pixels>,
    cell_height: Pixels,
    snapshot: ScrollbarSnapshot,
}

type ScrollToRowHandler = Box<dyn Fn(usize)>;
/// Callback notified with the terminal canvas's window-space bounds after layout.
pub type BoundsChangedHandler = Box<dyn FnMut(Bounds<Pixels>, &mut App)>;

/// Bridges [`ScrollbarSnapshot`] from `strata-term` to `gpui-component`'s [`ScrollbarHandle`].
///
/// The scrollbar lives alongside [`TerminalGrid`] inside [`terminal_viewport`];
/// dragging its thumb invokes the supplied scroll callback with the target
/// buffer row (`0..=max_offset`).
#[derive(Clone)]
pub struct TerminalScrollbarHandle {
    state: Rc<RefCell<ScrollbarState>>,
    on_scroll_to_row: Rc<RefCell<ScrollToRowHandler>>,
}

impl TerminalScrollbarHandle {
    pub fn new(on_scroll_to_row: impl Fn(usize) + 'static) -> Self {
        Self {
            state: Rc::new(RefCell::new(ScrollbarState::default())),
            on_scroll_to_row: Rc::new(RefCell::new(Box::new(on_scroll_to_row))),
        }
    }

    pub fn set_scroll_handler(&self, on_scroll_to_row: impl Fn(usize) + 'static) {
        *self.on_scroll_to_row.borrow_mut() = Box::new(on_scroll_to_row);
    }

    pub fn sync_snapshot(&self, snapshot: ScrollbarSnapshot) {
        self.state.borrow_mut().snapshot = snapshot;
    }

    pub fn sync_layout(&self, viewport_bounds: Bounds<Pixels>, cell_height: Pixels) {
        let mut state = self.state.borrow_mut();
        state.viewport_bounds = viewport_bounds;
        state.cell_height = cell_height;
    }

    pub fn element_id(&self) -> (&'static str, usize) {
        ("terminal-scrollbar", Rc::as_ptr(&self.state) as usize)
    }
}

impl ScrollbarHandle for TerminalScrollbarHandle {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.state.borrow().viewport_bounds
    }

    fn offset(&self) -> Point<Pixels> {
        let state = self.state.borrow();
        let ch = f32::from(state.cell_height).max(1.0);
        let row = state
            .snapshot
            .viewport_offset
            .min(state.snapshot.max_offset());
        point(px(0.0), px(-(row as f32 * ch)))
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        let target_row = {
            let mut state = self.state.borrow_mut();
            let ch = f32::from(state.cell_height).max(1.0);
            let max = state.snapshot.max_offset();
            let row = ((-f32::from(offset.y)) / ch).round().clamp(0.0, max as f32) as u64;
            state.snapshot.viewport_offset = row;
            row as usize
        };
        (self.on_scroll_to_row.borrow())(target_row);
    }

    fn content_size(&self) -> Size<Pixels> {
        let state = self.state.borrow();
        let ch = f32::from(state.cell_height).max(1.0);
        let extra_height = state.snapshot.max_offset() as f32 * ch;
        size(
            state.viewport_bounds.size.width,
            state.viewport_bounds.size.height + px(extra_height),
        )
    }
}

/// Pure data passed to [`TerminalGrid`] for one paint pass.
pub struct TerminalGridProps<V: EntityInputHandler + 'static> {
    pub snapshot: TerminalSnapshot,
    pub selection: Option<TerminalSelection>,
    pub selection_color: Hsla,
    pub default_cursor_color: Hsla,
    pub ime_bg: Hsla,
    pub ime_fg: Hsla,
    pub font_family: SharedString,
    pub font_size: Pixels,
    pub cell_size: Size<Pixels>,
    pub zoom: f32,
    pub focused: bool,
    pub marked_text: Option<String>,
    pub focus_handle: FocusHandle,
    pub input_entity: Entity<V>,
    pub scrollbar: TerminalScrollbarHandle,
    pub on_bounds_changed: BoundsChangedHandler,
}

/// Monospace canvas element that paints `TerminalSnapshot`, selection highlights,
/// cursor shapes, and inline IME composition text.
pub struct TerminalGrid<V: EntityInputHandler + 'static> {
    props: TerminalGridProps<V>,
}

impl<V: EntityInputHandler + 'static> TerminalGrid<V> {
    pub fn new(props: TerminalGridProps<V>) -> Self {
        Self { props }
    }
}

impl<V: EntityInputHandler + 'static> IntoElement for TerminalGrid<V> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<V: EntityInputHandler + 'static> Element for TerminalGrid<V> {
    type RequestLayoutState = ();
    type PrepaintState = Bounds<Pixels>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = gpui::Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.props
            .scrollbar
            .sync_layout(bounds, self.props.cell_size.height);
        (self.props.on_bounds_changed)(bounds, cx);
        bounds
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let snapshot = &self.props.snapshot;
        window.handle_input(
            &self.props.focus_handle,
            ElementInputHandler::new(bounds, self.props.input_entity.clone()),
            cx,
        );

        let (pad_x, pad_y) = grid_padding(self.props.zoom);
        let origin = point(bounds.origin.x + px(pad_x), bounds.origin.y + px(pad_y));
        let default_bg = rgb_to_hsla(snapshot.default_bg);
        window.paint_quad(fill(bounds, default_bg));

        let cw = self.props.cell_size.width;
        let ch = self.props.cell_size.height;
        let fallbacks = style::font_fallbacks();

        // 1. Paint non-default cell backgrounds (coalesced horizontally per row).
        for (row_idx, row) in snapshot.grid.iter().enumerate() {
            let y = origin.y + ch * (row_idx as f32);
            let mut col_idx = 0usize;
            while col_idx < row.cells.len() {
                let cell = &row.cells[col_idx];
                if !cell.paints_background() {
                    col_idx += 1;
                    continue;
                }
                let bg = cell.effective_bg();
                let start = col_idx;
                col_idx += 1;
                while col_idx < row.cells.len()
                    && row.cells[col_idx].paints_background()
                    && row.cells[col_idx].effective_bg() == bg
                {
                    col_idx += 1;
                }
                let span_bounds = Bounds::new(
                    point(origin.x + cw * (start as f32), y),
                    size(cw * ((col_idx - start) as f32), ch),
                );
                window.paint_quad(fill(span_bounds, rgb_to_hsla(bg)));
            }
        }

        // 2. Paint selection highlight spans (one quad per covered row).
        if let Some(selection) = self.props.selection {
            for (row_idx, row) in snapshot.grid.iter().enumerate() {
                let Some(range) = selection.viewport_row_col_range(
                    &snapshot.scrollbar,
                    row_idx as u16,
                    snapshot.cols,
                ) else {
                    continue;
                };
                let mut start = usize::from(*range.start());
                let mut end = usize::from(*range.end());
                if row
                    .cells
                    .get(start)
                    .is_some_and(|c| c.width == CellWidth::SpacerTail)
                    && start > 0
                {
                    start -= 1;
                }
                if row
                    .cells
                    .get(end)
                    .is_some_and(|c| c.width == CellWidth::Wide)
                    && end + 1 < row.cells.len()
                {
                    end += 1;
                }
                let y = origin.y + ch * (row_idx as f32);
                let span_bounds = Bounds::new(
                    point(origin.x + cw * (start as f32), y),
                    size(cw * ((end - start + 1) as f32), ch),
                );
                window.paint_quad(fill(span_bounds, self.props.selection_color));
            }
        }

        // Compute cursor state before text so an opaque block cursor can paint
        // under the glyph and recolor that single cell for high contrast.
        let cursor_state = (snapshot.cursor.in_viewport && snapshot.cursor.visible).then(|| {
            let col = if snapshot.cursor.wide_tail && snapshot.cursor.col > 0 {
                snapshot.cursor.col - 1
            } else {
                snapshot.cursor.col
            };
            let shape = if self.props.focused {
                snapshot.cursor.shape
            } else {
                CursorShape::BlockHollow
            };
            let color = snapshot
                .explicit_cursor_color
                .map(rgb_to_hsla)
                .unwrap_or(self.props.default_cursor_color);
            (snapshot.cursor.row, col, shape, color)
        });

        let block_cursor_cell = cursor_state.and_then(|(row, col, shape, color)| {
            (shape == CursorShape::Block).then(|| {
                let cx_pos = origin.x + cw * f32::from(col);
                let cy_pos = origin.y + ch * f32::from(row);
                let cursor_w = if snapshot
                    .grid
                    .get(usize::from(row))
                    .and_then(|r| r.cells.get(usize::from(col)))
                    .is_some_and(|c| c.width == CellWidth::Wide)
                {
                    cw * 2.0
                } else {
                    cw
                };
                window.paint_quad(fill(
                    Bounds::new(point(cx_pos, cy_pos), size(cursor_w, ch)),
                    color,
                ));
                (usize::from(row), usize::from(col), contrasting_fg(color))
            })
        });

        // 3. Paint text runs per row. Narrow runs batch contiguous cells; wide
        // CJK cells are positioned individually so fallback glyphs never drift columns.
        for (row_idx, row) in snapshot.grid.iter().enumerate() {
            let y = origin.y + ch * (row_idx as f32);
            let mut col_idx = 0usize;
            while col_idx < row.cells.len() {
                let cell = &row.cells[col_idx];
                if matches!(cell.width, CellWidth::SpacerTail | CellWidth::SpacerHead)
                    || cell.attrs.invisible
                {
                    col_idx += 1;
                    continue;
                }

                if cell.width == CellWidth::Wide {
                    if !cell.text.is_empty() {
                        let fg_override = block_cursor_cell
                            .filter(|(r, c, _)| *r == row_idx && *c == col_idx)
                            .map(|(_, _, fg)| fg);
                        paint_text_fragment(
                            window,
                            cx,
                            point(origin.x + cw * (col_idx as f32), y),
                            ch,
                            self.props.font_size,
                            &self.props.font_family,
                            &fallbacks,
                            &cell.text,
                            cell,
                            fg_override,
                        );
                        if cell.attrs.overline {
                            paint_overline(
                                window,
                                point(origin.x + cw * (col_idx as f32), y),
                                cw * 2.0,
                                fg_override.unwrap_or_else(|| cell_fg(cell)),
                            );
                        }
                    }
                    col_idx += 1;
                    continue;
                }

                if is_blank_unstyled(cell) {
                    col_idx += 1;
                    continue;
                }

                let start_col = col_idx;
                let mut text = String::new();
                let mut runs = Vec::new();
                while col_idx < row.cells.len() {
                    let cur = &row.cells[col_idx];
                    if cur.width != CellWidth::Narrow || cur.attrs.invisible {
                        break;
                    }
                    // Break across gaps of 2+ unstyled blank cells so sparse lines
                    // re-anchor at exact column coordinates.
                    if is_blank_unstyled(cur) {
                        let next_is_blank =
                            row.cells.get(col_idx + 1).is_none_or(is_blank_unstyled);
                        if next_is_blank {
                            break;
                        }
                    }

                    let slice = if cur.text.is_empty() {
                        " "
                    } else {
                        cur.text.as_str()
                    };
                    let fg_override = block_cursor_cell
                        .filter(|(r, c, _)| *r == row_idx && *c == col_idx)
                        .map(|(_, _, fg)| fg);
                    let run = cell_to_text_run(
                        &self.props.font_family,
                        &fallbacks,
                        slice.len(),
                        cur,
                        fg_override,
                    );
                    if cur.attrs.overline {
                        paint_overline(
                            window,
                            point(origin.x + cw * (col_idx as f32), y),
                            cw,
                            run.color,
                        );
                    }
                    text.push_str(slice);
                    if let Some(last) = runs.last_mut()
                        && same_text_run_style(last, &run)
                    {
                        last.len += run.len;
                    } else {
                        runs.push(run);
                    }
                    col_idx += 1;
                }

                if !text.is_empty() {
                    let shaped = window.text_system().shape_line(
                        SharedString::from(text),
                        self.props.font_size,
                        &runs,
                        None,
                    );
                    let _ = shaped.paint(
                        point(origin.x + cw * (start_col as f32), y),
                        ch,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                } else {
                    col_idx += 1;
                }
            }
        }

        // 4. Paint non-block cursors (or hollow block when unfocused).
        if let Some((row, col, shape, color)) = cursor_state {
            let cx_pos = origin.x + cw * f32::from(col);
            let cy_pos = origin.y + ch * f32::from(row);
            let cursor_w = if snapshot
                .grid
                .get(usize::from(row))
                .and_then(|r| r.cells.get(usize::from(col)))
                .is_some_and(|c| c.width == CellWidth::Wide)
            {
                cw * 2.0
            } else {
                cw
            };
            let cursor_bar = px(2.0 * self.props.zoom);
            match shape {
                CursorShape::Block => {}
                CursorShape::Bar => {
                    window.paint_quad(fill(
                        Bounds::new(point(cx_pos, cy_pos), size(cursor_bar, ch)),
                        color,
                    ));
                }
                CursorShape::Underline => {
                    window.paint_quad(fill(
                        Bounds::new(
                            point(cx_pos, cy_pos + ch - cursor_bar),
                            size(cursor_w, cursor_bar),
                        ),
                        color,
                    ));
                }
                CursorShape::BlockHollow => {
                    let t = px(1.5 * self.props.zoom);
                    window.paint_quad(fill(
                        Bounds::new(point(cx_pos, cy_pos), size(cursor_w, t)),
                        color,
                    ));
                    window.paint_quad(fill(
                        Bounds::new(point(cx_pos, cy_pos + ch - t), size(cursor_w, t)),
                        color,
                    ));
                    window.paint_quad(fill(Bounds::new(point(cx_pos, cy_pos), size(t, ch)), color));
                    window.paint_quad(fill(
                        Bounds::new(point(cx_pos + cursor_w - t, cy_pos), size(t, ch)),
                        color,
                    ));
                }
            }
        }

        // 5. Paint inline IME preedit composition overlay at the cursor cell.
        if let Some(marked) = &self.props.marked_text
            && !marked.is_empty()
        {
            let col = if snapshot.cursor.in_viewport {
                f32::from(snapshot.cursor.col)
            } else {
                0.0
            };
            let row = if snapshot.cursor.in_viewport {
                f32::from(snapshot.cursor.row)
            } else {
                0.0
            };
            let ime_pos = point(origin.x + cw * col, origin.y + ch * row);
            let run = TextRun {
                len: marked.len(),
                font: Font {
                    family: self.props.font_family.clone(),
                    fallbacks: Some(fallbacks),
                    ..Font::default()
                },
                color: self.props.ime_fg,
                background_color: None,
                underline: Some(GpuiUnderlineStyle {
                    thickness: px(1.5 * self.props.zoom),
                    color: Some(self.props.ime_fg),
                    wavy: false,
                }),
                strikethrough: None,
            };
            let shaped = window.text_system().shape_line(
                SharedString::from(marked.clone()),
                self.props.font_size,
                &[run],
                None,
            );
            let bg_bounds = Bounds::new(
                ime_pos,
                size(shaped.width() + px(4.0 * self.props.zoom), ch),
            );
            window.paint_quad(fill(bg_bounds, self.props.ime_bg));
            let _ = shaped.paint(ime_pos, ch, TextAlign::Left, None, window, cx);
        }
    }
}

/// Wraps a [`TerminalGrid`] in a container with the app's overlay vertical scrollbar.
pub fn terminal_viewport(
    grid: impl IntoElement,
    scrollbar: &TerminalScrollbarHandle,
) -> AnyElement {
    div()
        .relative()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .child(grid)
        .child(
            div().absolute().inset_0().child(
                Scrollbar::vertical(scrollbar)
                    .id(scrollbar.element_id())
                    .viewport_from_layout(),
            ),
        )
        .into_any_element()
}

/// Measures the monospace cell advance width and line height for `font_family` at `font_size`.
pub fn measure_cell_size(
    window: &Window,
    font_family: &SharedString,
    font_size: Pixels,
) -> Size<Pixels> {
    let width = style::monospace_advance(window, font_family, font_size).max(px(6.0));
    let height = (font_size * 1.4).ceil().max(px(12.0));
    size(width, height)
}

/// Computes `(cols, rows)` that fit inside `bounds` for cells of `cell_size`.
pub fn grid_dimensions_for_bounds(
    bounds: Bounds<Pixels>,
    cell_size: Size<Pixels>,
    zoom: f32,
) -> (u16, u16) {
    let (pad_x, pad_y) = grid_padding(zoom);
    let cw = f32::from(cell_size.width).max(1.0);
    let ch = f32::from(cell_size.height).max(1.0);
    let avail_w = (f32::from(bounds.size.width) - pad_x * 2.0).max(cw);
    let avail_h = (f32::from(bounds.size.height) - pad_y * 2.0).max(ch);
    let cols = (avail_w / cw).floor().clamp(2.0, 500.0) as u16;
    let rows = (avail_h / ch).floor().clamp(2.0, 300.0) as u16;
    (cols, rows)
}

/// Converts window-relative pixel coordinates into clamped 0-indexed viewport `(col, row)`.
pub fn pixel_to_viewport_cell(
    position: Point<Pixels>,
    bounds: Bounds<Pixels>,
    cell_size: Size<Pixels>,
    cols: u16,
    rows: u16,
    zoom: f32,
) -> (u16, u16) {
    let (pad_x, pad_y) = grid_padding(zoom);
    let cw = f32::from(cell_size.width).max(1.0);
    let ch = f32::from(cell_size.height).max(1.0);
    let rel_x = (f32::from(position.x - bounds.origin.x) - pad_x).max(0.0);
    let rel_y = (f32::from(position.y - bounds.origin.y) - pad_y).max(0.0);
    let col = (rel_x / cw)
        .floor()
        .clamp(0.0, f32::from(cols.saturating_sub(1))) as u16;
    let row = (rel_y / ch)
        .floor()
        .clamp(0.0, f32::from(rows.saturating_sub(1))) as u16;
    (col, row)
}

/// Computes the window-space bounds of the cursor cell for IME candidate placement.
pub fn ime_cursor_bounds(
    element_bounds: Bounds<Pixels>,
    cell_size: Size<Pixels>,
    cursor: strata_term::CursorSnapshot,
    zoom: f32,
) -> Bounds<Pixels> {
    let (pad_x, pad_y) = grid_padding(zoom);
    let col = if cursor.in_viewport {
        f32::from(cursor.col)
    } else {
        0.0
    };
    let row = if cursor.in_viewport {
        f32::from(cursor.row)
    } else {
        0.0
    };
    let x = element_bounds.origin.x + px(pad_x) + cell_size.width * col;
    let y = element_bounds.origin.y + px(pad_y) + cell_size.height * row;
    Bounds::new(point(x, y), cell_size)
}

/// Maps the app's [`Design`] tokens to a `strata-term` [`TerminalTheme`].
pub fn terminal_theme(design: &Design) -> TerminalTheme {
    TerminalTheme {
        foreground: hsla_to_rgb(design.terminal_foreground),
        background: hsla_to_rgb(design.terminal_background),
        cursor: Some(hsla_to_rgb(design.terminal_cursor)),
        palette: Some(design.terminal_ansi.map(hsla_to_rgb)),
    }
}

pub fn hsla_to_rgb(color: Hsla) -> Rgb {
    let rgb = color.to_rgb();
    Rgb::new(
        (rgb.r.clamp(0.0, 1.0) * 255.0).round() as u8,
        (rgb.g.clamp(0.0, 1.0) * 255.0).round() as u8,
        (rgb.b.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

pub fn rgb_to_hsla(c: Rgb) -> Hsla {
    let hex = (u32::from(c.r) << 16) | (u32::from(c.g) << 8) | u32::from(c.b);
    gpui::rgb(hex).into()
}

fn is_blank_unstyled(cell: &CellSnapshot) -> bool {
    cell.text.is_empty()
        && cell.attrs.underline == UnderlineStyle::None
        && !cell.attrs.strikethrough
        && !cell.attrs.overline
}

fn cell_fg(cell: &CellSnapshot) -> Hsla {
    let fg = rgb_to_hsla(cell.effective_fg());
    if cell.attrs.faint {
        fg.opacity(0.6)
    } else {
        fg
    }
}

fn contrasting_fg(bg: Hsla) -> Hsla {
    let rgb = bg.to_rgb();
    let luma = 0.2126 * rgb.r + 0.7152 * rgb.g + 0.0722 * rgb.b;
    if luma > 0.45 {
        rgb_to_hsla(Rgb::new(0x16, 0x18, 0x1d))
    } else {
        Hsla::white()
    }
}

fn paint_overline(window: &mut Window, origin: Point<Pixels>, width: Pixels, color: Hsla) {
    window.paint_quad(fill(Bounds::new(origin, size(width, px(1.0))), color));
}

#[allow(clippy::too_many_arguments)]
fn paint_text_fragment(
    window: &mut Window,
    cx: &mut App,
    origin: Point<Pixels>,
    line_height: Pixels,
    font_size: Pixels,
    font_family: &SharedString,
    fallbacks: &gpui::FontFallbacks,
    text: &str,
    cell: &CellSnapshot,
    fg_override: Option<Hsla>,
) {
    let run = cell_to_text_run(font_family, fallbacks, text.len(), cell, fg_override);
    let shaped = window.text_system().shape_line(
        SharedString::from(text.to_string()),
        font_size,
        &[run],
        None,
    );
    let _ = shaped.paint(origin, line_height, TextAlign::Left, None, window, cx);
}

fn cell_to_text_run(
    font_family: &SharedString,
    fallbacks: &gpui::FontFallbacks,
    len: usize,
    cell: &CellSnapshot,
    fg_override: Option<Hsla>,
) -> TextRun {
    let fg = fg_override.unwrap_or_else(|| cell_fg(cell));
    let mut font = Font {
        family: font_family.clone(),
        fallbacks: Some(fallbacks.clone()),
        ..Font::default()
    };
    if cell.attrs.bold {
        font.weight = FontWeight::BOLD;
    }
    if cell.attrs.italic {
        font.style = FontStyle::Italic;
    }
    let underline = if cell.attrs.underline != UnderlineStyle::None {
        Some(GpuiUnderlineStyle {
            thickness: px(1.0),
            color: Some(fg),
            wavy: cell.attrs.underline == UnderlineStyle::Curly,
        })
    } else {
        None
    };
    let strikethrough = if cell.attrs.strikethrough {
        Some(StrikethroughStyle {
            thickness: px(1.0),
            color: Some(fg),
        })
    } else {
        None
    };

    TextRun {
        len,
        font,
        color: fg,
        background_color: None,
        underline,
        strikethrough,
    }
}

fn same_text_run_style(a: &TextRun, b: &TextRun) -> bool {
    a.font.weight == b.font.weight
        && a.font.style == b.font.style
        && a.color == b.color
        && a.underline == b.underline
        && a.strikethrough == b.strikethrough
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use gpui_component::{Theme, ThemeMode};

    fn contrast_ratio(a: Rgb, b: Rgb) -> f32 {
        fn lum(c: Rgb) -> f32 {
            let ch = |v: u8| {
                let s = f32::from(v) / 255.0;
                if s <= 0.039_28 {
                    s / 12.92
                } else {
                    ((s + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * ch(c.r) + 0.7152 * ch(c.g) + 0.0722 * ch(c.b)
        }
        let l1 = lum(a);
        let l2 = lum(b);
        (l1.max(l2) + 0.05) / (l1.min(l2) + 0.05)
    }

    #[test]
    fn grid_dimensions_and_pixel_to_cell_mapping() {
        let cell = size(px(8.0), px(16.0));
        let bounds = Bounds::new(point(px(10.0), px(20.0)), size(px(816.0), px(396.0)));
        // Available: (816 - 16) / 8 = 100 cols, (396 - 12) / 16 = 24 rows.
        assert_eq!(grid_dimensions_for_bounds(bounds, cell, 1.0), (100, 24));

        // Origin inside padding maps to (0, 0).
        assert_eq!(
            pixel_to_viewport_cell(point(px(18.0), px(26.0)), bounds, cell, 100, 24, 1.0),
            (0, 0)
        );
        // Outside top-left clamps to (0, 0).
        assert_eq!(
            pixel_to_viewport_cell(point(px(0.0), px(0.0)), bounds, cell, 100, 24, 1.0),
            (0, 0)
        );
        // Inside cell (5, 3).
        assert_eq!(
            pixel_to_viewport_cell(
                point(
                    px(10.0 + 8.0 + 5.0 * 8.0 + 4.0),
                    px(20.0 + 6.0 + 3.0 * 16.0 + 8.0)
                ),
                bounds,
                cell,
                100,
                24,
                1.0,
            ),
            (5, 3)
        );
        // Beyond bottom-right clamps to (99, 23).
        assert_eq!(
            pixel_to_viewport_cell(point(px(5000.0), px(5000.0)), bounds, cell, 100, 24, 1.0),
            (99, 23)
        );

        // At 150% zoom, padding scales from (8, 6) to (12, 9) and cells scale by 1.5.
        assert_eq!(grid_padding(1.5), (12.0, 9.0));
        let zoomed_cell = size(px(12.0), px(24.0));
        let zoomed_bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(1224.0), px(594.0)));
        assert_eq!(
            grid_dimensions_for_bounds(zoomed_bounds, zoomed_cell, 1.5),
            (100, 24)
        );
        assert_eq!(
            pixel_to_viewport_cell(
                point(px(12.0 + 2.0 * 12.0 + 1.0), px(9.0 + 3.0 * 24.0 + 1.0)),
                zoomed_bounds,
                zoomed_cell,
                100,
                24,
                1.5,
            ),
            (2, 3)
        );
    }

    #[test]
    fn scrollbar_handle_maps_offsets_and_drags() {
        let dragged = Rc::new(RefCell::new(Vec::new()));
        let sink = dragged.clone();
        let handle = TerminalScrollbarHandle::new(move |row| sink.borrow_mut().push(row));

        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(400.0), px(160.0)));
        handle.sync_layout(bounds, px(16.0));
        handle.sync_snapshot(ScrollbarSnapshot {
            total_rows: 30,
            viewport_offset: 20,
            viewport_len: 10,
        });

        assert_eq!(handle.content_size(), size(px(400.0), px(480.0)));
        assert_eq!(handle.offset(), point(px(0.0), px(-320.0)));

        handle.set_offset(point(px(0.0), px(-80.0)));
        assert_eq!(*dragged.borrow(), vec![5]);
        assert_eq!(handle.offset(), point(px(0.0), px(-80.0)));
    }

    #[test]
    fn terminal_theme_is_readable_in_both_light_and_dark() {
        let settings = Settings::default();
        let light_design = Design::default();
        let light = terminal_theme(&light_design);
        assert!(contrast_ratio(light.foreground, light.background) >= 10.0);
        let light_pal = light.palette.expect("light palette");
        for (idx, color) in light_pal.iter().enumerate() {
            assert!(
                contrast_ratio(*color, light.background) >= 3.5,
                "light palette[{idx}] ({color:?}) lacks contrast on {:?}",
                light.background
            );
        }

        let mut dark_theme = Theme::default();
        dark_theme.mode = ThemeMode::Dark;
        let dark_design = Design::new(&dark_theme, &settings);
        let dark = terminal_theme(&dark_design);
        assert!(contrast_ratio(dark.foreground, dark.background) >= 10.0);
        let dark_pal = dark.palette.expect("dark palette");
        // Non-black ANSI colors (1..16) must read cleanly against dark background.
        for (idx, color) in dark_pal.iter().enumerate().skip(1) {
            assert!(
                contrast_ratio(*color, dark.background) >= 3.5,
                "dark palette[{idx}] ({color:?}) lacks contrast on {:?}",
                dark.background
            );
        }

        for (id, is_dark) in [
            ("catppuccin-latte", false),
            ("catppuccin-frappe", true),
            ("catppuccin-macchiato", true),
            ("catppuccin-mocha", true),
        ] {
            let mut mode_theme = Theme::default();
            mode_theme.mode = if is_dark {
                ThemeMode::Dark
            } else {
                ThemeMode::Light
            };
            let mut s = Settings::default();
            if is_dark {
                s.appearance.dark_theme = id.to_owned();
            } else {
                s.appearance.light_theme = id.to_owned();
            }
            let design = Design::new(&mode_theme, &s);
            let term = terminal_theme(&design);
            assert!(
                contrast_ratio(term.foreground, term.background) >= 7.0,
                "{id} terminal fg/bg contrast is too low"
            );
            assert_eq!(term.cursor, Some(hsla_to_rgb(design.terminal_cursor)));
            assert!(term.palette.is_some());
        }
    }
}
