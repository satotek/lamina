# Parallel development with agents

Strata is built by several coding agents working in parallel, each in its own
git worktree, with one integrator (a person or a lead agent) combining their
work. This document is the process both sides follow.

## Splitting work

- **Contracts first.** Shared types (`src/types.rs`, `src/error.rs`, the type
  definitions at the top of `src/graph/mod.rs` and `src/diff/mod.rs`) and module
  stubs are committed before any task starts. Every task branches from that
  commit, so nobody invents their own version of a shared type.
- **Disjoint ownership.** Each task brief names the files it may edit. Two tasks
  never own the same file.
- **Contract changes go through the integrator.** A task that needs a shared type
  to change reports it instead of editing it. The integrator changes the type on
  the integration branch and restarts or rebases the affected tasks.
- **Task size.** A task should finish in one session. Long-lived branches drift
  from the integration branch and become expensive to land.

## What a task delivers

- One branch, normally one commit, with a Conventional Commit message.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass
  for the code it owns.
- Tests check behaviour against an external reference where one exists (for
  example `git log --date-order`, `git status --porcelain=v2`, `git apply`), not
  only against the task's own expectations.
- A final report: branch, commit SHA, summary, known limitations.

## Integrating

Work lands on an integration branch (for the MVP: `feat/mvp-phase0`), which
reaches `main` through a single pull request.

1. Land tasks one at a time, leaves of the dependency graph first (core
   modules before the app).
2. **Keep history linear.** Cherry-pick (or rebase) the task's commit onto the
   integration branch. No merge commits. Squash only when a task left several
   work-in-progress commits.
3. After every pick, run fmt, clippy and the full test suite on the integration
   branch. Branches that pass on their own can still conflict in meaning once
   combined.
4. Never hand-merge `Cargo.lock`. Take one side and let `cargo` regenerate it.
5. Cherry-picked commits get new SHAs; refer to commits by message, not by the
   SHA a task reported.

Linear history keeps one commit per module, which makes `git bisect` and
per-module `git revert` straightforward. Because tasks own disjoint files,
reverting one module rarely breaks another.

## Shared resources

- Each worktree has its own `target/`, so N parallel Rust tasks compile the
  dependency tree N times. Use `sccache` when running many tasks at once.
- Tasks that start servers or databases must not share ports or data
  directories.
- Remove a task's worktree and branch once it has landed or been abandoned.
