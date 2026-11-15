//! UI-independent terminal core and PTY session actor.
//!
//! `libghostty-vt` handles are not thread-safe. [`TerminalCore`] owns the C
//! handles without implementing `Send` or `Sync`, and [`TerminalSession`] runs
//! it on a single dedicated actor thread communicating over channels. Only
//! plain-data snapshots, queries, and events cross thread boundaries.

use std::ffi::{CStr, c_void};
use std::fmt;
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::keys::{TermKey, TermKeyAction, TermKeyEvent};
use crate::mouse::{MouseOutcome, TermMouseEvent, TermMouseKind};
use crate::selection::{BufferPoint, TerminalSelection};
use crate::snapshot::{
    CellAttributes, CellSnapshot, CellWidth, CursorShape, CursorSnapshot, ExitStatus, Rgb,
    RowSnapshot, ScrollbarSnapshot, TerminalSnapshot, TerminalTheme,
};
use crate::sys;

const DEFAULT_SCROLLBACK_MAX_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_SCROLLBACK_MAX_LINES: usize = 10_000;
const DEC_MODE_ALT_SCROLL: u16 = 1007;
const DEC_MODE_FOCUS_EVENT: u16 = 1004;
const DEC_MODE_BRACKETED_PASTE: u16 = 2004;
const MAX_WHEEL_STEPS_PER_EVENT: u32 = 64;

/// Errors returned by terminal initialization, backend calls, and synchronous queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermError {
    /// PTY setup, spawning, or I/O failed.
    Io(String),
    /// A `libghostty-vt` C ABI call returned a non-success status.
    Backend { op: &'static str, code: i32 },
    /// The background terminal actor has already shut down.
    SessionStopped,
    /// Caller supplied an invalid argument.
    InvalidInput(String),
}

impl fmt::Display for TermError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(msg) => write!(f, "terminal I/O error: {msg}"),
            Self::Backend { op, code } => {
                write!(f, "libghostty-vt `{op}` failed with code {code}")
            }
            Self::SessionStopped => write!(f, "terminal session has stopped"),
            Self::InvalidInput(msg) => write!(f, "invalid terminal input: {msg}"),
        }
    }
}

impl std::error::Error for TermError {}

/// Options for spawning a [`TerminalSession`] or creating a [`TerminalCore`].
#[derive(Debug, Clone)]
pub struct TerminalOptions {
    pub cols: u16,
    pub rows: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    pub scrollback_max_bytes: usize,
    pub scrollback_max_lines: usize,
    pub theme: TerminalTheme,
    pub working_dir: Option<PathBuf>,
    /// Optional shell executable override. When `None`, uses `$SHELL` as a
    /// login-style interactive shell on Unix and `%COMSPEC%` on Windows.
    pub shell: Option<String>,
    /// Extra arguments passed to the shell. Ignored when `shell` is `None` and
    /// `shell_args` is empty so the default login-shell invocation is preserved.
    pub shell_args: Vec<String>,
    /// Additional environment variables merged before enforcing `TERM` and `COLORTERM`.
    pub env: Vec<(String, String)>,
    pub macos_option_as_alt: bool,
}

impl Default for TerminalOptions {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            scrollback_max_bytes: DEFAULT_SCROLLBACK_MAX_BYTES,
            scrollback_max_lines: DEFAULT_SCROLLBACK_MAX_LINES,
            theme: TerminalTheme::default(),
            working_dir: None,
            shell: None,
            shell_args: Vec::new(),
            env: Vec::new(),
            macos_option_as_alt: false,
        }
    }
}

/// Viewport scroll operations.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ScrollAction {
    /// Scroll by a relative row delta (negative = up into scrollback, positive = down).
    Delta(isize),
    /// Scroll up by one viewport height.
    PageUp,
    /// Scroll down by one viewport height.
    PageDown,
    /// Jump to the top of scrollback.
    Top,
    /// Jump to the live active area at the bottom.
    Bottom,
    /// Jump to an absolute row offset in the scrollable area (`0` = oldest scrollback row).
    Row(usize),
}

/// Asynchronous events emitted by a [`TerminalSession`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    /// A new [`TerminalSnapshot`] with changed grid, cursor, theme, or mode state
    /// has been published and can be read via [`TerminalSession::snapshot`].
    SnapshotChanged,
    /// The running application updated the terminal title (for example via OSC 0/2).
    TitleChanged(String),
    /// The terminal received a BEL (`\x07`) control character.
    Bell,
    /// The child process exited and was reaped.
    ChildExited(Option<ExitStatus>),
}

/// Callback invoked from the terminal actor thread for each [`TerminalEvent`].
pub type EventCallback = Box<dyn FnMut(TerminalEvent) + Send>;

struct CallbackState {
    pending_pty_writes: Vec<u8>,
    title: String,
    title_changed: bool,
    bell: bool,
}

unsafe extern "C" fn on_write_pty(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    if userdata.is_null() || data.is_null() || len == 0 {
        return;
    }
    // Safety: `userdata` points to the pinned `Box<CallbackState>` owned by `TerminalCore`,
    // which is otherwise unaliased on the same thread during `ghostty_terminal_vt_write`.
    let state = unsafe { &mut *(userdata as *mut CallbackState) };
    let slice = unsafe { std::slice::from_raw_parts(data, len) };
    state.pending_pty_writes.extend_from_slice(slice);
}

unsafe extern "C" fn on_title_changed(terminal: sys::GhosttyTerminal, userdata: *mut c_void) {
    if userdata.is_null() || terminal.is_null() {
        return;
    }
    let state = unsafe { &mut *(userdata as *mut CallbackState) };
    let mut title = sys::GhosttyString {
        ptr: std::ptr::null(),
        len: 0,
    };
    let rc = unsafe {
        sys::ghostty_terminal_get(
            terminal,
            sys::GhosttyTerminalData::Title,
            (&mut title as *mut sys::GhosttyString).cast(),
        )
    };
    if rc == sys::GhosttyResult::Success && !title.ptr.is_null() && title.len > 0 {
        let bytes = unsafe { std::slice::from_raw_parts(title.ptr, title.len) };
        state.title = String::from_utf8_lossy(bytes).into_owned();
    } else {
        state.title.clear();
    }
    state.title_changed = true;
}

unsafe extern "C" fn on_bell(_terminal: sys::GhosttyTerminal, userdata: *mut c_void) {
    if userdata.is_null() {
        return;
    }
    let state = unsafe { &mut *(userdata as *mut CallbackState) };
    state.bell = true;
}

/// Returns the linked `libghostty-vt` version string.
pub fn library_version() -> &'static str {
    let ptr = unsafe { sys::ghostty_version() };
    if ptr.is_null() {
        return "";
    }
    unsafe { CStr::from_ptr(ptr) }.to_str().unwrap_or_default()
}

/// Returns the `libghostty-vt` C type manifest JSON for the linked build.
pub fn abi_type_manifest_json() -> &'static str {
    let ptr = unsafe { sys::ghostty_type_json() };
    if ptr.is_null() {
        return "";
    }
    unsafe { CStr::from_ptr(ptr) }.to_str().unwrap_or_default()
}

/// Single-threaded wrapper around `libghostty-vt` terminal, render state, and encoders.
///
/// Intentionally `!Send` and `!Sync` so C handles can never be shared across threads.
pub struct TerminalCore {
    terminal: sys::GhosttyTerminal,
    render_state: sys::GhosttyRenderState,
    row_iter: sys::GhosttyRenderStateRowIterator,
    row_cells: sys::GhosttyRenderStateRowCells,
    key_encoder: sys::GhosttyKeyEncoder,
    key_event: sys::GhosttyKeyEvent,
    mouse_encoder: sys::GhosttyMouseEncoder,
    mouse_event: sys::GhosttyMouseEvent,
    formatter: sys::GhosttyFormatter,
    callbacks: Box<CallbackState>,
    cols: u16,
    rows: u16,
    cell_width_px: u32,
    cell_height_px: u32,
    default_fg: Rgb,
    default_bg: Rgb,
    macos_option_as_alt: bool,
    mouse_encoder_needs_sync: bool,
    mouse_encoder_size_dirty: bool,
    pressed_mouse_buttons: u8,
    cached_cols: u16,
    cached_grid: Vec<RowSnapshot>,
    last_dirty_rows: usize,
    last_published: Option<TerminalSnapshot>,
    _not_send_sync: PhantomData<*const ()>,
}

impl TerminalCore {
    /// Allocate and configure a new `libghostty-vt` terminal instance.
    pub fn new(options: &TerminalOptions) -> Result<Self, TermError> {
        let cols = options.cols.max(1);
        let rows = options.rows.max(1);
        let cell_width_px = options.cell_width_px.max(1);
        let cell_height_px = options.cell_height_px.max(1);

        let mut terminal: sys::GhosttyTerminal = std::ptr::null_mut();
        check_result(
            unsafe { sys::ghostty_terminal_new(std::ptr::null(), &mut terminal, cols, rows) },
            "ghostty_terminal_new",
        )?;

        let result = (|| -> Result<Self, TermError> {
            let mut render_state: sys::GhosttyRenderState = std::ptr::null_mut();
            check_result(
                unsafe { sys::ghostty_render_state_new(std::ptr::null(), &mut render_state) },
                "ghostty_render_state_new",
            )?;

            let mut row_iter: sys::GhosttyRenderStateRowIterator = std::ptr::null_mut();
            if let Err(err) = check_result(
                unsafe {
                    sys::ghostty_render_state_row_iterator_new(std::ptr::null(), &mut row_iter)
                },
                "ghostty_render_state_row_iterator_new",
            ) {
                unsafe { sys::ghostty_render_state_free(render_state) };
                return Err(err);
            }

            let mut row_cells: sys::GhosttyRenderStateRowCells = std::ptr::null_mut();
            if let Err(err) = check_result(
                unsafe {
                    sys::ghostty_render_state_row_cells_new(std::ptr::null(), &mut row_cells)
                },
                "ghostty_render_state_row_cells_new",
            ) {
                unsafe {
                    sys::ghostty_render_state_row_iterator_free(row_iter);
                    sys::ghostty_render_state_free(render_state);
                }
                return Err(err);
            }

            let mut key_encoder: sys::GhosttyKeyEncoder = std::ptr::null_mut();
            if let Err(err) = check_result(
                unsafe { sys::ghostty_key_encoder_new(std::ptr::null(), &mut key_encoder) },
                "ghostty_key_encoder_new",
            ) {
                unsafe {
                    sys::ghostty_render_state_row_cells_free(row_cells);
                    sys::ghostty_render_state_row_iterator_free(row_iter);
                    sys::ghostty_render_state_free(render_state);
                }
                return Err(err);
            }

            let mut key_event: sys::GhosttyKeyEvent = std::ptr::null_mut();
            if let Err(err) = check_result(
                unsafe { sys::ghostty_key_event_new(std::ptr::null(), &mut key_event) },
                "ghostty_key_event_new",
            ) {
                unsafe {
                    sys::ghostty_key_encoder_free(key_encoder);
                    sys::ghostty_render_state_row_cells_free(row_cells);
                    sys::ghostty_render_state_row_iterator_free(row_iter);
                    sys::ghostty_render_state_free(render_state);
                }
                return Err(err);
            }

            let mut mouse_encoder: sys::GhosttyMouseEncoder = std::ptr::null_mut();
            if let Err(err) = check_result(
                unsafe { sys::ghostty_mouse_encoder_new(std::ptr::null(), &mut mouse_encoder) },
                "ghostty_mouse_encoder_new",
            ) {
                unsafe {
                    sys::ghostty_key_event_free(key_event);
                    sys::ghostty_key_encoder_free(key_encoder);
                    sys::ghostty_render_state_row_cells_free(row_cells);
                    sys::ghostty_render_state_row_iterator_free(row_iter);
                    sys::ghostty_render_state_free(render_state);
                }
                return Err(err);
            }

            let mut mouse_event: sys::GhosttyMouseEvent = std::ptr::null_mut();
            if let Err(err) = check_result(
                unsafe { sys::ghostty_mouse_event_new(std::ptr::null(), &mut mouse_event) },
                "ghostty_mouse_event_new",
            ) {
                unsafe {
                    sys::ghostty_mouse_encoder_free(mouse_encoder);
                    sys::ghostty_key_event_free(key_event);
                    sys::ghostty_key_encoder_free(key_encoder);
                    sys::ghostty_render_state_row_cells_free(row_cells);
                    sys::ghostty_render_state_row_iterator_free(row_iter);
                    sys::ghostty_render_state_free(render_state);
                }
                return Err(err);
            }

            let mut formatter: sys::GhosttyFormatter = std::ptr::null_mut();
            if let Err(err) = check_result(
                unsafe {
                    sys::ghostty_formatter_terminal_new(std::ptr::null(), &mut formatter, terminal)
                },
                "ghostty_formatter_terminal_new",
            ) {
                unsafe {
                    sys::ghostty_mouse_event_free(mouse_event);
                    sys::ghostty_mouse_encoder_free(mouse_encoder);
                    sys::ghostty_key_event_free(key_event);
                    sys::ghostty_key_encoder_free(key_encoder);
                    sys::ghostty_render_state_row_cells_free(row_cells);
                    sys::ghostty_render_state_row_iterator_free(row_iter);
                    sys::ghostty_render_state_free(render_state);
                }
                return Err(err);
            }

            let callbacks = Box::new(CallbackState {
                pending_pty_writes: Vec::new(),
                title: String::new(),
                title_changed: false,
                bell: false,
            });

            Ok(Self {
                terminal,
                render_state,
                row_iter,
                row_cells,
                key_encoder,
                key_event,
                mouse_encoder,
                mouse_event,
                formatter,
                callbacks,
                cols,
                rows,
                cell_width_px,
                cell_height_px,
                default_fg: options.theme.foreground,
                default_bg: options.theme.background,
                macos_option_as_alt: options.macos_option_as_alt,
                mouse_encoder_needs_sync: true,
                mouse_encoder_size_dirty: true,
                pressed_mouse_buttons: 0,
                cached_cols: cols,
                cached_grid: Vec::new(),
                last_dirty_rows: 0,
                last_published: None,
                _not_send_sync: PhantomData,
            })
        })();

        let mut core = match result {
            Ok(core) => core,
            Err(err) => {
                unsafe { sys::ghostty_terminal_free(terminal) };
                return Err(err);
            }
        };

        core.configure(options)?;
        Ok(core)
    }

    fn configure(&mut self, options: &TerminalOptions) -> Result<(), TermError> {
        let userdata: *mut CallbackState = &mut *self.callbacks;
        unsafe {
            check_result(
                sys::ghostty_terminal_set(
                    self.terminal,
                    sys::GhosttyTerminalOption::Userdata,
                    userdata.cast(),
                ),
                "set userdata",
            )?;
            // Callback options take the function pointer directly rather than a pointer to it.
            check_result(
                sys::ghostty_terminal_set(
                    self.terminal,
                    sys::GhosttyTerminalOption::WritePty,
                    on_write_pty as *const c_void,
                ),
                "set write_pty",
            )?;
            check_result(
                sys::ghostty_terminal_set(
                    self.terminal,
                    sys::GhosttyTerminalOption::TitleChanged,
                    on_title_changed as *const c_void,
                ),
                "set title_changed",
            )?;
            check_result(
                sys::ghostty_terminal_set(
                    self.terminal,
                    sys::GhosttyTerminalOption::Bell,
                    on_bell as *const c_void,
                ),
                "set bell",
            )?;

            check_result(
                sys::ghostty_terminal_set(
                    self.terminal,
                    sys::GhosttyTerminalOption::ScrollbackMaxBytes,
                    (&options.scrollback_max_bytes as *const usize).cast(),
                ),
                "set scrollback_max_bytes",
            )?;
            check_result(
                sys::ghostty_terminal_set(
                    self.terminal,
                    sys::GhosttyTerminalOption::ScrollbackMaxLines,
                    (&options.scrollback_max_lines as *const usize).cast(),
                ),
                "set scrollback_max_lines",
            )?;

            let term_name = b"xterm-256color";
            let term_str = sys::GhosttyString {
                ptr: term_name.as_ptr(),
                len: term_name.len(),
            };
            check_result(
                sys::ghostty_terminal_set(
                    self.terminal,
                    sys::GhosttyTerminalOption::TerminfoName,
                    (&term_str as *const sys::GhosttyString).cast(),
                ),
                "set terminfo_name",
            )?;

            let track_last_cell = true;
            check_result(
                sys::ghostty_mouse_encoder_set(
                    self.mouse_encoder,
                    sys::GhosttyMouseEncoderOption::TrackLastCell,
                    (&track_last_cell as *const bool).cast(),
                ),
                "set mouse track_last_cell",
            )?;

            // ConPTY maintains its own viewport without scrollback, so pulling
            // scrollback rows back into the active grid during resize desyncs
            // Windows shells from the terminal model.
            #[cfg(windows)]
            {
                let pull_scrollback = false;
                check_result(
                    sys::ghostty_terminal_set(
                        self.terminal,
                        sys::GhosttyTerminalOption::ResizePullScrollback,
                        (&pull_scrollback as *const bool).cast(),
                    ),
                    "set resize_pull_scrollback",
                )?;
            }
        }

        self.set_theme(&options.theme)?;
        self.resize(
            self.cols,
            self.rows,
            self.cell_width_px,
            self.cell_height_px,
        )?;
        Ok(())
    }

    /// Apply foreground, background, cursor, and 16-color ANSI palette settings.
    ///
    /// Marks palette state dirty so the next snapshot update rebuilds with the
    /// updated colors.
    pub fn set_theme(&mut self, theme: &TerminalTheme) -> Result<(), TermError> {
        let fg = sys::GhosttyColorRgb {
            r: theme.foreground.r,
            g: theme.foreground.g,
            b: theme.foreground.b,
        };
        let bg = sys::GhosttyColorRgb {
            r: theme.background.r,
            g: theme.background.g,
            b: theme.background.b,
        };

        unsafe {
            check_result(
                sys::ghostty_terminal_set(
                    self.terminal,
                    sys::GhosttyTerminalOption::ColorForeground,
                    (&fg as *const sys::GhosttyColorRgb).cast(),
                ),
                "set default fg",
            )?;
            check_result(
                sys::ghostty_terminal_set(
                    self.terminal,
                    sys::GhosttyTerminalOption::ColorBackground,
                    (&bg as *const sys::GhosttyColorRgb).cast(),
                ),
                "set default bg",
            )?;

            if let Some(cursor) = theme.cursor {
                let raw = sys::GhosttyColorRgb {
                    r: cursor.r,
                    g: cursor.g,
                    b: cursor.b,
                };
                check_result(
                    sys::ghostty_terminal_set(
                        self.terminal,
                        sys::GhosttyTerminalOption::ColorCursor,
                        (&raw as *const sys::GhosttyColorRgb).cast(),
                    ),
                    "set cursor color",
                )?;
            } else {
                check_result(
                    sys::ghostty_terminal_set(
                        self.terminal,
                        sys::GhosttyTerminalOption::ColorCursor,
                        std::ptr::null(),
                    ),
                    "clear cursor color",
                )?;
            }

            if let Some(ansi16) = theme.palette {
                let mut full_palette = [sys::GhosttyColorRgb::default(); 256];
                check_result(
                    sys::ghostty_terminal_get(
                        self.terminal,
                        sys::GhosttyTerminalData::ColorPaletteDefault,
                        full_palette.as_mut_ptr().cast(),
                    ),
                    "get default color palette",
                )?;
                for (dst, src) in full_palette[..16].iter_mut().zip(ansi16.iter()) {
                    *dst = sys::GhosttyColorRgb {
                        r: src.r,
                        g: src.g,
                        b: src.b,
                    };
                }
                check_result(
                    sys::ghostty_terminal_set(
                        self.terminal,
                        sys::GhosttyTerminalOption::ColorPalette,
                        full_palette.as_ptr().cast(),
                    ),
                    "set color palette",
                )?;
            } else {
                check_result(
                    sys::ghostty_terminal_set(
                        self.terminal,
                        sys::GhosttyTerminalOption::ColorPalette,
                        std::ptr::null(),
                    ),
                    "reset color palette",
                )?;
            }
        }

        self.default_fg = theme.foreground;
        self.default_bg = theme.background;
        Ok(())
    }

    /// Feed raw VT bytes from the PTY (or a test fixture) and return any
    /// synchronous terminal query responses that must be written back to the PTY.
    pub fn feed_bytes(&mut self, bytes: &[u8]) -> Vec<u8> {
        if bytes.is_empty() {
            return Vec::new();
        }
        unsafe {
            sys::ghostty_terminal_vt_write(self.terminal, bytes.as_ptr(), bytes.len());
        }
        // Modes (such as mouse tracking/format) may have changed inside VT stream.
        self.mouse_encoder_needs_sync = true;
        std::mem::take(&mut self.callbacks.pending_pty_writes)
    }

    /// Returns the new title if an OSC title update arrived since the previous check.
    pub fn take_title_changed(&mut self) -> Option<String> {
        if self.callbacks.title_changed {
            self.callbacks.title_changed = false;
            Some(self.callbacks.title.clone())
        } else {
            None
        }
    }

    /// Returns `true` if a BEL sequence was processed since the previous check.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.callbacks.bell)
    }

    /// Resize the terminal grid and pixel cell geometry.
    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> Result<(), TermError> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let cell_width_px = cell_width_px.max(1);
        let cell_height_px = cell_height_px.max(1);

        check_result(
            unsafe {
                sys::ghostty_terminal_resize(
                    self.terminal,
                    cols,
                    rows,
                    cell_width_px,
                    cell_height_px,
                )
            },
            "ghostty_terminal_resize",
        )?;

        self.cols = cols;
        self.rows = rows;
        self.cell_width_px = cell_width_px;
        self.cell_height_px = cell_height_px;
        self.mouse_encoder_size_dirty = true;
        Ok(())
    }

    /// Scroll the viewport relative to scrollback history.
    pub fn scroll_viewport(&mut self, action: ScrollAction) -> Result<(), TermError> {
        let behavior = match action {
            ScrollAction::Top => sys::GhosttyTerminalScrollViewport {
                tag: sys::GhosttyTerminalScrollViewportTag::Top,
                value: sys::GhosttyTerminalScrollViewportValue { _padding: [0; 2] },
            },
            ScrollAction::Bottom => sys::GhosttyTerminalScrollViewport {
                tag: sys::GhosttyTerminalScrollViewportTag::Bottom,
                value: sys::GhosttyTerminalScrollViewportValue { _padding: [0; 2] },
            },
            ScrollAction::Delta(delta) => sys::GhosttyTerminalScrollViewport {
                tag: sys::GhosttyTerminalScrollViewportTag::Delta,
                value: sys::GhosttyTerminalScrollViewportValue { delta },
            },
            ScrollAction::PageUp => sys::GhosttyTerminalScrollViewport {
                tag: sys::GhosttyTerminalScrollViewportTag::Delta,
                value: sys::GhosttyTerminalScrollViewportValue {
                    delta: -(self.rows.max(1) as isize),
                },
            },
            ScrollAction::PageDown => sys::GhosttyTerminalScrollViewport {
                tag: sys::GhosttyTerminalScrollViewportTag::Delta,
                value: sys::GhosttyTerminalScrollViewportValue {
                    delta: self.rows.max(1) as isize,
                },
            },
            ScrollAction::Row(row) => sys::GhosttyTerminalScrollViewport {
                tag: sys::GhosttyTerminalScrollViewportTag::Row,
                value: sys::GhosttyTerminalScrollViewportValue { row },
            },
        };
        check_result(
            unsafe { sys::ghostty_terminal_scroll_viewport(self.terminal, behavior) },
            "ghostty_terminal_scroll_viewport",
        )
    }

    /// Encode a key event against the terminal's current keyboard modes (legacy,
    /// application cursor keys, modifyOtherKeys, Kitty keyboard protocol, etc.).
    pub fn encode_key(&mut self, event: &TermKeyEvent) -> Result<Vec<u8>, TermError> {
        unsafe {
            check_result(
                sys::ghostty_key_encoder_set_from_terminal(self.key_encoder, self.terminal),
                "ghostty_key_encoder_set_from_terminal",
            )?;

            if self.macos_option_as_alt {
                let opt = sys::GhosttyOptionAsAlt::True;
                check_result(
                    sys::ghostty_key_encoder_set(
                        self.key_encoder,
                        sys::GhosttyKeyEncoderOption::MacosOptionAsAlt,
                        (&opt as *const sys::GhosttyOptionAsAlt).cast(),
                    ),
                    "set macos_option_as_alt",
                )?;
            }

            check_result(
                sys::ghostty_key_event_set_action(self.key_event, event.action.into()),
                "set key action",
            )?;
            check_result(
                sys::ghostty_key_event_set_key(self.key_event, event.key.to_ghostty_key()),
                "set key code",
            )?;
            check_result(
                sys::ghostty_key_event_set_mods(self.key_event, event.modifiers.to_ghostty_mods()),
                "set key mods",
            )?;
            check_result(
                sys::ghostty_key_event_set_consumed_mods(
                    self.key_event,
                    event.consumed_modifiers.to_ghostty_mods(),
                ),
                "set consumed mods",
            )?;
            check_result(
                sys::ghostty_key_event_set_composing(self.key_event, event.composing),
                "set composing",
            )?;

            let unshifted = event.key.unshifted_ascii().map_or(0, |ch| ch as u32);
            check_result(
                sys::ghostty_key_event_set_unshifted_codepoint(self.key_event, unshifted),
                "set unshifted codepoint",
            )?;

            let text_bytes = event.sanitized_utf8().unwrap_or("");
            let utf8 = sys::GhosttyString {
                ptr: if text_bytes.is_empty() {
                    std::ptr::null()
                } else {
                    text_bytes.as_ptr()
                },
                len: text_bytes.len(),
            };
            check_result(
                sys::ghostty_key_event_set_utf8(self.key_event, utf8),
                "set key utf8",
            )?;

            encode_with_retry("ghostty_key_encoder_encode", |buf, len, out_written| {
                sys::ghostty_key_encoder_encode(
                    self.key_encoder,
                    self.key_event,
                    buf,
                    len,
                    out_written,
                )
            })
        }
    }

    /// Encode clipboard text for pasting into the terminal, sanitizing control
    /// bytes and wrapping with bracketed-paste markers (`ESC[200~` .. `ESC[201~`)
    /// when DEC mode 2004 is enabled.
    pub fn encode_paste(&self, text: &str) -> Result<Vec<u8>, TermError> {
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let bracketed = query_dec_mode(self.terminal, DEC_MODE_BRACKETED_PASTE)?;
        // `ghostty_paste_encode` scrubs unsafe bytes in-place inside `data`, and
        // retains those scrubbed bytes if it returns `OutOfSpace` for a retry.
        let mut scratch = text.as_bytes().to_vec();
        unsafe {
            encode_with_retry("ghostty_paste_encode", |buf, len, out_written| {
                sys::ghostty_paste_encode(
                    scratch.as_mut_ptr(),
                    scratch.len(),
                    bracketed,
                    buf,
                    len,
                    out_written,
                )
            })
        }
    }

    /// Encode a focus gained/lost report (`CSI I` / `CSI O`) if the terminal
    /// has focus reporting mode (DEC mode 1004) enabled.
    pub fn encode_focus(&self, focused: bool) -> Result<Option<Vec<u8>>, TermError> {
        if !query_dec_mode(self.terminal, DEC_MODE_FOCUS_EVENT)? {
            return Ok(None);
        }
        let event = if focused {
            sys::GhosttyFocusEvent::Gained
        } else {
            sys::GhosttyFocusEvent::Lost
        };
        let bytes = unsafe {
            encode_with_retry("ghostty_focus_encode", |buf, len, out_written| {
                sys::ghostty_focus_encode(event, buf, len, out_written)
            })?
        };
        Ok(Some(bytes))
    }

    /// Route a mouse interaction according to active terminal mouse modes:
    /// - When mouse reporting is active, encodes press/release/motion/wheel with
    ///   `libghostty-vt`'s mouse encoder and returns [`MouseOutcome::Reported`].
    /// - When mouse reporting is off and the event is a wheel scroll on the
    ///   primary screen, scrolls the scrollback viewport and returns
    ///   [`MouseOutcome::ScrolledViewport`].
    /// - When mouse reporting is off and the event is a vertical wheel scroll on
    ///   the alternate screen with DEC mode 1007 enabled, translates wheel steps
    ///   into ArrowUp/ArrowDown key sequences.
    pub fn handle_mouse(&mut self, event: TermMouseEvent) -> Result<MouseOutcome, TermError> {
        let mut mouse_tracking = false;
        unsafe {
            check_result(
                sys::ghostty_terminal_get(
                    self.terminal,
                    sys::GhosttyTerminalData::MouseTracking,
                    (&mut mouse_tracking as *mut bool).cast(),
                ),
                "query mouse_tracking",
            )?;
        }

        if !mouse_tracking {
            self.update_pressed_buttons(&event.kind);
            return self.handle_local_mouse(event);
        }

        let was_any_pressed = match event.kind {
            TermMouseKind::Press(btn) => {
                self.pressed_mouse_buttons |= btn.mask_bit();
                true
            }
            TermMouseKind::Move { button } => self.pressed_mouse_buttons != 0 || button.is_some(),
            TermMouseKind::Release(_) | TermMouseKind::Wheel { .. } => {
                self.pressed_mouse_buttons != 0
            }
        };

        let result = self.encode_tracked_mouse(&event, was_any_pressed);

        if let TermMouseKind::Release(btn) = event.kind {
            self.pressed_mouse_buttons &= !btn.mask_bit();
        }

        result
    }

    fn update_pressed_buttons(&mut self, kind: &TermMouseKind) {
        match kind {
            TermMouseKind::Press(btn) => self.pressed_mouse_buttons |= btn.mask_bit(),
            TermMouseKind::Release(btn) => self.pressed_mouse_buttons &= !btn.mask_bit(),
            TermMouseKind::Move { .. } | TermMouseKind::Wheel { .. } => {}
        }
    }

    fn handle_local_mouse(&mut self, event: TermMouseEvent) -> Result<MouseOutcome, TermError> {
        let TermMouseKind::Wheel { delta_y, .. } = event.kind else {
            return Ok(MouseOutcome::Ignored);
        };
        if delta_y == 0 {
            return Ok(MouseOutcome::Ignored);
        }

        let mut screen = sys::GhosttyTerminalScreen::Primary;
        unsafe {
            let _ = sys::ghostty_terminal_get(
                self.terminal,
                sys::GhosttyTerminalData::ActiveScreen,
                (&mut screen as *mut sys::GhosttyTerminalScreen).cast(),
            );
        }

        if screen == sys::GhosttyTerminalScreen::Primary {
            // Public wheel delta uses +y = scroll up into scrollback; viewport Delta
            // uses negative values to move up toward older history.
            self.scroll_viewport(ScrollAction::Delta(-(delta_y as isize)))?;
            return Ok(MouseOutcome::ScrolledViewport);
        }

        if query_dec_mode(self.terminal, DEC_MODE_ALT_SCROLL)? {
            let key = if delta_y > 0 {
                TermKey::ArrowUp
            } else {
                TermKey::ArrowDown
            };
            let steps = delta_y.unsigned_abs().min(MAX_WHEEL_STEPS_PER_EVENT);
            let mut out = Vec::new();
            let key_event = TermKeyEvent {
                action: TermKeyAction::Press,
                key,
                ..TermKeyEvent::default()
            };
            for _ in 0..steps {
                let encoded = self.encode_key(&key_event)?;
                out.extend_from_slice(&encoded);
            }
            return if out.is_empty() {
                Ok(MouseOutcome::Ignored)
            } else {
                Ok(MouseOutcome::Reported(out))
            };
        }

        Ok(MouseOutcome::Ignored)
    }

    fn encode_tracked_mouse(
        &mut self,
        event: &TermMouseEvent,
        any_button_pressed: bool,
    ) -> Result<MouseOutcome, TermError> {
        unsafe {
            if self.mouse_encoder_needs_sync {
                // `set_from_terminal` resets last_cell deduplication state, so only
                // sync after terminal state changed (e.g. init or VT writes), not on
                // every consecutive motion event.
                check_result(
                    sys::ghostty_mouse_encoder_set_from_terminal(self.mouse_encoder, self.terminal),
                    "ghostty_mouse_encoder_set_from_terminal",
                )?;
                self.mouse_encoder_needs_sync = false;
            }

            if self.mouse_encoder_size_dirty {
                // Setting `Size` resets `last_cell`, so only push geometry on init/resize.
                let mut size = sys::GhosttyMouseEncoderSize::default();
                sys::ghostty_sized_init(
                    (&mut size as *mut sys::GhosttyMouseEncoderSize).cast(),
                    std::mem::size_of::<sys::GhosttyMouseEncoderSize>(),
                );
                size.screen_width = u32::from(self.cols) * self.cell_width_px;
                size.screen_height = u32::from(self.rows) * self.cell_height_px;
                size.cell_width = self.cell_width_px;
                size.cell_height = self.cell_height_px;

                check_result(
                    sys::ghostty_mouse_encoder_set(
                        self.mouse_encoder,
                        sys::GhosttyMouseEncoderOption::Size,
                        (&size as *const sys::GhosttyMouseEncoderSize).cast(),
                    ),
                    "set mouse size",
                )?;
                self.mouse_encoder_size_dirty = false;
            }
            check_result(
                sys::ghostty_mouse_encoder_set(
                    self.mouse_encoder,
                    sys::GhosttyMouseEncoderOption::AnyButtonPressed,
                    (&any_button_pressed as *const bool).cast(),
                ),
                "set mouse any_button_pressed",
            )?;

            check_result(
                sys::ghostty_mouse_event_set_mods(
                    self.mouse_event,
                    event.modifiers.to_ghostty_mods(),
                ),
                "set mouse mods",
            )?;

            let (px, py) = event
                .position
                .to_pixels(self.cell_width_px, self.cell_height_px);
            check_result(
                sys::ghostty_mouse_event_set_position(
                    self.mouse_event,
                    sys::GhosttyMousePosition { x: px, y: py },
                ),
                "set mouse position",
            )?;
        }

        let mut encoded = Vec::new();
        match event.kind {
            TermMouseKind::Press(btn) => {
                unsafe {
                    check_result(
                        sys::ghostty_mouse_event_set_action(
                            self.mouse_event,
                            sys::GhosttyMouseAction::Press,
                        ),
                        "set mouse press",
                    )?;
                    check_result(
                        sys::ghostty_mouse_event_set_button(
                            self.mouse_event,
                            btn.to_ghostty_button(),
                        ),
                        "set mouse button",
                    )?;
                }
                encoded = self.encode_current_mouse_event()?;
            }
            TermMouseKind::Release(btn) => {
                unsafe {
                    check_result(
                        sys::ghostty_mouse_event_set_action(
                            self.mouse_event,
                            sys::GhosttyMouseAction::Release,
                        ),
                        "set mouse release",
                    )?;
                    check_result(
                        sys::ghostty_mouse_event_set_button(
                            self.mouse_event,
                            btn.to_ghostty_button(),
                        ),
                        "set mouse button",
                    )?;
                }
                encoded = self.encode_current_mouse_event()?;
            }
            TermMouseKind::Move { button } => {
                unsafe {
                    check_result(
                        sys::ghostty_mouse_event_set_action(
                            self.mouse_event,
                            sys::GhosttyMouseAction::Motion,
                        ),
                        "set mouse motion",
                    )?;
                    if let Some(btn) = button {
                        check_result(
                            sys::ghostty_mouse_event_set_button(
                                self.mouse_event,
                                btn.to_ghostty_button(),
                            ),
                            "set mouse motion button",
                        )?;
                    } else {
                        check_result(
                            sys::ghostty_mouse_event_clear_button(self.mouse_event),
                            "clear mouse motion button",
                        )?;
                    }
                }
                encoded = self.encode_current_mouse_event()?;
            }
            TermMouseKind::Wheel { delta_x, delta_y } => {
                unsafe {
                    check_result(
                        sys::ghostty_mouse_event_set_action(
                            self.mouse_event,
                            sys::GhosttyMouseAction::Press,
                        ),
                        "set wheel action",
                    )?;
                }
                if delta_y != 0 {
                    let button = if delta_y > 0 {
                        sys::GhosttyMouseButton::Four
                    } else {
                        sys::GhosttyMouseButton::Five
                    };
                    unsafe {
                        check_result(
                            sys::ghostty_mouse_event_set_button(self.mouse_event, button),
                            "set wheel vertical button",
                        )?;
                    }
                    let steps = delta_y.unsigned_abs().min(MAX_WHEEL_STEPS_PER_EVENT);
                    for _ in 0..steps {
                        let bytes = self.encode_current_mouse_event()?;
                        encoded.extend_from_slice(&bytes);
                    }
                }
                if delta_x != 0 {
                    let button = if delta_x < 0 {
                        sys::GhosttyMouseButton::Six
                    } else {
                        sys::GhosttyMouseButton::Seven
                    };
                    unsafe {
                        check_result(
                            sys::ghostty_mouse_event_set_button(self.mouse_event, button),
                            "set wheel horizontal button",
                        )?;
                    }
                    let steps = delta_x.unsigned_abs().min(MAX_WHEEL_STEPS_PER_EVENT);
                    for _ in 0..steps {
                        let bytes = self.encode_current_mouse_event()?;
                        encoded.extend_from_slice(&bytes);
                    }
                }
            }
        }

        if encoded.is_empty() {
            Ok(MouseOutcome::Ignored)
        } else {
            Ok(MouseOutcome::Reported(encoded))
        }
    }

    fn encode_current_mouse_event(&self) -> Result<Vec<u8>, TermError> {
        unsafe {
            encode_with_retry("ghostty_mouse_encoder_encode", |buf, len, out_written| {
                sys::ghostty_mouse_encoder_encode(
                    self.mouse_encoder,
                    self.mouse_event,
                    buf,
                    len,
                    out_written,
                )
            })
        }
    }

    /// Derive a word selection in full buffer coordinates starting at `point`.
    pub fn select_word(&self, point: BufferPoint) -> Result<Option<TerminalSelection>, TermError> {
        let Some(grid_ref) = self.screen_grid_ref(point)? else {
            return Ok(None);
        };
        let mut opts: sys::GhosttyTerminalSelectWordOptions = unsafe { std::mem::zeroed() };
        unsafe {
            sys::ghostty_sized_init(
                (&mut opts as *mut sys::GhosttyTerminalSelectWordOptions).cast(),
                std::mem::size_of::<sys::GhosttyTerminalSelectWordOptions>(),
            );
        }
        opts.grid_ref = grid_ref;
        let mut raw_sel = sys::GhosttySelection::default();
        let rc = unsafe { sys::ghostty_terminal_select_word(self.terminal, &opts, &mut raw_sel) };
        match rc {
            sys::GhosttyResult::Success => Ok(Some(TerminalSelection {
                start: self.buffer_point_from_grid_ref(&raw_sel.start)?,
                end: self.buffer_point_from_grid_ref(&raw_sel.end)?,
                rectangle: raw_sel.rectangle,
            })),
            sys::GhosttyResult::NoValue => Ok(None),
            other => Err(TermError::Backend {
                op: "ghostty_terminal_select_word",
                code: other as i32,
            }),
        }
    }

    /// Derive a logical line selection (spanning soft-wrapped rows) at `point`.
    pub fn select_line(&self, point: BufferPoint) -> Result<Option<TerminalSelection>, TermError> {
        let Some(grid_ref) = self.screen_grid_ref(point)? else {
            return Ok(None);
        };
        let mut opts: sys::GhosttyTerminalSelectLineOptions = unsafe { std::mem::zeroed() };
        unsafe {
            sys::ghostty_sized_init(
                (&mut opts as *mut sys::GhosttyTerminalSelectLineOptions).cast(),
                std::mem::size_of::<sys::GhosttyTerminalSelectLineOptions>(),
            );
        }
        opts.grid_ref = grid_ref;
        let mut raw_sel = sys::GhosttySelection::default();
        let rc = unsafe { sys::ghostty_terminal_select_line(self.terminal, &opts, &mut raw_sel) };
        match rc {
            sys::GhosttyResult::Success => Ok(Some(TerminalSelection {
                start: self.buffer_point_from_grid_ref(&raw_sel.start)?,
                end: self.buffer_point_from_grid_ref(&raw_sel.end)?,
                rectangle: raw_sel.rectangle,
            })),
            sys::GhosttyResult::NoValue => Ok(None),
            other => Err(TermError::Backend {
                op: "ghostty_terminal_select_line",
                code: other as i32,
            }),
        }
    }

    /// Extract plain text for `selection`, joining soft-wrapped rows and trimming
    /// trailing blanks.
    pub fn selected_text(&self, selection: &TerminalSelection) -> Result<String, TermError> {
        let Some(start_ref) = self.screen_grid_ref(selection.start)? else {
            return Ok(String::new());
        };
        let Some(end_ref) = self.screen_grid_ref(selection.end)? else {
            return Ok(String::new());
        };

        let raw_sel = sys::GhosttySelection {
            start: start_ref,
            end: end_ref,
            rectangle: selection.rectangle,
        };

        let emit = sys::GhosttyFormatterFormat::Plain;
        let unwrap = true;
        let trim = true;

        unsafe {
            check_result(
                sys::ghostty_formatter_set(
                    self.formatter,
                    sys::GhosttyFormatterOption::Emit,
                    (&emit as *const sys::GhosttyFormatterFormat).cast(),
                ),
                "formatter set emit",
            )?;
            check_result(
                sys::ghostty_formatter_set(
                    self.formatter,
                    sys::GhosttyFormatterOption::Unwrap,
                    (&unwrap as *const bool).cast(),
                ),
                "formatter set unwrap",
            )?;
            check_result(
                sys::ghostty_formatter_set(
                    self.formatter,
                    sys::GhosttyFormatterOption::Trim,
                    (&trim as *const bool).cast(),
                ),
                "formatter set trim",
            )?;
            check_result(
                sys::ghostty_formatter_set(
                    self.formatter,
                    sys::GhosttyFormatterOption::Selection,
                    (&raw_sel as *const sys::GhosttySelection).cast(),
                ),
                "formatter set selection",
            )?;
        }

        let format_result = unsafe {
            encode_with_retry("ghostty_formatter_format_buf", |buf, len, out_written| {
                sys::ghostty_formatter_format_buf(self.formatter, buf, len, out_written)
            })
        };

        // Clear borrowed grid-ref selection before returning so stale refs are never retained.
        unsafe {
            let _ = sys::ghostty_formatter_set(
                self.formatter,
                sys::GhosttyFormatterOption::Selection,
                std::ptr::null(),
            );
        }

        let bytes = format_result?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn total_rows(&self) -> usize {
        let mut count: usize = 0;
        let rc = unsafe {
            sys::ghostty_terminal_get(
                self.terminal,
                sys::GhosttyTerminalData::TotalRows,
                (&mut count as *mut usize).cast(),
            )
        };
        if rc == sys::GhosttyResult::Success {
            count
        } else {
            0
        }
    }

    fn screen_grid_ref(
        &self,
        point: BufferPoint,
    ) -> Result<Option<sys::GhosttyGridRef>, TermError> {
        let total_rows = self.total_rows();
        if self.cols == 0 || total_rows == 0 {
            return Ok(None);
        }
        let x = point.col.min(self.cols - 1);
        let y = u32::try_from(point.row.min(total_rows - 1)).unwrap_or(u32::MAX);
        let pt = sys::GhosttyPoint {
            tag: sys::GhosttyPointTag::Screen,
            value: sys::GhosttyPointValue {
                coordinate: sys::GhosttyPointCoordinate { x, y },
            },
        };
        let mut grid_ref = sys::GhosttyGridRef::default();
        check_result(
            unsafe { sys::ghostty_terminal_grid_ref(self.terminal, pt, &mut grid_ref) },
            "ghostty_terminal_grid_ref",
        )?;
        Ok(Some(grid_ref))
    }

    fn buffer_point_from_grid_ref(
        &self,
        grid_ref: &sys::GhosttyGridRef,
    ) -> Result<BufferPoint, TermError> {
        let mut coord = sys::GhosttyPointCoordinate { x: 0, y: 0 };
        check_result(
            unsafe {
                sys::ghostty_terminal_point_from_grid_ref(
                    self.terminal,
                    grid_ref,
                    sys::GhosttyPointTag::Screen,
                    &mut coord,
                )
            },
            "ghostty_terminal_point_from_grid_ref",
        )?;
        Ok(BufferPoint::new(coord.y as usize, coord.x))
    }

    /// Update render state incrementally using dirty row tracking and return
    /// a new [`TerminalSnapshot`] only if any visible grid cell, color, cursor,
    /// mode, title, or exit state actually changed since the previous publication.
    pub fn poll_snapshot(
        &mut self,
        exited: bool,
        exit_status: Option<ExitStatus>,
    ) -> Result<Option<TerminalSnapshot>, TermError> {
        let (snap, changed) = self.build_snapshot(exited, exit_status)?;
        Ok(changed.then_some(snap))
    }

    /// Update render state incrementally and return the current [`TerminalSnapshot`].
    pub fn snapshot(
        &mut self,
        exited: bool,
        exit_status: Option<ExitStatus>,
    ) -> Result<TerminalSnapshot, TermError> {
        let (snap, _) = self.build_snapshot(exited, exit_status)?;
        Ok(snap)
    }

    /// Number of viewport rows rebuilt during the most recent snapshot update.
    pub fn last_dirty_rows_rebuilt(&self) -> usize {
        self.last_dirty_rows
    }

    fn build_snapshot(
        &mut self,
        exited: bool,
        exit_status: Option<ExitStatus>,
    ) -> Result<(TerminalSnapshot, bool), TermError> {
        unsafe {
            check_result(
                sys::ghostty_render_state_update(self.render_state, self.terminal),
                "ghostty_render_state_update",
            )?;

            let mut cols: u16 = 0;
            let mut rows: u16 = 0;
            check_result(
                sys::ghostty_render_state_get(
                    self.render_state,
                    sys::GhosttyRenderStateData::Cols,
                    (&mut cols as *mut u16).cast(),
                ),
                "render_state cols",
            )?;
            check_result(
                sys::ghostty_render_state_get(
                    self.render_state,
                    sys::GhosttyRenderStateData::Rows,
                    (&mut rows as *mut u16).cast(),
                ),
                "render_state rows",
            )?;

            let mut dirty = sys::GhosttyRenderStateDirty::Full;
            check_result(
                sys::ghostty_render_state_get(
                    self.render_state,
                    sys::GhosttyRenderStateData::Dirty,
                    (&mut dirty as *mut sys::GhosttyRenderStateDirty).cast(),
                ),
                "render_state dirty",
            )?;

            let mut colors: sys::GhosttyRenderStateColors = std::mem::zeroed();
            sys::ghostty_sized_init(
                (&mut colors as *mut sys::GhosttyRenderStateColors).cast(),
                std::mem::size_of::<sys::GhosttyRenderStateColors>(),
            );
            check_result(
                sys::ghostty_render_state_get(
                    self.render_state,
                    sys::GhosttyRenderStateData::Colors,
                    (&mut colors as *mut sys::GhosttyRenderStateColors).cast(),
                ),
                "render_state colors",
            )?;

            let mut cursor_raw: sys::GhosttyRenderStateCursor = std::mem::zeroed();
            sys::ghostty_sized_init(
                (&mut cursor_raw as *mut sys::GhosttyRenderStateCursor).cast(),
                std::mem::size_of::<sys::GhosttyRenderStateCursor>(),
            );
            check_result(
                sys::ghostty_render_state_get(
                    self.render_state,
                    sys::GhosttyRenderStateData::Cursor,
                    (&mut cursor_raw as *mut sys::GhosttyRenderStateCursor).cast(),
                ),
                "render_state cursor",
            )?;

            let default_fg = Rgb::from(colors.foreground);
            let default_bg = Rgb::from(colors.background);
            let explicit_cursor_color = if colors.cursor_has_value {
                Some(Rgb::from(colors.cursor))
            } else {
                None
            };
            let mut palette = [Rgb::default(); 16];
            for (dst, src) in palette.iter_mut().zip(colors.palette[..16].iter()) {
                *dst = Rgb::from(*src);
            }

            let target_rows = usize::from(rows);
            if self.cached_grid.len() != target_rows || self.cached_cols != cols {
                self.cached_grid
                    .resize_with(target_rows, RowSnapshot::default);
                self.cached_cols = cols;
            }

            self.last_dirty_rows = 0;
            if dirty != sys::GhosttyRenderStateDirty::False {
                check_result(
                    sys::ghostty_render_state_get(
                        self.render_state,
                        sys::GhosttyRenderStateData::RowIterator,
                        (&mut self.row_iter as *mut sys::GhosttyRenderStateRowIterator).cast(),
                    ),
                    "render_state row_iterator",
                )?;

                let mut utf8_buf = [0u8; 64];
                let mut y: u16 = 0;
                while sys::ghostty_render_state_row_iterator_next_dirty(self.row_iter, &mut y) {
                    let row_idx = usize::from(y);
                    if row_idx >= self.cached_grid.len() {
                        continue;
                    }
                    self.cached_grid[row_idx] = read_render_row(
                        self.row_iter,
                        self.row_cells,
                        cols,
                        default_fg,
                        default_bg,
                        &mut utf8_buf,
                    )?;
                    self.last_dirty_rows += 1;
                }

                let _ = sys::ghostty_render_state_clean(self.render_state);
            }

            let mut scrollbar = sys::GhosttyTerminalScrollbar::default();
            let _ = sys::ghostty_terminal_get(
                self.terminal,
                sys::GhosttyTerminalData::Scrollbar,
                (&mut scrollbar as *mut sys::GhosttyTerminalScrollbar).cast(),
            );

            let mut scrollback_rows: usize = 0;
            let _ = sys::ghostty_terminal_get(
                self.terminal,
                sys::GhosttyTerminalData::ScrollbackRows,
                (&mut scrollback_rows as *mut usize).cast(),
            );

            let mut viewport_active = true;
            let _ = sys::ghostty_terminal_get(
                self.terminal,
                sys::GhosttyTerminalData::ViewportActive,
                (&mut viewport_active as *mut bool).cast(),
            );

            let mut screen = sys::GhosttyTerminalScreen::Primary;
            let _ = sys::ghostty_terminal_get(
                self.terminal,
                sys::GhosttyTerminalData::ActiveScreen,
                (&mut screen as *mut sys::GhosttyTerminalScreen).cast(),
            );

            let mut mouse_tracking = false;
            let _ = sys::ghostty_terminal_get(
                self.terminal,
                sys::GhosttyTerminalData::MouseTracking,
                (&mut mouse_tracking as *mut bool).cast(),
            );

            let bracketed_paste =
                query_dec_mode(self.terminal, DEC_MODE_BRACKETED_PASTE).unwrap_or(false);
            let focus_reporting =
                query_dec_mode(self.terminal, DEC_MODE_FOCUS_EVENT).unwrap_or(false);

            let snap = TerminalSnapshot {
                cols,
                rows,
                default_fg,
                default_bg,
                explicit_cursor_color,
                palette,
                cursor: CursorSnapshot {
                    in_viewport: cursor_raw.viewport_has_value,
                    col: cursor_raw.viewport_x,
                    row: cursor_raw.viewport_y,
                    wide_tail: cursor_raw.wide_tail,
                    visible: cursor_raw.visible,
                    blinking: cursor_raw.blinking,
                    password_input: cursor_raw.password_input,
                    shape: CursorShape::from(cursor_raw.visual_style),
                },
                grid: self.cached_grid.clone(),
                scrollbar: ScrollbarSnapshot {
                    total_rows: scrollbar.total,
                    viewport_offset: scrollbar.offset,
                    viewport_len: scrollbar.len,
                },
                scrollback_rows,
                viewport_active,
                alternate_screen: screen == sys::GhosttyTerminalScreen::Alternate,
                mouse_tracking,
                bracketed_paste,
                focus_reporting,
                title: self.callbacks.title.clone(),
                exited,
                exit_status,
            };

            let changed = self.last_published.as_ref() != Some(&snap);
            if changed {
                self.last_published = Some(snap.clone());
            }

            Ok((snap, changed))
        }
    }

    /// Number of historical scrollback rows currently retained above the active area.
    pub fn scrollback_len(&self) -> usize {
        let mut count: usize = 0;
        let rc = unsafe {
            sys::ghostty_terminal_get(
                self.terminal,
                sys::GhosttyTerminalData::ScrollbackRows,
                (&mut count as *mut usize).cast(),
            )
        };
        if rc == sys::GhosttyResult::Success {
            count
        } else {
            0
        }
    }

    /// Read up to `count` rows from scrollback history starting at `start_row`
    /// (`0` is the oldest retained row).
    pub fn read_scrollback_rows(
        &self,
        start_row: usize,
        count: usize,
    ) -> Result<Vec<RowSnapshot>, TermError> {
        let total_history = self.scrollback_len();
        if start_row >= total_history || count == 0 {
            return Ok(Vec::new());
        }

        let end_row = start_row.saturating_add(count).min(total_history);
        let mut cols: u16 = 0;
        unsafe {
            check_result(
                sys::ghostty_terminal_get(
                    self.terminal,
                    sys::GhosttyTerminalData::Cols,
                    (&mut cols as *mut u16).cast(),
                ),
                "terminal cols",
            )?;
        }

        let mut palette = [sys::GhosttyColorRgb::default(); 256];
        unsafe {
            check_result(
                sys::ghostty_terminal_get(
                    self.terminal,
                    sys::GhosttyTerminalData::ColorPalette,
                    palette.as_mut_ptr().cast(),
                ),
                "terminal color palette",
            )?;
        }

        let mut rows_out = Vec::with_capacity(end_row - start_row);
        let mut cp_buf = [0u32; 16];

        for y in start_row..end_row {
            let mut cells_out = Vec::with_capacity(usize::from(cols));
            let mut wrap = false;
            let mut wrap_continuation = false;

            for x in 0..cols {
                let point = sys::GhosttyPoint {
                    tag: sys::GhosttyPointTag::History,
                    value: sys::GhosttyPointValue {
                        coordinate: sys::GhosttyPointCoordinate { x, y: y as u32 },
                    },
                };
                let mut grid_ref = sys::GhosttyGridRef::default();
                let rc =
                    unsafe { sys::ghostty_terminal_grid_ref(self.terminal, point, &mut grid_ref) };
                if rc != sys::GhosttyResult::Success {
                    break;
                }

                if x == 0 {
                    let mut raw_row: sys::GhosttyRow = 0;
                    if unsafe { sys::ghostty_grid_ref_row(&grid_ref, &mut raw_row) }
                        == sys::GhosttyResult::Success
                    {
                        unsafe {
                            let _ = sys::ghostty_row_get(
                                raw_row,
                                sys::GhosttyRowData::Wrap,
                                (&mut wrap as *mut bool).cast(),
                            );
                            let _ = sys::ghostty_row_get(
                                raw_row,
                                sys::GhosttyRowData::WrapContinuation,
                                (&mut wrap_continuation as *mut bool).cast(),
                            );
                        }
                    }
                }

                let mut raw_cell: sys::GhosttyCell = 0;
                unsafe {
                    let _ = sys::ghostty_grid_ref_cell(&grid_ref, &mut raw_cell);
                }

                let mut wide = sys::GhosttyCellWide::Narrow;
                unsafe {
                    let _ = sys::ghostty_cell_get(
                        raw_cell,
                        sys::GhosttyCellData::Wide,
                        (&mut wide as *mut sys::GhosttyCellWide).cast(),
                    );
                }
                let width = CellWidth::from(wide);

                let mut style: sys::GhosttyStyle = unsafe { std::mem::zeroed() };
                unsafe {
                    let _ = sys::ghostty_grid_ref_style(&grid_ref, &mut style);
                }
                let attrs = CellAttributes::from_ghostty_flags(style.flags);

                let fg = resolve_style_color(style.fg_color, &palette).unwrap_or(self.default_fg);
                let (bg, has_explicit_bg) = match resolve_style_color(style.bg_color, &palette) {
                    Some(c) => (c, true),
                    None => (self.default_bg, false),
                };

                let text = if matches!(width, CellWidth::SpacerTail | CellWidth::SpacerHead) {
                    String::new()
                } else {
                    read_grid_ref_text(&grid_ref, &mut cp_buf)
                };

                cells_out.push(CellSnapshot {
                    text,
                    fg,
                    bg,
                    has_explicit_bg,
                    attrs,
                    width,
                });
            }

            rows_out.push(RowSnapshot {
                cells: cells_out,
                wrap,
                wrap_continuation,
            });
        }

        Ok(rows_out)
    }
}

impl Drop for TerminalCore {
    fn drop(&mut self) {
        // Clear callbacks first so terminal destruction never dereferences freed userdata.
        unsafe {
            let _ = sys::ghostty_terminal_set(
                self.terminal,
                sys::GhosttyTerminalOption::WritePty,
                std::ptr::null(),
            );
            let _ = sys::ghostty_terminal_set(
                self.terminal,
                sys::GhosttyTerminalOption::TitleChanged,
                std::ptr::null(),
            );
            let _ = sys::ghostty_terminal_set(
                self.terminal,
                sys::GhosttyTerminalOption::Bell,
                std::ptr::null(),
            );
            let _ = sys::ghostty_terminal_set(
                self.terminal,
                sys::GhosttyTerminalOption::Userdata,
                std::ptr::null(),
            );

            sys::ghostty_formatter_free(self.formatter);
            sys::ghostty_mouse_event_free(self.mouse_event);
            sys::ghostty_mouse_encoder_free(self.mouse_encoder);
            sys::ghostty_key_event_free(self.key_event);
            sys::ghostty_key_encoder_free(self.key_encoder);
            sys::ghostty_render_state_row_cells_free(self.row_cells);
            sys::ghostty_render_state_row_iterator_free(self.row_iter);
            sys::ghostty_render_state_free(self.render_state);
            sys::ghostty_terminal_free(self.terminal);
        }
    }
}

fn check_result(rc: sys::GhosttyResult, op: &'static str) -> Result<(), TermError> {
    if rc == sys::GhosttyResult::Success {
        Ok(())
    } else {
        Err(TermError::Backend {
            op,
            code: rc as i32,
        })
    }
}

fn query_dec_mode(terminal: sys::GhosttyTerminal, number: u16) -> Result<bool, TermError> {
    let mut cfg = sys::GhosttyTerminalModeConfig {
        mode: sys::ghostty_mode_new(number, false),
        value: false,
    };
    check_result(
        unsafe {
            sys::ghostty_terminal_get(
                terminal,
                sys::GhosttyTerminalData::Mode,
                (&mut cfg as *mut sys::GhosttyTerminalModeConfig).cast(),
            )
        },
        "query terminal mode",
    )?;
    Ok(cfg.value)
}

unsafe fn encode_with_retry(
    op: &'static str,
    mut f: impl FnMut(*mut u8, usize, *mut usize) -> sys::GhosttyResult,
) -> Result<Vec<u8>, TermError> {
    let mut stack_buf = [0u8; 256];
    let mut written: usize = 0;
    let rc = f(stack_buf.as_mut_ptr(), stack_buf.len(), &mut written);
    match rc {
        sys::GhosttyResult::Success => Ok(stack_buf[..written].to_vec()),
        sys::GhosttyResult::OutOfSpace => {
            let mut heap_buf = vec![0u8; written];
            let mut retry_written: usize = 0;
            check_result(
                f(heap_buf.as_mut_ptr(), heap_buf.len(), &mut retry_written),
                op,
            )?;
            heap_buf.truncate(retry_written);
            Ok(heap_buf)
        }
        other => Err(TermError::Backend {
            op,
            code: other as i32,
        }),
    }
}

unsafe fn read_render_row(
    row_iter: sys::GhosttyRenderStateRowIterator,
    row_cells: sys::GhosttyRenderStateRowCells,
    cols: u16,
    default_fg: Rgb,
    default_bg: Rgb,
    utf8_buf: &mut [u8; 64],
) -> Result<RowSnapshot, TermError> {
    let mut raw_row: sys::GhosttyRow = 0;
    check_result(
        unsafe {
            sys::ghostty_render_state_row_get(
                row_iter,
                sys::GhosttyRenderStateRowData::Raw,
                (&mut raw_row as *mut sys::GhosttyRow).cast(),
            )
        },
        "row raw",
    )?;

    let mut wrap = false;
    let mut wrap_continuation = false;
    unsafe {
        let _ = sys::ghostty_row_get(
            raw_row,
            sys::GhosttyRowData::Wrap,
            (&mut wrap as *mut bool).cast(),
        );
        let _ = sys::ghostty_row_get(
            raw_row,
            sys::GhosttyRowData::WrapContinuation,
            (&mut wrap_continuation as *mut bool).cast(),
        );

        check_result(
            sys::ghostty_render_state_row_get(
                row_iter,
                sys::GhosttyRenderStateRowData::Cells,
                (&row_cells as *const sys::GhosttyRenderStateRowCells as *mut c_void).cast(),
            ),
            "row cells",
        )?;
    }

    let mut cells = Vec::with_capacity(usize::from(cols));
    while unsafe { sys::ghostty_render_state_row_cells_next(row_cells) } {
        let mut raw_cell: sys::GhosttyCell = 0;
        check_result(
            unsafe {
                sys::ghostty_render_state_row_cells_get(
                    row_cells,
                    sys::GhosttyRenderStateRowCellsData::Raw,
                    (&mut raw_cell as *mut sys::GhosttyCell).cast(),
                )
            },
            "cell raw",
        )?;

        let mut wide = sys::GhosttyCellWide::Narrow;
        unsafe {
            let _ = sys::ghostty_cell_get(
                raw_cell,
                sys::GhosttyCellData::Wide,
                (&mut wide as *mut sys::GhosttyCellWide).cast(),
            );
        }
        let width = CellWidth::from(wide);

        let mut style: sys::GhosttyStyle = unsafe { std::mem::zeroed() };
        unsafe {
            let _ = sys::ghostty_render_state_row_cells_get(
                row_cells,
                sys::GhosttyRenderStateRowCellsData::Style,
                (&mut style as *mut sys::GhosttyStyle).cast(),
            );
        }
        let attrs = CellAttributes::from_ghostty_flags(style.flags);

        let mut fg_raw = sys::GhosttyColorRgb::default();
        let fg_rc = unsafe {
            sys::ghostty_render_state_row_cells_get(
                row_cells,
                sys::GhosttyRenderStateRowCellsData::FgColor,
                (&mut fg_raw as *mut sys::GhosttyColorRgb).cast(),
            )
        };
        let fg = if fg_rc == sys::GhosttyResult::Success {
            Rgb::from(fg_raw)
        } else {
            default_fg
        };

        let mut bg_raw = sys::GhosttyColorRgb::default();
        let bg_rc = unsafe {
            sys::ghostty_render_state_row_cells_get(
                row_cells,
                sys::GhosttyRenderStateRowCellsData::BgColor,
                (&mut bg_raw as *mut sys::GhosttyColorRgb).cast(),
            )
        };
        let (bg, has_explicit_bg) = if bg_rc == sys::GhosttyResult::Success {
            (Rgb::from(bg_raw), true)
        } else {
            (default_bg, false)
        };

        let text = if matches!(width, CellWidth::SpacerTail | CellWidth::SpacerHead) {
            String::new()
        } else {
            unsafe { read_cell_utf8(row_cells, utf8_buf) }
        };

        cells.push(CellSnapshot {
            text,
            fg,
            bg,
            has_explicit_bg,
            attrs,
            width,
        });
    }

    Ok(RowSnapshot {
        cells,
        wrap,
        wrap_continuation,
    })
}

unsafe fn read_cell_utf8(
    cells: sys::GhosttyRenderStateRowCells,
    stack_buf: &mut [u8; 64],
) -> String {
    let mut buf = sys::GhosttyBuffer {
        ptr: stack_buf.as_mut_ptr(),
        cap: stack_buf.len(),
        len: 0,
    };
    let rc = unsafe {
        sys::ghostty_render_state_row_cells_get(
            cells,
            sys::GhosttyRenderStateRowCellsData::GraphemesUtf8,
            (&mut buf as *mut sys::GhosttyBuffer).cast(),
        )
    };
    match rc {
        sys::GhosttyResult::Success => {
            if buf.len == 0 {
                String::new()
            } else {
                String::from_utf8_lossy(&stack_buf[..buf.len]).into_owned()
            }
        }
        sys::GhosttyResult::OutOfSpace => {
            let mut heap = vec![0u8; buf.len];
            let mut retry_buf = sys::GhosttyBuffer {
                ptr: heap.as_mut_ptr(),
                cap: heap.len(),
                len: 0,
            };
            let retry_rc = unsafe {
                sys::ghostty_render_state_row_cells_get(
                    cells,
                    sys::GhosttyRenderStateRowCellsData::GraphemesUtf8,
                    (&mut retry_buf as *mut sys::GhosttyBuffer).cast(),
                )
            };
            if retry_rc == sys::GhosttyResult::Success {
                String::from_utf8_lossy(&heap[..retry_buf.len]).into_owned()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

fn read_grid_ref_text(grid_ref: &sys::GhosttyGridRef, stack_cp: &mut [u32; 16]) -> String {
    let mut written: usize = 0;
    let rc = unsafe {
        sys::ghostty_grid_ref_graphemes(
            grid_ref,
            stack_cp.as_mut_ptr(),
            stack_cp.len(),
            &mut written,
        )
    };
    let mut heap_cp = Vec::new();
    let slice: &[u32] = match rc {
        sys::GhosttyResult::Success => &stack_cp[..written],
        sys::GhosttyResult::OutOfSpace => {
            heap_cp.resize(written, 0);
            let mut retry_written: usize = 0;
            let retry_rc = unsafe {
                sys::ghostty_grid_ref_graphemes(
                    grid_ref,
                    heap_cp.as_mut_ptr(),
                    heap_cp.len(),
                    &mut retry_written,
                )
            };
            if retry_rc != sys::GhosttyResult::Success {
                return String::new();
            }
            &heap_cp[..retry_written]
        }
        _ => return String::new(),
    };

    let mut out = String::with_capacity(slice.len() * 4);
    for &cp in slice {
        if let Some(ch) = char::from_u32(cp) {
            out.push(ch);
        }
    }
    out
}

fn resolve_style_color(
    color: sys::GhosttyStyleColor,
    palette: &[sys::GhosttyColorRgb; 256],
) -> Option<Rgb> {
    match color.tag {
        x if x == sys::GhosttyStyleColorTag::None as u8 => None,
        x if x == sys::GhosttyStyleColorTag::Palette as u8 => {
            let idx = unsafe { color.value.palette };
            Some(Rgb::from(palette[usize::from(idx)]))
        }
        x if x == sys::GhosttyStyleColorTag::Rgb as u8 => {
            let rgb = unsafe { color.value.rgb };
            Some(Rgb::from(rgb))
        }
        _ => None,
    }
}

enum ActorCmd {
    PtyBytes(Vec<u8>),
    PtyEof,
    WriteBytes(Vec<u8>),
    SendKey(TermKeyEvent),
    Paste(String),
    FocusChanged(bool),
    Mouse(TermMouseEvent),
    Resize {
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    },
    Scroll(ScrollAction),
    SetTheme(TerminalTheme),
    SelectWord {
        point: BufferPoint,
        reply: SyncSender<Result<Option<TerminalSelection>, TermError>>,
    },
    SelectLine {
        point: BufferPoint,
        reply: SyncSender<Result<Option<TerminalSelection>, TermError>>,
    },
    SelectedText {
        selection: TerminalSelection,
        reply: SyncSender<Result<String, TermError>>,
    },
    ReadScrollback {
        start_row: usize,
        count: usize,
        reply: SyncSender<Result<Vec<RowSnapshot>, TermError>>,
    },
    Shutdown,
}

/// Interactive PTY-backed terminal session running on dedicated background threads.
///
/// Dropping a `TerminalSession` kills the child process (and its Unix process
/// group), reaps it, closes the PTY master handle, and joins both background threads.
pub struct TerminalSession {
    cmd_tx: Sender<ActorCmd>,
    snapshot: Arc<RwLock<TerminalSnapshot>>,
    child_pid: Option<u32>,
    actor_thread: Option<JoinHandle<()>>,
    reader_thread: Option<JoinHandle<()>>,
}

impl TerminalSession {
    /// Spawn a shell in a PTY and return the session together with an event channel.
    pub fn spawn(options: TerminalOptions) -> Result<(Self, Receiver<TerminalEvent>), TermError> {
        let (event_tx, event_rx) = mpsc::channel();
        let session = Self::spawn_with_callback(
            options,
            Box::new(move |event| {
                let _ = event_tx.send(event);
            }),
        )?;
        Ok((session, event_rx))
    }

    /// Spawn a shell in a PTY, delivering [`TerminalEvent`] notifications directly
    /// to `on_event` on the actor thread.
    pub fn spawn_with_callback(
        options: TerminalOptions,
        on_event: EventCallback,
    ) -> Result<Self, TermError> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: options.rows.max(1),
                cols: options.cols.max(1),
                pixel_width: options.cols.saturating_mul(options.cell_width_px as u16),
                pixel_height: options.rows.saturating_mul(options.cell_height_px as u16),
            })
            .map_err(|err| TermError::Io(format!("openpty failed: {err}")))?;

        let mut cmd = build_shell_command(&options);
        let child = pair
            .slave
            .spawn_command(cmd.take())
            .map_err(|err| TermError::Io(format!("spawn shell failed: {err}")))?;
        let child_pid = child.process_id();
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| TermError::Io(format!("clone pty reader failed: {err}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|err| TermError::Io(format!("take pty writer failed: {err}")))?;

        let (cmd_tx, cmd_rx) = mpsc::channel::<ActorCmd>();
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<TerminalSnapshot, TermError>>(1);
        let shared_snapshot = Arc::new(RwLock::new(TerminalSnapshot::default()));
        let actor_snapshot = Arc::clone(&shared_snapshot);

        let reader_tx = cmd_tx.clone();
        let reader_thread = thread::Builder::new()
            .name("strata-term-pty-read".to_string())
            .spawn(move || pty_reader_loop(reader, reader_tx))
            .map_err(|err| TermError::Io(format!("spawn pty reader thread failed: {err}")))?;

        let actor_thread = thread::Builder::new()
            .name("strata-term-vt".to_string())
            .spawn(move || {
                actor_loop(
                    options,
                    pair.master,
                    writer,
                    child,
                    cmd_rx,
                    actor_snapshot,
                    on_event,
                    init_tx,
                );
            })
            .map_err(|err| TermError::Io(format!("spawn terminal actor thread failed: {err}")))?;

        let initial_snapshot = init_rx.recv().map_err(|_| TermError::SessionStopped)??;
        if let Ok(mut guard) = shared_snapshot.write() {
            *guard = initial_snapshot;
        }

        Ok(Self {
            cmd_tx,
            snapshot: shared_snapshot,
            child_pid,
            actor_thread: Some(actor_thread),
            reader_thread: Some(reader_thread),
        })
    }

    /// Read the latest published grid snapshot.
    pub fn snapshot(&self) -> TerminalSnapshot {
        self.snapshot
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Read the latest published scrollbar metrics (for host scrollbar rendering).
    pub fn scrollbar(&self) -> ScrollbarSnapshot {
        self.snapshot
            .read()
            .map(|guard| guard.scrollbar)
            .unwrap_or_default()
    }

    /// OS process ID of the spawned shell child process, if exposed by the platform PTY.
    pub fn child_pid(&self) -> Option<u32> {
        self.child_pid
    }

    /// Write UTF-8 text directly to the PTY (e.g. an IME commit).
    pub fn write_str(&self, text: &str) {
        self.write_bytes(text.as_bytes());
    }

    /// Write raw VT/UTF-8 input bytes directly to the PTY.
    pub fn write_bytes(&self, bytes: impl Into<Vec<u8>>) {
        let data = bytes.into();
        if !data.is_empty() {
            let _ = self.cmd_tx.send(ActorCmd::WriteBytes(data));
        }
    }

    /// Encode a structured key event via `libghostty-vt` and write the resulting
    /// escape sequence (if any) to the PTY.
    pub fn send_key(&self, event: TermKeyEvent) {
        let _ = self.cmd_tx.send(ActorCmd::SendKey(event));
    }

    /// Paste clipboard text, honoring bracketed-paste mode (DEC mode 2004).
    pub fn paste(&self, text: impl Into<String>) {
        let text = text.into();
        if !text.is_empty() {
            let _ = self.cmd_tx.send(ActorCmd::Paste(text));
        }
    }

    /// Notify the terminal when focus enters or leaves; writes `CSI I` / `CSI O`
    /// only if focus reporting (DEC mode 1004) is enabled.
    pub fn focus_changed(&self, focused: bool) {
        let _ = self.cmd_tx.send(ActorCmd::FocusChanged(focused));
    }

    /// Handle a mouse event: encodes mouse press/release/motion/wheel sequences
    /// when mouse tracking is active, or scrolls local scrollback / sends
    /// alternate-screen wheel keys otherwise.
    pub fn handle_mouse(&self, event: TermMouseEvent) {
        let _ = self.cmd_tx.send(ActorCmd::Mouse(event));
    }

    /// Resize both the PTY and `libghostty-vt` grid.
    pub fn resize(&self, cols: u16, rows: u16, cell_width_px: u32, cell_height_px: u32) {
        let _ = self.cmd_tx.send(ActorCmd::Resize {
            cols: cols.max(1),
            rows: rows.max(1),
            cell_width_px: cell_width_px.max(1),
            cell_height_px: cell_height_px.max(1),
        });
    }

    /// Scroll the terminal viewport into scrollback history or back to the bottom.
    pub fn scroll(&self, action: ScrollAction) {
        let _ = self.cmd_tx.send(ActorCmd::Scroll(action));
    }

    /// Update terminal foreground, background, cursor, and 16-color ANSI palette,
    /// triggering a re-render and [`TerminalEvent::SnapshotChanged`].
    pub fn set_theme(&self, theme: TerminalTheme) {
        let _ = self.cmd_tx.send(ActorCmd::SetTheme(theme));
    }

    /// Select the word under `point` in full buffer coordinates.
    pub fn select_word(&self, point: BufferPoint) -> Result<Option<TerminalSelection>, TermError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.cmd_tx
            .send(ActorCmd::SelectWord {
                point,
                reply: reply_tx,
            })
            .map_err(|_| TermError::SessionStopped)?;
        reply_rx.recv().map_err(|_| TermError::SessionStopped)?
    }

    /// Select the logical line (including soft-wrapped continuations) at `point`.
    pub fn select_line(&self, point: BufferPoint) -> Result<Option<TerminalSelection>, TermError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.cmd_tx
            .send(ActorCmd::SelectLine {
                point,
                reply: reply_tx,
            })
            .map_err(|_| TermError::SessionStopped)?;
        reply_rx.recv().map_err(|_| TermError::SessionStopped)?
    }

    /// Extract plain text covered by `selection`, joining soft-wrapped rows and
    /// trimming trailing blanks.
    pub fn selected_text(&self, selection: TerminalSelection) -> Result<String, TermError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.cmd_tx
            .send(ActorCmd::SelectedText {
                selection,
                reply: reply_tx,
            })
            .map_err(|_| TermError::SessionStopped)?;
        reply_rx.recv().map_err(|_| TermError::SessionStopped)?
    }

    /// Query a range of rows directly from scrollback history on the terminal thread.
    pub fn read_scrollback_rows(
        &self,
        start_row: usize,
        count: usize,
    ) -> Result<Vec<RowSnapshot>, TermError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.cmd_tx
            .send(ActorCmd::ReadScrollback {
                start_row,
                count,
                reply: reply_tx,
            })
            .map_err(|_| TermError::SessionStopped)?;
        reply_rx.recv().map_err(|_| TermError::SessionStopped)?
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(ActorCmd::Shutdown);
        if let Some(handle) = self.actor_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
    }
}

struct OwnedCommand(CommandBuilder);

impl OwnedCommand {
    fn take(&mut self) -> CommandBuilder {
        std::mem::replace(&mut self.0, CommandBuilder::new(""))
    }
}

fn build_shell_command(options: &TerminalOptions) -> OwnedCommand {
    let mut cmd = match (&options.shell, options.shell_args.is_empty()) {
        // `CommandBuilder::new_default_prog` launches `$SHELL` as a login shell
        // (`argv[0]` prefixed with `-`) on Unix and `%COMSPEC%` on Windows.
        // Note that `CommandBuilder::arg` panics on a default_prog builder, so we
        // fall back to explicit executable resolution only when `shell_args` is non-empty.
        (None, true) => CommandBuilder::new_default_prog(),
        (None, false) => {
            let mut b = CommandBuilder::new(default_shell_executable());
            b.args(&options.shell_args);
            b
        }
        (Some(program), _) => {
            let mut b = CommandBuilder::new(program);
            if !options.shell_args.is_empty() {
                b.args(&options.shell_args);
            }
            b
        }
    };

    if let Some(dir) = &options.working_dir {
        cmd.cwd(dir);
    }
    for (k, v) in &options.env {
        cmd.env(k, v);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    OwnedCommand(cmd)
}

fn default_shell_executable() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "cmd.exe".to_string())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "/bin/sh".to_string())
    }
}

fn pty_reader_loop(mut reader: Box<dyn Read + Send>, cmd_tx: Sender<ActorCmd>) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => {
                let _ = cmd_tx.send(ActorCmd::PtyEof);
                break;
            }
            Ok(n) => {
                if cmd_tx.send(ActorCmd::PtyBytes(buf[..n].to_vec())).is_err() {
                    break;
                }
            }
        }
    }
}

fn convert_exit_status(status: portable_pty::ExitStatus) -> ExitStatus {
    ExitStatus {
        code: status.exit_code(),
        signal: status.signal().map(str::to_string),
    }
}

fn reap_child_after_eof(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
) -> Option<ExitStatus> {
    let deadline = Instant::now() + Duration::from_millis(200);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(convert_exit_status(status)),
            Ok(None) => {
                if Instant::now() >= deadline {
                    return None;
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}

fn terminate_and_reap(child: &mut Box<dyn portable_pty::Child + Send + Sync>) {
    let pid = child.process_id();
    let _ = child.kill();

    #[cfg(unix)]
    if let Some(pid) = pid
        && let Ok(pgid) = libc::pid_t::try_from(pid)
        && pgid > 0
    {
        // Each PTY child is spawned in its own session (`pgid == pid`). Killing the
        // process group ensures background children holding the slave fd also exit.
        unsafe {
            libc::kill(-pgid, libc::SIGHUP);
            libc::kill(-pgid, libc::SIGKILL);
        }
    }

    let _ = child.wait();
}

struct ActorState<'a> {
    core: &'a mut TerminalCore,
    master: &'a dyn MasterPty,
    writer: &'a mut Box<dyn Write + Send>,
    child: &'a mut Box<dyn portable_pty::Child + Send + Sync>,
    exited: bool,
    exit_status: Option<ExitStatus>,
    child_exited_pending: bool,
    current_cols: u16,
    current_rows: u16,
}

impl ActorState<'_> {
    fn write_pty(&mut self, bytes: &[u8]) {
        if !bytes.is_empty() {
            let _ = self.writer.write_all(bytes);
            let _ = self.writer.flush();
        }
    }

    fn handle_cmd(&mut self, cmd: ActorCmd) -> (bool, bool) {
        match cmd {
            ActorCmd::PtyBytes(bytes) => {
                let replies = self.core.feed_bytes(&bytes);
                self.write_pty(&replies);
                (true, false)
            }
            ActorCmd::PtyEof => {
                if !self.exited {
                    self.exited = true;
                    self.exit_status = reap_child_after_eof(self.child);
                    self.child_exited_pending = true;
                }
                (true, false)
            }
            ActorCmd::WriteBytes(bytes) => {
                self.write_pty(&bytes);
                (false, false)
            }
            ActorCmd::SendKey(event) => {
                if let Ok(encoded) = self.core.encode_key(&event) {
                    self.write_pty(&encoded);
                }
                (false, false)
            }
            ActorCmd::Paste(text) => {
                if let Ok(encoded) = self.core.encode_paste(&text) {
                    self.write_pty(&encoded);
                }
                (false, false)
            }
            ActorCmd::FocusChanged(focused) => {
                if let Ok(Some(encoded)) = self.core.encode_focus(focused) {
                    self.write_pty(&encoded);
                }
                (false, false)
            }
            ActorCmd::Mouse(event) => match self.core.handle_mouse(event) {
                Ok(MouseOutcome::Reported(bytes)) => {
                    self.write_pty(&bytes);
                    (false, false)
                }
                Ok(MouseOutcome::ScrolledViewport) => (true, false),
                Ok(MouseOutcome::Ignored) | Err(_) => (false, false),
            },
            ActorCmd::Resize {
                cols,
                rows,
                cell_width_px,
                cell_height_px,
            } => {
                let _ = self.core.resize(cols, rows, cell_width_px, cell_height_px);
                if cols != self.current_cols || rows != self.current_rows {
                    self.current_cols = cols;
                    self.current_rows = rows;
                    let _ = self.master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: cols.saturating_mul(cell_width_px as u16),
                        pixel_height: rows.saturating_mul(cell_height_px as u16),
                    });
                }
                (true, false)
            }
            ActorCmd::Scroll(action) => {
                let _ = self.core.scroll_viewport(action);
                (true, false)
            }
            ActorCmd::SetTheme(theme) => {
                let _ = self.core.set_theme(&theme);
                (true, false)
            }
            ActorCmd::SelectWord { point, reply } => {
                let _ = reply.send(self.core.select_word(point));
                (false, false)
            }
            ActorCmd::SelectLine { point, reply } => {
                let _ = reply.send(self.core.select_line(point));
                (false, false)
            }
            ActorCmd::SelectedText { selection, reply } => {
                let _ = reply.send(self.core.selected_text(&selection));
                (false, false)
            }
            ActorCmd::ReadScrollback {
                start_row,
                count,
                reply,
            } => {
                let _ = reply.send(self.core.read_scrollback_rows(start_row, count));
                (false, false)
            }
            ActorCmd::Shutdown => (false, true),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn actor_loop(
    options: TerminalOptions,
    master: Box<dyn MasterPty + Send>,
    mut writer: Box<dyn Write + Send>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    cmd_rx: Receiver<ActorCmd>,
    shared_snapshot: Arc<RwLock<TerminalSnapshot>>,
    mut on_event: EventCallback,
    init_tx: SyncSender<Result<TerminalSnapshot, TermError>>,
) {
    let mut core = match TerminalCore::new(&options) {
        Ok(core) => core,
        Err(err) => {
            terminate_and_reap(&mut child);
            drop(writer);
            drop(master);
            let _ = init_tx.send(Err(err));
            return;
        }
    };

    match core.snapshot(false, None) {
        Ok(snap) => {
            let _ = init_tx.send(Ok(snap));
        }
        Err(err) => {
            terminate_and_reap(&mut child);
            drop(writer);
            drop(master);
            let _ = init_tx.send(Err(err));
            return;
        }
    }

    let mut state = ActorState {
        core: &mut core,
        master: &*master,
        writer: &mut writer,
        child: &mut child,
        exited: false,
        exit_status: None,
        child_exited_pending: false,
        current_cols: options.cols.max(1),
        current_rows: options.rows.max(1),
    };

    while let Ok(first) = cmd_rx.recv() {
        let (mut visual_pending, mut shutdown) = state.handle_cmd(first);

        // Coalesce bursty PTY reads so high-throughput output publishes one
        // snapshot per batch instead of stalling the UI on intermediate frames.
        for _ in 0..64 {
            if shutdown {
                break;
            }
            match cmd_rx.try_recv() {
                Ok(next) => {
                    let (v, s) = state.handle_cmd(next);
                    visual_pending |= v;
                    shutdown |= s;
                }
                Err(_) => break,
            }
        }

        if shutdown {
            break;
        }

        if state.core.take_bell() {
            on_event(TerminalEvent::Bell);
        }
        if let Some(title) = state.core.take_title_changed() {
            on_event(TerminalEvent::TitleChanged(title));
        }
        if visual_pending
            && let Ok(Some(snap)) = state
                .core
                .poll_snapshot(state.exited, state.exit_status.clone())
        {
            if let Ok(mut guard) = shared_snapshot.write() {
                *guard = snap;
            }
            on_event(TerminalEvent::SnapshotChanged);
        }
        if state.child_exited_pending {
            state.child_exited_pending = false;
            on_event(TerminalEvent::ChildExited(state.exit_status.clone()));
        }
    }

    terminate_and_reap(&mut child);
    // Closing `writer` and `master` immediately unblocks the PTY reader thread.
    drop(writer);
    drop(master);
}
