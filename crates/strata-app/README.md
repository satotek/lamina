# strata-app

GPUI front end for Strata. Right now this crate exists to answer one question:
**can GPUI render a huge commit graph and a highlighted diff smoothly on macOS,
Windows and Linux?** It is a technical validation ("phase 0"), not the real UI.

## What the window shows

- 200,000 synthetic commits in a `uniform_list` (virtualised, ~25 rows alive).
- A graph column painted with `canvas`: stroked lane lines, cubic Bézier lane
  changes and filled nodes, driven by `strata_core::graph::{GraphRow, Segment,
  Half}`.
- Summary / author / date / short-hash columns, generated on demand so the
  200k-row dataset does not hold 600k `String`s.
- A diff pane below the list: `strata_core::diff::{FileDiff, Hunk, DiffLine,
  LineKind}` built by hand, with line numbers, `+`/`-` backgrounds,
  word-level emphasis ranges and a monospace font. Also a `uniform_list`.
- Light/dark follows the OS appearance through `gpui-component`'s theme.
- `F12` toggles a frame-time overlay (mean frame interval, FPS, element build
  time, p95). While it is on the app keeps requesting animation frames, so the
  numbers reflect continuous rendering rather than idle frames; intervals above
  200 ms are treated as idle gaps and left out, so opening the overlay after the
  app has been sitting still does not report a bogus average.

Keys: `↑`/`↓` or `j`/`k` select, `PageUp`/`PageDown` jump, `Home`/`End` jump to
the ends, `F12` toggles the overlay.

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
direnv exec . cargo build -p strata-app --release
direnv exec . ./target/release/strata
```

or without direnv:

```sh
nix develop -c cargo build -p strata-app --release
nix develop -c ./target/release/strata
```

macOS and Windows need no Nix; `cargo build -p strata-app` from a normal Rust
toolchain is enough.

## Headless smoke test

`STRATA_SMOKE=1` starts with the overlay enabled, scrolls the commit list every
frame, prints frame statistics to stderr and quits after 300 frames. `RUST_LOG`
drives `env_logger`, which is how the X11/Wayland/GPU/font problems above were
found in the first place:

```sh
Xvfb :99 -screen 0 1600x1000x24 &
DISPLAY=:99 RUST_LOG=info STRATA_SMOKE=1 ./target/release/strata
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
strata phase 0: generated 200000 commits and graph rows in 16-20ms
smoke: 300 frames, 53 ms/frame mean, 18-22 fps, p95 64-80 ms, build 0.06 ms
```

Reading the numbers:

- Building the element tree for the whole window (25 commit rows with canvas
  graph columns, plus ~25 diff rows) costs **0.07 ms**: virtualisation and the
  canvas column are not the bottleneck.
- Nearly all of the ~53 ms frame is presentation: llvmpipe rasterising a
  1360x860 surface at ~500k pixels per frame. That number says nothing about
  GPU-backed macOS/Windows, where this workload should be trivial.
- `p95` is worse than the mean because every frame re-lays out the list while
  it scrolls.

What this machine cannot answer: whether a real GPU sustains 60 fps. That needs
a run on macOS or Windows (or a Linux box with a hardware Vulkan device); the
benchmark is built for exactly that.

## Swapping in the real graph layout

`src/fake.rs` is the only place that knows how a `GraphRow` is produced. When
`GraphLayout::push` lands, replace `FakeRepo` with the real repository walk and
keep the row/renderer contract (`node_lane`, `segments`, `width`) intact.
