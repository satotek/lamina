# Strata

Lightweight cross-platform Git client (macOS and Windows primary, Linux best effort).

## Workspace layout

- `crates/strata-core` — UI-independent Git core (`repo` reads, `git_cli` writes, `session` orchestration, diff, graph, highlight, merge, patch, encoding, watch). Must never depend on GPUI or any UI crate.
- `crates/strata-term` — UI-independent embedded terminal core built on `libghostty-vt` and `portable-pty`. Must stay UI-independent in `[dependencies]` (GPUI is only in `[dev-dependencies]` for `examples/demo.rs`).
- `crates/strata-app` — GPUI desktop front end (`strata` binary). Embedded terminal support is gated behind the default `terminal` feature (`--no-default-features` builds without `strata-term` or Zig).

## Rules

- **Reads vs. writes:** Reads use `gix` (`crates/strata-core/src/repo`). Writes run the `git` CLI (`crates/strata-core/src/git_cli`) so user hooks, signing, credential helpers, and LFS behave identically to `git` in a terminal.
- **Shared types:** Public shared types live in `crates/strata-core/src/types.rs`, `crates/strata-core/src/error.rs`, and the type definitions at the top of `crates/strata-core/src/graph/mod.rs` and `crates/strata-core/src/diff/mod.rs`. Do not change their public shape without a strong reason; add fields only if unavoidable.
- **Errors:** Do not add variants to `Error` (`crates/strata-core/src/error.rs`); use `Error::gix(..)`, `Error::other(..)`, or `Error::GitCommand`.
- **Test fixtures:** Tests that need a repository create one in a `tempfile::TempDir` via `TestRepo` (`crates/strata-core/src/test_support.rs`) by running `git`. Isolate git config (`GIT_CONFIG_GLOBAL` pointing at a temporary file, `GIT_CONFIG_NOSYSTEM=1`) and set author/committer env vars so tests never depend on the host machine's git config.
- **Comments and language:** Code comments explain why, not what. Use English for code, comments, and commit messages. Never name other git clients, terminal applications, or competitor products.
- **Shared UI parts:** In `crates/strata-app`, buttons, dialogs, list rows, badges, banners, checkboxes, tabs, split handles, and similar primitives come from `crate::ui`; add a part there instead of styling ad-hoc `div()`s in feature modules.
- **i18n:** Every user-visible string in `crates/strata-app` must go through `t!("...")` (`rust-i18n`) with a literal key and exist with matching `%{...}` placeholders in both `crates/strata-app/locales/en.yml` and `crates/strata-app/locales/ja.yml` (verified by tests in `crates/strata-app/src/i18n.rs`).

## Build and test

- When using Nix (Linux and optionally macOS), run cargo through the dev shell (`direnv allow && direnv exec . cargo …`, or `nix develop -c cargo …`). It provides the native libraries GPUI needs on Linux, Zig `0.17.1` for `libghostty-vt`, and points `CARGO_TARGET_DIR` at a shared directory (`${XDG_CACHE_HOME:-$HOME/.cache}/strata/target`), so binaries are not under `./target` (use `cargo run`).
- **Build only what you touch:**
  - `strata-core` only: `cargo clippy -p strata-core --all-targets -- -D warnings` and `cargo test -p strata-core` (avoid building `strata-app` needlessly; GPUI is large).
  - `strata-term` only: `cargo clippy -p strata-term --all-targets -- -D warnings` and `cargo test -p strata-term`.
  - App or cross-crate changes: `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` (also check `--no-default-features` on `strata-app` when touching `terminal` feature gating).
- Before finishing, run `cargo fmt --all` along with the clippy and test commands above.
