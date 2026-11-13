# Strata

Lightweight cross-platform git client (macOS / Windows required, Linux best effort).
Rust workspace: `crates/strata-core` (UI-independent) and `crates/strata-app` (GPUI).

## Rules

- Reads use `gix` (`repo` module). Writes run the `git` CLI (`git_cli` module).
- `strata-core` must never depend on GPUI or any UI crate.
- Shared types live in `src/types.rs`, `src/error.rs`, and the type definitions at the
  top of `src/graph/mod.rs` and `src/diff/mod.rs`. Do not change their public shape
  without a strong reason; add fields only if unavoidable and say so in the commit.
- Do not add variants to `Error`; use `Error::gix(..)`, `Error::other(..)`, or `GitCommand`.
- Tests that need a repository create one in a `tempfile::TempDir` by running `git`.
  Set `GIT_CONFIG_GLOBAL=/dev/null`-equivalent isolation and author/committer env vars
  so tests do not depend on the machine's git config.
- Code comments explain why, not what. English for code, comments, commits.
- Before finishing: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## Parallel work

Work is split across agents in separate worktrees and integrated by cherry-pick
into a linear history. If you are one of those agents: edit only the files your
brief names, report (do not make) changes to shared types, and deliver one
commit. See `docs/parallel-development.md` for the full process.
