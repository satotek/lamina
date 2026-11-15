//! Raw C ABI bindings to libghostty-vt 1.0.1.
//!
//! Kept minimal and self-contained so the workspace pins an exact upstream release
//! rather than tracking pre-1.0 third-party `-sys` crates.

#![allow(dead_code, non_camel_case_types)]

use std::ffi::{c_char, c_void};

#[repr(C)]
pub struct GhosttyTerminalImpl {
    _private: [u8; 0],
}
pub type GhosttyTerminal = *mut GhosttyTerminalImpl;

#[repr(C)]
pub struct GhosttyRenderStateImpl {
    _private: [u8; 0],
}
pub type GhosttyRenderState = *mut GhosttyRenderStateImpl;

#[repr(C)]
pub struct GhosttyRenderStateRowIteratorImpl {
    _private: [u8; 0],
}
pub type GhosttyRenderStateRowIterator = *mut GhosttyRenderStateRowIteratorImpl;

#[repr(C)]
pub struct GhosttyRenderStateRowCellsImpl {
    _private: [u8; 0],
}
pub type GhosttyRenderStateRowCells = *mut GhosttyRenderStateRowCellsImpl;

#[repr(C)]
pub struct GhosttyKeyEncoderImpl {
    _private: [u8; 0],
}
pub type GhosttyKeyEncoder = *mut GhosttyKeyEncoderImpl;

#[repr(C)]
pub struct GhosttyKeyEventImpl {
    _private: [u8; 0],
}
pub type GhosttyKeyEvent = *mut GhosttyKeyEventImpl;

#[repr(C)]
pub struct GhosttyMouseEncoderImpl {
    _private: [u8; 0],
}
pub type GhosttyMouseEncoder = *mut GhosttyMouseEncoderImpl;

#[repr(C)]
pub struct GhosttyMouseEventImpl {
    _private: [u8; 0],
}
pub type GhosttyMouseEvent = *mut GhosttyMouseEventImpl;

#[repr(C)]
pub struct GhosttyFormatterImpl {
    _private: [u8; 0],
}
pub type GhosttyFormatter = *mut GhosttyFormatterImpl;

pub type GhosttyAllocator = c_void;
pub type GhosttyCell = u64;
pub type GhosttyRow = u64;
pub type GhosttyKittyKeyFlags = u8;
pub type GhosttyMods = u16;
pub type GhosttyMode = u16;
pub type GhosttyStyleFlags = u16;

pub const fn ghostty_mode_new(value: u16, ansi: bool) -> GhosttyMode {
    (value & 0x7FFF) | ((ansi as u16) << 15)
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyResult {
    Success = 0,
    OutOfMemory = -1,
    InvalidValue = -2,
    OutOfSpace = -3,
    NoValue = -4,
    IoError = -5,
    LimitExceeded = -6,
    Rejected = -7,
    Unsupported = -8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct GhosttyColorRgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct GhosttyString {
    pub ptr: *const u8,
    pub len: usize,
}

#[repr(C)]
pub struct GhosttyBuffer {
    pub ptr: *mut u8,
    pub cap: usize,
    pub len: usize,
}

#[repr(C)]
pub struct GhosttyCodepointBuffer {
    pub ptr: *mut u32,
    pub cap: usize,
    pub len: usize,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyCursorStyle {
    Bar = 0,
    Block = 1,
    Underline = 2,
    BlockHollow = 3,
}

#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyStyleColorTag {
    None = 0,
    Palette = 1,
    Rgb = 2,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union GhosttyStyleColorValue {
    pub palette: u8,
    pub rgb: GhosttyColorRgb,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct GhosttyStyleColor {
    pub tag: u8,
    pub value: GhosttyStyleColorValue,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct GhosttyStyle {
    pub fg_color: GhosttyStyleColor,
    pub bg_color: GhosttyStyleColor,
    pub underline_color: GhosttyStyleColor,
    pub flags: GhosttyStyleFlags,
    pub reserved: u16,
}

pub const GHOSTTY_STYLE_FLAG_BOLD: u16 = 1 << 0;
pub const GHOSTTY_STYLE_FLAG_ITALIC: u16 = 1 << 1;
pub const GHOSTTY_STYLE_FLAG_FAINT: u16 = 1 << 2;
pub const GHOSTTY_STYLE_FLAG_BLINK: u16 = 1 << 3;
pub const GHOSTTY_STYLE_FLAG_INVERSE: u16 = 1 << 4;
pub const GHOSTTY_STYLE_FLAG_INVISIBLE: u16 = 1 << 5;
pub const GHOSTTY_STYLE_FLAG_STRIKETHROUGH: u16 = 1 << 6;
pub const GHOSTTY_STYLE_FLAG_OVERLINE: u16 = 1 << 7;
pub const GHOSTTY_STYLE_UNDERLINE_SHIFT: u16 = 8;
pub const GHOSTTY_STYLE_UNDERLINE_MASK: u16 = 0x7 << GHOSTTY_STYLE_UNDERLINE_SHIFT;

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttySgrUnderline {
    None = 0,
    Single = 1,
    Double = 2,
    Curly = 3,
    Dotted = 4,
    Dashed = 5,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyCellWide {
    Narrow = 0,
    Wide = 1,
    SpacerTail = 2,
    SpacerHead = 3,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyCellData {
    Invalid = 0,
    Codepoint = 1,
    ContentTag = 2,
    Wide = 3,
    HasText = 4,
    HasStyling = 5,
    StyleId = 6,
    HasHyperlink = 7,
    Protected = 8,
    SemanticContent = 9,
    ColorPalette = 10,
    ColorRgb = 11,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyRowData {
    Invalid = 0,
    Wrap = 1,
    WrapContinuation = 2,
    Grapheme = 3,
    Styled = 4,
    Hyperlink = 5,
    SemanticPrompt = 6,
    KittyVirtualPlaceholder = 7,
    Dirty = 8,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyRenderStateDirty {
    False = 0,
    Partial = 1,
    Full = 2,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyRenderStateData {
    Invalid = 0,
    Cols = 1,
    Rows = 2,
    Dirty = 3,
    RowIterator = 4,
    ColorBackground = 5,
    ColorForeground = 6,
    ColorCursor = 7,
    ColorCursorHasValue = 8,
    ColorPalette = 9,
    CursorVisualStyle = 10,
    CursorVisible = 11,
    CursorBlinking = 12,
    CursorPasswordInput = 13,
    CursorViewportHasValue = 14,
    CursorViewportX = 15,
    CursorViewportY = 16,
    CursorViewportWideTail = 17,
    Cursor = 18,
    Colors = 19,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyRenderStateRowData {
    Invalid = 0,
    Dirty = 1,
    Raw = 2,
    Cells = 3,
    Selection = 4,
    CellsRaw = 5,
    ViewportY = 6,
    Id = 7,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyRenderStateRowCellsData {
    Invalid = 0,
    Raw = 1,
    Style = 2,
    GraphemesLen = 3,
    GraphemesBuf = 4,
    BgColor = 5,
    FgColor = 6,
    Selected = 7,
    HasStyling = 8,
    GraphemesUtf8 = 9,
}

#[repr(C)]
pub struct GhosttyRenderStateCursor {
    pub size: usize,
    pub viewport_has_value: bool,
    pub viewport_x: u16,
    pub viewport_y: u16,
    pub wide_tail: bool,
    pub visible: bool,
    pub blinking: bool,
    pub password_input: bool,
    pub visual_style: GhosttyCursorStyle,
}

#[repr(C)]
pub struct GhosttyRenderStateColors {
    pub size: usize,
    pub background: GhosttyColorRgb,
    pub foreground: GhosttyColorRgb,
    pub cursor: GhosttyColorRgb,
    pub cursor_has_value: bool,
    pub palette: [GhosttyColorRgb; 256],
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyTerminalScreen {
    Primary = 0,
    Alternate = 1,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyTerminalOption {
    Invalid = 0,
    Userdata = 1,
    WritePty = 2,
    Bell = 3,
    Enquiry = 4,
    Xtversion = 5,
    TitleChanged = 6,
    Size = 7,
    ColorScheme = 8,
    DeviceAttributes = 9,
    Title = 10,
    Pwd = 11,
    ColorForeground = 12,
    ColorBackground = 13,
    ColorCursor = 14,
    ColorPalette = 15,
    KittyImageStorageLimit = 16,
    KittyImageMediumFile = 17,
    KittyImageMediumTempFile = 18,
    KittyImageMediumSharedMem = 19,
    ApcMaxBytes = 20,
    ApcMaxBytesKitty = 21,
    Selection = 22,
    DefaultCursorStyle = 23,
    DefaultCursorBlink = 24,
    GlyphProtocol = 25,
    PwdChanged = 26,
    ClipboardWrite = 27,
    ScrollbackMaxBytes = 28,
    ScrollbackMaxLines = 29,
    DesktopNotification = 30,
    ProgressReport = 31,
    ContinuationMaxBytes = 32,
    TitleReport = 33,
    ModeDefault = 34,
    Mode = 35,
    UnknownSequence = 36,
    UnknownMaxBytes = 37,
    TerminfoName = 38,
    ClipboardRead = 39,
    ClipboardWriteMaxBytes = 40,
    ResizePullScrollback = 41,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyTerminalData {
    Invalid = 0,
    Cols = 1,
    Rows = 2,
    CursorX = 3,
    CursorY = 4,
    CursorPendingWrap = 5,
    ActiveScreen = 6,
    CursorVisible = 7,
    KittyKeyboardFlags = 8,
    Scrollbar = 9,
    CursorSgrStyle = 10,
    MouseTracking = 11,
    Title = 12,
    Pwd = 13,
    TotalRows = 14,
    ScrollbackRows = 15,
    WidthPx = 16,
    HeightPx = 17,
    ColorForeground = 18,
    ColorBackground = 19,
    ColorCursor = 20,
    ColorPalette = 21,
    ColorPaletteDefault = 25,
    ViewportActive = 32,
    Mode = 37,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct GhosttyTerminalModeConfig {
    pub mode: GhosttyMode,
    pub value: bool,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct GhosttyTerminalScrollbar {
    pub total: u64,
    pub offset: u64,
    pub len: u64,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyTerminalScrollViewportTag {
    Top = 0,
    Bottom = 1,
    Delta = 2,
    Row = 3,
    DeltaPrompt = 4,
}

#[repr(C)]
pub union GhosttyTerminalScrollViewportValue {
    pub delta: isize,
    pub row: usize,
    pub delta_prompt: isize,
    pub _padding: [u64; 2],
}

#[repr(C)]
pub struct GhosttyTerminalScrollViewport {
    pub tag: GhosttyTerminalScrollViewportTag,
    pub value: GhosttyTerminalScrollViewportValue,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyPointCoordinate {
    pub x: u16,
    pub y: u32,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyPointTag {
    Active = 0,
    Viewport = 1,
    Screen = 2,
    History = 3,
}

#[repr(C)]
pub union GhosttyPointValue {
    pub coordinate: GhosttyPointCoordinate,
    pub _padding: [u64; 2],
}

#[repr(C)]
pub struct GhosttyPoint {
    pub tag: GhosttyPointTag,
    pub value: GhosttyPointValue,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct GhosttyGridRef {
    pub node: *mut c_void,
    pub x: u16,
    pub y: u16,
}

impl Default for GhosttyGridRef {
    fn default() -> Self {
        Self {
            node: std::ptr::null_mut(),
            x: 0,
            y: 0,
        }
    }
}

pub type GhosttyTerminalWritePtyFn = unsafe extern "C" fn(
    terminal: GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
);

pub type GhosttyTerminalTitleChangedFn =
    unsafe extern "C" fn(terminal: GhosttyTerminal, userdata: *mut c_void);

pub type GhosttyTerminalBellFn =
    unsafe extern "C" fn(terminal: GhosttyTerminal, userdata: *mut c_void);

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyFocusEvent {
    Gained = 0,
    Lost = 1,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyMouseTrackingMode {
    None = 0,
    X10 = 1,
    Normal = 2,
    Button = 3,
    Any = 4,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyMouseFormat {
    X10 = 0,
    Utf8 = 1,
    Sgr = 2,
    Urxvt = 3,
    SgrPixels = 4,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct GhosttyMouseEncoderSize {
    pub size: usize,
    pub screen_width: u32,
    pub screen_height: u32,
    pub cell_width: u32,
    pub cell_height: u32,
    pub padding_top: u32,
    pub padding_bottom: u32,
    pub padding_right: u32,
    pub padding_left: u32,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyMouseEncoderOption {
    Invalid = 0,
    Event = 1,
    Format = 2,
    Size = 3,
    AnyButtonPressed = 4,
    TrackLastCell = 5,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyMouseAction {
    Press = 0,
    Release = 1,
    Motion = 2,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyMouseButton {
    Unknown = 0,
    Left = 1,
    Right = 2,
    Middle = 3,
    Four = 4,
    Five = 5,
    Six = 6,
    Seven = 7,
    Eight = 8,
    Nine = 9,
    Ten = 10,
    Eleven = 11,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct GhosttyMousePosition {
    pub x: f32,
    pub y: f32,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct GhosttySelection {
    pub start: GhosttyGridRef,
    pub end: GhosttyGridRef,
    pub rectangle: bool,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttySelectionOrder {
    Forward = 0,
    Reverse = 1,
    MirroredForward = 2,
    MirroredReverse = 3,
}

#[repr(C)]
pub struct GhosttyTerminalSelectWordOptions {
    pub size: usize,
    pub grid_ref: GhosttyGridRef,
    pub boundary_codepoints: *const u32,
    pub boundary_codepoints_len: usize,
}

#[repr(C)]
pub struct GhosttyTerminalSelectLineOptions {
    pub size: usize,
    pub grid_ref: GhosttyGridRef,
    pub whitespace: *const u32,
    pub whitespace_len: usize,
    pub semantic_prompt_boundary: bool,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyFormatterFormat {
    Plain = 0,
    Vt = 1,
    Html = 2,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyFormatterOption {
    Invalid = 0,
    Emit = 1,
    Unwrap = 2,
    Trim = 3,
    Selection = 4,
    Extras = 5,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyKeyAction {
    Release = 0,
    Press = 1,
    Repeat = 2,
}

pub const GHOSTTY_MODS_SHIFT: GhosttyMods = 1 << 0;
pub const GHOSTTY_MODS_CTRL: GhosttyMods = 1 << 1;
pub const GHOSTTY_MODS_ALT: GhosttyMods = 1 << 2;
pub const GHOSTTY_MODS_SUPER: GhosttyMods = 1 << 3;
pub const GHOSTTY_MODS_CAPS_LOCK: GhosttyMods = 1 << 4;
pub const GHOSTTY_MODS_NUM_LOCK: GhosttyMods = 1 << 5;

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyOptionAsAlt {
    False = 0,
    True = 1,
    Left = 2,
    Right = 3,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyKeyEncoderOption {
    Invalid = 0,
    CursorKeyApplication = 1,
    KeypadKeyApplication = 2,
    IgnoreKeypadWithNumlock = 3,
    AltEscPrefix = 4,
    ModifyOtherKeysState2 = 5,
    KittyFlags = 6,
    MacosOptionAsAlt = 7,
    BackarrowKeyMode = 8,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyKey {
    Unidentified = 0,

    // Writing System Keys
    Backquote = 1,
    Backslash = 2,
    BracketLeft = 3,
    BracketRight = 4,
    Comma = 5,
    Digit0 = 6,
    Digit1 = 7,
    Digit2 = 8,
    Digit3 = 9,
    Digit4 = 10,
    Digit5 = 11,
    Digit6 = 12,
    Digit7 = 13,
    Digit8 = 14,
    Digit9 = 15,
    Equal = 16,
    IntlBackslash = 17,
    IntlRo = 18,
    IntlYen = 19,
    A = 20,
    B = 21,
    C = 22,
    D = 23,
    E = 24,
    F = 25,
    G = 26,
    H = 27,
    I = 28,
    J = 29,
    K = 30,
    L = 31,
    M = 32,
    N = 33,
    O = 34,
    P = 35,
    Q = 36,
    R = 37,
    S = 38,
    T = 39,
    U = 40,
    V = 41,
    W = 42,
    X = 43,
    Y = 44,
    Z = 45,
    Minus = 46,
    Period = 47,
    Quote = 48,
    Semicolon = 49,
    Slash = 50,

    // Functional Keys
    AltLeft = 51,
    AltRight = 52,
    Backspace = 53,
    CapsLock = 54,
    ContextMenu = 55,
    ControlLeft = 56,
    ControlRight = 57,
    Enter = 58,
    MetaLeft = 59,
    MetaRight = 60,
    ShiftLeft = 61,
    ShiftRight = 62,
    Space = 63,
    Tab = 64,

    // Control Pad
    Delete = 68,
    End = 69,
    Help = 70,
    Home = 71,
    Insert = 72,
    PageDown = 73,
    PageUp = 74,

    // Arrow Pad
    ArrowDown = 75,
    ArrowLeft = 76,
    ArrowRight = 77,
    ArrowUp = 78,

    // Function Section
    Escape = 120,
    F1 = 121,
    F2 = 122,
    F3 = 123,
    F4 = 124,
    F5 = 125,
    F6 = 126,
    F7 = 127,
    F8 = 128,
    F9 = 129,
    F10 = 130,
    F11 = 131,
    F12 = 132,
    F13 = 133,
    F14 = 134,
    F15 = 135,
    F16 = 136,
    F17 = 137,
    F18 = 138,
    F19 = 139,
    F20 = 140,
    F21 = 141,
    F22 = 142,
    F23 = 143,
    F24 = 144,
}

unsafe extern "C" {
    pub fn ghostty_version() -> *const c_char;
    pub fn ghostty_sized_init(ptr: *mut c_void, size: usize);
    pub fn ghostty_type_json() -> *const c_char;

    pub fn ghostty_terminal_new(
        allocator: *const GhosttyAllocator,
        terminal: *mut GhosttyTerminal,
        cols: u16,
        rows: u16,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_free(terminal: GhosttyTerminal);
    pub fn ghostty_terminal_reset(terminal: GhosttyTerminal);
    pub fn ghostty_terminal_resize(
        terminal: GhosttyTerminal,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_set(
        terminal: GhosttyTerminal,
        option: GhosttyTerminalOption,
        value: *const c_void,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_get(
        terminal: GhosttyTerminal,
        data: GhosttyTerminalData,
        out: *mut c_void,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_vt_write(terminal: GhosttyTerminal, data: *const u8, len: usize);
    pub fn ghostty_terminal_scroll_viewport(
        terminal: GhosttyTerminal,
        behavior: GhosttyTerminalScrollViewport,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_grid_ref(
        terminal: GhosttyTerminal,
        point: GhosttyPoint,
        out_ref: *mut GhosttyGridRef,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_point_from_grid_ref(
        terminal: GhosttyTerminal,
        grid_ref: *const GhosttyGridRef,
        tag: GhosttyPointTag,
        out: *mut GhosttyPointCoordinate,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_select_word(
        terminal: GhosttyTerminal,
        options: *const GhosttyTerminalSelectWordOptions,
        out_selection: *mut GhosttySelection,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_select_line(
        terminal: GhosttyTerminal,
        options: *const GhosttyTerminalSelectLineOptions,
        out_selection: *mut GhosttySelection,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_selection_ordered(
        terminal: GhosttyTerminal,
        selection: *const GhosttySelection,
        desired: GhosttySelectionOrder,
        out_selection: *mut GhosttySelection,
    ) -> GhosttyResult;

    pub fn ghostty_grid_ref_cell(
        grid_ref: *const GhosttyGridRef,
        out_cell: *mut GhosttyCell,
    ) -> GhosttyResult;
    pub fn ghostty_grid_ref_row(
        grid_ref: *const GhosttyGridRef,
        out_row: *mut GhosttyRow,
    ) -> GhosttyResult;
    pub fn ghostty_grid_ref_graphemes(
        grid_ref: *const GhosttyGridRef,
        buf: *mut u32,
        buf_len: usize,
        out_written: *mut usize,
    ) -> GhosttyResult;
    pub fn ghostty_grid_ref_style(
        grid_ref: *const GhosttyGridRef,
        out_style: *mut GhosttyStyle,
    ) -> GhosttyResult;

    pub fn ghostty_cell_get(
        cell: GhosttyCell,
        data: GhosttyCellData,
        out: *mut c_void,
    ) -> GhosttyResult;
    pub fn ghostty_row_get(
        row: GhosttyRow,
        data: GhosttyRowData,
        out: *mut c_void,
    ) -> GhosttyResult;

    pub fn ghostty_render_state_new(
        allocator: *const GhosttyAllocator,
        state: *mut GhosttyRenderState,
    ) -> GhosttyResult;
    pub fn ghostty_render_state_free(state: GhosttyRenderState);
    pub fn ghostty_render_state_update(
        state: GhosttyRenderState,
        terminal: GhosttyTerminal,
    ) -> GhosttyResult;
    pub fn ghostty_render_state_clean(state: GhosttyRenderState) -> GhosttyResult;
    pub fn ghostty_render_state_get(
        state: GhosttyRenderState,
        data: GhosttyRenderStateData,
        out: *mut c_void,
    ) -> GhosttyResult;

    pub fn ghostty_render_state_row_iterator_new(
        allocator: *const GhosttyAllocator,
        out_iterator: *mut GhosttyRenderStateRowIterator,
    ) -> GhosttyResult;
    pub fn ghostty_render_state_row_iterator_free(iterator: GhosttyRenderStateRowIterator);
    pub fn ghostty_render_state_row_iterator_next(iterator: GhosttyRenderStateRowIterator) -> bool;
    pub fn ghostty_render_state_row_iterator_next_dirty(
        iterator: GhosttyRenderStateRowIterator,
        out_y: *mut u16,
    ) -> bool;
    pub fn ghostty_render_state_row_get(
        iterator: GhosttyRenderStateRowIterator,
        data: GhosttyRenderStateRowData,
        out: *mut c_void,
    ) -> GhosttyResult;

    pub fn ghostty_render_state_row_cells_new(
        allocator: *const GhosttyAllocator,
        out_cells: *mut GhosttyRenderStateRowCells,
    ) -> GhosttyResult;
    pub fn ghostty_render_state_row_cells_free(cells: GhosttyRenderStateRowCells);
    pub fn ghostty_render_state_row_cells_next(cells: GhosttyRenderStateRowCells) -> bool;
    pub fn ghostty_render_state_row_cells_get(
        cells: GhosttyRenderStateRowCells,
        data: GhosttyRenderStateRowCellsData,
        out: *mut c_void,
    ) -> GhosttyResult;

    pub fn ghostty_key_encoder_new(
        allocator: *const GhosttyAllocator,
        encoder: *mut GhosttyKeyEncoder,
    ) -> GhosttyResult;
    pub fn ghostty_key_encoder_free(encoder: GhosttyKeyEncoder);
    pub fn ghostty_key_encoder_set(
        encoder: GhosttyKeyEncoder,
        option: GhosttyKeyEncoderOption,
        value: *const c_void,
    ) -> GhosttyResult;
    pub fn ghostty_key_encoder_set_from_terminal(
        encoder: GhosttyKeyEncoder,
        terminal: GhosttyTerminal,
    ) -> GhosttyResult;
    pub fn ghostty_key_encoder_encode(
        encoder: GhosttyKeyEncoder,
        event: GhosttyKeyEvent,
        buf: *mut u8,
        buf_len: usize,
        out_written: *mut usize,
    ) -> GhosttyResult;

    pub fn ghostty_key_event_new(
        allocator: *const GhosttyAllocator,
        event: *mut GhosttyKeyEvent,
    ) -> GhosttyResult;
    pub fn ghostty_key_event_free(event: GhosttyKeyEvent);
    pub fn ghostty_key_event_set_action(
        event: GhosttyKeyEvent,
        action: GhosttyKeyAction,
    ) -> GhosttyResult;
    pub fn ghostty_key_event_set_key(event: GhosttyKeyEvent, key: GhosttyKey) -> GhosttyResult;
    pub fn ghostty_key_event_set_mods(event: GhosttyKeyEvent, mods: GhosttyMods) -> GhosttyResult;
    pub fn ghostty_key_event_set_consumed_mods(
        event: GhosttyKeyEvent,
        consumed_mods: GhosttyMods,
    ) -> GhosttyResult;
    pub fn ghostty_key_event_set_composing(
        event: GhosttyKeyEvent,
        composing: bool,
    ) -> GhosttyResult;
    pub fn ghostty_key_event_set_utf8(event: GhosttyKeyEvent, utf8: GhosttyString)
    -> GhosttyResult;
    pub fn ghostty_key_event_set_unshifted_codepoint(
        event: GhosttyKeyEvent,
        codepoint: u32,
    ) -> GhosttyResult;

    pub fn ghostty_focus_encode(
        event: GhosttyFocusEvent,
        buf: *mut u8,
        buf_len: usize,
        out_written: *mut usize,
    ) -> GhosttyResult;

    pub fn ghostty_paste_is_safe(data: *const u8, len: usize) -> bool;
    pub fn ghostty_paste_encode(
        data: *mut u8,
        data_len: usize,
        bracketed: bool,
        buf: *mut u8,
        buf_len: usize,
        out_written: *mut usize,
    ) -> GhosttyResult;

    pub fn ghostty_mouse_encoder_new(
        allocator: *const GhosttyAllocator,
        encoder: *mut GhosttyMouseEncoder,
    ) -> GhosttyResult;
    pub fn ghostty_mouse_encoder_free(encoder: GhosttyMouseEncoder);
    pub fn ghostty_mouse_encoder_set(
        encoder: GhosttyMouseEncoder,
        option: GhosttyMouseEncoderOption,
        value: *const c_void,
    ) -> GhosttyResult;
    pub fn ghostty_mouse_encoder_set_from_terminal(
        encoder: GhosttyMouseEncoder,
        terminal: GhosttyTerminal,
    ) -> GhosttyResult;
    pub fn ghostty_mouse_encoder_reset(encoder: GhosttyMouseEncoder) -> GhosttyResult;
    pub fn ghostty_mouse_encoder_encode(
        encoder: GhosttyMouseEncoder,
        event: GhosttyMouseEvent,
        buf: *mut u8,
        buf_len: usize,
        out_written: *mut usize,
    ) -> GhosttyResult;

    pub fn ghostty_mouse_event_new(
        allocator: *const GhosttyAllocator,
        event: *mut GhosttyMouseEvent,
    ) -> GhosttyResult;
    pub fn ghostty_mouse_event_free(event: GhosttyMouseEvent);
    pub fn ghostty_mouse_event_set_action(
        event: GhosttyMouseEvent,
        action: GhosttyMouseAction,
    ) -> GhosttyResult;
    pub fn ghostty_mouse_event_set_button(
        event: GhosttyMouseEvent,
        button: GhosttyMouseButton,
    ) -> GhosttyResult;
    pub fn ghostty_mouse_event_clear_button(event: GhosttyMouseEvent) -> GhosttyResult;
    pub fn ghostty_mouse_event_set_mods(
        event: GhosttyMouseEvent,
        mods: GhosttyMods,
    ) -> GhosttyResult;
    pub fn ghostty_mouse_event_set_position(
        event: GhosttyMouseEvent,
        position: GhosttyMousePosition,
    ) -> GhosttyResult;

    pub fn ghostty_formatter_terminal_new(
        allocator: *const GhosttyAllocator,
        formatter: *mut GhosttyFormatter,
        terminal: GhosttyTerminal,
    ) -> GhosttyResult;
    pub fn ghostty_formatter_set(
        formatter: GhosttyFormatter,
        option: GhosttyFormatterOption,
        value: *const c_void,
    ) -> GhosttyResult;
    pub fn ghostty_formatter_format_buf(
        formatter: GhosttyFormatter,
        buf: *mut u8,
        buf_len: usize,
        out_written: *mut usize,
    ) -> GhosttyResult;
    pub fn ghostty_formatter_free(formatter: GhosttyFormatter);
}
