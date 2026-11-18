# Packaging

Everything here turns a release build into something a user can launch. All
output lands in `dist/`, which is git-ignored.

| Path | What it does |
| --- | --- |
| `lib/common.sh` | Version, target-directory and arch helpers the scripts share. |
| `macos/bundle.sh` | Builds `dist/Strata.app` with `Info.plist`, the `.icns`, the licences and the notices, ad-hoc signed. |
| `macos/dmg.sh` | Wraps that app in `dist/Strata-<version>-macos-<arch>.dmg`. |
| `linux/package.sh` | Builds `dist/strata-<version>-linux-<arch>.tar.gz`. |
| `linux/strata.desktop` | Desktop entry for menus and file managers. |
| `linux/install.sh` | Installs the binary, icons and desktop entry into a prefix; works from an unpacked archive or a checkout. |
| `windows/package.ps1` | Builds and zips `strata.exe`, the README, the licences and the notices. |
| `release/update-manifest.sh` | Signs the release archives and writes `latest.json` for the app's updater. |
| `third-party/generate.sh` | Writes the third-party notices that every artifact carries. |
| `third-party/libghostty-vt.md` | The part of those notices that cargo-about cannot see: the Zig side of the terminal backend. |
| `third-party/catppuccin.md` | MIT notice for the bundled Catppuccin colour palettes. |

The app icon lives with the other resources, not here:
`crates/strata-app/resources/icon/`. `generate.sh` there renders every platform
file from the SVGs with `resvg`, and all of its outputs are checked in, so no
build step needs an image tool. How to regenerate them is the last section.

## CI and releases

`.github/workflows/ci.yml` runs rustfmt, then builds and runs the test suite
with warnings denied on Ubuntu, macOS and Windows, plus one job that checks
the app with `--no-default-features` to prove it still builds without the
terminal feature and therefore without Zig. Clippy runs locally before each
commit rather than in CI, so the workspace is compiled once per platform.

Zig 0.17.1 is required by `crates/strata-term/build.rs`, which downloads
libghostty-vt 1.0.1 and its dependency tarballs (verifying their SHA-256 sums)
and compiles the static library. The compiled archive and the tarballs are
cached under `<target dir>/ghostty-vt-cache`, and CI caches that directory
separately from `Swatinem/rust-cache` so a dependency bump does not throw away
several minutes of Zig work.

`.github/workflows/release.yml` runs on a `v*` tag and on manual dispatch:

1. `version` fails unless the tag equals the `[workspace.package] version` in
   `Cargo.toml` (so `v0.1.0` needs `version = "0.1.0"`).
2. `build` compiles with `--release --locked` on three runners, packages the
   result with the scripts below, and uploads each archive as a workflow
   artifact:
   - `Strata-<version>-macos-arm64.dmg` on `macos-15`
     (`aarch64-apple-darwin`)
   - `Strata-<version>-windows-x64.zip` on `windows-latest`
     (`x86_64-pc-windows-msvc`)
   - `strata-<version>-linux-x86_64.tar.gz` on `ubuntu-22.04`
     (`x86_64-unknown-linux-gnu`, the oldest hosted runner, so the archive
     needs no newer glibc than 2.35)
3. `publish` (tags only) downloads the archives, signs them and writes
   `latest.json` when the signing secret is present, writes `SHA256SUMS.txt`
   (which covers the signatures and the manifest too) and creates a **draft**
   GitHub release with generated notes. A manual dispatch stops after step 2 and
   leaves the archives in the workflow run.

Every artifact carries both licences and `THIRD-PARTY-NOTICES.md`, which
`third-party/generate.sh` produces from `cargo-about` plus the pinned Zig
dependency list. macOS releases target Apple silicon only; Intel Macs are not
supported.

## Update manifest and signatures

`publish` runs `release/update-manifest.sh` after every archive has been
downloaded and before `SHA256SUMS.txt` is written, so the sums cover the
signatures and the manifest as well. The script signs each archive into
`<archive>.minisig` and writes `latest.json`, the document the app's updater
reads to decide whether a newer release exists:

```json
{
  "version": "0.1.7",
  "tag": "v0.1.7",
  "notes_url": "https://github.com/satotek/strata/releases/tag/v0.1.7",
  "assets": {
    "macos-aarch64": {"name": "Strata-0.1.7-macos-arm64.dmg", "sha256": "<hex>", "minisig": "<full .minisig text>"},
    "windows-x86_64": {"name": "Strata-0.1.7-windows-x64-setup.exe", "sha256": "<hex>", "minisig": "<full .minisig text>"},
    "windows-x86_64-portable": {"name": "Strata-0.1.7-windows-x64.zip", "sha256": "<hex>", "minisig": "<full .minisig text>"},
    "linux-x86_64": {"name": "strata-0.1.7-linux-x86_64.tar.gz", "sha256": "<hex>", "minisig": "<full .minisig text>"}
  }
}
```

A platform with no archive in the run is left out of `assets` instead of
failing the job, so the Windows installer only shows up once a build job
produces one. An archive that matches no known name pattern fails the job, as
do two archives for the same platform: either would produce a manifest the
updater cannot use. The manifest itself is signed into `latest.json.minisig`, so
one verification vouches for the whole document.

### The signing key

One key pair signs every release. It is separate from the platform code signing
keys because the app checks it itself:

```sh
minisign -G -W -p strata-update.pub -s strata-update.key
```

`-W` leaves the secret key unencrypted, because the workflow has no terminal to
answer a passphrase prompt. Neither file belongs in the repository: the public
key is compiled into the app as the app's embedded update key, and the secret
key is a repository secret:

```sh
gh secret set STRATA_UPDATE_KEY < strata-update.key
```

Keep `strata-update.key` out of the working tree and backed up somewhere only
release managers can reach. Without it a release cannot be signed, and anyone
holding it can sign an update the app will install. Rotating the key means
generating a pair, embedding the new public key, publishing a release with it
and then switching the secret; the app has to accept the old key until every
version users still run knows the new one.

A run without the secret — a fork that never set it, for instance — logs a
`::warning::` and publishes unsigned archives with no `latest.json`.

### Verifying by hand

```sh
minisign -V -p strata-update.pub -m Strata-0.1.7-macos-arm64.dmg
minisign -V -p strata-update.pub -m latest.json
```

Installing `latest.json.minisig` next to `latest.json` is enough for minisign to
find it. The workflow installs minisign 0.12 from a pinned release tarball and
checks its SHA-256 before using it, so signing does not depend on whatever
version the runner image happens to ship.

To run the script against a directory of archives:

```sh
packaging/release/update-manifest.sh --key ~/strata-update.key dist 0.1.7 v0.1.7
```

It needs `jq` and `minisign` on `PATH`.

## macOS

```sh
direnv allow
direnv exec . packaging/macos/bundle.sh
direnv exec . packaging/macos/dmg.sh
open dist/Strata.app
```

`bundle.sh` runs `cargo build --release --locked -p strata-app`, copies the
binary to `Contents/MacOS/strata` and `Strata.icns`, `LICENSE-MIT`,
`LICENSE-APACHE` and `THIRD-PARTY-NOTICES.md` to `Contents/Resources/`, writes
`Contents/Info.plist`, and signs the bundle ad hoc (`codesign -s -`). It needs
`dist/THIRD-PARTY-NOTICES.md`, so run `third-party/generate.sh` first, or point
`NOTICES` at another file.

The signature is enough for local use; Gatekeeper still treats the app as
unsigned for anything downloaded, and there is no notarization. The version
comes from the workspace `Cargo.toml`; the bundle id is `dev.strata.Strata`.

Pass `--target <triple>` to build for the other architecture, and `--no-build`
to bundle a binary that already exists. Both scripts resolve the binary through
`CARGO_TARGET_DIR` and the target triple, because the Nix dev shell points
cargo at a shared target directory instead of `./target`.

`dmg.sh` copies the app into a staging folder with an `/Applications` symlink
and the three licence files, then runs `hdiutil create` (`UDZO`), so the
resulting image can be dragged straight into Applications. It refuses to run
before `bundle.sh`.

The bundle carries no dylibs: the release binary links only system frameworks,
so the `.app` is the whole product. `LSMinimumSystemVersion` is 14.0.

`CFBundleIconFile` names `Strata.icns`, which the icon `generate.sh` builds with
`iconutil` from an `.iconset`. `iconutil` is macOS-only, so regenerating that
one file has to happen on a Mac; the other sizes still regenerate anywhere.

## Linux

```sh
direnv exec . packaging/linux/package.sh
direnv exec . packaging/linux/install.sh            # /usr/local
PREFIX=$HOME/.local direnv exec . packaging/linux/install.sh
```

`package.sh` builds the release binary and writes
`dist/strata-<version>-linux-<arch>.tar.gz`. The archive unpacks into one
directory holding `strata`, `strata.desktop`, `icons/<size>.png` (16–512),
`install.sh`, `LICENSE-MIT`, `LICENSE-APACHE` and `THIRD-PARTY-NOTICES.md`, so
it installs on a machine with no toolchain:

```sh
tar -xzf strata-0.1.0-linux-x86_64.tar.gz
PREFIX=$HOME/.local strata-0.1.0-linux-x86_64/install.sh
```

`install.sh` detects that layout. Run from a checkout instead, it builds the
release binary first and installs from the repository. Either way it drops the
binary in `bin/`, `strata.desktop` in `share/applications/`, the icons in
`share/icons/hicolor/<size>x<size>/apps/strata.png` and the licences in
`share/doc/strata/`. It is a convenience for a development machine or the
release archive, not a distribution package: a distro build needs its own
dependency and library-path handling.

`package.sh` takes `--target <triple>` and `--no-build` like the macOS scripts,
and `NOTICES=<path>` to point at a notices file other than
`dist/THIRD-PARTY-NOTICES.md`.

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

Builds with `cargo build --release --locked -p strata-app` and writes
`dist\Strata-<version>-windows-x64.zip` containing `strata.exe`, `README.txt`,
both licences and `THIRD-PARTY-NOTICES.md`. `-Target <triple>` and `-NoBuild`
mirror the shell scripts, and the script resolves the binary through
`CARGO_TARGET_DIR`. `build.rs` embeds `resources/icon/strata.ico` in the
executable, which is what Explorer, the taskbar and every window show, so the
zip needs no icon files. Build on Windows: GPUI compiles its shaders with
`fxc.exe` from the Windows SDK, so there is no cross-build.

## Third-party notices

```sh
nix shell nixpkgs#cargo-about --command \
    direnv exec . packaging/third-party/generate.sh dist/THIRD-PARTY-NOTICES.md
```

The script renders `about.hbs` through `cargo-about` (configuration in
`about.toml`, which accepts only the licences actually present in the
lockfile), appends `third-party/libghostty-vt.md` and
`third-party/catppuccin.md`, and writes the result. It needs `cargo-about` on
PATH; the Nix package and `cargo install cargo-about` both work, and CI installs
it with `taiki-e/install-action`.

The appended section matters because `cargo-about` only walks Cargo's graph:
libghostty-vt, its Zig packages (Highway, Wuffs, Aro, `translate_c`, uucode) and
its vendored simdutf are compiled by `build.rs` with Zig and never appear as
crates. The table there is the upstream `deps/libghostty-vt-third-party.json`
of the pinned release, so bumping libghostty-vt means updating that table too.

## Cutting a release

1. Bump `[workspace.package] version` in `Cargo.toml` and commit it.
2. Tag the commit and push the tag:

   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```

3. Wait for the Release workflow. It fails immediately if the tag and the
   manifest version disagree.
4. Review the draft release on GitHub: check the four archives, their
   `.minisig` files, `latest.json`, `SHA256SUMS.txt` and the generated notes,
   then publish it.

## Regenerating the icons

```sh
crates/strata-app/resources/icon/generate.sh
```

This writes `strata.ico` (Windows), `Strata.icns` (macOS) and
`hicolor/{16,32,48,64,128,256,512}.png` (Linux) from the SVGs in the same
directory. The small sizes use the dedicated drawings rather than downscales;
see that directory's `README.md`. Commit the regenerated files with the SVG
change.
