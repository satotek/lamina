//! Shared parts every pane builds its controls from.
//!
//! The panes used to style raw `div()`s: three chip/primary button helpers, a
//! hand-drawn confirmation overlay, per-module list rows and message panes. The
//! parts here describe the app's few control shapes once, on top of
//! [`gpui_component`] and the [`Design`](crate::style::Design) tokens, so a
//! feature module says *what* a control is and not *how* it is painted.
//! Context menus are one of those shapes: [`context_menu`] attaches the same
//! menu trigger everywhere, so a menu does not inherit the row's text style.
//!
//! Nothing here reads a repository or a session: a part takes the data it draws
//! and the palette, and nothing else.

mod badge;
mod banner;
mod button;
mod checkbox;
mod confirm;
mod focus;
pub(crate) mod gallery;
pub mod lfs;
mod menu;
mod rail;
mod row;
mod signature;
mod split;
mod tab;
#[cfg(feature = "terminal")]
mod terminal_grid;
mod text;

mod askpass;

pub use askpass::askpass_prompt;
pub use badge::{
    BadgeTone, avatar, change_badge, check_dot, count_pill, kbd, ref_badge, status_badge,
};
pub use banner::{BannerKind, banner};
pub use button::{ButtonKind, ButtonSize, UiButton, button, content_button, icon_button};
pub use checkbox::checkbox;
pub use confirm::{Confirm, confirm, confirm_with};
pub(crate) use focus::focus_input;
pub use lfs::{install_hint, lfs_badge, lfs_card};
pub use menu::{context_menu, menu_label};
pub use rail::{RAIL_WIDTH, rail, rail_item};
pub use row::{RowState, RowTipExt, list_row, row_surface};
pub use signature::{CommitSignatureState, signature_badge, signature_details};
pub use split::{SplitExtent, clamp_fraction, split_handle, split_pixels};
pub use tab::{tab, tab_strip};
#[cfg(feature = "terminal")]
pub use terminal_grid::{
    TerminalGrid, TerminalGridProps, TerminalScrollbarHandle, grid_dimensions_for_bounds,
    ime_cursor_bounds, measure_cell_size, pixel_to_viewport_cell, terminal_theme,
    terminal_viewport,
};
pub use text::{empty_state, message, section_header};
