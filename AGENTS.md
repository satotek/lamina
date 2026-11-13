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
- Run cargo through the Nix dev shell: `direnv allow && direnv exec . cargo …`. It provides
  the native libraries GPUI needs on Linux and points every worktree at one shared
  target directory, so binaries are not under `./target` (use `cargo run`).
- Build only what you touch. A task that edits only `strata-core` runs
  `cargo clippy -p strata-core --all-targets -- -D warnings` and `cargo test -p strata-core`;
  it never builds `strata-app` (GPUI is several GB). App tasks use `--workspace`.
- Before finishing: `cargo fmt` plus the clippy and test commands above.

## Parallel work

Work is split across agents in separate worktrees and integrated by cherry-pick
into a linear history. If you are one of those agents: edit only the files your
brief names, report (do not make) changes to shared types, and deliver one
commit. See `docs/parallel-development.md` for the full process.
