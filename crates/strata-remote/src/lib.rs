//! Headless repository helper used by the app over a byte stream.
//!
//! The protocol lives in `strata-remote-protocol`; this crate opens a local
//! session and forwards snapshots, diffs, writes and ticketed queries.

mod server;

pub use server::{BoxError, build_id, parse_root, serve};
