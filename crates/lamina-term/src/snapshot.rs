//! Plain-data terminal snapshot types safe to send across threads.
//!
//! `libghostty-vt` handles are not thread-safe. The terminal actor owns them
//! on a dedicated thread and publishes immutable [`TerminalSnapshot`] values
//! for UI rendering and inspection.

use crate::sys;

/// 8-bit sRGB color.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

impl From<sys::GhosttyColorRgb> for Rgb {
    fn from(c: sys::GhosttyColorRgb) -> Self {
        Self {
            r: c.r,
            g: c.g,
            b: c.b,
        }
    }
}

/// Visual shape of the terminal cursor.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum CursorShape {
    Bar,
    #[default]
    Block,
    Underline,
    BlockHollow,
}

impl From<sys::GhosttyCursorStyle> for CursorShape {
    fn from(style: sys::GhosttyCursorStyle) -> Self {
        match style {
            sys::GhosttyCursorStyle::Bar => Self::Bar,
            sys::GhosttyCursorStyle::Block => Self::Block,
            sys::GhosttyCursorStyle::Underline => Self::Underline,
            sys::GhosttyCursorStyle::BlockHollow => Self::BlockHollow,
        }
    }
}

/// Cursor state within the visible viewport.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct CursorSnapshot {
    /// Whether the cursor falls inside the visible viewport (false when scrolled up).
    pub in_viewport: bool,
    /// Column (0-indexed) inside the viewport when `in_viewport` is true.
    pub col: u16,
    /// Row (0-indexed) inside the viewport when `in_viewport` is true.
    pub row: u16,
    /// True if the cursor sits on the trailing spacer of a wide character.
    pub wide_tail: bool,
    /// Whether the cursor is shown according to terminal modes (DECTCEM).
    pub visible: bool,
    /// Whether the cursor should blink.
    pub blinking: bool,
    /// True if the terminal suspects a password input prompt.
    pub password_input: bool,
    /// Visual shape requested by DECSCUSR or terminal default.
    pub shape: CursorShape,
}

/// Underline decoration style for a cell.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

impl UnderlineStyle {
    pub fn is_underlined(self) -> bool {
        self != Self::None
    }

    pub(crate) fn from_style_flags(flags: u16) -> Self {
        let raw = (flags & sys::GHOSTTY_STYLE_UNDERLINE_MASK) >> sys::GHOSTTY_STYLE_UNDERLINE_SHIFT;
        match raw {
            1 => Self::Single,
            2 => Self::Double,
            3 => Self::Curly,
            4 => Self::Dotted,
            5 => Self::Dashed,
            _ => Self::None,
        }
    }
}

/// Width classification of a grid cell.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum CellWidth {
    #[default]
    Narrow,
    Wide,
    /// Trailing spacer cell immediately after a wide character.
    SpacerTail,
    /// Leading spacer at the end of a soft-wrapped line before a wide character.
    SpacerHead,
}

impl From<sys::GhosttyCellWide> for CellWidth {
    fn from(wide: sys::GhosttyCellWide) -> Self {
        match wide {
            sys::GhosttyCellWide::Narrow => Self::Narrow,
            sys::GhosttyCellWide::Wide => Self::Wide,
            sys::GhosttyCellWide::SpacerTail => Self::SpacerTail,
            sys::GhosttyCellWide::SpacerHead => Self::SpacerHead,
        }
    }
}

/// Text decoration attributes of a single cell.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct CellAttributes {
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: UnderlineStyle,
}

impl CellAttributes {
    pub(crate) fn from_ghostty_flags(flags: u16) -> Self {
        Self {
            bold: (flags & sys::GHOSTTY_STYLE_FLAG_BOLD) != 0,
            italic: (flags & sys::GHOSTTY_STYLE_FLAG_ITALIC) != 0,
            faint: (flags & sys::GHOSTTY_STYLE_FLAG_FAINT) != 0,
            blink: (flags & sys::GHOSTTY_STYLE_FLAG_BLINK) != 0,
            inverse: (flags & sys::GHOSTTY_STYLE_FLAG_INVERSE) != 0,
            invisible: (flags & sys::GHOSTTY_STYLE_FLAG_INVISIBLE) != 0,
            strikethrough: (flags & sys::GHOSTTY_STYLE_FLAG_STRIKETHROUGH) != 0,
            overline: (flags & sys::GHOSTTY_STYLE_FLAG_OVERLINE) != 0,
            underline: UnderlineStyle::from_style_flags(flags),
        }
    }
}

/// Plain-data representation of one cell in a row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CellSnapshot {
    /// UTF-8 character or grapheme cluster in this cell (empty if blank or spacer).
    pub text: String,
    /// Resolved foreground color (before applying `attrs.inverse`).
    pub fg: Rgb,
    /// Resolved background color (before applying `attrs.inverse`).
    pub bg: Rgb,
    /// True if the cell had an explicit background color (different from default bg).
    pub has_explicit_bg: bool,
    /// Text attributes (bold, italic, underline, inverse, etc.).
    pub attrs: CellAttributes,
    /// Wide character / spacer status.
    pub width: CellWidth,
}

impl CellSnapshot {
    /// Effective foreground color after applying SGR inverse.
    pub fn effective_fg(&self) -> Rgb {
        if self.attrs.inverse { self.bg } else { self.fg }
    }

    /// Effective background color after applying SGR inverse.
    pub fn effective_bg(&self) -> Rgb {
        if self.attrs.inverse { self.fg } else { self.bg }
    }

    /// True if either an explicit background color or SGR inverse sets a non-default cell background.
    pub fn paints_background(&self) -> bool {
        self.has_explicit_bg || self.attrs.inverse
    }
}

/// Plain-data representation of a single terminal row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RowSnapshot {
    pub cells: Vec<CellSnapshot>,
    /// True if this row soft-wrapped onto the next row.
    pub wrap: bool,
    /// True if this row is a continuation of the previous soft-wrapped row.
    pub wrap_continuation: bool,
}

impl RowSnapshot {
    /// Extract trimmed plain text for this row, skipping wide-char spacer cells
    /// and treating empty cells as spaces.
    pub fn text(&self) -> String {
        let mut out = String::with_capacity(self.cells.len());
        for cell in &self.cells {
            if matches!(cell.width, CellWidth::SpacerTail | CellWidth::SpacerHead) {
                continue;
            }
            if cell.text.is_empty() {
                out.push(' ');
            } else {
                out.push_str(&cell.text);
            }
        }
        while out.ends_with(' ') {
            out.pop();
        }
        out
    }
}

/// Scrollbar state reported by libghostty-vt.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct ScrollbarSnapshot {
    /// Total rows in the active screen including scrollback.
    pub total_rows: u64,
    /// Top visible row offset from the start of scrollback (0 = oldest row).
    pub viewport_offset: u64,
    /// Number of visible rows in the viewport.
    pub viewport_len: u64,
}

impl ScrollbarSnapshot {
    /// Maximum reachable `viewport_offset` (when scrolled to the live bottom).
    pub fn max_offset(&self) -> u64 {
        self.total_rows.saturating_sub(self.viewport_len)
    }

    /// True when scrollback contains rows above the current viewport height.
    pub fn is_scrollable(&self) -> bool {
        self.total_rows > self.viewport_len
    }

    /// True when the viewport is pinned to the live bottom.
    pub fn is_at_bottom(&self) -> bool {
        self.viewport_offset >= self.max_offset()
    }
}

/// Caller-configurable color theme for the terminal foreground, background,
/// cursor, and 16-color ANSI palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalTheme {
    pub foreground: Rgb,
    pub background: Rgb,
    /// Explicit cursor color override; `None` restores the terminal/renderer default.
    pub cursor: Option<Rgb>,
    /// Optional 16-color ANSI palette override (indices 0..16); `None` uses the
    /// terminal's built-in palette.
    pub palette: Option<[Rgb; 16]>,
}

impl Default for TerminalTheme {
    fn default() -> Self {
        Self {
            foreground: Rgb::new(0xE6, 0xED, 0xF3),
            background: Rgb::new(0x0D, 0x11, 0x17),
            cursor: None,
            palette: None,
        }
    }
}

/// Child process termination status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitStatus {
    pub code: u32,
    pub signal: Option<String>,
}

impl ExitStatus {
    /// Returns `true` when the child exited cleanly with code 0 and no signal.
    pub fn success(&self) -> bool {
        self.signal.is_none() && self.code == 0
    }
}

/// Plain-data snapshot of the visible terminal grid and metadata.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TerminalSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub default_fg: Rgb,
    pub default_bg: Rgb,
    pub explicit_cursor_color: Option<Rgb>,
    /// Current 16-color ANSI palette (indices 0..16), reflecting both theme
    /// overrides and application OSC 4 updates.
    pub palette: [Rgb; 16],
    pub cursor: CursorSnapshot,
    pub grid: Vec<RowSnapshot>,
    pub scrollbar: ScrollbarSnapshot,
    pub scrollback_rows: usize,
    pub viewport_active: bool,
    pub alternate_screen: bool,
    pub mouse_tracking: bool,
    pub bracketed_paste: bool,
    pub focus_reporting: bool,
    pub title: String,
    pub exited: bool,
    pub exit_status: Option<ExitStatus>,
}

impl TerminalSnapshot {
    /// Trimmed text of a single visible row.
    pub fn row_text(&self, row: usize) -> String {
        self.grid
            .get(row)
            .map(RowSnapshot::text)
            .unwrap_or_default()
    }

    /// All visible rows joined with `\n`, trimming trailing blank lines.
    pub fn text(&self) -> String {
        let lines: Vec<String> = self.grid.iter().map(RowSnapshot::text).collect();
        let last_non_empty = lines.iter().rposition(|l| !l.is_empty());
        match last_non_empty {
            Some(idx) => lines[..=idx].join("\n"),
            None => String::new(),
        }
    }
}
