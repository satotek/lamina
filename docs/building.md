# Building Strata

Strata is a Rust workspace with three crates:

- `crates/strata-core` — UI-independent Git core
- `crates/strata-term` — UI-independent embedded terminal core (`libghostty-vt` + `portable-pty`)
- `crates/strata-app` — GPUI desktop application (`strata` binary)

## Common Prerequisites

- **Rust:** [rust-toolchain.toml](../rust-toolchain.toml) selects the `stable` channel with `rustfmt` and `clippy`. Install Rust via [rustup](https://rustup.rs) (or use the Nix dev shell below).
- **Git (`>= 2.30.0`):** Must be on `PATH` at runtime for all write operations and for running tests.
- **Zig `0.17.1` (for the embedded terminal):** Enabled by default via the `terminal` Cargo feature on `strata-app`. On first build, `crates/strata-term/build.rs` downloads the `libghostty-vt` `1.0.1` source and dependency tarballs with `curl` (verified by SHA-256) and compiles them with `zig`.
  - To build **without** the embedded terminal (and without Zig):
    ```sh
    cargo build --release -p strata-app --no-default-features
    ```
  - For custom or offline terminal builds, `crates/strata-term/build.rs` also recognizes `ZIG` (path to the `zig` `0.17.1` binary), `LIBGHOSTTY_VT_LIB_DIR` (directory containing a prebuilt `libghostty-vt.a` or `ghostty-vt-static.lib`), or both `LIBGHOSTTY_VT_SOURCE_DIR` and `LIBGHOSTTY_VT_ZIG_SYSTEM_DIR`.

## macOS

1. Install Xcode (or the Xcode Command Line Tools). On Xcode 26+, also install the Metal Toolchain so GPUI can compile Metal shaders with `xcrun metal`:
   ```sh
   xcodebuild -downloadComponent MetalToolchain
   ```
2. Install Rust with `rustup` and Zig `0.17.1` (for example via Homebrew or the Nix dev shell).
3. Build and run:
   ```sh
   cargo run --release -p strata-app
   ```
   Or build without the embedded terminal:
   ```sh
   cargo run --release -p strata-app --no-default-features
   ```
4. To assemble a standalone `dist/Strata.app` bundle (minimum macOS 14.0, ad-hoc signed), see [packaging/README.md](../packaging/README.md):
   ```sh
   packaging/macos/bundle.sh
   ```

## Windows (x86_64)

Build natively on Windows. GPUI compiles its HLSL shaders with `fxc.exe` from the Windows SDK at build time.

1. Install **Visual Studio Build Tools** (or Visual Studio) with the **"Desktop development with C++"** workload. This provides MSVC and the Windows SDK, including `fxc.exe`.
2. Install Rust with [rustup](https://rustup.rs) (default `x86_64-pc-windows-msvc` host).
3. Install [Git for Windows](https://git-scm.com/download/win) and ensure `git` is on `PATH`.
4. Install **Zig `0.17.1`** and put it on `PATH` (or set `ZIG=C:\path\to\zig.exe`). Alternatively, build with `--no-default-features` to omit the embedded terminal.
5. Clone the repository into a short path such as `C:\src\strata` (deep paths can hit Windows' 260-character path limit inside `target\`):
   ```powershell
   git config --global core.longpaths true
   git clone <repo-url> C:\src\strata
   cd C:\src\strata
   ```
6. Build and run from **"Developer PowerShell for VS"** (so `fxc.exe` and the MSVC toolchain are in the environment):
   ```powershell
   cargo build --release -p strata-app
   .\target\release\strata.exe
   ```
   Or without the embedded terminal:
   ```powershell
   cargo build --release -p strata-app --no-default-features
   .\target\release\strata.exe
   ```
7. To create the release zip (`dist\strata-<version>-windows-x86_64.zip`):
   ```powershell
   powershell -ExecutionPolicy Bypass -File packaging\windows\package.ps1
   ```

## Linux

### Option 1: Nix Dev Shell (Recommended on Nix)

The repository root provides [flake.nix](../flake.nix) and [.envrc](../.envrc). On Linux, the dev shell provides Rust, `zig_0_17`, `pkg-config`, `openssh`, `xvfb`, fonts (DejaVu, Liberation, and Noto Sans CJK JP), and all native libraries GPUI needs (`alsa-lib`, `fontconfig`, `freetype`, `libGL`, X11/xcb, `libxkbcommon`, `openssl`, `vulkan-loader`, `wayland`, plus Mesa `lavapipe` on `x86_64` for headless/software Vulkan).

With `direnv`:

```sh
direnv allow
direnv exec . cargo run --release -p strata-app
```

Or directly with `nix develop`:

```sh
nix develop -c cargo run --release -p strata-app
```

### Option 2: System Packages (Debian / Ubuntu)

If you are not using Nix, install the native build and runtime dependencies (matching the CI workflow in [.github/workflows/ci.yml](../.github/workflows/ci.yml)):

```sh
sudo apt-get update
sudo apt-get install -y \
  build-essential \
  clang \
  cmake \
  curl \
  elfutils \
  gcc \
  g++ \
  git \
  jq \
  libasound2-dev \
  libfontconfig-dev \
  libfreetype-dev \
  libgit2-dev \
  libglib2.0-dev \
  libsqlite3-dev \
  libssl-dev \
  libstdc++-14-dev \
  libva-dev \
  libvulkan-dev \
  libwayland-dev \
  libx11-xcb-dev \
  libxcb1-dev \
  libxkbcommon-dev \
  libxkbcommon-x11-dev \
  libzstd-dev \
  lld \
  llvm \
  make \
  musl-dev \
  musl-tools \
  pkg-config
```

Install Rust via `rustup` and Zig `0.17.1` on `PATH` (or pass `--no-default-features` to skip the embedded terminal), then run:

```sh
cargo run --release -p strata-app
```

## Where Build Artifacts Land

- **Outside the Nix dev shell:** Cargo writes artifacts to `./target` (`./target/release/strata` or `.\target\release\strata.exe`).
- **Inside the Nix dev shell:** [flake.nix](../flake.nix) sets `CARGO_TARGET_DIR` to `${XDG_CACHE_HOME:-$HOME/.cache}/strata/target` so multiple worktrees share a single build cache. Because binaries are not written under `./target`, launch via `cargo run` or read from `$CARGO_TARGET_DIR` (the scripts under `packaging/` resolve `CARGO_TARGET_DIR` automatically).
- **Downloaded `libghostty-vt` tarballs and cached static archives:** Stored under `<target_dir>/ghostty-vt-cache`.
- **Packaged bundles and archives:** Written to `dist/` (git-ignored).

## Running Checks and Tests

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

To verify `strata-app` without the embedded terminal:

```sh
cargo clippy -p strata-app --no-default-features --all-targets -- -D warnings
```

For headless Linux UI smoke runs (`STRATA_SMOKE=1` under `Xvfb`) and CLI debug flags, see [crates/strata-app/README.md](../crates/strata-app/README.md).

## Remote helper binaries

Opening a repository over SSH uploads `strata-remote` and runs it on the host.
The file has to be a musl build: the machine that built the app may be macOS
or Windows, and the host must be able to execute the helper without that
machine's C library. This repository does not yet bundle those binaries or
cross-compile them in the release workflow. A release still needs to:

- Build `strata-remote` for `x86_64-unknown-linux-musl` and
  `aarch64-unknown-linux-musl` with the same `STRATA_BUILD_ID` the app was
  built with. The id the client checks is `<version>+<STRATA_BUILD_ID>`, or
  `<version>+dev` when the variable is unset. A mismatch makes the client
  delete its upload and try once more; it will not keep a helper from a
  different build.
- Name the files `strata-remote-<target>` and point the launcher at their
  directory with `STRATA_REMOTE_HELPER_DIR`.
- Leave every other operating system and architecture unsupported. The client
  reports an error. It does not compile on the remote host.

`STRATA_REMOTE_HELPER`, set in the remote user's environment to an executable
path, skips the upload. The client still checks the build id and does not
delete that file when the id does not match.
