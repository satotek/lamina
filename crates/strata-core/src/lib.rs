//! UI-independent core of Strata.
//!
//! Reads go through `gix` (`repo`), writes go through the `git` CLI (`git_cli`),
//! so user hooks, signing and credential helpers behave exactly as in a terminal.

pub mod askpass;
pub mod diff;
pub mod encoding;
pub mod error;
pub mod forge;
pub mod git_cli;
pub mod graph;
pub mod highlight;
pub mod image;
pub mod lfs;
pub mod merge;
pub mod patch;
pub mod repo;
pub mod session;
pub mod text;
pub mod tools;
pub mod types;
pub mod watch;

#[cfg(test)]
mod i18n_tests;
#[cfg(test)]
mod test_support;

pub use error::{Error, Result};
pub use types::*;
