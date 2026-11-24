//! Integration tests for `lamina-core`.
//!
//! All files live in one test binary, one module per former file. Building a
//! binary per file dominated CI wall time (link time, not test time), so the
//! split is by filter instead: `cargo test -p lamina-core --test it queries::`.
//!
//! Modules are independent: helpers are module-private, and no test relies on
//! process-global state (environment variables, working directory, signal
//! handlers) that another module would observe.

mod commit_diff_bench;
mod commit_graph;
mod commit_stat;
mod conflicts;
mod content_search;
mod discard_staged;
mod follow;
mod git_cli;
mod history_ops;
mod in_progress;
mod messages;
mod patch_apply;
mod pull;
mod queries;
mod repo_log;
mod repo_status;
mod session;
mod signature_bench;
mod signatures;
mod staging_pipeline;
mod stash_paths;
mod submodules_worktrees;
mod tree;
mod upstream;
mod watch;
