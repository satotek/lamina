//! Shared parts every pane builds its controls from.
//!
//! The panes used to style raw `div()`s: three chip/primary button helpers, a
//! hand-drawn confirmation overlay, per-module list rows and message panes. The
//! parts here describe the app's few control shapes once, on top of
//! [`gpui_component`] and the [`Design`](crate::style::Design) tokens, so a
//! feature module says *what* a control is and not *how* it is painted.
//!
//! Nothing here reads a repository or a session: a part takes the data it draws
//! and the palette, and nothing else.

mod badge;
mod banner;
mod button;
mod confirm;
mod focus;
pub(crate) mod gallery;
mod row;
mod text;

pub use badge::{avatar, change_badge, count_pill, kbd, ref_badge};
pub use banner::{BannerKind, banner};
pub use button::{ButtonKind, ButtonSize, UiButton, button, content_button, icon_button};
pub use confirm::{Confirm, confirm};
pub(crate) use focus::focus_input;
pub use row::{RowState, list_row, row_surface};
pub use text::{empty_state, message, section_header};
