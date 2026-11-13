# strata-app

GPUI front end for Strata. It opens a real repository through
`strata_core::session::Session` and renders the commit graph, refs, worktree
status and diffs. A synthetic mode (`--fake`) keeps the phase-0 render
benchmark: 200,000 generated commits, for judging whether GPUI can draw a huge
graph and a highlighted diff smoothly on macOS, Windows and Linux.

## Usage

```sh
strata [PATH]       # repository containing PATH; without it, the last
                    # repository opened, or the welcome screen
strata --fake [N]   # N synthetic commits (default 200000)
```

If the path is not a repository the window shows the error instead of exiting.

## What the window shows

- A virtualised `uniform_list` over the loaded commits. Each row has the graph
  column, ref badges (local branch, remote branch, tag, with HEAD's branch
  emphasised), summary, author, date and short hash.
- A graph column painted with `canvas`: stroked lane lines, cubic Bézier lane
  changes and filled nodes, driven by `strata_core::graph::{GraphRow, Segment,
  Half}`. The column is as wide as the widest loaded row; that maximum grows
  only with newly arrived chunks so lanes never move while scrolling.
- A file list for the selected commit and a diff pane below it, built from
  `strata_core::session::DiffView`: line numbers, `+`/`-` backgrounds,
  word-level emphasis and syntax colours from `LineHighlights`.
- A sidebar with the worktree status, local/remote branches, tags and stashes.
  A status bar shows the last git command and background load errors.
- Light/dark follows the OS appearance through `gpui-component`'s theme.
- `F12` toggles a frame-time overlay (mean frame interval, p95, FPS, element
  build time, display refresh target). While it is on the app keeps requesting
  animation frames, so the numbers reflect continuous rendering rather than
  idle frames; intervals above 200 ms are treated as idle gaps and left out, so
  opening the overlay after the app has been sitting still does not report a
  bogus average.

Session events arrive on background threads; an unbounded async channel feeds
one foreground task that updates the view entity and calls `cx.notify()`. The
UI only ever reads `Session::snapshot()`, and file lists and diffs are computed
off the UI thread (see `src/repo_view.rs`).

Keys: `↑`/`↓` or `j`/`k` select, `PageUp`/`PageDown` jump, `Home`/`End` jump to
the ends, `F12` toggles the overlay, `Ctrl+Q` (`Cmd+Q` on macOS) quits. Every
command is a GPUI action bound in `src/actions.rs` under the `RepoView` key
context, so new commands add an action and a binding there rather than
matching raw keys in a view.

The window draws its own title bar (`gpui_component::TitleBar`) with the
repository and branch in it, so the title bar and the app read as one surface
on every platform.

## Dependencies: why `gpui-pre`

`gpui` is not published on crates.io under its own name. The current GPUI is
published by the GPUI Kit maintainers as **`gpui-pre` 0.3.12**, which is a
verbatim snapshot of `zed-industries/zed@d2a4fb6ded7c470be25d1541281da3882495b65b`
(zed's `gpui` 0.2.2). Its library is still named `gpui`, so `Cargo.toml`
renames the package:

```toml
gpui          = { package = "gpui-pre", version = "=0.3.12" }
gpui-platform = { package = "gpui-pre-platform", version = "=0.3.12" }
gpui-component = "0.9.1"
```

`gpui-component` 0.9.1 (`longbridge/gpui-kit`) pins exactly the same snapshot
(`gpui = "=0.3.12", package = "gpui-pre"`), and `gpui-pre-platform` supplies the
per-OS `Platform` implementation used by `gpui_platform::application()`.

Two things are easy to get wrong here:

- `gpui-pre-platform` depends on `gpui-pre-linux` with `default-features = false`.
  Unless the `wayland` and `x11` features are enabled on **`gpui-platform`**, the
  Linux app starts in headless mode: `open_window` succeeds, `render` runs, and
  no X11/Wayland window is ever created, with no error logged.
- `gpui-pre` is a pre-release snapshot. Pin it with `=` and keep `Cargo.lock`
  committed; the API moves between snapshots.

This is deliberately a crates.io-only dependency set: no git dependency and no
`[patch]`, so `cargo build` works from a cold registry cache on all three
targets.

`gpui-component` is used for the theme (palette, light/dark detection, mono
font family). The list/graph/diff painting uses raw `gpui` so the measurement
is about GPUI itself.

## Building on Linux (Nix)

The repository root has a `flake.nix` dev shell with the native libraries GPUI
needs (`libxkbcommon`, wayland, the X11 libraries, `vulkan-loader`,
`fontconfig`, `freetype`, `openssl`, `alsa-lib`). It sets:

- `LD_LIBRARY_PATH` for wgpu's and xkbcommon's runtime `dlopen` calls,
- `LIBRARY_PATH`, because pkg-config emits bare `-lxcb`/`-lxkbcommon` flags
  that `cc` then has to resolve,
- `PKG_CONFIG_PATH`, since `mkShell.packages` does not run the pkg-config
  setup hook (`yeslogic-fontconfig-sys` fails to build without it),
- `FONTCONFIG_FILE` pointing at a generated `fonts.conf` with DejaVu and
  Liberation, because Nix has no `/usr/share/fonts`; without fonts GPUI logs
  `requested font family contains no font` and renders no text at all,
- `XKB_CONFIG_ROOT` for libxkbcommon's keymap data,
- `VK_ICD_FILENAMES` (x86_64 only) pointing wgpu at Mesa's `lavapipe`, so a
  machine with no GPU still has a Vulkan device.

```sh
direnv allow
direnv exec . cargo run -p strata-app --release -- [PATH]
```

or without direnv:

```sh
nix develop -c cargo run -p strata-app --release -- [PATH]
```

The dev shell sets `CARGO_TARGET_DIR` to a directory shared by every worktree,
so the binary is not under `./target`; `cargo run` finds it either way.

macOS and Windows need no Nix; `cargo build -p strata-app` from a normal Rust
toolchain is enough.

## Headless smoke test

`STRATA_SMOKE=1` starts with the overlay enabled, scrolls the commit list every
frame, prints frame statistics to stderr and quits after 300 frames. `RUST_LOG`
drives `env_logger`, which is how the X11/Wayland/GPU/font problems above were
found in the first place:

```sh
Xvfb :99 -screen 0 1600x1000x24 &
DISPLAY=:99 RUST_LOG=info STRATA_SMOKE=1 direnv exec . cargo run -p strata-app --release
```

This is how the phase-0 numbers on a headless Linux box were produced. On X11,
GPUI drives continuous frames from a periodic refresh timer that only starts
once the window is mapped and visible; `Xvfb` with no window manager is enough,
but a window that never becomes visible will render exactly one frame and then
sit idle.

## Measured on this machine (headless Linux, no GPU)

Release build, `Xvfb` 1600x1000 with a 1360x860 window, Vulkan through Mesa
`lavapipe` (software rasteriser, no hardware adapter available). The smoke run
scrolls the commit list by three rows every frame for 300 frames:

```text
strata: generated 200000 synthetic commits and graph rows in ~17-20ms
smoke: 300 frames, ~50 ms/frame mean, ~20 fps, p95 ~64 ms, build 0.07 ms, target 16.7 ms, 200000 commits
```

Against this checkout's history (131 commits reachable from HEAD and all refs):

```text
smoke: 300 frames, 50.8 ms/frame mean, 19.7 fps, p95 64.0 ms, build 0.06 ms, target 16.7 ms, 131 commits
```

Reading the numbers:

- Building the element tree for the whole window (25 commit rows with canvas
  graph columns, plus the diff rows and the sidebar) costs **0.07 ms**:
  virtualisation and the canvas column are not the bottleneck. The real-repo run
  and the 200k-commit run cost the same, which is the point.
- Nearly all of the ~50 ms frame is presentation: llvmpipe rasterising a
  1360x860 surface at ~500k pixels per frame. That number says nothing about
  GPU-backed macOS/Windows, where this workload should be trivial.
- `p95` is worse than the mean because every frame re-lays out the list while
  it scrolls.
- The overlay compares the mean against the display's reported refresh interval
  (60 Hz here) and shows p95 next to it, so "below target" is not hard-coded to
  a 55 fps threshold.

What this machine cannot answer: whether a real GPU sustains 60 fps. That needs
a run on macOS or Windows (or a Linux box with a hardware Vulkan device); the
benchmark is built for exactly that.

## Graph rows

`GraphRow`s now come from two producers: `strata_core::session::CommitList` (the
real walk, laid out by `strata_core::graph::GraphLayout`) and `src/fake.rs` for
the synthetic benchmark. Both are exposed through `src/data.rs::Source`, so the
row renderer (`src/repo_view/chrome.rs`) and `graph_canvas` only ever see
`GraphRow`.
