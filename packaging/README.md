# Packaging

Everything here turns a release build into something a user can launch. All
output lands in `dist/`, which is git-ignored.

| Path | What it does |
| --- | --- |
| `macos/bundle.sh` | Builds `dist/Strata.app` with `Info.plist` and the `.icns`, ad-hoc signed. |
| `linux/strata.desktop` | Desktop entry for menus and file managers. |
| `linux/install.sh` | Builds and installs the binary, icons and desktop entry into a prefix. |
| `windows/package.ps1` | Builds and zips `strata.exe` with `README.txt`. |

The app icon lives with the other resources, not here:
`crates/strata-app/resources/icon/`. `generate.sh` there renders every platform
file from the SVGs with `resvg`, and all of its outputs are checked in, so no
build step needs an image tool.

## macOS

```sh
direnv allow
direnv exec . packaging/macos/bundle.sh
open dist/Strata.app
```

`bundle.sh` runs `cargo build --release -p strata-app`, copies the binary to
`Contents/MacOS/strata` and `Strata.icns` to `Contents/Resources/`, writes
`Contents/Info.plist`, and signs the bundle ad hoc (`codesign -s -`). The
signature is enough for local use; Gatekeeper still treats the app as unsigned
for anything downloaded, and there is no notarization. The version comes from
the workspace `Cargo.toml`; the bundle id is `dev.strata.Strata`.

The bundle carries no dylibs: the release binary links only system frameworks,
so the `.app` is the whole product. `LSMinimumSystemVersion` is 11.0.

The script resolves the binary through `CARGO_TARGET_DIR` because the Nix dev
shell points cargo at a shared target directory instead of `./target`.

`CFBundleIconFile` names `Strata.icns`, which `generate.sh` builds with
`iconutil` from an `.iconset`. `iconutil` is macOS-only, so regenerating that
one file has to happen on a Mac; the other sizes still regenerate anywhere.

## Linux

```sh
direnv exec . packaging/linux/install.sh            # /usr/local
PREFIX=$HOME/.local direnv exec . packaging/linux/install.sh
```

The install drops the binary in `bin/`, `strata.desktop` in
`share/applications/`, and the 16–512px PNGs in the matching
`share/icons/hicolor/<size>x<size>/apps/strata.png` paths. It is a convenience
for a development machine built in the Nix dev shell, not a distribution
package: a distro build needs its own dependency and library-path handling.

The desktop entry uses `Icon=strata` and `StartupWMClass=strata`, and the app
sets `WindowOptions::app_id` to `strata` so X11/Wayland group the window with
the entry. Wayland takes the icon from the desktop file; X11 has no such
lookup, so on Linux the app embeds `resources/icon/hicolor/128.png` with
`include_bytes!` and hands the decoded pixels to `WindowOptions::icon`, which
the X11 backend publishes as `_NET_WM_ICON`. That path needs the `image` crate
for the PNG decoder only (`default-features = false, features = ["png"]`), and
is behind `cfg(target_os = "linux")`.

## Windows

```powershell
powershell -ExecutionPolicy Bypass -File packaging\windows\package.ps1
```

Builds with `cargo build --release -p strata-app` and writes
`dist\strata-<version>-windows-x86_64.zip` containing `strata.exe` and
`README.txt`. `build.rs` embeds `resources/icon/strata.ico` in the executable,
which is what Explorer, the taskbar and every window show, so the zip needs no
icon files. The script assumes the default `target\release` layout and is meant
to run on Windows; GPUI compiles its shaders with `fxc.exe` from the Windows
SDK, so there is no cross-build.

## Regenerating the icons

```sh
crates/strata-app/resources/icon/generate.sh
```

This writes `strata.ico` (Windows), `Strata.icns` (macOS) and
`hicolor/{16,32,48,64,128,256,512}.png` (Linux) from the SVGs in the same
directory. The small sizes use the dedicated drawings rather than downscales;
see that directory's `README.md`. Commit the regenerated files with the SVG
change.
