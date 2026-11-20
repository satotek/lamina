//! UI-independent keyboard input types and libghostty-vt key encoding.

use crate::sys;

/// Key event action.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum TermKeyAction {
    Release,
    #[default]
    Press,
    Repeat,
}

impl From<TermKeyAction> for sys::GhosttyKeyAction {
    fn from(action: TermKeyAction) -> Self {
        match action {
            TermKeyAction::Release => Self::Release,
            TermKeyAction::Press => Self::Press,
            TermKeyAction::Repeat => Self::Repeat,
        }
    }
}

/// Modifier flags carried by a key event.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct TermModifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub super_key: bool,
    pub caps_lock: bool,
    pub num_lock: bool,
}

impl TermModifiers {
    pub(crate) fn to_ghostty_mods(self) -> sys::GhosttyMods {
        let mut mods = 0u16;
        if self.shift {
            mods |= sys::GHOSTTY_MODS_SHIFT;
        }
        if self.ctrl {
            mods |= sys::GHOSTTY_MODS_CTRL;
        }
        if self.alt {
            mods |= sys::GHOSTTY_MODS_ALT;
        }
        if self.super_key {
            mods |= sys::GHOSTTY_MODS_SUPER;
        }
        if self.caps_lock {
            mods |= sys::GHOSTTY_MODS_CAPS_LOCK;
        }
        if self.num_lock {
            mods |= sys::GHOSTTY_MODS_NUM_LOCK;
        }
        mods
    }
}

/// Physical key codes understood by libghostty-vt's key encoder (W3C UI Events).
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum TermKey {
    #[default]
    Unidentified,
    Backquote,
    Backslash,
    BracketLeft,
    BracketRight,
    Comma,
    Digit0,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,
    Equal,
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    W,
    X,
    Y,
    Z,
    Minus,
    Period,
    Quote,
    Semicolon,
    Slash,
    Backspace,
    Enter,
    Space,
    Tab,
    Delete,
    End,
    Home,
    Insert,
    PageDown,
    PageUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    Escape,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
}

impl TermKey {
    /// Map a W3C key name or common GPUI key identifier (case-insensitive) to [`TermKey`].
    pub fn from_name(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "a" | "keya" => Self::A,
            "b" | "keyb" => Self::B,
            "c" | "keyc" => Self::C,
            "d" | "keyd" => Self::D,
            "e" | "keye" => Self::E,
            "f" | "keyf" => Self::F,
            "g" | "keyg" => Self::G,
            "h" | "keyh" => Self::H,
            "i" | "keyi" => Self::I,
            "j" | "keyj" => Self::J,
            "k" | "keyk" => Self::K,
            "l" | "keyl" => Self::L,
            "m" | "keym" => Self::M,
            "n" | "keyn" => Self::N,
            "o" | "keyo" => Self::O,
            "p" | "keyp" => Self::P,
            "q" | "keyq" => Self::Q,
            "r" | "keyr" => Self::R,
            "s" | "keys" => Self::S,
            "t" | "keyt" => Self::T,
            "u" | "keyu" => Self::U,
            "v" | "keyv" => Self::V,
            "w" | "keyw" => Self::W,
            "x" | "keyx" => Self::X,
            "y" | "keyy" => Self::Y,
            "z" | "keyz" => Self::Z,
            "0" | "digit0" | ")" => Self::Digit0,
            "1" | "digit1" | "!" => Self::Digit1,
            "2" | "digit2" | "@" => Self::Digit2,
            "3" | "digit3" | "#" => Self::Digit3,
            "4" | "digit4" | "$" => Self::Digit4,
            "5" | "digit5" | "%" => Self::Digit5,
            "6" | "digit6" | "^" => Self::Digit6,
            "7" | "digit7" | "&" => Self::Digit7,
            "8" | "digit8" | "*" => Self::Digit8,
            "9" | "digit9" | "(" => Self::Digit9,
            "`" | "~" | "backquote" => Self::Backquote,
            "\\" | "|" | "backslash" => Self::Backslash,
            "[" | "{" | "bracketleft" => Self::BracketLeft,
            "]" | "}" | "bracketright" => Self::BracketRight,
            "," | "<" | "comma" => Self::Comma,
            "=" | "+" | "equal" => Self::Equal,
            "-" | "_" | "minus" => Self::Minus,
            "." | ">" | "period" => Self::Period,
            "'" | "\"" | "quote" => Self::Quote,
            ";" | ":" | "semicolon" => Self::Semicolon,
            "/" | "?" | "slash" => Self::Slash,
            "backspace" => Self::Backspace,
            "enter" | "return" => Self::Enter,
            "space" | " " => Self::Space,
            "tab" => Self::Tab,
            "delete" => Self::Delete,
            "end" => Self::End,
            "home" => Self::Home,
            "insert" => Self::Insert,
            "pagedown" => Self::PageDown,
            "pageup" => Self::PageUp,
            "down" | "arrowdown" => Self::ArrowDown,
            "left" | "arrowleft" => Self::ArrowLeft,
            "right" | "arrowright" => Self::ArrowRight,
            "up" | "arrowup" => Self::ArrowUp,
            "escape" | "esc" => Self::Escape,
            "f1" => Self::F1,
            "f2" => Self::F2,
            "f3" => Self::F3,
            "f4" => Self::F4,
            "f5" => Self::F5,
            "f6" => Self::F6,
            "f7" => Self::F7,
            "f8" => Self::F8,
            "f9" => Self::F9,
            "f10" => Self::F10,
            "f11" => Self::F11,
            "f12" => Self::F12,
            _ => Self::Unidentified,
        }
    }

    pub(crate) fn to_ghostty_key(self) -> sys::GhosttyKey {
        match self {
            Self::Unidentified => sys::GhosttyKey::Unidentified,
            Self::Backquote => sys::GhosttyKey::Backquote,
            Self::Backslash => sys::GhosttyKey::Backslash,
            Self::BracketLeft => sys::GhosttyKey::BracketLeft,
            Self::BracketRight => sys::GhosttyKey::BracketRight,
            Self::Comma => sys::GhosttyKey::Comma,
            Self::Digit0 => sys::GhosttyKey::Digit0,
            Self::Digit1 => sys::GhosttyKey::Digit1,
            Self::Digit2 => sys::GhosttyKey::Digit2,
            Self::Digit3 => sys::GhosttyKey::Digit3,
            Self::Digit4 => sys::GhosttyKey::Digit4,
            Self::Digit5 => sys::GhosttyKey::Digit5,
            Self::Digit6 => sys::GhosttyKey::Digit6,
            Self::Digit7 => sys::GhosttyKey::Digit7,
            Self::Digit8 => sys::GhosttyKey::Digit8,
            Self::Digit9 => sys::GhosttyKey::Digit9,
            Self::Equal => sys::GhosttyKey::Equal,
            Self::A => sys::GhosttyKey::A,
            Self::B => sys::GhosttyKey::B,
            Self::C => sys::GhosttyKey::C,
            Self::D => sys::GhosttyKey::D,
            Self::E => sys::GhosttyKey::E,
            Self::F => sys::GhosttyKey::F,
            Self::G => sys::GhosttyKey::G,
            Self::H => sys::GhosttyKey::H,
            Self::I => sys::GhosttyKey::I,
            Self::J => sys::GhosttyKey::J,
            Self::K => sys::GhosttyKey::K,
            Self::L => sys::GhosttyKey::L,
            Self::M => sys::GhosttyKey::M,
            Self::N => sys::GhosttyKey::N,
            Self::O => sys::GhosttyKey::O,
            Self::P => sys::GhosttyKey::P,
            Self::Q => sys::GhosttyKey::Q,
            Self::R => sys::GhosttyKey::R,
            Self::S => sys::GhosttyKey::S,
            Self::T => sys::GhosttyKey::T,
            Self::U => sys::GhosttyKey::U,
            Self::V => sys::GhosttyKey::V,
            Self::W => sys::GhosttyKey::W,
            Self::X => sys::GhosttyKey::X,
            Self::Y => sys::GhosttyKey::Y,
            Self::Z => sys::GhosttyKey::Z,
            Self::Minus => sys::GhosttyKey::Minus,
            Self::Period => sys::GhosttyKey::Period,
            Self::Quote => sys::GhosttyKey::Quote,
            Self::Semicolon => sys::GhosttyKey::Semicolon,
            Self::Slash => sys::GhosttyKey::Slash,
            Self::Backspace => sys::GhosttyKey::Backspace,
            Self::Enter => sys::GhosttyKey::Enter,
            Self::Space => sys::GhosttyKey::Space,
            Self::Tab => sys::GhosttyKey::Tab,
            Self::Delete => sys::GhosttyKey::Delete,
            Self::End => sys::GhosttyKey::End,
            Self::Home => sys::GhosttyKey::Home,
            Self::Insert => sys::GhosttyKey::Insert,
            Self::PageDown => sys::GhosttyKey::PageDown,
            Self::PageUp => sys::GhosttyKey::PageUp,
            Self::ArrowDown => sys::GhosttyKey::ArrowDown,
            Self::ArrowLeft => sys::GhosttyKey::ArrowLeft,
            Self::ArrowRight => sys::GhosttyKey::ArrowRight,
            Self::ArrowUp => sys::GhosttyKey::ArrowUp,
            Self::Escape => sys::GhosttyKey::Escape,
            Self::F1 => sys::GhosttyKey::F1,
            Self::F2 => sys::GhosttyKey::F2,
            Self::F3 => sys::GhosttyKey::F3,
            Self::F4 => sys::GhosttyKey::F4,
            Self::F5 => sys::GhosttyKey::F5,
            Self::F6 => sys::GhosttyKey::F6,
            Self::F7 => sys::GhosttyKey::F7,
            Self::F8 => sys::GhosttyKey::F8,
            Self::F9 => sys::GhosttyKey::F9,
            Self::F10 => sys::GhosttyKey::F10,
            Self::F11 => sys::GhosttyKey::F11,
            Self::F12 => sys::GhosttyKey::F12,
        }
    }

    /// Best-effort unshifted US ASCII codepoint for Kitty keyboard protocol reporting.
    pub fn unshifted_ascii(self) -> Option<char> {
        match self {
            Self::Backquote => Some('`'),
            Self::Backslash => Some('\\'),
            Self::BracketLeft => Some('['),
            Self::BracketRight => Some(']'),
            Self::Comma => Some(','),
            Self::Digit0 => Some('0'),
            Self::Digit1 => Some('1'),
            Self::Digit2 => Some('2'),
            Self::Digit3 => Some('3'),
            Self::Digit4 => Some('4'),
            Self::Digit5 => Some('5'),
            Self::Digit6 => Some('6'),
            Self::Digit7 => Some('7'),
            Self::Digit8 => Some('8'),
            Self::Digit9 => Some('9'),
            Self::Equal => Some('='),
            Self::A => Some('a'),
            Self::B => Some('b'),
            Self::C => Some('c'),
            Self::D => Some('d'),
            Self::E => Some('e'),
            Self::F => Some('f'),
            Self::G => Some('g'),
            Self::H => Some('h'),
            Self::I => Some('i'),
            Self::J => Some('j'),
            Self::K => Some('k'),
            Self::L => Some('l'),
            Self::M => Some('m'),
            Self::N => Some('n'),
            Self::O => Some('o'),
            Self::P => Some('p'),
            Self::Q => Some('q'),
            Self::R => Some('r'),
            Self::S => Some('s'),
            Self::T => Some('t'),
            Self::U => Some('u'),
            Self::V => Some('v'),
            Self::W => Some('w'),
            Self::X => Some('x'),
            Self::Y => Some('y'),
            Self::Z => Some('z'),
            Self::Minus => Some('-'),
            Self::Period => Some('.'),
            Self::Quote => Some('\''),
            Self::Semicolon => Some(';'),
            Self::Slash => Some('/'),
            Self::Space => Some(' '),
            _ => None,
        }
    }
}

/// UI-independent key event submitted to the terminal key encoder.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TermKeyEvent {
    pub action: TermKeyAction,
    pub key: TermKey,
    pub modifiers: TermModifiers,
    pub consumed_modifiers: TermModifiers,
    pub text: Option<String>,
    pub composing: bool,
}

impl TermKeyEvent {
    /// Filter out C0 control characters and platform PUA function-key chars as
    /// required by `ghostty_key_event_set_utf8`.
    pub(crate) fn sanitized_utf8(&self) -> Option<&str> {
        let text = self.text.as_deref()?;
        if text.is_empty() {
            return None;
        }
        let all_valid = text.chars().all(|ch| {
            let code = ch as u32;
            code >= 0x20 && code != 0x7f && !(0xF700..=0xF8FF).contains(&code)
        });
        if all_valid { Some(text) } else { None }
    }
}
