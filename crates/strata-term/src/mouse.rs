//! UI-independent mouse input types and dispatch semantics.

use crate::keys::TermModifiers;
use crate::sys;

/// Mouse buttons supported by terminal mouse reporting protocols.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum TermMouseButton {
    Left,
    Middle,
    Right,
    /// Side "back" button (encoded as button 8).
    Back,
    /// Side "forward" button (encoded as button 9).
    Forward,
}

impl TermMouseButton {
    pub(crate) fn to_ghostty_button(self) -> sys::GhosttyMouseButton {
        match self {
            Self::Left => sys::GhosttyMouseButton::Left,
            Self::Middle => sys::GhosttyMouseButton::Middle,
            Self::Right => sys::GhosttyMouseButton::Right,
            Self::Back => sys::GhosttyMouseButton::Eight,
            Self::Forward => sys::GhosttyMouseButton::Nine,
        }
    }

    pub(crate) fn mask_bit(self) -> u8 {
        match self {
            Self::Left => 1 << 0,
            Self::Middle => 1 << 1,
            Self::Right => 1 << 2,
            Self::Back => 1 << 3,
            Self::Forward => 1 << 4,
        }
    }
}

/// Pointer location relative to the terminal viewport.
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum MousePosition {
    /// 0-indexed viewport grid coordinates (`(0, 0)` is the top-left cell).
    Cell { col: u16, row: u16 },
    /// Surface-space pixels relative to the top-left of the terminal grid.
    Pixels { x: f32, y: f32 },
}

impl MousePosition {
    pub(crate) fn to_pixels(self, cell_width_px: u32, cell_height_px: u32) -> (f32, f32) {
        match self {
            Self::Cell { col, row } => {
                let cw = cell_width_px.max(1) as f32;
                let ch = cell_height_px.max(1) as f32;
                (
                    f32::from(col) * cw + cw * 0.5,
                    f32::from(row) * ch + ch * 0.5,
                )
            }
            Self::Pixels { x, y } => (x, y),
        }
    }
}

/// Kind of mouse interaction routed through [`crate::TerminalSession::handle_mouse`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum TermMouseKind {
    Press(TermMouseButton),
    Release(TermMouseButton),
    /// Pointer movement. Provide `button: Some(...)` while dragging a pressed button.
    Move {
        button: Option<TermMouseButton>,
    },
    /// Discrete mouse wheel scroll in lines/columns.
    ///
    /// Positive `delta_y` scrolls up (toward older scrollback history); negative
    /// `delta_y` scrolls down (toward newer rows). Positive `delta_x` scrolls
    /// right; negative `delta_x` scrolls left.
    Wheel {
        delta_x: i32,
        delta_y: i32,
    },
}

/// Normalized mouse event submitted to the terminal session.
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct TermMouseEvent {
    pub kind: TermMouseKind,
    pub position: MousePosition,
    pub modifiers: TermModifiers,
}

impl TermMouseEvent {
    pub fn press(
        button: TermMouseButton,
        position: MousePosition,
        modifiers: TermModifiers,
    ) -> Self {
        Self {
            kind: TermMouseKind::Press(button),
            position,
            modifiers,
        }
    }

    pub fn release(
        button: TermMouseButton,
        position: MousePosition,
        modifiers: TermModifiers,
    ) -> Self {
        Self {
            kind: TermMouseKind::Release(button),
            position,
            modifiers,
        }
    }

    pub fn motion(
        button: Option<TermMouseButton>,
        position: MousePosition,
        modifiers: TermModifiers,
    ) -> Self {
        Self {
            kind: TermMouseKind::Move { button },
            position,
            modifiers,
        }
    }

    pub fn wheel(
        delta_x: i32,
        delta_y: i32,
        position: MousePosition,
        modifiers: TermModifiers,
    ) -> Self {
        Self {
            kind: TermMouseKind::Wheel { delta_x, delta_y },
            position,
            modifiers,
        }
    }
}

/// Result of handling a [`TermMouseEvent`] against current terminal state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MouseOutcome {
    /// Mouse or alternate-scroll escape sequences generated and written to the PTY.
    Reported(Vec<u8>),
    /// Local scrollback viewport was scrolled because mouse tracking was off.
    ScrolledViewport,
    /// Event produced neither a PTY report nor a viewport scroll.
    Ignored,
}
