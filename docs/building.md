# Building Strata

Strata is a Rust workspace. The app binary is `strata` in `crates/strata-app`.

## Windows (x86_64)

Build natively on Windows. GPUI compiles its HLSL shaders with Microsoft's
`fxc.exe` at build time, and its build script only does so on a Windows host,
so a Windows build is also what we ship.

1. Install **Visual Studio Build Tools** (or Visual Studio) with the
   "Desktop development with C++" workload. This provides MSVC and the
   Windows SDK, including `fxc.exe`.
2. Install Rust with [rustup](https://rustup.rs) (default `x86_64-pc-windows-msvc`
   host). `rust-toolchain.toml` selects the stable channel.
3. Install [Git for Windows](https://git-scm.com/download/win). Strata runs `git`
   for every write, so it must be on `PATH` at runtime too.
4. Get the source into a short path such as `C:\src\strata` (deep paths can hit
   Windows' 260-character limit inside `target\`):

   ```powershell
   git config --global core.longpaths true
   git clone strata.bundle C:\src\strata -b feat/mvp-phase0
   cd C:\src\strata
   ```

5. Build and run from a "Developer PowerShell for VS" (so `fxc.exe` is found):

   ```powershell
   cargo build --release -p strata-app
   .\target\release\strata.exe
   ```

## macOS

Install Xcode (or the Command Line Tools) and Rust with rustup, then:

```sh
cargo run --release -p strata-app
```

## Linux (Nix)

Native libraries and fonts come from the flake's dev shell:

```sh
direnv allow
direnv exec . cargo run --release -p strata-app
```

The dev shell builds into a shared directory (`$XDG_CACHE_HOME/strata/target`),
not `./target`.

See `crates/strata-app/README.md` for the Linux pitfalls this shell works around.

## What the current build shows

The app is still the phase 0 prototype: 200,000 synthetic commits, a graph
column and a sample diff, not a real repository. Useful things to check on a
real machine:

- Scrolling the commit list quickly stays smooth. Press **F12** for the frame
  time overlay; the target is 60 fps (16.7 ms per frame).
- Text is crisp at your display scaling (100%, 150%, 200%), and the monospace
  font in the diff pane is readable.
- Up/Down and mouse clicks change the selected commit and the diff updates.
- The window follows the OS light/dark setting.
- Memory in Task Manager / Activity Monitor (the prototype used 238 MB on Linux
  with software rendering; the MVP target is under 150 MB).
