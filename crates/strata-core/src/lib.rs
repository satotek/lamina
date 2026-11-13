//! UI-independent core of Strata.
//!
//! Reads go through `gix` (`repo`), writes go through the `git` CLI (`git_cli`),
//! so user hooks, signing and credential helpers behave exactly as in a terminal.

pub mod diff;
pub mod error;
pub mod git_cli;
pub mod graph;
pub mod highlight;
pub mod patch;
pub mod repo;
pub mod session;
pub mod types;
pub mod watch;

pub use error::{Error, Result};
pub use types::*;
