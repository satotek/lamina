//! Embedded PTY terminal panel hosted at the bottom of a repository view.
//!
//! Each repository tab owns at most one [`TerminalPanel`] entity, created
//! lazily when the panel is first toggled open. Keyboard input flows through
//! `strata-term`'s `libghostty-vt` key encoder, IME composition goes through
//! `EntityInputHandler`, and mouse events route either to PTY mouse tracking
//! or local scrollback selection.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use futures::StreamExt as _;
use gpui::{
    AnyElement, App, Bounds, ClipboardItem, Context, EntityInputHandler, FocusHandle, Focusable,
    FontWeight, InteractiveElement as _, IntoElement, KeyBinding, KeyDownEvent, Keystroke,
    Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, NavigationDirection,
    NoAction, ParentElement as _, Pixels, Point, Render, ScrollDelta, ScrollEvent, SharedString,
    Size, Styled as _, Subscription, Task, UTF16Selection, Window, div, px,
};
use gpui_component::{ActiveTheme as _, Icon};
use gpui_kit_assets::IconName;
use strata_term::{
    BufferPoint, ExitStatus, MousePosition, ScrollAction, TermKey, TermKeyAction, TermKeyEvent,
    TermModifiers, TermMouseButton, TermMouseEvent, TerminalEvent, TerminalOptions,
    TerminalSelection, TerminalSession, TerminalSnapshot, TerminalTheme,
};

use crate::external_tools::split_command;
use crate::repo_view::drop_in_background;
use crate::settings::Settings;
use crate::style::Design;
use crate::ui::{
    self, BadgeTone, BannerKind, ButtonSize, TerminalGrid, TerminalGridProps,
    TerminalScrollbarHandle, grid_dimensions_for_bounds, ime_cursor_bounds, measure_cell_size,
    pixel_to_viewport_cell, terminal_theme, terminal_viewport,
};

/// Key context applied to the terminal panel while focused.
///
/// The rendered element adds `Input` (`"TerminalPanel Input"`) so single-key
/// `RepoView && !Input` shortcuts never steal characters typed into the shell,
/// while the explicit `NoAction` bindings registered in [`bind`] prevent
/// unmodified navigation keys (`escape`, `enter`, arrows, paging) from reaching
/// `RepoView` handlers.
pub const TERMINAL_PANEL: &str = "TerminalPanel";

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 16;
const BELL_FLASH_DURATION: Duration = Duration::from_millis(180);

/// Registers key overrides that keep terminal navigation and `Escape` in the
/// PTY rather than invoking `RepoView` actions.
pub fn bind(cx: &mut App) {
    #[allow(unused_mut)]
    let mut keys = vec![
        "escape",
        "enter",
        "up",
        "down",
        "shift-up",
        "shift-down",
        "pageup",
        "pagedown",
        "home",
        "end",
        "alt-left",
    ];
    // On Linux and Windows `secondary` expands to `ctrl`, so repository
    // shortcuts using `ctrl-*` would intercept standard terminal control chords
    // (such as Ctrl+F forward / Ctrl+B back in shells and editors).
    #[cfg(not(target_os = "macos"))]
    keys.extend([
        "ctrl-f",
        "ctrl-b",
        "ctrl-shift-b",
        "ctrl-shift-s",
        "ctrl-alt-f",
        "ctrl-alt-l",
        "ctrl-alt-p",
        "ctrl-enter",
    ]);

    cx.bind_keys(
        keys.into_iter()
            .map(|key| KeyBinding::new(key, NoAction, Some(TERMINAL_PANEL)))
            .collect::<Vec<_>>(),
    );
}

/// Callback invoked when the user clicks the terminal panel's close button.
pub(crate) type CloseHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// Interactive terminal panel backed by a single [`TerminalSession`].
pub struct TerminalPanel {
    session: Option<TerminalSession>,
    snapshot: TerminalSnapshot,
    working_dir: PathBuf,
    focus_handle: FocusHandle,
    parent_focus: FocusHandle,
    on_close: Option<CloseHandler>,
    fallback_title: String,
    osc_title: Option<String>,
    spawn_error: Option<String>,
    exited_status: Option<Option<ExitStatus>>,
    bell_active: bool,
    bell_generation: u64,
    font_family: SharedString,
    font_size: Pixels,
    cell_size: Size<Pixels>,
    cols: u16,
    rows: u16,
    last_bounds: Option<Bounds<Pixels>>,
    selection: Option<TerminalSelection>,
    dragging_selection: bool,
    scroll_accum_y: f32,
    scrollbar: TerminalScrollbarHandle,
    marked_text: Option<String>,
    marked_selected_utf16: Range<usize>,
    last_theme: Option<TerminalTheme>,
    _event_tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
    #[cfg(test)]
    quiet: bool,
    #[cfg(test)]
    quiet_rx: Option<std::sync::mpsc::Receiver<TerminalEvent>>,
}

impl Drop for TerminalPanel {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            drop_in_background(session);
        }
    }
}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalPanel {
    /// Creates a terminal panel and spawns its initial shell in `working_dir`.
    pub fn new(
        working_dir: PathBuf,
        parent_focus: FocusHandle,
        on_close: Option<CloseHandler>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::create(working_dir, parent_focus, on_close, false, window, cx)
    }

    #[cfg(test)]
    pub(crate) fn new_quiet(
        working_dir: PathBuf,
        parent_focus: FocusHandle,
        on_close: Option<CloseHandler>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::create(working_dir, parent_focus, on_close, true, window, cx)
    }

    fn create(
        working_dir: PathBuf,
        parent_focus: FocusHandle,
        on_close: Option<CloseHandler>,
        #[cfg_attr(not(test), allow(unused_variables))] quiet: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let settings = Settings::get(cx);
        let design = Design::new(cx.theme(), settings);
        let font_family = design.mono.clone();
        let font_size = px(design.code);
        let cell_size = measure_cell_size(window, &font_family, font_size);
        let theme = terminal_theme(&design);
        let scrollbar = TerminalScrollbarHandle::new(|_| {});

        let in_sub = cx.on_focus_in(&focus_handle, window, |this, _window, cx| {
            if let Some(session) = &this.session {
                session.focus_changed(true);
            }
            cx.notify();
        });
        let out_sub = cx.on_focus_out(&focus_handle, window, |this, _event, _window, cx| {
            this.dragging_selection = false;
            if let Some(session) = &this.session {
                session.focus_changed(false);
            }
            cx.notify();
        });
        let weak = cx.weak_entity();
        let appearance_sub = window.observe_window_appearance(move |window, cx| {
            let _ = weak.update(cx, |this, cx| this.sync_appearance(Some(window), cx));
        });
        let settings_sub = cx.observe_global::<Settings>(|this, cx| {
            this.sync_appearance(None, cx);
        });

        let mut panel = Self {
            session: None,
            snapshot: TerminalSnapshot::default(),
            working_dir,
            focus_handle,
            parent_focus,
            on_close,
            fallback_title: default_shell_label(),
            osc_title: None,
            spawn_error: None,
            exited_status: None,
            bell_active: false,
            bell_generation: 0,
            font_family,
            font_size,
            cell_size,
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            last_bounds: None,
            selection: None,
            dragging_selection: false,
            scroll_accum_y: 0.0,
            scrollbar,
            marked_text: None,
            marked_selected_utf16: 0..0,
            last_theme: Some(theme.clone()),
            _event_tasks: Vec::new(),
            _subscriptions: vec![in_sub, out_sub, appearance_sub, settings_sub],
            #[cfg(test)]
            quiet,
            #[cfg(test)]
            quiet_rx: None,
        };
        panel.spawn_session(theme, cx);
        panel
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> &TerminalSnapshot {
        &self.snapshot
    }

    #[cfg(test)]
    pub(crate) fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    #[cfg(test)]
    pub(crate) fn has_session(&self) -> bool {
        self.session.is_some()
    }

    #[cfg(test)]
    pub(crate) fn last_theme(&self) -> Option<&TerminalTheme> {
        self.last_theme.as_ref()
    }

    // Only the PTY event test uses it, and that test needs a Unix shell.
    #[cfg(all(test, not(windows)))]
    pub(crate) fn flush_quiet_events(&mut self, cx: &mut Context<Self>) {
        let mut pending = Vec::new();
        if let Some(rx) = &self.quiet_rx {
            while let Ok(ev) = rx.try_recv() {
                pending.push(ev);
            }
        }
        for ev in pending {
            self.handle_terminal_event(ev, cx);
        }
    }

    /// Re-applies font metrics and `TerminalTheme` when appearance or settings change.
    pub fn sync_appearance(&mut self, window: Option<&Window>, cx: &mut Context<Self>) {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let theme = terminal_theme(&design);
        let mut changed = false;

        if self.last_theme.as_ref() != Some(&theme) {
            self.last_theme = Some(theme.clone());
            if let Some(session) = &self.session {
                session.set_theme(theme);
            }
            changed = true;
        }

        let new_font = design.mono.clone();
        let new_size = px(design.code);
        if let Some(window) = window
            && (self.font_family != new_font || self.font_size != new_size)
        {
            self.font_family = new_font;
            self.font_size = new_size;
            self.cell_size = measure_cell_size(window, &self.font_family, self.font_size);
            if let Some(bounds) = self.last_bounds {
                self.resize_for_bounds(bounds);
            }
            changed = true;
        } else if self.font_family != new_font || self.font_size != new_size {
            self.font_family = new_font;
            self.font_size = new_size;
            changed = true;
        }

        if changed {
            cx.notify();
        }
    }

    /// Restarts the shell process in the same working directory using current settings.
    pub fn restart(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let theme = terminal_theme(&design);
        self.last_theme = Some(theme.clone());
        self.spawn_session(theme, cx);
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn spawn_session(&mut self, theme: TerminalTheme, cx: &mut Context<Self>) {
        self._event_tasks.clear();
        self.scrollbar.set_scroll_handler(|_| {});
        if let Some(old) = self.session.take() {
            drop_in_background(old);
        }
        self.osc_title = None;
        self.exited_status = None;
        self.spawn_error = None;
        self.selection = None;
        self.dragging_selection = false;
        self.marked_text = None;
        self.marked_selected_utf16 = 0..0;

        let configured = Settings::get(cx).tools.embedded_shell.as_deref();
        let (shell, shell_args) = match parse_shell_command(configured) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.fallback_title = default_shell_label();
                self.spawn_error = Some(t!("terminal.invalid_shell", error = error).to_string());
                return;
            }
        };

        self.fallback_title = shell
            .as_deref()
            .and_then(shell_basename)
            .unwrap_or_else(default_shell_label);

        let options = TerminalOptions {
            cols: self.cols.max(2),
            rows: self.rows.max(2),
            cell_width_px: f32::from(self.cell_size.width).round().max(1.0) as u32,
            cell_height_px: f32::from(self.cell_size.height).round().max(1.0) as u32,
            theme,
            working_dir: Some(self.working_dir.clone()),
            shell,
            shell_args,
            ..TerminalOptions::default()
        };

        #[cfg(test)]
        if self.quiet {
            let (tx, rx) = std::sync::mpsc::channel();
            self.quiet_rx = Some(rx);
            match TerminalSession::spawn_with_callback(
                options,
                Box::new(move |event| {
                    let _ = tx.send(event);
                }),
            ) {
                Ok(session) => {
                    let snapshot = session.snapshot();
                    self.scrollbar.sync_snapshot(snapshot.scrollbar);
                    self.snapshot = snapshot;
                    self.session = Some(session);
                }
                Err(error) => {
                    self.spawn_error =
                        Some(t!("terminal.start_failed", error = error.to_string()).to_string());
                }
            }
            return;
        }

        let (wake_tx, mut wake_rx) = futures::channel::mpsc::unbounded::<TerminalEvent>();
        match TerminalSession::spawn_with_callback(
            options,
            Box::new(move |event| {
                let _ = wake_tx.unbounded_send(event);
            }),
        ) {
            Ok(session) => {
                let snapshot = session.snapshot();
                self.scrollbar.sync_snapshot(snapshot.scrollbar);
                self.snapshot = snapshot;
                self.session = Some(session);

                let (scroll_tx, mut scroll_rx) = futures::channel::mpsc::unbounded::<usize>();
                self.scrollbar.set_scroll_handler(move |row| {
                    let _ = scroll_tx.unbounded_send(row);
                });

                let event_task = cx.spawn(async move |this, cx| {
                    while let Some(event) = wake_rx.next().await {
                        let alive = this.update(cx, |panel, cx| {
                            panel.handle_terminal_event(event, cx);
                        });
                        if alive.is_err() {
                            break;
                        }
                    }
                });
                let scroll_task = cx.spawn(async move |this, cx| {
                    while let Some(row) = scroll_rx.next().await {
                        let alive = this.update(cx, |panel, _| {
                            if let Some(session) = &panel.session {
                                session.scroll(ScrollAction::Row(row));
                            }
                        });
                        if alive.is_err() {
                            break;
                        }
                    }
                });
                self._event_tasks = vec![event_task, scroll_task];
            }
            Err(error) => {
                self.spawn_error =
                    Some(t!("terminal.start_failed", error = error.to_string()).to_string());
            }
        }
    }

    fn handle_terminal_event(&mut self, event: TerminalEvent, cx: &mut Context<Self>) {
        match event {
            TerminalEvent::SnapshotChanged => {
                if let Some(session) = &self.session {
                    self.snapshot = session.snapshot();
                    self.scrollbar.sync_snapshot(self.snapshot.scrollbar);
                    cx.notify();
                }
            }
            TerminalEvent::TitleChanged(title) => {
                let trimmed = title.trim();
                self.osc_title = (!trimmed.is_empty()).then_some(title);
                cx.notify();
            }
            TerminalEvent::Bell => {
                self.trigger_bell_flash(cx);
            }
            TerminalEvent::ChildExited(status) => {
                if let Some(session) = &self.session {
                    self.snapshot = session.snapshot();
                    self.scrollbar.sync_snapshot(self.snapshot.scrollbar);
                }
                self.exited_status = Some(status);
                cx.notify();
            }
        }
    }

    fn trigger_bell_flash(&mut self, cx: &mut Context<Self>) {
        self.bell_active = true;
        self.bell_generation = self.bell_generation.wrapping_add(1);
        let generation = self.bell_generation;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(BELL_FLASH_DURATION).await;
            let _ = this.update(cx, |panel, cx| {
                if panel.bell_generation == generation && panel.bell_active {
                    panel.bell_active = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn resize_for_bounds(&mut self, bounds: Bounds<Pixels>) {
        self.last_bounds = Some(bounds);
        let (new_cols, new_rows) = grid_dimensions_for_bounds(bounds, self.cell_size);
        if new_cols != self.cols || new_rows != self.rows {
            self.cols = new_cols;
            self.rows = new_rows;
            if let Some(session) = &self.session {
                let cw = f32::from(self.cell_size.width).round().max(1.0) as u32;
                let ch = f32::from(self.cell_size.height).round().max(1.0) as u32;
                session.resize(new_cols, new_rows, cw, ch);
            }
        }
    }

    fn copy_selection(&self, cx: &mut App) -> bool {
        let Some(selection) = self.selection else {
            return false;
        };
        let Some(session) = &self.session else {
            return false;
        };
        if let Ok(text) = session.selected_text(selection)
            && !text.is_empty()
        {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            return true;
        }
        false
    }

    fn paste_clipboard(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(session) = &self.session else {
            return false;
        };
        if let Some(item) = cx.read_from_clipboard()
            && let Some(text) = item.text()
            && !text.is_empty()
        {
            session.paste(text);
            if self.selection.take().is_some() {
                cx.notify();
            }
            return true;
        }
        false
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.marked_text.is_some() || is_ime_key(&event.keystroke.key) {
            return;
        }

        let ks = &event.keystroke;

        #[cfg(target_os = "macos")]
        if ks.modifiers.platform && !ks.modifiers.control && !ks.modifiers.alt {
            if ks.key.eq_ignore_ascii_case("c") && !ks.modifiers.shift {
                if self.copy_selection(cx) {
                    cx.stop_propagation();
                }
                return;
            }
            if ks.key.eq_ignore_ascii_case("v") && !ks.modifiers.shift {
                if self.paste_clipboard(cx) {
                    cx.stop_propagation();
                }
                return;
            }
            if ks.key.eq_ignore_ascii_case("k") && !ks.modifiers.shift {
                if let Some(session) = &self.session {
                    session.write_str("\x0c");
                    self.selection = None;
                    cx.notify();
                    cx.stop_propagation();
                }
                return;
            }
            return;
        }

        #[cfg(not(target_os = "macos"))]
        if ks.modifiers.control && ks.modifiers.shift && !ks.modifiers.alt && !ks.modifiers.platform
        {
            if ks.key.eq_ignore_ascii_case("c") {
                if self.copy_selection(cx) {
                    cx.stop_propagation();
                }
                return;
            }
            if ks.key.eq_ignore_ascii_case("v") {
                if self.paste_clipboard(cx) {
                    cx.stop_propagation();
                }
                return;
            }
        }

        let Some(session) = &self.session else {
            return;
        };

        if let Some(term_event) = keystroke_to_term_event(ks) {
            session.send_key(term_event);
            if self.selection.take().is_some() {
                cx.notify();
            }
            cx.stop_propagation();
            return;
        }

        if !ks.modifiers.control
            && !ks.modifiers.platform
            && let Some(text) = &ks.key_char
            && !text.is_empty()
        {
            session.write_str(text);
            if self.selection.take().is_some() {
                cx.notify();
            }
            cx.stop_propagation();
        }
    }

    fn cell_at(&self, position: Point<Pixels>) -> Option<(u16, u16)> {
        let bounds = self.last_bounds?;
        Some(pixel_to_viewport_cell(
            position,
            bounds,
            self.cell_size,
            self.snapshot.cols,
            self.snapshot.rows,
        ))
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        let Some((col, row)) = self.cell_at(event.position) else {
            return;
        };
        let Some(session) = &self.session else {
            return;
        };

        if self.snapshot.mouse_tracking && !event.modifiers.shift {
            if let Some(button) = gpui_button_to_term(event.button) {
                session.handle_mouse(TermMouseEvent::press(
                    button,
                    MousePosition::Cell { col, row },
                    gpui_mods_to_term(event.modifiers),
                ));
                cx.stop_propagation();
            }
            return;
        }

        if event.button != MouseButton::Left {
            return;
        }

        let point = BufferPoint::from_viewport(&self.snapshot.scrollbar, col, row);
        match event.click_count {
            2 => {
                self.selection = session.select_word(point).ok().flatten();
                self.dragging_selection = false;
            }
            n if n >= 3 => {
                self.selection = session.select_line(point).ok().flatten();
                self.dragging_selection = false;
            }
            _ => {
                self.selection = Some(if event.modifiers.alt {
                    TerminalSelection::rectangular(point, point)
                } else {
                    TerminalSelection::new(point, point)
                });
                self.dragging_selection = true;
            }
        }
        cx.notify();
        cx.stop_propagation();
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((col, row)) = self.cell_at(event.position) else {
            return;
        };
        let Some(session) = &self.session else {
            return;
        };

        if self.snapshot.mouse_tracking && !event.modifiers.shift {
            let button = event.pressed_button.and_then(gpui_button_to_term);
            session.handle_mouse(TermMouseEvent::motion(
                button,
                MousePosition::Cell { col, row },
                gpui_mods_to_term(event.modifiers),
            ));
            return;
        }

        if self.dragging_selection && event.pressed_button == Some(MouseButton::Left) {
            let point = BufferPoint::from_viewport(&self.snapshot.scrollbar, col, row);
            if let Some(sel) = &mut self.selection {
                sel.end = point;
                sel.rectangle = event.modifiers.alt;
                cx.notify();
            }
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let was_dragging = self.dragging_selection;
        self.dragging_selection = false;

        let Some((col, row)) = self.cell_at(event.position) else {
            return;
        };
        let Some(session) = &self.session else {
            return;
        };

        if self.snapshot.mouse_tracking && !event.modifiers.shift {
            if let Some(button) = gpui_button_to_term(event.button) {
                session.handle_mouse(TermMouseEvent::release(
                    button,
                    MousePosition::Cell { col, row },
                    gpui_mods_to_term(event.modifiers),
                ));
                cx.stop_propagation();
            }
            return;
        }

        if event.button == MouseButton::Left
            && was_dragging
            && let Some(sel) = self.selection
            && sel.start == sel.end
        {
            self.selection = None;
            cx.notify();
        }
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = &self.session else {
            return;
        };
        let line_height = f32::from(self.cell_size.height).max(1.0);
        let delta_lines = match event.delta {
            ScrollDelta::Lines(lines) => lines.y * 3.0,
            ScrollDelta::Pixels(pixels) => f32::from(pixels.y) / line_height,
        };
        self.scroll_accum_y += delta_lines;
        let whole_rows = self.scroll_accum_y.trunc() as i32;
        if whole_rows != 0 {
            self.scroll_accum_y -= whole_rows as f32;
            let position = match self.cell_at(event.position) {
                Some((col, row)) => MousePosition::Cell { col, row },
                None => MousePosition::Pixels {
                    x: f32::from(event.position.x),
                    y: f32::from(event.position.y),
                },
            };
            session.handle_mouse(TermMouseEvent::wheel(
                0,
                whole_rows,
                position,
                gpui_mods_to_term(event.modifiers),
            ));
            cx.stop_propagation();
        }
    }

    fn display_title(&self) -> String {
        self.osc_title
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| self.fallback_title.clone())
    }

    fn header(&self, design: &Design, cx: &mut Context<Self>) -> AnyElement {
        let header_bg = if self.bell_active {
            design.accent_soft
        } else {
            design.chrome
        };

        let mut left = div()
            .flex()
            .items_center()
            .gap_2()
            .min_w_0()
            .flex_1()
            .child(
                Icon::new(IconName::SquareTerminal)
                    .size(px(14.0))
                    .text_color(if self.bell_active {
                        design.accent_text
                    } else {
                        design.muted
                    }),
            )
            .child(
                div()
                    .text_size(px(design.small()))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(design.text)
                    .truncate()
                    .child(self.display_title()),
            );

        if let Some(exited) = &self.exited_status {
            let (label, tone) = format_exit_status(exited.as_ref());
            left = left.child(ui::status_badge(label, tone, design));
        }

        let mut right = div().flex().items_center().gap_1().flex_none();
        if self.exited_status.is_some() || self.spawn_error.is_some() {
            right = right.child(
                ui::button("terminal-restart", t!("terminal.restart").to_string())
                    .size(ButtonSize::Tiny)
                    .on_click(cx.listener(|this, _, window, cx| this.restart(window, cx))),
            );
        }
        let on_close = self.on_close.clone();
        let parent_focus = self.parent_focus.clone();
        right = right.child(
            ui::icon_button("terminal-close", IconName::X)
                .size(ButtonSize::Tiny)
                .ghost()
                .tooltip(t!("common.close"))
                .on_click(move |_, window, cx| {
                    if let Some(on_close) = &on_close {
                        on_close(window, cx);
                    } else {
                        window.focus(&parent_focus, cx);
                    }
                }),
        );

        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .h(px(30.0))
            .flex_none()
            .px_3()
            .bg(header_bg)
            .border_t_1()
            .border_b_1()
            .border_color(design.border)
            .child(left)
            .child(right)
            .into_any_element()
    }
}

impl EntityInputHandler for TerminalPanel {
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
        if !text.is_empty()
            && let Some(session) = &self.session
        {
            session.write_str(text);
            self.selection = None;
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
        Some(ime_cursor_bounds(
            element_bounds,
            self.cell_size,
            self.snapshot.cursor,
        ))
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

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let expected_theme = terminal_theme(&design);
        if self.last_theme.as_ref() != Some(&expected_theme) {
            self.last_theme = Some(expected_theme.clone());
            if let Some(session) = &self.session {
                session.set_theme(expected_theme);
            }
        }
        if self.font_family != design.mono || self.font_size != px(design.code) {
            self.font_family = design.mono.clone();
            self.font_size = px(design.code);
            self.cell_size = measure_cell_size(window, &self.font_family, self.font_size);
        }

        let focused = self.focus_handle.is_focused(window);
        let entity = cx.entity().clone();
        let weak = cx.weak_entity();
        let bg = design.terminal_background;

        let grid = TerminalGrid::new(TerminalGridProps {
            snapshot: self.snapshot.clone(),
            selection: self.selection,
            selection_color: design.terminal_selection,
            default_cursor_color: design.terminal_cursor,
            ime_bg: design.field,
            ime_fg: design.text,
            font_family: self.font_family.clone(),
            font_size: self.font_size,
            cell_size: self.cell_size,
            focused,
            marked_text: self.marked_text.clone(),
            focus_handle: self.focus_handle.clone(),
            input_entity: entity,
            scrollbar: self.scrollbar.clone(),
            on_bounds_changed: Box::new(move |bounds, cx| {
                let _ = weak.update(cx, |panel, _| {
                    panel.resize_for_bounds(bounds);
                });
            }),
        });

        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .min_w_0()
            .bg(bg)
            .track_focus(&self.focus_handle)
            .key_context("TerminalPanel Input")
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_scroll(cx.listener(Self::on_scroll_wheel))
            .child(self.header(&design, cx));

        if let Some(error) = &self.spawn_error {
            root = root.child(ui::banner(BannerKind::Danger, error.clone(), &design));
        }

        root.child(terminal_viewport(grid, &self.scrollbar))
    }
}

/// Converts a GPUI [`Keystroke`] into a `strata-term` [`TermKeyEvent`] when a
/// recognized key code exists; callers fall back to raw UTF-8 on `None`.
pub(crate) fn keystroke_to_term_event(ks: &Keystroke) -> Option<TermKeyEvent> {
    let term_key = TermKey::from_name(&ks.key);
    if term_key == TermKey::Unidentified {
        return None;
    }
    Some(TermKeyEvent {
        action: TermKeyAction::Press,
        key: term_key,
        modifiers: TermModifiers {
            shift: ks.modifiers.shift,
            ctrl: ks.modifiers.control,
            alt: ks.modifiers.alt,
            super_key: ks.modifiers.platform,
            ..TermModifiers::default()
        },
        text: ks.key_char.clone(),
        ..TermKeyEvent::default()
    })
}

fn gpui_mods_to_term(mods: Modifiers) -> TermModifiers {
    TermModifiers {
        shift: mods.shift,
        ctrl: mods.control,
        alt: mods.alt,
        super_key: mods.platform,
        ..TermModifiers::default()
    }
}

fn gpui_button_to_term(button: MouseButton) -> Option<TermMouseButton> {
    match button {
        MouseButton::Left => Some(TermMouseButton::Left),
        MouseButton::Middle => Some(TermMouseButton::Middle),
        MouseButton::Right => Some(TermMouseButton::Right),
        MouseButton::Navigate(NavigationDirection::Back) => Some(TermMouseButton::Back),
        MouseButton::Navigate(NavigationDirection::Forward) => Some(TermMouseButton::Forward),
    }
}

fn is_ime_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "process"
            | "hanja"
            | "kanjimode"
            | "eisu"
            | "kana"
            | "henkan"
            | "muhenkan"
            | "katakana"
            | "hiragana"
            | "romaji"
    )
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

pub(crate) fn parse_shell_command(
    configured: Option<&str>,
) -> Result<(Option<String>, Vec<String>), String> {
    let Some(raw) = configured.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok((None, Vec::new()));
    };
    let mut parts = split_command(raw)?;
    if parts.is_empty() {
        return Ok((None, Vec::new()));
    }
    let program = parts.remove(0);
    Ok((Some(program), parts))
}

fn shell_basename(program: &str) -> Option<String> {
    Path::new(program)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
}

fn default_shell_label() -> String {
    #[cfg(not(windows))]
    if let Ok(shell) = std::env::var("SHELL")
        && let Some(name) = shell_basename(&shell)
    {
        return name;
    }
    #[cfg(windows)]
    if let Ok(comspec) = std::env::var("COMSPEC")
        && let Some(name) = shell_basename(&comspec)
    {
        return name;
    }
    t!("terminal.title").to_string()
}

fn format_exit_status(status: Option<&ExitStatus>) -> (String, BadgeTone) {
    match status {
        Some(status) if status.success() => (
            t!("terminal.exited_code", code = status.code).to_string(),
            BadgeTone::Neutral,
        ),
        Some(status) => {
            if let Some(signal) = &status.signal {
                (
                    t!("terminal.exited_signal", signal = signal).to_string(),
                    BadgeTone::Danger,
                )
            } else {
                (
                    t!("terminal.exited_code", code = status.code).to_string(),
                    BadgeTone::Warning,
                )
            }
        }
        None => (
            t!("terminal.exited_unknown").to_string(),
            BadgeTone::Neutral,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keystroke_mapping_covers_escape_ctrl_c_arrows_and_fallback() {
        let esc = Keystroke::parse("escape").expect("escape");
        let mapped = keystroke_to_term_event(&esc).expect("mapped esc");
        assert_eq!(mapped.key, TermKey::Escape);
        assert!(!mapped.modifiers.ctrl);

        let ctrl_c = Keystroke::parse("ctrl-c").expect("ctrl-c");
        let mapped = keystroke_to_term_event(&ctrl_c).expect("mapped ctrl-c");
        assert_eq!(mapped.key, TermKey::C);
        assert!(mapped.modifiers.ctrl);

        let up = Keystroke::parse("up").expect("up");
        let mapped = keystroke_to_term_event(&up).expect("mapped up");
        assert_eq!(mapped.key, TermKey::ArrowUp);

        let ime = Keystroke::parse("kanjimode").unwrap_or_else(|_| Keystroke {
            modifiers: Modifiers::default(),
            key: "KanjiMode".to_string(),
            key_char: None,
        });
        assert!(is_ime_key(&ime.key));
    }

    #[test]
    fn parse_shell_command_splits_program_and_arguments() {
        assert_eq!(parse_shell_command(None), Ok((None, Vec::new())));
        assert_eq!(parse_shell_command(Some("   ")), Ok((None, Vec::new())));
        assert_eq!(
            parse_shell_command(Some("/bin/zsh -l -i")),
            Ok((
                Some("/bin/zsh".to_string()),
                vec!["-l".to_string(), "-i".to_string()]
            ))
        );
        assert!(parse_shell_command(Some("/bin/sh -c 'unclosed")).is_err());
    }

    #[gpui::test]
    fn ime_input_handler_tracks_marked_text_and_cursor_bounds(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            // Only Unix overrides the shell, so Windows never mutates it.
            #[cfg_attr(not(unix), allow(unused_mut))]
            let mut settings = Settings::default();
            #[cfg(unix)]
            {
                settings.tools.embedded_shell = Some("/bin/sh".to_owned());
            }
            cx.set_global(settings);
        });

        let dir = tempfile::TempDir::new().expect("tempdir");
        let workdir = dir.path().to_path_buf();
        let window = cx.add_window(|window, cx| {
            let parent = cx.focus_handle();
            TerminalPanel::new_quiet(workdir, parent, None, window, cx)
        });
        let panel = window.root(cx).expect("root panel");
        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);

        panel.update_in(&mut vcx, |panel, window, cx| {
            panel.replace_and_mark_text_in_range(None, "かな入力", Some(2..4), window, cx);
            assert_eq!(panel.marked_text_range(window, cx), Some(0..4));
            assert_eq!(
                panel
                    .selected_text_range(false, window, cx)
                    .map(|s| s.range),
                Some(2..4)
            );
            let mut adjusted = None;
            assert_eq!(
                panel
                    .text_for_range(1..3, &mut adjusted, window, cx)
                    .as_deref(),
                Some("な入")
            );
            assert_eq!(adjusted, Some(1..3));

            let bounds = Bounds::new(
                gpui::point(px(20.0), px(40.0)),
                gpui::size(px(500.0), px(200.0)),
            );
            let cursor_bounds = panel
                .bounds_for_range(0..1, bounds, window, cx)
                .expect("cursor bounds");
            assert!(cursor_bounds.origin.x >= bounds.origin.x);
            assert!(cursor_bounds.origin.y >= bounds.origin.y);

            panel.replace_text_in_range(None, "仮名", window, cx);
            assert_eq!(panel.marked_text_range(window, cx), None);

            panel.replace_and_mark_text_in_range(None, "abc", None, window, cx);
            panel.unmark_text(window, cx);
            assert_eq!(panel.marked_text_range(window, cx), None);
        });
    }

    #[cfg(not(windows))]
    #[gpui::test]
    fn pty_events_update_title_bell_exit_and_restart(cx: &mut gpui::TestAppContext) {
        use std::time::{Duration, Instant};

        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            let mut settings = Settings::default();
            settings.tools.embedded_shell = Some(
                "/bin/sh -c \"printf '\\033]0;strata-title\\007\\007hello-pty\\n'; exit 7\""
                    .to_owned(),
            );
            cx.set_global(settings);
        });

        let dir = tempfile::TempDir::new().expect("tempdir");
        let workdir = dir.path().to_path_buf();
        let window = cx.add_window(|window, cx| {
            let parent = cx.focus_handle();
            TerminalPanel::new_quiet(workdir, parent, None, window, cx)
        });
        let panel = window.root(cx).expect("root panel");
        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_exit = false;
        while Instant::now() < deadline {
            panel.update(&mut vcx, |panel, cx| {
                panel.flush_quiet_events(cx);
                saw_exit = panel.exited_status.is_some();
            });
            if saw_exit {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        panel.read_with(&vcx, |panel, _| {
            assert!(saw_exit, "timed out waiting for shell exit event");
            assert_eq!(panel.display_title(), "strata-title");
            assert!(panel.bell_active, "bell flash must activate on BEL");
            let status = panel
                .exited_status
                .as_ref()
                .and_then(|s| s.as_ref())
                .expect("exit status");
            assert_eq!(status.code, 7);
            let grid_text = (0..panel.snapshot().grid.len())
                .map(|r| panel.snapshot().row_text(r))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                grid_text.contains("hello-pty"),
                "expected output in snapshot, got: {grid_text:?}"
            );
        });

        // Restarting spawns a fresh shell and clears exit status / title.
        vcx.update(|_, cx| {
            let mut settings = Settings::get(cx).clone();
            settings.tools.embedded_shell = Some("/bin/sh".to_owned());
            cx.set_global(settings);
        });
        panel.update_in(&mut vcx, |panel, window, cx| {
            panel.restart(window, cx);
            assert!(panel.exited_status.is_none());
            assert!(panel.osc_title.is_none());
            assert!(panel.has_session());
        });
    }
}
