//! UI-independent core of Strata.
//!
//! Reads go through `gix` (`repo`), writes go through the `git` CLI (`git_cli`),
//! so user hooks, signing and credential helpers behave exactly as in a terminal.

pub mod askpass;
pub mod diff;
pub mod encoding;
#[cfg(unix)]
pub mod env_path;
pub mod error;
pub mod forge;
pub mod fs_path;
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
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use error::{Error, Result};
pub use types::*;
