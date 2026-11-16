//! Embedded PTY terminal panel hosted at the bottom of a repository view.
//!
//! Each repository tab owns at most one [`TerminalPanel`] entity, created
//! lazily when the panel is first toggled open. Inside the panel, each tab
//! owns an independent [`TerminalSession`] started in the repository's working
//! directory, with its own snapshot, scrollback, selection, bell state, and IME
//! composition state. Keyboard input flows through `lamina-term`'s
//! `libghostty-vt` key encoder, IME composition goes through
//! `EntityInputHandler`, and mouse events route either to PTY mouse tracking or
//! local scrollback selection.

use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use futures::StreamExt as _;
use gpui::{
    AnyElement, App, Bounds, ClipboardItem, Context, EntityInputHandler, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyBinding, KeyDownEvent, Keystroke, Modifiers,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, NavigationDirection, NoAction,
    ParentElement as _, Pixels, Point, Render, ScrollDelta, ScrollEvent, ScrollHandle,
    SharedString, Size, Styled as _, Subscription, Task, UTF16Selection, Window, div, px,
};
use gpui_component::ActiveTheme as _;
use gpui_kit_assets::IconName;
use lamina_term::{
    BufferPoint, ExitStatus, MousePosition, ScrollAction, TermKey, TermKeyAction, TermKeyEvent,
    TermModifiers, TermMouseButton, TermMouseEvent, TerminalEvent, TerminalOptions,
    TerminalSelection, TerminalSession, TerminalSnapshot, TerminalTheme,
};

use crate::actions::{CloseTerminalTab, NewTerminalTab, NextTerminalTab, PreviousTerminalTab};
use crate::external_tools::split_command;
use crate::repo_view::drop_in_background;
use crate::settings::Settings;
use crate::style::Design;
use crate::tabs::{CloseOutcome, TabDeck};
use crate::ui::{
    self, BadgeTone, BannerKind, ButtonSize, COMPACT_TAB_STRIP_HEIGHT, TerminalGrid,
    TerminalGridProps, TerminalScrollbarHandle, grid_dimensions_for_bounds, ime_cursor_bounds,
    measure_cell_size, pixel_to_viewport_cell, terminal_theme, terminal_viewport,
};

/// Key context applied to the terminal panel while focused.
///
/// The rendered element adds `Input` (`"TerminalPanel Input"`) so single-key
/// `RepoView && !Input` shortcuts never steal characters typed into the shell,
/// while the explicit `NoAction` bindings registered in [`bind`] prevent
/// unmodified navigation keys (`escape`, `enter`, `tab`, arrows, paging) from
/// reaching `Root` focus traversal or `RepoView` handlers.
pub const TERMINAL_PANEL: &str = "TerminalPanel";

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 16;
const BELL_FLASH_DURATION: Duration = Duration::from_millis(180);

/// Registers key overrides that keep terminal navigation, `Tab`, and `Escape`
/// in the PTY rather than triggering focus traversal or `RepoView` actions,
/// plus the terminal tab actions from [`crate::keymap::Group::Terminal`].
pub fn bind(cx: &mut App) {
    crate::keymap::bind_group(cx, crate::keymap::Group::Terminal);

    #[allow(unused_mut)]
    // `gpui_component::Root` binds `tab` and `shift-tab` to `Tab` / `TabPrev`
    // focus traversal in context `"Root"`. Binding them to `NoAction` here
    // cancels traversal while the terminal owns focus and lets `on_key_down`
    // forward `\t` and CSI Z to the PTY; `ctrl-tab` / `ctrl-shift-tab` have
    // different modifiers and continue reaching `Workspace`.
    let mut keys = vec![
        "escape",
        "enter",
        "tab",
        "shift-tab",
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

/// Why the terminal panel is asking its host [`RepoView`] to close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalCloseReason {
    /// The panel close button was clicked; keep existing sessions for the next toggle.
    Hide,
    /// The last terminal tab was closed; drop the empty panel so the next toggle
    /// starts a fresh session.
    LastTabClosed,
}

/// Callback invoked when the terminal panel should hide or be torn down.
pub(crate) type CloseHandler = Rc<dyn Fn(TerminalCloseReason, &mut Window, &mut App)>;

/// A fixed command for every tab in a panel, instead of the shell from the
/// settings. A remote tab runs its `ssh` here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TerminalCommand {
    pub program: String,
    pub args: Vec<String>,
    /// Tab title until the shell sets its own, so a remote tab reads
    /// `host:path` rather than "Terminal 1".
    pub title: String,
}

struct TerminalTab {
    id: u64,
    number: usize,
    session: Option<TerminalSession>,
    /// Fallback title from the panel's fixed command, under any title the
    /// shell reports through OSC.
    title: Option<String>,
    /// Set while the tab waits for the panel's first layout before it spawns its
    /// shell, so the shell starts at the real grid size instead of the default.
    spawn_pending: bool,
    snapshot: TerminalSnapshot,
    osc_title: Option<String>,
    spawn_error: Option<String>,
    exited_status: Option<Option<ExitStatus>>,
    bell_active: bool,
    bell_generation: u64,
    cols: u16,
    rows: u16,
    selection: Option<TerminalSelection>,
    dragging_selection: bool,
    scroll_accum_y: f32,
    scrollbar: TerminalScrollbarHandle,
    marked_text: Option<String>,
    marked_selected_utf16: Range<usize>,
    _event_tasks: Vec<Task<()>>,
    #[cfg(test)]
    quiet_rx: Option<std::sync::mpsc::Receiver<TerminalEvent>>,
}

impl Drop for TerminalTab {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            drop_in_background(session);
        }
    }
}

impl TerminalTab {
    fn new(id: u64, number: usize, cols: u16, rows: u16) -> Self {
        Self {
            id,
            number,
            session: None,
            title: None,
            spawn_pending: false,
            snapshot: TerminalSnapshot::default(),
            osc_title: None,
            spawn_error: None,
            exited_status: None,
            bell_active: false,
            bell_generation: 0,
            cols,
            rows,
            selection: None,
            dragging_selection: false,
            scroll_accum_y: 0.0,
            scrollbar: TerminalScrollbarHandle::new(|_| {}),
            marked_text: None,
            marked_selected_utf16: 0..0,
            _event_tasks: Vec::new(),
            #[cfg(test)]
            quiet_rx: None,
        }
    }

    fn display_title(&self) -> String {
        self.osc_title
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| self.title.clone())
            .unwrap_or_else(|| t!("terminal.tab_title", index = self.number).to_string())
    }

    fn sync_size(&mut self, cols: u16, rows: u16, cell_size: Size<Pixels>) {
        if self.cols == cols && self.rows == rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        if let Some(session) = &self.session {
            let cw = f32::from(cell_size.width).round().max(1.0) as u32;
            let ch = f32::from(cell_size.height).round().max(1.0) as u32;
            session.resize(cols, rows, cw, ch);
        }
    }
}

/// Interactive terminal panel managing a tabbed deck of PTY sessions.
pub struct TerminalPanel {
    tabs: TabDeck<TerminalTab>,
    next_tab_id: u64,
    working_dir: PathBuf,
    /// Set when the panel is not a plain local shell: a remote tab runs `ssh`
    /// in every tab instead of the configured embedded shell.
    command: Option<TerminalCommand>,
    focus_handle: FocusHandle,
    parent_focus: FocusHandle,
    on_close: Option<CloseHandler>,
    font_family: SharedString,
    font_size: Pixels,
    cell_size: Size<Pixels>,
    zoom: f32,
    cols: u16,
    rows: u16,
    last_bounds: Option<Bounds<Pixels>>,
    last_theme: Option<TerminalTheme>,
    /// Whether a new shell waits until the panel has been laid out before it
    /// starts, so it spawns at the real grid size rather than the placeholder.
    spawn_awaits_bounds: bool,
    /// Horizontal offset of the tab strip, so the active tab stays reachable
    /// once the tabs no longer fit the panel width.
    tab_strip_scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
    #[cfg(test)]
    quiet: bool,
}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// What a new panel is built from. Grouped so the constructors and `create`
/// stay readable, since the panel has more setup than one call wants.
struct PanelSetup {
    working_dir: PathBuf,
    /// A fixed command for every tab, instead of the configured shell.
    command: Option<TerminalCommand>,
    parent_focus: FocusHandle,
    on_close: Option<CloseHandler>,
    /// Whether a new tab waits for the panel's first layout before spawning.
    spawn_awaits_bounds: bool,
}

impl TerminalPanel {
    /// Creates a terminal panel with an initial shell tab in `working_dir`.
    ///
    /// The shell starts on the panel's first layout, so it spawns at the grid
    /// size the user actually sees.
    pub fn new(
        working_dir: PathBuf,
        parent_focus: FocusHandle,
        on_close: Option<CloseHandler>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let setup = PanelSetup {
            working_dir,
            command: None,
            parent_focus,
            on_close,
            spawn_awaits_bounds: true,
        };
        Self::create(setup, false, window, cx)
    }

    /// Creates a panel whose tabs always run `command`, the way a remote tab
    /// runs its `ssh` instead of a local shell.
    pub(crate) fn new_with_command(
        working_dir: PathBuf,
        command: TerminalCommand,
        parent_focus: FocusHandle,
        on_close: Option<CloseHandler>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let setup = PanelSetup {
            working_dir,
            command: Some(command),
            parent_focus,
            on_close,
            spawn_awaits_bounds: true,
        };
        Self::create(setup, false, window, cx)
    }

    #[cfg(test)]
    pub(crate) fn new_quiet(
        working_dir: PathBuf,
        parent_focus: FocusHandle,
        on_close: Option<CloseHandler>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let setup = PanelSetup {
            working_dir,
            command: None,
            parent_focus,
            on_close,
            // Quiet panels are never laid out and their events are drained by
            // hand, so their shells start right away.
            spawn_awaits_bounds: false,
        };
        Self::create(setup, true, window, cx)
    }

    /// Like [`Self::new_with_command`] without the layout wait, for tests.
    #[cfg(test)]
    pub(crate) fn new_quiet_with_command(
        working_dir: PathBuf,
        command: TerminalCommand,
        parent_focus: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let setup = PanelSetup {
            working_dir,
            command: Some(command),
            parent_focus,
            on_close: None,
            spawn_awaits_bounds: false,
        };
        Self::create(setup, true, window, cx)
    }

    /// Like [`Self::new_quiet`], but with production spawn timing so a test can
    /// observe the shell waiting for the panel's first bounds.
    #[cfg(all(test, not(windows)))]
    pub(crate) fn new_quiet_awaiting_bounds(
        working_dir: PathBuf,
        parent_focus: FocusHandle,
        on_close: Option<CloseHandler>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let setup = PanelSetup {
            working_dir,
            command: None,
            parent_focus,
            on_close,
            spawn_awaits_bounds: true,
        };
        Self::create(setup, true, window, cx)
    }

    fn create(
        setup: PanelSetup,
        #[cfg_attr(not(test), allow(unused_variables))] quiet: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let PanelSetup {
            working_dir,
            command,
            parent_focus,
            on_close,
            spawn_awaits_bounds,
        } = setup;
        // A verbatim `\\?\` directory would leave cmd.exe (and so every batch
        // file run from the shell) in the Windows directory instead.
        let working_dir = lamina_core::fs_path::strip_verbatim(working_dir);
        let focus_handle = cx.focus_handle();
        let settings = Settings::get(cx);
        let design = Design::new(cx.theme(), settings);
        let font_family = design.mono.clone();
        let font_size = px(design.code);
        let cell_size = measure_cell_size(window, &font_family, font_size);
        let theme = terminal_theme(&design);

        let in_sub = cx.on_focus_in(&focus_handle, window, |this, _window, cx| {
            if let Some(tab) = this.tabs.active()
                && let Some(session) = &tab.session
            {
                session.focus_changed(true);
            }
            cx.notify();
        });
        let out_sub = cx.on_focus_out(&focus_handle, window, |this, _event, _window, cx| {
            if let Some(tab) = this.tabs.active_mut() {
                tab.dragging_selection = false;
                if let Some(session) = &tab.session {
                    session.focus_changed(false);
                }
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
            tabs: TabDeck::new(),
            next_tab_id: 1,
            working_dir,
            command,
            focus_handle,
            parent_focus,
            on_close,
            font_family,
            font_size,
            cell_size,
            zoom: design.zoom,
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            last_bounds: None,
            last_theme: Some(theme.clone()),
            spawn_awaits_bounds,
            tab_strip_scroll: ScrollHandle::default(),
            _subscriptions: vec![in_sub, out_sub, appearance_sub, settings_sub],
            #[cfg(test)]
            quiet,
        };
        panel.open_new_tab_session(theme, false, window, cx);
        panel
    }

    /// Returns `true` when every terminal tab in the panel has been closed.
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// Terminates every active PTY session in the panel when its host view tears down.
    pub(crate) fn close_all_sessions(&mut self) {
        for tab in self.tabs.items_mut() {
            tab._event_tasks.clear();
            tab.scrollbar.set_scroll_handler(|_| {});
            if let Some(session) = tab.session.take() {
                drop_in_background(session);
            }
        }
    }

    #[cfg(all(test, not(windows)))]
    pub(crate) fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    #[cfg(all(test, not(windows)))]
    pub(crate) fn active_tab_index(&self) -> usize {
        self.tabs.active_index()
    }

    #[cfg(all(test, not(windows)))]
    pub(crate) fn tab_titles(&self) -> Vec<String> {
        self.tabs
            .items()
            .iter()
            .map(TerminalTab::display_title)
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> &TerminalSnapshot {
        &self.tabs.active().expect("active terminal tab").snapshot
    }

    #[cfg(test)]
    pub(crate) fn working_dir(&self) -> &std::path::Path {
        &self.working_dir
    }

    #[cfg(test)]
    pub(crate) fn has_session(&self) -> bool {
        self.tabs.active().is_some_and(|tab| tab.session.is_some())
    }

    #[cfg(test)]
    pub(crate) fn last_theme(&self) -> Option<&TerminalTheme> {
        self.last_theme.as_ref()
    }

    #[cfg(all(test, not(windows)))]
    pub(crate) fn active_child_pid(&self) -> Option<u32> {
        self.tabs
            .active()
            .and_then(|tab| tab.session.as_ref())
            .and_then(TerminalSession::child_pid)
    }

    #[cfg(all(test, not(windows)))]
    pub(crate) fn tab_child_pids(&self) -> Vec<Option<u32>> {
        self.tabs
            .items()
            .iter()
            .map(|tab| tab.session.as_ref().and_then(TerminalSession::child_pid))
            .collect()
    }

    #[cfg(all(test, not(windows)))]
    pub(crate) fn active_exited_status(&self) -> Option<Option<&ExitStatus>> {
        self.tabs
            .active()
            .and_then(|tab| tab.exited_status.as_ref().map(Option::as_ref))
    }

    #[cfg(all(test, not(windows)))]
    pub(crate) fn active_bell_active(&self) -> bool {
        self.tabs.active().is_some_and(|tab| tab.bell_active)
    }

    #[cfg(all(test, not(windows)))]
    pub(crate) fn active_osc_title(&self) -> Option<&str> {
        self.tabs.active().and_then(|tab| tab.osc_title.as_deref())
    }

    #[cfg(all(test, not(windows)))]
    pub(crate) fn tab_cols_rows(&self) -> Vec<(u16, u16)> {
        self.tabs
            .items()
            .iter()
            .map(|tab| (tab.cols, tab.rows))
            .collect()
    }

    #[cfg(all(test, not(windows)))]
    pub(crate) fn active_selection(&self) -> Option<TerminalSelection> {
        self.tabs.active().and_then(|tab| tab.selection)
    }

    #[cfg(all(test, not(windows)))]
    pub(crate) fn set_active_selection_for_test(&mut self, sel: Option<TerminalSelection>) {
        if let Some(tab) = self.tabs.active_mut() {
            tab.selection = sel;
        }
    }

    #[cfg(all(test, not(windows)))]
    pub(crate) fn active_tab_id(&self) -> Option<u64> {
        self.tabs.active().map(|tab| tab.id)
    }

    #[cfg(all(test, not(windows)))]
    pub(crate) fn flush_quiet_events(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<u64> = self.tabs.items().iter().map(|tab| tab.id).collect();
        for tab_id in ids {
            let mut pending = Vec::new();
            if let Some(tab) = self.tabs.items().iter().find(|t| t.id == tab_id)
                && let Some(rx) = &tab.quiet_rx
            {
                while let Ok(ev) = rx.try_recv() {
                    pending.push(ev);
                }
            }
            for ev in pending {
                self.handle_terminal_event(tab_id, ev, cx);
            }
        }
    }

    fn tab_by_id_mut(&mut self, tab_id: u64) -> Option<&mut TerminalTab> {
        self.tabs.items_mut().iter_mut().find(|t| t.id == tab_id)
    }

    fn current_theme(&mut self, cx: &App) -> TerminalTheme {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let theme = terminal_theme(&design);
        self.last_theme = Some(theme.clone());
        theme
    }

    /// Opens a new terminal tab in the repository working directory and focuses it.
    pub fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let theme = self.current_theme(cx);
        self.open_new_tab_session(theme, true, window, cx);
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn open_new_tab_session(
        &mut self,
        theme: TerminalTheme,
        focused: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(prev) = self.tabs.active_mut() {
            prev.dragging_selection = false;
            if let Some(session) = &prev.session {
                session.focus_changed(false);
            }
        }

        let id = self.next_tab_id;
        self.next_tab_id = self.next_tab_id.wrapping_add(1);
        // One past the highest open tab, so names keep their order while tabs
        // are open and start over at 1 once every tab has been closed.
        let number = self
            .tabs
            .items()
            .iter()
            .map(|tab| tab.number)
            .max()
            .map_or(1, |highest| highest + 1);

        let mut tab = TerminalTab::new(id, number, self.cols, self.rows);
        if let Some(command) = &self.command {
            tab.title = Some(command.title.clone());
        }
        self.tabs.open(tab);
        self.reveal_tab(self.tabs.active_index());
        self.spawn_session_for_tab(id, theme, focused, cx);
    }

    fn reveal_tab(&self, index: usize) {
        ui::reveal_tab(&self.tab_strip_scroll, index);
    }

    /// Switches to the terminal tab at `index` and returns keyboard focus to the PTY.
    pub fn select_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        if index != self.tabs.active_index() {
            if let Some(prev) = self.tabs.active_mut() {
                prev.dragging_selection = false;
                if let Some(session) = &prev.session {
                    session.focus_changed(false);
                }
            }
            self.tabs.select(index);
            let cols = self.cols;
            let rows = self.rows;
            let cell_size = self.cell_size;
            if let Some(active) = self.tabs.active_mut() {
                active.sync_size(cols, rows, cell_size);
                if let Some(session) = &active.session {
                    session.focus_changed(true);
                }
            }
        }
        self.reveal_tab(self.tabs.active_index());
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Cycles forward (`delta > 0`) or backward (`delta < 0`) across terminal tabs.
    pub fn cycle_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            window.focus(&self.focus_handle, cx);
            return;
        }
        if let Some(prev) = self.tabs.active_mut() {
            prev.dragging_selection = false;
            if let Some(session) = &prev.session {
                session.focus_changed(false);
            }
        }
        if self.tabs.cycle(delta).is_some() {
            let cols = self.cols;
            let rows = self.rows;
            let cell_size = self.cell_size;
            if let Some(active) = self.tabs.active_mut() {
                active.sync_size(cols, rows, cell_size);
                if let Some(session) = &active.session {
                    session.focus_changed(true);
                }
            }
            self.reveal_tab(self.tabs.active_index());
            cx.notify();
        }
        window.focus(&self.focus_handle, cx);
    }

    /// Closes the terminal tab at `index`. Closing the last tab hides the panel.
    pub fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        match self.tabs.close(index) {
            CloseOutcome::OutOfBounds => {}
            CloseOutcome::ClosedLast(_removed) => {
                cx.notify();
                if let Some(on_close) = &self.on_close {
                    on_close(TerminalCloseReason::LastTabClosed, window, cx);
                } else {
                    window.focus(&self.parent_focus, cx);
                }
            }
            CloseOutcome::Closed { .. } => {
                let cols = self.cols;
                let rows = self.rows;
                let cell_size = self.cell_size;
                if let Some(active) = self.tabs.active_mut() {
                    active.sync_size(cols, rows, cell_size);
                    if let Some(session) = &active.session {
                        session.focus_changed(true);
                    }
                }
                self.reveal_tab(self.tabs.active_index());
                window.focus(&self.focus_handle, cx);
                cx.notify();
            }
        }
    }

    /// Closes the currently active terminal tab.
    pub fn close_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_tab(self.tabs.active_index(), window, cx);
    }

    /// Re-applies font metrics and `TerminalTheme` when appearance or settings change.
    pub fn sync_appearance(&mut self, window: Option<&Window>, cx: &mut Context<Self>) {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let theme = terminal_theme(&design);
        let mut changed = false;

        if self.last_theme.as_ref() != Some(&theme) {
            self.last_theme = Some(theme.clone());
            for tab in self.tabs.items() {
                if let Some(session) = &tab.session {
                    session.set_theme(theme.clone());
                }
            }
            changed = true;
        }

        let new_font = design.mono.clone();
        let new_size = px(design.code);
        let new_zoom = design.zoom;
        if self.zoom != new_zoom {
            self.zoom = new_zoom;
            if let Some(bounds) = self.last_bounds {
                self.resize_for_bounds(bounds, cx);
            }
            changed = true;
        }
        if let Some(window) = window
            && (self.font_family != new_font || self.font_size != new_size)
        {
            self.font_family = new_font;
            self.font_size = new_size;
            self.cell_size = measure_cell_size(window, &self.font_family, self.font_size);
            if let Some(bounds) = self.last_bounds {
                self.resize_for_bounds(bounds, cx);
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

    /// Restarts the active tab's shell process in the same working directory.
    pub fn restart(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab_id) = self.tabs.active().map(|tab| tab.id) else {
            return;
        };
        let theme = self.current_theme(cx);
        let focused = self.focus_handle.is_focused(window);
        self.spawn_session_for_tab(tab_id, theme, focused, cx);
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn spawn_session_for_tab(
        &mut self,
        tab_id: u64,
        theme: TerminalTheme,
        focused: bool,
        cx: &mut Context<Self>,
    ) {
        // A shell started before the panel has been laid out only knows the
        // placeholder grid, and ConPTY keeps wrapping its output at that width
        // until something resizes it. The panel spawns such a tab from
        // `resize_for_bounds` instead, once the real grid size is known.
        if !self.spawn_ready() {
            if let Some(tab) = self.tab_by_id_mut(tab_id) {
                tab.spawn_pending = true;
            }
            return;
        }

        let configured = Settings::get(cx).tools.embedded_shell.as_deref();
        let parsed = match &self.command {
            // A fixed command replaces the configured shell entirely: the
            // settings slot describes a local shell, and ssh has its own argv.
            Some(command) => Ok((Some(command.program.clone()), command.args.clone())),
            None => parse_shell_command(configured),
        };
        let panel_cols = self.cols;
        let panel_rows = self.rows;
        let cols = panel_cols.max(2);
        let rows = panel_rows.max(2);
        let cell_width_px = f32::from(self.cell_size.width).round().max(1.0) as u32;
        let cell_height_px = f32::from(self.cell_size.height).round().max(1.0) as u32;
        let working_dir = self.working_dir.clone();
        #[cfg(test)]
        let quiet = self.quiet;

        let Some(tab) = self.tab_by_id_mut(tab_id) else {
            return;
        };
        tab._event_tasks.clear();
        tab.scrollbar.set_scroll_handler(|_| {});
        if let Some(old) = tab.session.take() {
            drop_in_background(old);
        }
        tab.osc_title = None;
        tab.exited_status = None;
        tab.spawn_error = None;
        tab.selection = None;
        tab.dragging_selection = false;
        tab.marked_text = None;
        tab.marked_selected_utf16 = 0..0;
        tab.spawn_pending = false;
        tab.cols = panel_cols;
        tab.rows = panel_rows;

        let (shell, shell_args) = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                tab.spawn_error = Some(t!("terminal.invalid_shell", error = error).to_string());
                return;
            }
        };

        let options = TerminalOptions {
            cols,
            rows,
            cell_width_px,
            cell_height_px,
            theme,
            working_dir: Some(working_dir),
            shell,
            shell_args,
            ..TerminalOptions::default()
        };

        #[cfg(test)]
        if quiet {
            let (tx, rx) = std::sync::mpsc::channel();
            tab.quiet_rx = Some(rx);
            match TerminalSession::spawn_with_callback(
                options,
                Box::new(move |event| {
                    let _ = tx.send(event);
                }),
            ) {
                Ok(session) => {
                    if focused {
                        session.focus_changed(true);
                    }
                    let snapshot = session.snapshot();
                    tab.scrollbar.sync_snapshot(snapshot.scrollbar);
                    tab.snapshot = snapshot;
                    tab.session = Some(session);
                }
                Err(error) => {
                    tab.spawn_error =
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
                if focused {
                    session.focus_changed(true);
                }
                let snapshot = session.snapshot();
                tab.scrollbar.sync_snapshot(snapshot.scrollbar);
                tab.snapshot = snapshot;
                tab.session = Some(session);

                let (scroll_tx, mut scroll_rx) = futures::channel::mpsc::unbounded::<usize>();
                tab.scrollbar.set_scroll_handler(move |row| {
                    let _ = scroll_tx.unbounded_send(row);
                });

                let event_task = cx.spawn(async move |this, cx| {
                    while let Some(event) = wake_rx.next().await {
                        let alive = this.update(cx, |panel, cx| {
                            panel.handle_terminal_event(tab_id, event, cx);
                        });
                        if alive.is_err() {
                            break;
                        }
                    }
                });
                let scroll_task = cx.spawn(async move |this, cx| {
                    while let Some(row) = scroll_rx.next().await {
                        let alive = this.update(cx, |panel, _| {
                            if let Some(tab) = panel.tab_by_id_mut(tab_id)
                                && let Some(session) = &tab.session
                            {
                                session.scroll(ScrollAction::Row(row));
                            }
                        });
                        if alive.is_err() {
                            break;
                        }
                    }
                });
                if let Some(tab) = self.tab_by_id_mut(tab_id) {
                    tab._event_tasks = vec![event_task, scroll_task];
                }
            }
            Err(error) => {
                tab.spawn_error =
                    Some(t!("terminal.start_failed", error = error.to_string()).to_string());
            }
        }
    }

    fn handle_terminal_event(&mut self, tab_id: u64, event: TerminalEvent, cx: &mut Context<Self>) {
        let Some(tab) = self.tab_by_id_mut(tab_id) else {
            return;
        };
        match event {
            TerminalEvent::SnapshotChanged => {
                if let Some(session) = &tab.session {
                    tab.snapshot = session.snapshot();
                    tab.scrollbar.sync_snapshot(tab.snapshot.scrollbar);
                    cx.notify();
                }
            }
            TerminalEvent::TitleChanged(title) => {
                let trimmed = title.trim();
                tab.osc_title = (!trimmed.is_empty()).then_some(title);
                cx.notify();
            }
            TerminalEvent::Bell => {
                self.trigger_bell_flash(tab_id, cx);
            }
            TerminalEvent::ChildExited(status) => {
                if let Some(session) = &tab.session {
                    tab.snapshot = session.snapshot();
                    tab.scrollbar.sync_snapshot(tab.snapshot.scrollbar);
                }
                tab.exited_status = Some(status);
                cx.notify();
            }
            TerminalEvent::ResizeFailed(error) => {
                log::warn!("terminal resize failed: {error}");
            }
        }
    }

    fn trigger_bell_flash(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(tab) = self.tab_by_id_mut(tab_id) else {
            return;
        };
        tab.bell_active = true;
        tab.bell_generation = tab.bell_generation.wrapping_add(1);
        let generation = tab.bell_generation;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(BELL_FLASH_DURATION).await;
            let _ = this.update(cx, |panel, cx| {
                if let Some(tab) = panel.tab_by_id_mut(tab_id)
                    && tab.bell_generation == generation
                    && tab.bell_active
                {
                    tab.bell_active = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Whether the panel knows the grid size a new shell should start at.
    fn spawn_ready(&self) -> bool {
        !self.spawn_awaits_bounds || self.last_bounds.is_some()
    }

    fn resize_for_bounds(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        self.last_bounds = Some(bounds);
        let (new_cols, new_rows) = grid_dimensions_for_bounds(bounds, self.cell_size, self.zoom);
        self.cols = new_cols;
        self.rows = new_rows;
        let cell_size = self.cell_size;
        for tab in self.tabs.items_mut() {
            tab.sync_size(new_cols, new_rows, cell_size);
        }
        self.spawn_pending_tabs(cx);
    }

    /// Starts the shells of tabs created before the panel had real bounds.
    fn spawn_pending_tabs(&mut self, cx: &mut Context<Self>) {
        let pending: Vec<u64> = self
            .tabs
            .items()
            .iter()
            .filter(|tab| tab.spawn_pending)
            .map(|tab| tab.id)
            .collect();
        if pending.is_empty() {
            return;
        }
        // This usually runs from the grid's prepaint, so the frame being built
        // still shows the empty grid; the notify below (or the session's first
        // output event) repaints it.
        let theme = self.current_theme(cx);
        for tab_id in pending {
            self.spawn_session_for_tab(tab_id, theme.clone(), false, cx);
        }
        // A failed spawn only sets `spawn_error`, which needs a frame to show.
        cx.notify();
    }

    fn copy_selection(&self, cx: &mut App) -> bool {
        let Some(tab) = self.tabs.active() else {
            return false;
        };
        let Some(selection) = tab.selection else {
            return false;
        };
        let Some(session) = &tab.session else {
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
        let Some(tab) = self.tabs.active_mut() else {
            return false;
        };
        let Some(session) = &tab.session else {
            return false;
        };
        if let Some(item) = cx.read_from_clipboard()
            && let Some(text) = item.text()
            && !text.is_empty()
        {
            session.paste(text);
            if tab.selection.take().is_some() {
                cx.notify();
            }
            return true;
        }
        false
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.active() else {
            return;
        };
        if tab.marked_text.is_some() || is_ime_key(&event.keystroke.key) {
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
                if let Some(tab) = self.tabs.active_mut()
                    && let Some(session) = &tab.session
                {
                    session.write_str("\x0c");
                    tab.selection = None;
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

        // Keep an exited tab visible with its final output and exit badge so
        // the user can read or copy it (or click Restart), and close it on any
        // ordinary key press; closing the last tab hides the panel.
        if self
            .tabs
            .active()
            .is_some_and(|tab| tab.exited_status.is_some())
            && !ks.modifiers.control
            && !ks.modifiers.platform
            && !ks.modifiers.alt
        {
            self.close_active_tab(window, cx);
            cx.stop_propagation();
            return;
        }

        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        let Some(session) = &tab.session else {
            return;
        };

        if let Some(term_event) = keystroke_to_term_event(ks) {
            session.send_key(term_event);
            if tab.selection.take().is_some() {
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
            if tab.selection.take().is_some() {
                cx.notify();
            }
            cx.stop_propagation();
        }
    }

    fn cell_at(&self, position: Point<Pixels>) -> Option<(u16, u16)> {
        let bounds = self.last_bounds?;
        let tab = self.tabs.active()?;
        Some(pixel_to_viewport_cell(
            position,
            bounds,
            self.cell_size,
            tab.snapshot.cols,
            tab.snapshot.rows,
            self.zoom,
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
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        let Some(session) = &tab.session else {
            return;
        };

        if tab.snapshot.mouse_tracking && !event.modifiers.shift {
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

        let point = BufferPoint::from_viewport(&tab.snapshot.scrollbar, col, row);
        match event.click_count {
            2 => {
                tab.selection = session.select_word(point).ok().flatten();
                tab.dragging_selection = false;
            }
            n if n >= 3 => {
                tab.selection = session.select_line(point).ok().flatten();
                tab.dragging_selection = false;
            }
            _ => {
                tab.selection = Some(if event.modifiers.alt {
                    TerminalSelection::rectangular(point, point)
                } else {
                    TerminalSelection::new(point, point)
                });
                tab.dragging_selection = true;
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
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        let Some(session) = &tab.session else {
            return;
        };

        if tab.snapshot.mouse_tracking && !event.modifiers.shift {
            let button = event.pressed_button.and_then(gpui_button_to_term);
            session.handle_mouse(TermMouseEvent::motion(
                button,
                MousePosition::Cell { col, row },
                gpui_mods_to_term(event.modifiers),
            ));
            return;
        }

        if tab.dragging_selection && event.pressed_button == Some(MouseButton::Left) {
            let point = BufferPoint::from_viewport(&tab.snapshot.scrollbar, col, row);
            if let Some(sel) = &mut tab.selection {
                sel.end = point;
                sel.rectangle = event.modifiers.alt;
                cx.notify();
            }
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        let was_dragging = tab.dragging_selection;
        tab.dragging_selection = false;

        let Some((col, row)) = self.cell_at(event.position) else {
            return;
        };
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        let Some(session) = &tab.session else {
            return;
        };

        if tab.snapshot.mouse_tracking && !event.modifiers.shift {
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
            && let Some(sel) = tab.selection
            && sel.start == sel.end
        {
            tab.selection = None;
            cx.notify();
        }
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position = match self.cell_at(event.position) {
            Some((col, row)) => MousePosition::Cell { col, row },
            None => MousePosition::Pixels {
                x: f32::from(event.position.x),
                y: f32::from(event.position.y),
            },
        };
        let line_height = f32::from(self.cell_size.height).max(1.0);
        let delta_lines = match event.delta {
            ScrollDelta::Lines(lines) => lines.y * 3.0,
            ScrollDelta::Pixels(pixels) => f32::from(pixels.y) / line_height,
        };
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        let Some(session) = &tab.session else {
            return;
        };
        tab.scroll_accum_y += delta_lines;
        let whole_rows = tab.scroll_accum_y.trunc() as i32;
        if whole_rows != 0 {
            tab.scroll_accum_y -= whole_rows as f32;
            session.handle_mouse(TermMouseEvent::wheel(
                0,
                whole_rows,
                position,
                gpui_mods_to_term(event.modifiers),
            ));
            cx.stop_propagation();
        }
    }

    #[cfg(all(test, not(windows)))]
    pub(crate) fn display_title(&self) -> String {
        self.tabs
            .active()
            .map(TerminalTab::display_title)
            .unwrap_or_default()
    }

    fn header(&self, design: &Design, cx: &mut Context<Self>) -> AnyElement {
        let active_index = self.tabs.active_index();
        let active_tab = self.tabs.active();
        let weak = cx.weak_entity();

        let tabs = self
            .tabs
            .items()
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let is_active = index == active_index;
                let separator = !is_active && index > 0 && index - 1 != active_index;
                let title = tab.display_title();
                let select_panel = weak.clone();
                let close_panel = weak.clone();
                ui::tab(
                    ("terminal-tab", tab.id as usize),
                    index,
                    title.clone(),
                    is_active,
                    design,
                )
                .compact(true)
                .active_bg(design.terminal_background)
                .separator(separator)
                .busy(tab.bell_active)
                .always_show_close(true)
                .tooltip(title)
                .on_select(move |_, window, cx| {
                    let _ = select_panel.update(cx, |this, cx| this.select_tab(index, window, cx));
                })
                .on_close(move |window, cx| {
                    let _ = close_panel.update(cx, |this, cx| this.close_tab(index, window, cx));
                })
            })
            .collect::<Vec<_>>();

        let strip_area = div()
            .flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .items_end()
            // The strip is full width by default, which pushed the "+" button
            // out of the panel; here it only takes what its tabs need and
            // shrinks (scrolling) when they do not fit.
            .child(
                ui::compact_tab_strip("terminal-tab-strip", design, &self.tab_strip_scroll)
                    .w_auto()
                    .flex_shrink(1.0)
                    .min_w_0()
                    .children(tabs),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .h_full()
                    .px_1()
                    .border_b_1()
                    .border_color(design.border)
                    .child(
                        ui::icon_button("terminal-new-tab", IconName::Plus)
                            .size(ButtonSize::Tiny)
                            .ghost()
                            .tooltip(t!("terminal.new_tab"))
                            .on_click(cx.listener(|this, _, window, cx| this.new_tab(window, cx))),
                    ),
            )
            // Carries the strip's bottom rule on to the panel's right edge.
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .border_b_1()
                    .border_color(design.border),
            );

        let mut right = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .h_full()
            .px_2()
            .border_b_1()
            .border_color(design.border);

        if let Some(tab) = active_tab {
            if let Some(exited) = &tab.exited_status {
                let (label, tone) = format_exit_status(exited.as_ref());
                right = right.child(ui::status_badge(label, tone, design));
            }
            if tab.exited_status.is_some() || tab.spawn_error.is_some() {
                right = right.child(
                    ui::button("terminal-restart", t!("terminal.restart").to_string())
                        .size(ButtonSize::Tiny)
                        .on_click(cx.listener(|this, _, window, cx| this.restart(window, cx))),
                );
            }
        }

        let on_close = self.on_close.clone();
        let parent_focus = self.parent_focus.clone();
        right = right.child(
            ui::icon_button("terminal-close", IconName::X)
                .size(ButtonSize::Tiny)
                .ghost()
                .tooltip(t!("terminal.hide_panel"))
                .on_click(move |_, window, cx| {
                    if let Some(on_close) = &on_close {
                        on_close(TerminalCloseReason::Hide, window, cx);
                    } else {
                        window.focus(&parent_focus, cx);
                    }
                }),
        );

        div()
            .flex()
            .items_center()
            .justify_between()
            .h(design.px(COMPACT_TAB_STRIP_HEIGHT))
            .flex_none()
            .bg(design.chrome)
            .border_t_1()
            .border_color(design.border)
            .child(strip_area)
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
        let marked = self.tabs.active()?.marked_text.as_deref()?;
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
        let tab = self.tabs.active()?;
        Some(UTF16Selection {
            range: tab.marked_selected_utf16.clone(),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.tabs
            .active()?
            .marked_text
            .as_deref()
            .map(|text| 0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.active_mut()
            && tab.marked_text.take().is_some()
        {
            tab.marked_selected_utf16 = 0..0;
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
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        tab.marked_text = None;
        tab.marked_selected_utf16 = 0..0;
        if !text.is_empty()
            && let Some(session) = &tab.session
        {
            session.write_str(text);
            tab.selection = None;
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
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        if new_text.is_empty() {
            tab.marked_text = None;
            tab.marked_selected_utf16 = 0..0;
        } else {
            let utf16_len = new_text.encode_utf16().count();
            tab.marked_text = Some(new_text.to_string());
            tab.marked_selected_utf16 = new_selected_range
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
        let tab = self.tabs.active()?;
        Some(ime_cursor_bounds(
            element_bounds,
            self.cell_size,
            tab.snapshot.cursor,
            self.zoom,
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
            for tab in self.tabs.items() {
                if let Some(session) = &tab.session {
                    session.set_theme(expected_theme.clone());
                }
            }
        }
        self.zoom = design.zoom;
        if self.font_family != design.mono || self.font_size != px(design.code) {
            self.font_family = design.mono.clone();
            self.font_size = px(design.code);
            self.cell_size = measure_cell_size(window, &self.font_family, self.font_size);
        }

        let focused = self.focus_handle.is_focused(window);
        let entity = cx.entity().clone();
        let weak = cx.weak_entity();
        let bg = design.terminal_background;

        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .min_w_0()
            .bg(bg)
            .track_focus(&self.focus_handle)
            .key_context("TerminalPanel Input")
            .on_action(cx.listener(|this, _: &NewTerminalTab, window, cx| {
                this.new_tab(window, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &CloseTerminalTab, window, cx| {
                this.close_active_tab(window, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &NextTerminalTab, window, cx| {
                this.cycle_tab(1, window, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &PreviousTerminalTab, window, cx| {
                this.cycle_tab(-1, window, cx);
                cx.stop_propagation();
            }))
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

        if let Some(tab) = self.tabs.active() {
            if let Some(error) = &tab.spawn_error {
                root = root.child(ui::banner(BannerKind::Danger, error.clone(), &design));
            }
            let grid = TerminalGrid::new(TerminalGridProps {
                snapshot: tab.snapshot.clone(),
                selection: tab.selection,
                selection_color: design.terminal_selection,
                default_cursor_color: design.terminal_cursor,
                ime_bg: design.field,
                ime_fg: design.text,
                font_family: self.font_family.clone(),
                font_size: self.font_size,
                cell_size: self.cell_size,
                zoom: design.zoom,
                focused,
                marked_text: tab.marked_text.clone(),
                focus_handle: self.focus_handle.clone(),
                input_entity: entity,
                scrollbar: tab.scrollbar.clone(),
                on_bounds_changed: Box::new(move |bounds, cx| {
                    let _ = weak.update(cx, |panel, cx| {
                        panel.resize_for_bounds(bounds, cx);
                    });
                }),
            });
            root = root.child(terminal_viewport(grid, &tab.scrollbar));
        }

        root
    }
}

/// Converts a GPUI [`Keystroke`] into a `lamina-term` [`TermKeyEvent`] when a
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

    #[cfg(all(test, not(windows)))]
    #[test]
    fn a_fixed_command_panel_titles_its_tabs_with_the_target() {
        // The title is the only part of the fixed command a panel can show
        // without a window, so this covers the fallback the tab strip reads.
        let mut tab = TerminalTab::new(1, 1, 80, 24);
        tab.title = Some("git@example.com:/srv/my repo".to_owned());
        assert_eq!(tab.display_title(), "git@example.com:/srv/my repo");
        assert_eq!(
            TerminalTab::new(1, 1, 80, 24).display_title(),
            t!("terminal.tab_title", index = 1).to_string(),
            "a plain shell keeps the numbered title"
        );
    }

    #[cfg(all(test, not(windows)))]
    #[gpui::test]
    fn a_fixed_command_replaces_the_configured_shell(cx: &mut gpui::TestAppContext) {
        use std::time::Instant;

        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            let mut settings = Settings::default();
            // A shell that cannot start: only the fixed command can print.
            settings.tools.embedded_shell = Some("/nonexistent/lamina-shell".to_owned());
            cx.set_global(settings);
        });
        let dir = tempfile::TempDir::new().expect("tempdir");
        let command = TerminalCommand {
            program: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), "printf READY".to_owned()],
            title: "git@example.com:/srv/my repo".to_owned(),
        };
        let window = cx.add_window(|window, cx| {
            let parent = cx.focus_handle();
            TerminalPanel::new_quiet_with_command(
                dir.path().to_path_buf(),
                command,
                parent,
                window,
                cx,
            )
        });
        let panel = window.root(cx).expect("root panel");
        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        assert_eq!(
            panel.read_with(&vcx, |panel, _| panel.tab_titles()),
            ["git@example.com:/srv/my repo"]
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ready = false;
        while Instant::now() < deadline {
            panel.update(&mut vcx, |panel, cx| {
                panel.flush_quiet_events(cx);
                ready = panel.snapshot().text().contains("READY");
            });
            if ready {
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        assert!(ready, "the fixed command is what the tab runs");
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
                "/bin/sh -c \"printf '\\033]0;lamina-title\\007\\007hello-pty\\n'; exit 7\""
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
                saw_exit = panel.active_exited_status().is_some();
            });
            if saw_exit {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        panel.read_with(&vcx, |panel, _| {
            assert!(saw_exit, "timed out waiting for shell exit event");
            assert_eq!(panel.display_title(), "lamina-title");
            assert!(
                panel.active_bell_active(),
                "bell flash must activate on BEL"
            );
            let status = panel.active_exited_status().flatten().expect("exit status");
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
            assert!(panel.active_exited_status().is_none());
            assert!(panel.active_osc_title().is_none());
            assert_eq!(
                panel.display_title(),
                t!("terminal.tab_title", index = 1).to_string()
            );
            assert!(panel.has_session());
        });
    }

    #[cfg(not(windows))]
    #[gpui::test]
    fn tab_and_shift_tab_stay_in_terminal_and_reach_pty(cx: &mut gpui::TestAppContext) {
        use std::cell::Cell;
        use std::rc::Rc;
        use std::time::{Duration, Instant};

        use gpui::{
            AppContext as _, Entity, InteractiveElement as _, ParentElement as _, Styled as _, div,
        };
        use gpui_component::Root;

        use crate::actions::{NextTab, PreviousTab, WORKSPACE};

        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            crate::actions::bind(cx);
            super::bind(cx);

            let mut settings = Settings::default();
            settings.tools.embedded_shell = Some(
                "/bin/sh -c \"stty raw -echo; printf 'READY\\n'; dd bs=1 count=4 2>/dev/null | tr '\\t\\033' 'TE'; printf '\\n'; sleep 2\""
                    .to_owned(),
            );
            cx.set_global(settings);
        });

        struct TabProbe {
            workspace_focus: FocusHandle,
            sibling_focus: FocusHandle,
            panel: Entity<TerminalPanel>,
            tab_switches: Rc<Cell<isize>>,
        }

        impl Render for TabProbe {
            fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .track_focus(&self.workspace_focus)
                    .key_context(WORKSPACE)
                    .on_action(cx.listener(|this, _: &NextTab, _, _| {
                        this.tab_switches.set(this.tab_switches.get() + 1);
                    }))
                    .on_action(cx.listener(|this, _: &PreviousTab, _, _| {
                        this.tab_switches.set(this.tab_switches.get() - 1);
                    }))
                    .child(
                        div()
                            .id("sibling-tab-stop")
                            .track_focus(&self.sibling_focus)
                            .tab_stop(true)
                            .h(px(20.0)),
                    )
                    .child(div().flex_1().child(self.panel.clone()))
            }
        }

        let dir = tempfile::TempDir::new().expect("tempdir");
        let workdir = dir.path().to_path_buf();
        let tab_switches = Rc::new(Cell::new(0isize));
        let tab_switches_clone = tab_switches.clone();

        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                let probe = cx.new(|cx| {
                    let workspace_focus = cx.focus_handle();
                    let sibling_focus = cx.focus_handle();
                    let parent = workspace_focus.clone();
                    let panel =
                        cx.new(|cx| TerminalPanel::new_quiet(workdir, parent, None, window, cx));
                    TabProbe {
                        workspace_focus,
                        sibling_focus,
                        panel,
                        tab_switches: tab_switches_clone,
                    }
                });
                cx.new(|cx| Root::new(probe, window, cx))
            })
            .expect("open root window")
        });

        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        let (panel, sibling_focus) = vcx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let root = window.root::<Root>().flatten().expect("Root view");
            let probe = root
                .read(cx)
                .view()
                .clone()
                .downcast::<TabProbe>()
                .expect("TabProbe");
            let panel = probe.read(cx).panel.clone();
            let sibling_focus = probe.read(cx).sibling_focus.clone();
            let panel_focus = panel.read(cx).focus_handle(cx);
            window.focus(&panel_focus, cx);
            window.draw(cx).clear(cx);
            assert!(panel_focus.is_focused(window));
            (panel, sibling_focus)
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ready = false;
        while Instant::now() < deadline {
            panel.update(&mut vcx, |panel, cx| {
                panel.flush_quiet_events(cx);
                ready = panel.snapshot().text().contains("READY");
            });
            if ready {
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        assert!(ready, "timed out waiting for shell probe READY");

        // Send Tab and Shift-Tab while TerminalPanel has focus inside Root.
        vcx.simulate_keystrokes("tab shift-tab");
        vcx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let panel_focus = panel.read(cx).focus_handle(cx);
            assert!(
                panel_focus.is_focused(window),
                "Tab and Shift-Tab must keep keyboard focus in the terminal"
            );
            assert!(
                !sibling_focus.is_focused(window),
                "Tab must not traverse focus to sibling tab stop"
            );
        });

        let mut saw_bytes = false;
        while Instant::now() < deadline {
            panel.update(&mut vcx, |panel, cx| {
                panel.flush_quiet_events(cx);
                saw_bytes = panel.snapshot().text().contains("TE[Z");
            });
            if saw_bytes {
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        assert!(
            saw_bytes,
            "expected \\t (T) and \\x1b[Z (E[Z) echoed by PTY, got: {:?}",
            panel.read_with(&vcx, |p, _| p.snapshot().text())
        );

        // Ctrl-Tab and Ctrl-Shift-Tab must still switch repository tabs in Workspace.
        vcx.simulate_keystrokes("ctrl-tab");
        assert_eq!(tab_switches.get(), 1, "ctrl-tab must trigger NextTab");
        vcx.simulate_keystrokes("ctrl-shift-tab");
        assert_eq!(
            tab_switches.get(),
            0,
            "ctrl-shift-tab must trigger PreviousTab"
        );
    }

    #[cfg(not(windows))]
    #[gpui::test]
    fn multiple_terminal_tabs_isolate_titles_ime_selection_and_resize(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            let mut settings = Settings::default();
            settings.tools.embedded_shell = Some("/bin/sh".to_owned());
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
            assert_eq!(panel.tab_count(), 1);
            assert_eq!(panel.active_tab_index(), 0);
            assert_eq!(
                panel.tab_titles(),
                vec![t!("terminal.tab_title", index = 1).to_string()]
            );
            let tab0_id = panel.active_tab_id().expect("tab 0 id");

            panel.new_tab(window, cx);
            assert_eq!(panel.tab_count(), 2);
            assert_eq!(panel.active_tab_index(), 1);
            assert_eq!(
                panel.tab_titles(),
                vec![
                    t!("terminal.tab_title", index = 1).to_string(),
                    t!("terminal.tab_title", index = 2).to_string(),
                ]
            );
            let pids = panel.tab_child_pids();
            assert!(pids[0].is_some() && pids[1].is_some());
            assert_ne!(pids[0], pids[1], "each tab must own a separate PTY session");

            // OSC title on tab 0 updates only tab 0.
            panel.handle_terminal_event(
                tab0_id,
                TerminalEvent::TitleChanged("custom-shell".to_owned()),
                cx,
            );
            assert_eq!(
                panel.tab_titles(),
                vec![
                    "custom-shell".to_owned(),
                    t!("terminal.tab_title", index = 2).to_string(),
                ]
            );

            // IME marked text and selection are tracked independently per tab.
            panel.replace_and_mark_text_in_range(None, "かな", Some(1..2), window, cx);
            let sel = TerminalSelection::new(BufferPoint::new(0, 0), BufferPoint::new(0, 4));
            panel.set_active_selection_for_test(Some(sel));
            assert_eq!(panel.marked_text_range(window, cx), Some(0..2));
            assert_eq!(panel.active_selection(), Some(sel));

            panel.select_tab(0, window, cx);
            assert_eq!(panel.active_tab_index(), 0);
            assert_eq!(panel.marked_text_range(window, cx), None);
            assert_eq!(panel.active_selection(), None);

            panel.select_tab(1, window, cx);
            assert_eq!(panel.active_tab_index(), 1);
            assert_eq!(panel.marked_text_range(window, cx), Some(0..2));
            assert_eq!(panel.active_selection(), Some(sel));

            // Resize applies across all tabs.
            panel.resize_for_bounds(
                Bounds::new(
                    gpui::point(px(0.0), px(0.0)),
                    gpui::size(px(800.0), px(300.0)),
                ),
                cx,
            );
            let dims = panel.tab_cols_rows();
            assert_eq!(dims.len(), 2);
            assert_eq!(dims[0], dims[1]);
            assert!(dims[0].0 > 2 && dims[0].1 > 2);

            // Closing tab 0 leaves tab 2; opening another tab uses monotonic index 3.
            panel.close_tab(0, window, cx);
            assert_eq!(
                panel.tab_titles(),
                vec![t!("terminal.tab_title", index = 2).to_string()]
            );
            panel.new_tab(window, cx);
            assert_eq!(
                panel.tab_titles(),
                vec![
                    t!("terminal.tab_title", index = 2).to_string(),
                    t!("terminal.tab_title", index = 3).to_string(),
                ]
            );
        });
    }

    #[cfg(not(windows))]
    #[gpui::test]
    fn tab_created_before_bounds_spawns_at_the_bounds_size(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            let mut settings = Settings::default();
            settings.tools.embedded_shell = Some("/bin/sh".to_owned());
            cx.set_global(settings);
        });

        let dir = tempfile::TempDir::new().expect("tempdir");
        let workdir = dir.path().to_path_buf();
        let window = cx.add_window(|window, cx| {
            let parent = cx.focus_handle();
            let panel = TerminalPanel::new_quiet_awaiting_bounds(workdir, parent, None, window, cx);
            // Nothing has been laid out yet, so the shell must not start at the
            // placeholder grid.
            assert!(
                !panel.has_session(),
                "the shell must wait until the panel knows its size"
            );
            assert_eq!(panel.tab_cols_rows(), vec![(DEFAULT_COLS, DEFAULT_ROWS)]);
            panel
        });
        let panel = window.root(cx).expect("root panel");
        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);

        // The first paint reports the panel bounds, which is where the waiting
        // shell has to start.
        vcx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });

        panel.update_in(&mut vcx, |panel, _, cx| {
            let bounds = panel.last_bounds.expect("the panel drew with bounds");
            let expected = grid_dimensions_for_bounds(bounds, panel.cell_size, panel.zoom);
            assert!(
                expected.0 > DEFAULT_COLS,
                "the bounds must be wider than the default grid for this to prove anything"
            );
            assert!(
                panel.has_session(),
                "the shell must start once the first bounds are known"
            );
            assert_eq!(panel.tab_cols_rows(), vec![expected]);

            let snapshot = panel.snapshot();
            assert_eq!(
                (snapshot.cols, snapshot.rows),
                expected,
                "the shell must start at the bounds-derived grid, not the default one"
            );

            // A later bounds change resizes the running session instead of
            // spawning another one.
            let narrower = Bounds::new(
                gpui::point(px(0.0), px(0.0)),
                gpui::size(px(600.0), px(240.0)),
            );
            panel.resize_for_bounds(narrower, cx);
            let resized = grid_dimensions_for_bounds(narrower, panel.cell_size, panel.zoom);
            assert!(resized.0 < expected.0);
            assert_eq!(panel.tab_cols_rows(), vec![resized]);
        });
    }

    #[cfg(not(windows))]
    #[gpui::test]
    fn terminal_tab_shortcuts_require_panel_focus_and_hide_on_last_close(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::cell::RefCell;
        use std::rc::Rc;

        use gpui::{
            AppContext as _, Entity, InteractiveElement as _, ParentElement as _, Styled as _, div,
        };
        use gpui_component::Root;

        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            crate::actions::bind(cx);
            super::bind(cx);
            let mut settings = Settings::default();
            settings.tools.embedded_shell = Some("/bin/sh".to_owned());
            cx.set_global(settings);
        });

        struct ShortcutProbe {
            parent_focus: FocusHandle,
            panel: Entity<TerminalPanel>,
        }

        impl Render for ShortcutProbe {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                div()
                    .size_full()
                    .track_focus(&self.parent_focus)
                    .child(self.panel.clone())
            }
        }

        let dir = tempfile::TempDir::new().expect("tempdir");
        let workdir = dir.path().to_path_buf();
        let close_reasons: Rc<RefCell<Vec<TerminalCloseReason>>> =
            Rc::new(RefCell::new(Vec::new()));
        let close_reasons_cb = close_reasons.clone();

        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                let probe = cx.new(|cx| {
                    let parent_focus = cx.focus_handle();
                    let on_close: CloseHandler = Rc::new(move |reason, window, cx| {
                        close_reasons_cb.borrow_mut().push(reason);
                        let _ = (window, cx);
                    });
                    let panel = cx.new(|cx| {
                        TerminalPanel::new_quiet(
                            workdir,
                            parent_focus.clone(),
                            Some(on_close),
                            window,
                            cx,
                        )
                    });
                    ShortcutProbe {
                        parent_focus,
                        panel,
                    }
                });
                cx.new(|cx| Root::new(probe, window, cx))
            })
            .expect("open window")
        });

        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        let (panel, parent_focus) = vcx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let root = window.root::<Root>().flatten().expect("Root view");
            let probe = root
                .read(cx)
                .view()
                .clone()
                .downcast::<ShortcutProbe>()
                .expect("ShortcutProbe");
            let parent_focus = probe.read(cx).parent_focus.clone();
            let panel = probe.read(cx).panel.clone();
            window.focus(&parent_focus, cx);
            window.draw(cx).clear(cx);
            (panel, parent_focus)
        });

        // While focus is outside TerminalPanel, terminal tab shortcuts must not fire.
        vcx.simulate_keystrokes("ctrl-shift-n ctrl-shift-w ctrl-shift-right ctrl-shift-left");
        assert_eq!(panel.read_with(&vcx, |p, _| p.tab_count()), 1);
        assert!(close_reasons.borrow().is_empty());

        // Focus TerminalPanel and exercise New, Previous, Next, Close.
        vcx.update(|window, cx| {
            let focus = panel.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
            window.draw(cx).clear(cx);
        });
        vcx.simulate_keystrokes("ctrl-shift-n");
        assert_eq!(panel.read_with(&vcx, |p, _| p.tab_count()), 2);
        assert_eq!(panel.read_with(&vcx, |p, _| p.active_tab_index()), 1);

        vcx.simulate_keystrokes("ctrl-shift-left");
        assert_eq!(panel.read_with(&vcx, |p, _| p.active_tab_index()), 0);
        vcx.simulate_keystrokes("ctrl-shift-right");
        assert_eq!(panel.read_with(&vcx, |p, _| p.active_tab_index()), 1);

        vcx.simulate_keystrokes("ctrl-shift-w");
        assert_eq!(panel.read_with(&vcx, |p, _| p.tab_count()), 1);
        assert!(close_reasons.borrow().is_empty());

        // Closing the last tab emits LastTabClosed and avoids focusing panel.
        vcx.simulate_keystrokes("ctrl-shift-w");
        assert!(panel.read_with(&vcx, |p, _| p.is_empty()));
        assert_eq!(
            *close_reasons.borrow(),
            vec![TerminalCloseReason::LastTabClosed]
        );
        let _ = parent_focus;
    }

    #[cfg(not(windows))]
    #[gpui::test]
    fn exited_tab_closes_on_key_press_and_repo_view_restarts_after_last_tab(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::time::{Duration, Instant};

        use gpui::VisualTestContext;

        use crate::actions::ToggleTerminal;
        use crate::repo_view::{Launch, RepoView};

        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            crate::actions::bind(cx);
            super::bind(cx);
            let mut settings = Settings::default();
            settings.tools.embedded_shell =
                Some("/bin/sh -c \"printf 'DONE\\n'; exit 5\"".to_owned());
            cx.set_global(settings);
        });

        let dir = tempfile::TempDir::new().expect("tempdir");
        let workdir = dir.path().to_path_buf();
        let config = dir.path().join("gitconfig");
        std::fs::write(
            &config,
            "[user]\n\tname = Lamina Test\n\temail = lamina@example.invalid\n[commit]\n\tgpgsign = false\n[init]\n\tdefaultBranch = main\n",
        )
        .expect("write gitconfig");
        let run_git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&workdir)
                .env("GIT_CONFIG_GLOBAL", &config)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .expect("run git");
            assert!(out.status.success());
        };
        run_git(&["init", "-q"]);
        std::fs::write(workdir.join("README.md"), "hi\n").expect("write readme");
        run_git(&["add", "README.md"]);
        run_git(&["commit", "-q", "-m", "initial"]);

        let window = cx
            .add_window(|window, cx| RepoView::new(Launch::open_quiet(&workdir, None), window, cx));
        let view = window.root(cx).expect("root RepoView");
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.update(|_, cx| {
            view.update(cx, |v, _| v.quiet_terminal = true);
        });

        vcx.dispatch_action(ToggleTerminal);
        let first_panel = vcx.update(|window, cx| {
            window.draw(cx).clear(cx);
            view.read(cx).terminal.clone().expect("terminal created")
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_exit = false;
        while Instant::now() < deadline {
            first_panel.update(&mut vcx, |panel, cx| {
                panel.flush_quiet_events(cx);
                saw_exit = panel.active_exited_status().is_some();
            });
            if saw_exit {
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        assert!(saw_exit, "timed out waiting for child exit");
        assert_eq!(
            first_panel.read_with(&vcx, |p, _| p
                .active_exited_status()
                .flatten()
                .map(|s| s.code)),
            Some(5)
        );

        // Press an ordinary key on the exited last tab: closes tab -> hides panel & drops empty TerminalPanel.
        vcx.simulate_keystrokes("enter");
        vcx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let repo_view = view.read(cx);
            assert!(
                !repo_view.terminal_open,
                "closing last terminal tab must hide panel"
            );
            assert!(
                repo_view.terminal.is_none(),
                "closing last terminal tab must drop empty TerminalPanel"
            );
        });

        // Next ToggleTerminal creates a fresh TerminalPanel + session.
        vcx.update(|_, cx| {
            let mut settings = Settings::get(cx).clone();
            settings.tools.embedded_shell = Some("/bin/sh".to_owned());
            cx.set_global(settings);
        });
        vcx.dispatch_action(ToggleTerminal);
        vcx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let repo_view = view.read(cx);
            assert!(repo_view.terminal_open);
            let second_panel = repo_view.terminal.as_ref().expect("fresh terminal");
            assert_ne!(second_panel.entity_id(), first_panel.entity_id());
            assert_eq!(second_panel.read(cx).tab_count(), 1);
            assert!(second_panel.read(cx).has_session());
        });
    }
}
