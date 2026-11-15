# strata-app

GPUI desktop front end for Strata (binary: `strata`).

It opens repositories through `strata_core::session::Session` on background threads and renders the latest immutable `RepoSnapshot` without running Git work on the UI thread. Embedded terminal support is enabled by default via the `terminal` Cargo feature (`strata-term`); pass `--no-default-features` to build without it.

See [docs/building.md](../../docs/building.md) for build instructions and [docs/design/app.md](../../docs/design/app.md) for the application design notes (in Japanese).

## Usage

```sh
strata [PATH]               # open the repository containing PATH (or restore/welcome screen)
strata --fake [N]           # render N synthetic commits (default 200000) for benchmarking
strata --gallery            # open the shared UI component gallery without a repository
strata --askpass <prompt>   # internal git askpass helper mode (no display or window)
```

If `PATH` is not inside a Git repository, the window displays an error screen instead of exiting.

## Debug Launch Flags

For automated UI checks, `strata` accepts debug launch flags alongside `[PATH]`, `--fake`, or `--gallery` and applies them once the first snapshot loads:

- `--select <rev>` — select and scroll to a commit (id, ref, or comma-separated list)
- `--file-history <path>` — open file history for `<path>`
- `--tree <rev>[:<dir>]` — open the commit tree browser at `<rev>` (or `<dir>`)
- `--tree-file <rev>:<path>` — open `<path>` in the tree browser at `<rev>`
- `--blame <rev>:<path>` — open the blame view for `<path>` at `<rev>`
- `--worktree` — start in the working-tree view
- `--dialog <name>` — open a dialog (`create-branch`, `stash`, `settings`, `palette`, `open-repository`, `add-worktree`, `create-tag`)
- `--window-size <W>x<H>` — request an initial window size

Run `strata --help` for the full list.

## Frame Stats & Headless Smoke Test

- **`F12`** toggles a frame-time overlay (mean frame interval, p95, FPS, element build time, display refresh target).
- **`STRATA_SMOKE=1`** starts with the overlay enabled, scrolls the commit list every frame for 300 frames, prints frame statistics to `stderr`, and exits. Under headless Linux, `Xvfb` provides an X11 display:

```sh
Xvfb :99 -screen 0 1600x1000x24 &
DISPLAY=:99 RUST_LOG=info STRATA_SMOKE=1 direnv exec . cargo run -p strata-app --release
```

## Dependencies: `gpui-pre`

`gpui` is published on crates.io under `gpui-pre` / `gpui-pre-platform`, so [Cargo.toml](Cargo.toml) renames `gpui-pre` to `gpui` to match `gpui-component` (`0.9.1`). Two constraints to preserve:

- `gpui-platform` (`gpui-pre-platform`) must keep the `font-kit`, `wayland`, and `x11` features enabled; otherwise macOS loses text rendering or Linux opens in headless mode without creating a window.
- `gpui-pre` is a pinned pre-release snapshot (`=0.3.12`), so keep `Cargo.lock` committed.
