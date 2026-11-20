# Lamina

[日本語](README.ja.md)

Lamina is a fast, lightweight native Git client for macOS and Windows (with Linux supported on a best-effort basis), built in Rust with GPUI. It reads repositories directly with gitoxide (`gix`) and performs every write operation through the `git` CLI so hooks, commit signing, credential helpers, and Git LFS behave identically to running `git` in a terminal. The interface is available in English and Japanese.

<!-- Screenshot placeholder: add a screenshot once available -->

## Status

Lamina was called Strata until 0.1.10. An update from those releases keeps its settings, themes, keymap and proxy password.

Lamina is in early (`0.x`) development as a personal open-source project—expect rough edges and breaking changes.

## Features

### History & Commit Graph
- Virtualized commit list with a canvas-rendered commit graph, local/remote branch and tag badges, `HEAD` highlight, and relative or absolute dates.
- Incremental background history loading with an optional commit-graph generation prompt for large repositories.
- Commit details with changed-file stats, multi-commit selection (`Range changes` and `Between these two`), and two-dot (`A..B`) / three-dot (`A...B`) commit comparison.
- Shallow clone boundary indicators with one-click full-history fetch.

### Diffs, Blame & File Browsing
- Unified and side-by-side diff layouts, word-level intra-line emphasis, tree-sitter syntax highlighting (Rust, JavaScript, TypeScript, TSX, Python, Go, JSON, TOML, YAML, CSS, HTML, Bash, C, C++, Markdown), whitespace markers, line wrapping, configurable context lines, ignore-whitespace (`-w`), and expandable gaps / whole-file view.
- Image diff preview (PNG, JPEG, GIF, WebP, BMP, ICO, and SVG with a text/rendered toggle) in Side-by-side, Swipe, and Onion skin modes.
- File history (with rename tracking), line-by-line blame, and a commit tree browser.

### Working Tree, Staging & Conflict Resolution
- Unstaged, staged, and conflicted file views with multi-file selection and keyboard navigation.
- File-level, hunk-level, and line-level staging, unstaging, and discarding using byte-preserving patch generation.
- Commit and amend support with a configurable summary length warning, plus a built-in `--askpass` bridge for HTTP/SSH credential prompts.
- Full-window three-way merge editor (Side A, Side B, Base, and editable Result) with per-conflict resolution choices, manual editing, conflict marker checks, and external merge tool launch.
- In-progress operation banner with continue and abort/reset actions for merge, rebase, cherry-pick, revert, `git am`, and `git bisect`.

### Branches, Remotes, Stashes, Worktrees & Submodules
- Quick branch switcher, branch creation, renaming (including remote branches), deletion, upstream management, and branch/tag filtering.
- Fetch (with optional prune and background auto-fetch), pull, push, branch publishing, and force-push-with-lease confirmation.
- Stash creation (all changed files or selected files, optionally including untracked files), apply, pop, drop, and reveal in history.
- Linked worktrees (add, open in tab or window, remove, prune) and submodules (initialize, update, open in tab or window, navigate back to parent repository).
- Repository cloning (branch, shallow depth, recursive submodules) and new repository initialization.

### History Rewriting & Reflog Undo
- Single and multi-commit cherry-pick, revert, soft/mixed/hard reset, and rebase onto a branch or commit (with optional autostash).
- Interactive history editor (pick, reword, squash, fixup, drop, reorder) plus direct single-commit reword, squash, fixup, drop, and move actions.
- Reflog viewer with one-step Undo Last Action and restore-to-point.

### Search, LFS, Signatures & External Tools
- Commit filtering by summary, author, or hash, plus parallel history content search (`-S` substring / `-G` regex) with optional path filtering and commit-graph Bloom filter acceleration.
- Git LFS support (pointer detection, object download, tracking, file locking/unlocking).
- Commit signature status badges and background verification (`OpenPGP`, `SSH`, `X.509`).
- Optional GitHub CLI (`gh`) integration for branch pull-request badges (state, review, check counts), PR checkout dialog, opening commits/files/PRs on GitHub, and creating PRs (`gh pr create --web`).
- Optional `ghq` integration for repository discovery and clone destinations.
- Git config inspector and editor across all configuration layers with `include`/`includeIf` inspection and command/diff preview before writing.

### Workspace, Customization & Embedded Terminal
- Multi-tab and multi-window workspace with session restore, recent repository picker, light/dark/system theme (with bundled and user-provided TOML palettes), customizable UI and code fonts, and English/Japanese UI (`System` follows the OS language).
- Command palette, customizable keybindings (`keymap.json` and Keyboard settings), and custom commands (`commands.json` with `${repo}`, `${branch}`, `${commit}`, `${file}`, and `${remote}` variables).
- Embedded PTY terminal panel per repository tab (`lamina-term`, built on `libghostty-vt` and `portable-pty`, optional via the Cargo `terminal` feature).
- Japanese and legacy text encoding support: UTF-8, UTF-16LE/BE (BOM), `.gitattributes` `working-tree-encoding`, and automatic detection (`Shift_JIS`, `EUC-JP`, `ISO-2022-JP`, etc.) for display while preserving original bytes on writes and patches.

## Customization

### Custom Themes

Lamina loads user-defined `*.toml` colour themes from the `themes/` subdirectory of your configuration folder (or `$LAMINA_CONFIG_DIR/themes/` when `LAMINA_CONFIG_DIR` is set):

- **macOS:** `~/Library/Application Support/Lamina/themes/`
- **Windows:** `%APPDATA%\Lamina\themes\`
- **Linux:** `$XDG_CONFIG_HOME/lamina/themes/` (defaults to `~/.config/lamina/themes/`)

To start from a complete working file, open **Settings → Appearance** and click **Export current theme as a file**. That writes `<id>-custom.toml` into your themes folder with every token populated, and **Open themes folder** opens that directory. Saving edits to an active user theme re-applies it live across open windows.

Required fields (every token is required, unknown fields are rejected, and all colours use `#rrggbb` or `#rrggbbaa` hex strings):

- Top-level: `id` (unique string; bundled IDs cannot be overridden), `name` (picker label), `appearance` (`"light"` or `"dark"`)
- `[colors]`: all 36 UI, badge, diff, and syntax tokens
- `[terminal]`: `foreground`, `background`, `cursor`, `selection`, plus all 16 ANSI colours (`black`–`white` and `bright_black`–`bright_white`)
- `[graph]`: `lanes` (array of 8 hex colours)

## Installation (GitHub Releases)

Prebuilt binaries are published on GitHub Releases.

### Runtime Requirements
- **`git` (`>= 2.30.0`)** on `PATH` (required for all repository write operations).
- Optional: `gh` (GitHub CLI), `git-lfs`, and `ghq`.

### macOS
Download the `.dmg` (Apple silicon, macOS 14.0 or later), open it, and drag `Lamina.app` to `/Applications`. Intel Macs are not supported.

Release builds carry a self-signed signature and are **not notarized**. On first launch, right-click `Lamina.app` and choose **Open**, or clear the quarantine attribute:

```sh
xattr -dr com.apple.quarantine /Applications/Lamina.app
```

### Windows (x86_64)
Download and extract the `.zip` archive, then run `lamina.exe`.

Release builds are **unsigned**. If Windows SmartScreen displays a warning, click **More info → Run anyway**.

### Linux (Best Effort)
Download and extract the release tarball. Make sure the following runtime shared libraries and system fonts are installed on your distribution:
- Windowing & input: `libxkbcommon`, Wayland (`libwayland-client`) and/or X11 (`libX11`, `libxcb`, `libXcursor`, `libXi`, `libXrandr`, `libXrender`)
- Graphics & audio: `vulkan-loader` (`libvulkan`) with a Vulkan driver, `libGL`, `alsa-lib`
- Text, TLS & C++ runtime: `fontconfig`, `freetype`, `openssl` (`libssl`), `libstdc++`, and system fonts (including CJK fonts such as Noto Sans CJK JP if viewing Japanese text)

## Building from Source

Building Lamina requires Rust (stable, selected by [rust-toolchain.toml](rust-toolchain.toml)) and `git`. Building with the embedded terminal enabled (the default) also requires **Zig `0.17.1`** for `libghostty-vt`. To build without the embedded terminal (and without Zig), pass `--no-default-features`:

```sh
# macOS (with Xcode / Metal Toolchain installed)
cargo run --release -p lamina-app

# Windows (from "Developer PowerShell for VS")
cargo build --release -p lamina-app

# Linux (via the Nix dev shell)
direnv allow && direnv exec . cargo run --release -p lamina-app

# Without the embedded terminal (no Zig required)
cargo build --release -p lamina-app --no-default-features
```

See [docs/building.md](docs/building.md) for platform-specific prerequisites, Linux package lists, Windows setup, and Nix dev shell details.

## Links

- [Design documentation (Japanese)](docs/design/README.md)
- [Building from source](docs/building.md)
- [Packaging](packaging/README.md)

## License

Dual-licensed under either of:

- MIT License ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option. Third-party notices ship with release artifacts.
