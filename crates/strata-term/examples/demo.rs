//! Minimal GPUI terminal window prototype backed by `strata-term`.

use std::ops::Range;

use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Bounds, Context, Element, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, Font, FontStyle, FontWeight, GlobalElementId, Hsla,
    InspectorElementId, InteractiveElement as _, IntoElement, KeyDownEvent, LayoutId,
    ParentElement as _, Pixels, Point, Render, ScrollDelta, ScrollEvent, SharedString, Size,
    StrikethroughStyle, Styled, TextAlign, TextRun, UTF16Selection,
    UnderlineStyle as GpuiUnderlineStyle, Window, WindowBounds, WindowOptions, div, fill, point,
    px, rgb, size,
};
use gpui_platform::application;
use strata_term::{
    CellWidth, CursorShape, MousePosition, Rgb, TermKey, TermKeyAction, TermKeyEvent,
    TermModifiers, TermMouseEvent, TerminalEvent, TerminalOptions, TerminalSession,
    TerminalSnapshot, TerminalTheme, UnderlineStyle,
};

fn default_mono_font() -> SharedString {
    #[cfg(target_os = "macos")]
    {
        "Menlo".into()
    }
    #[cfg(target_os = "windows")]
    {
        "Consolas".into()
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        "DejaVu Sans Mono".into()
    }
}

fn rgb_to_hsla(c: Rgb) -> Hsla {
    let hex = (u32::from(c.r) << 16) | (u32::from(c.g) << 8) | u32::from(c.b);
    rgb(hex).into()
}

struct TerminalView {
    session: TerminalSession,
    snapshot: TerminalSnapshot,
    focus_handle: FocusHandle,
    font_family: SharedString,
    font_size: Pixels,
    cell_size: Size<Pixels>,
    cols: u16,
    rows: u16,
    scroll_accum_y: f32,
    marked_text: Option<String>,
    marked_selected_utf16: Range<usize>,
    _wakeup_task: gpui::Task<()>,
}

impl TerminalView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let font_family = default_mono_font();
        let font_size = px(13.0);
        let cell_size = measure_cell_size(window, &font_family, font_size);

        let cols = 100;
        let rows = 32;
        let (wake_tx, mut wake_rx) = futures::channel::mpsc::unbounded::<TerminalEvent>();

        let options = TerminalOptions {
            cols,
            rows,
            cell_width_px: f32::from(cell_size.width).round().max(1.0) as u32,
            cell_height_px: f32::from(cell_size.height).round().max(1.0) as u32,
            theme: TerminalTheme::default(),
            ..TerminalOptions::default()
        };

        let session = TerminalSession::spawn_with_callback(
            options,
            Box::new(move |event| {
                let _ = wake_tx.unbounded_send(event);
            }),
        )
        .expect("spawn PTY shell");

        let snapshot = session.snapshot();
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);

        let wakeup_task = cx.spawn(async move |this, cx| {
            while let Some(event) = wake_rx.next().await {
                if matches!(
                    event,
                    TerminalEvent::SnapshotChanged
                        | TerminalEvent::TitleChanged(_)
                        | TerminalEvent::ChildExited(_)
                ) && this
                    .update(cx, |view, cx| {
                        view.snapshot = view.session.snapshot();
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        Self {
            session,
            snapshot,
            focus_handle,
            font_family,
            font_size,
            cell_size,
            cols,
            rows,
            scroll_accum_y: 0.0,
            marked_text: None,
            marked_selected_utf16: 0..0,
            _wakeup_task: wakeup_task,
        }
    }

    fn update_grid_for_bounds(&mut self, bounds: Bounds<Pixels>) {
        let cw = f32::from(self.cell_size.width).max(1.0);
        let ch = f32::from(self.cell_size.height).max(1.0);
        let avail_w = (f32::from(bounds.size.width) - 16.0).max(cw);
        let avail_h = (f32::from(bounds.size.height) - 16.0).max(ch);
        let new_cols = (avail_w / cw).floor().clamp(2.0, 500.0) as u16;
        let new_rows = (avail_h / ch).floor().clamp(2.0, 300.0) as u16;

        if new_cols != self.cols || new_rows != self.rows {
            self.cols = new_cols;
            self.rows = new_rows;
            self.session.resize(
                new_cols,
                new_rows,
                cw.round().max(1.0) as u32,
                ch.round().max(1.0) as u32,
            );
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;

        // Let the OS handle standard Cmd shortcuts on macOS (Quit, Close, Hide),
        // except Cmd+V paste.
        #[cfg(target_os = "macos")]
        if ks.modifiers.platform {
            if ks.key.eq_ignore_ascii_case("v")
                && let Some(item) = cx.read_from_clipboard()
                && let Some(text) = item.text()
            {
                self.session.paste(text);
                cx.stop_propagation();
            }
            return;
        }

        #[cfg(not(target_os = "macos"))]
        if ks.modifiers.control
            && ks.modifiers.shift
            && ks.key.eq_ignore_ascii_case("v")
            && let Some(item) = cx.read_from_clipboard()
            && let Some(text) = item.text()
        {
            self.session.paste(text);
            cx.stop_propagation();
            return;
        }

        let term_key = TermKey::from_name(&ks.key);
        let modifiers = TermModifiers {
            shift: ks.modifiers.shift,
            ctrl: ks.modifiers.control,
            alt: ks.modifiers.alt,
            super_key: ks.modifiers.platform,
            ..TermModifiers::default()
        };

        if term_key == TermKey::Unidentified {
            if !ks.modifiers.control
                && !ks.modifiers.platform
                && let Some(text) = &ks.key_char
                && !text.is_empty()
            {
                self.session.write_str(text);
                cx.stop_propagation();
            }
            return;
        }

        let term_event = TermKeyEvent {
            action: TermKeyAction::Press,
            key: term_key,
            modifiers,
            text: ks.key_char.clone(),
            ..TermKeyEvent::default()
        };
        self.session.send_key(term_event);
        cx.stop_propagation();
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line_height = f32::from(self.cell_size.height).max(1.0);
        let delta_lines = match event.delta {
            ScrollDelta::Lines(lines) => lines.y * 3.0,
            ScrollDelta::Pixels(pixels) => f32::from(pixels.y) / line_height,
        };
        self.scroll_accum_y += delta_lines;
        let whole_rows = self.scroll_accum_y.trunc() as i32;
        if whole_rows != 0 {
            self.scroll_accum_y -= whole_rows as f32;
            self.session.handle_mouse(TermMouseEvent::wheel(
                0,
                whole_rows,
                MousePosition::Pixels {
                    x: f32::from(event.position.x),
                    y: f32::from(event.position.y),
                },
                TermModifiers {
                    shift: event.modifiers.shift,
                    ctrl: event.modifiers.control,
                    alt: event.modifiers.alt,
                    super_key: event.modifiers.platform,
                    ..TermModifiers::default()
                },
            ));
            cx.stop_propagation();
        }
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let marked = self.marked_text.as_deref()?;
        let utf16_len = marked.encode_utf16().count();
        let clamped = range.start.min(utf16_len)..range.end.min(utf16_len);
        *adjusted_range = Some(clamped.clone());
        Some(slice_utf16(marked, clamped))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.marked_selected_utf16.clone(),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_text
            .as_deref()
            .map(|text| 0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.marked_text.take().is_some() {
            self.marked_selected_utf16 = 0..0;
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = None;
        self.marked_selected_utf16 = 0..0;
        if !text.is_empty() {
            self.session.write_bytes(text.as_bytes().to_vec());
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if new_text.is_empty() {
            self.marked_text = None;
            self.marked_selected_utf16 = 0..0;
        } else {
            let utf16_len = new_text.encode_utf16().count();
            self.marked_text = Some(new_text.to_string());
            self.marked_selected_utf16 = new_selected_range
                .map(|r| r.start.min(utf16_len)..r.end.min(utf16_len))
                .unwrap_or(utf16_len..utf16_len);
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let col = if self.snapshot.cursor.in_viewport {
            f32::from(self.snapshot.cursor.col)
        } else {
            0.0
        };
        let row = if self.snapshot.cursor.in_viewport {
            f32::from(self.snapshot.cursor.row)
        } else {
            0.0
        };
        let x = element_bounds.origin.x + px(8.0) + self.cell_size.width * col;
        let y = element_bounds.origin.y + px(8.0) + self.cell_size.height * row;
        Some(Bounds::new(point(x, y), self.cell_size))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bg = rgb_to_hsla(self.snapshot.default_bg);
        let entity = cx.entity().clone();

        div()
            .track_focus(&self.focus_handle)
            .key_context("Terminal")
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll(cx.listener(Self::on_scroll_wheel))
            .size_full()
            .bg(bg)
            .child(TerminalCanvasElement { view: entity })
    }
}

struct TerminalCanvasElement {
    view: Entity<TerminalView>,
}

impl IntoElement for TerminalCanvasElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalCanvasElement {
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
        style.size.width = gpui::relative(1.0).into();
        style.size.height = gpui::relative(1.0).into();
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
        self.view.update(cx, |view, _| {
            view.update_grid_for_bounds(bounds);
        });
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
        let (focus_handle, snapshot, font_family, font_size, cell_size, marked_text) = {
            let view = self.view.read(cx);
            (
                view.focus_handle.clone(),
                view.snapshot.clone(),
                view.font_family.clone(),
                view.font_size,
                view.cell_size,
                view.marked_text.clone(),
            )
        };

        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );

        let origin = point(bounds.origin.x + px(8.0), bounds.origin.y + px(8.0));
        let default_bg = rgb_to_hsla(snapshot.default_bg);
        window.paint_quad(fill(bounds, default_bg));

        let cw = cell_size.width;
        let ch = cell_size.height;

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

        // 2. Paint text runs. Wide characters are placed at their exact column
        // coordinate so fallback CJK glyphs cannot shift subsequent columns.
        for (row_idx, row) in snapshot.grid.iter().enumerate() {
            let y = origin.y + ch * (row_idx as f32);
            let mut col_idx = 0usize;
            while col_idx < row.cells.len() {
                let cell = &row.cells[col_idx];
                if matches!(cell.width, CellWidth::SpacerTail | CellWidth::SpacerHead)
                    || cell.text.is_empty()
                    || cell.attrs.invisible
                {
                    col_idx += 1;
                    continue;
                }

                if cell.width == CellWidth::Wide {
                    paint_text_fragment(
                        window,
                        cx,
                        point(origin.x + cw * (col_idx as f32), y),
                        ch,
                        font_size,
                        &font_family,
                        &cell.text,
                        cell,
                    );
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
                    let slice = if cur.text.is_empty() {
                        " "
                    } else {
                        cur.text.as_str()
                    };
                    if slice == " "
                        && cur.attrs.underline == UnderlineStyle::None
                        && !cur.attrs.strikethrough
                    {
                        // Trim trailing runs of unstyled spaces.
                        let has_more_content = row.cells[col_idx + 1..].iter().any(|c| {
                            !c.text.is_empty()
                                || c.attrs.underline != UnderlineStyle::None
                                || c.attrs.strikethrough
                        });
                        if !has_more_content {
                            break;
                        }
                    }
                    let run = cell_to_text_run(&font_family, slice.len(), cur);
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
                        font_size,
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

        // 3. Paint cursor.
        if snapshot.cursor.in_viewport && snapshot.cursor.visible {
            let cursor_col = if snapshot.cursor.wide_tail && snapshot.cursor.col > 0 {
                snapshot.cursor.col - 1
            } else {
                snapshot.cursor.col
            };
            let cx_pos = origin.x + cw * f32::from(cursor_col);
            let cy_pos = origin.y + ch * f32::from(snapshot.cursor.row);
            let cursor_color = snapshot
                .explicit_cursor_color
                .map(rgb_to_hsla)
                .unwrap_or_else(|| Hsla {
                    h: 0.58,
                    s: 0.8,
                    l: 0.68,
                    a: 0.65,
                });

            match snapshot.cursor.shape {
                CursorShape::Block => {
                    window.paint_quad(fill(
                        Bounds::new(point(cx_pos, cy_pos), size(cw, ch)),
                        cursor_color,
                    ));
                }
                CursorShape::Bar => {
                    window.paint_quad(fill(
                        Bounds::new(point(cx_pos, cy_pos), size(px(2.0), ch)),
                        cursor_color,
                    ));
                }
                CursorShape::Underline => {
                    window.paint_quad(fill(
                        Bounds::new(point(cx_pos, cy_pos + ch - px(2.0)), size(cw, px(2.0))),
                        cursor_color,
                    ));
                }
                CursorShape::BlockHollow => {
                    let t = px(1.5);
                    window.paint_quad(fill(
                        Bounds::new(point(cx_pos, cy_pos), size(cw, t)),
                        cursor_color,
                    ));
                    window.paint_quad(fill(
                        Bounds::new(point(cx_pos, cy_pos + ch - t), size(cw, t)),
                        cursor_color,
                    ));
                    window.paint_quad(fill(
                        Bounds::new(point(cx_pos, cy_pos), size(t, ch)),
                        cursor_color,
                    ));
                    window.paint_quad(fill(
                        Bounds::new(point(cx_pos + cw - t, cy_pos), size(t, ch)),
                        cursor_color,
                    ));
                }
            }
        }

        // 4. Paint inline IME preedit composition overlay at the cursor cell.
        if let Some(marked) = marked_text
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
                    family: font_family.clone(),
                    ..Font::default()
                },
                color: rgb_to_hsla(snapshot.default_fg),
                background_color: None,
                underline: Some(GpuiUnderlineStyle {
                    thickness: px(1.5),
                    color: Some(rgb_to_hsla(snapshot.default_fg)),
                    wavy: false,
                }),
                strikethrough: None,
            };
            let shaped = window.text_system().shape_line(
                SharedString::from(marked),
                font_size,
                &[run],
                None,
            );
            let bg_bounds = Bounds::new(ime_pos, size(shaped.width() + px(4.0), ch));
            window.paint_quad(fill(bg_bounds, rgb_to_hsla(Rgb::new(0x1F, 0x29, 0x37))));
            let _ = shaped.paint(ime_pos, ch, TextAlign::Left, None, window, cx);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_text_fragment(
    window: &mut Window,
    cx: &mut App,
    origin: Point<Pixels>,
    line_height: Pixels,
    font_size: Pixels,
    font_family: &SharedString,
    text: &str,
    cell: &strata_term::CellSnapshot,
) {
    let run = cell_to_text_run(font_family, text.len(), cell);
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
    len: usize,
    cell: &strata_term::CellSnapshot,
) -> TextRun {
    let fg = rgb_to_hsla(cell.effective_fg());
    let mut font = Font {
        family: font_family.clone(),
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

fn measure_cell_size(
    window: &Window,
    font_family: &SharedString,
    font_size: Pixels,
) -> Size<Pixels> {
    let run = TextRun {
        len: 1,
        font: Font {
            family: font_family.clone(),
            ..Font::default()
        },
        color: Hsla::white(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let shaped = window
        .text_system()
        .shape_line("M".into(), font_size, &[run], None);
    let width = shaped.width().max(px(6.0));
    let height = (font_size * 1.35).ceil().max(px(12.0));
    size(width, height)
}

fn slice_utf16(s: &str, range: Range<usize>) -> String {
    let mut out = String::new();
    let mut utf16_pos = 0usize;
    for ch in s.chars() {
        let len16 = ch.len_utf16();
        let next = utf16_pos + len16;
        if next > range.start && utf16_pos < range.end {
            out.push(ch);
        }
        utf16_pos = next;
    }
    out
}

fn main() {
    application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(960.0), px(620.0)), cx);
        let _ = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..WindowOptions::default()
            },
            |window, cx| {
                let view = cx.new(|cx| TerminalView::new(window, cx));
                let focus_handle = view.read(cx).focus_handle.clone();
                window.focus(&focus_handle, cx);
                view
            },
        );
        cx.activate(true);
    });
}
