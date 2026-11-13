#!/bin/sh
# Installs a Linux build into a prefix: the binary, the hicolor icons and the
# desktop entry.
#
#     direnv exec . packaging/linux/install.sh
#     PREFIX=$HOME/.local direnv exec . packaging/linux/install.sh
#
# This packages a build made in the Nix dev shell, which links the shell's
# native libraries; a distribution package is a separate job.
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
prefix=${PREFIX:-/usr/local}
# The dev shell points cargo at a shared target directory, so resolve the
# binary the same way the other packaging scripts do.
target_dir=${CARGO_TARGET_DIR:-$root/target}
icons="$root/crates/strata-app/resources/icon"

cargo build --release -p strata-app

install -Dm755 "$target_dir/release/strata" "$prefix/bin/strata"
install -Dm644 "$root/packaging/linux/strata.desktop" \
    "$prefix/share/applications/strata.desktop"

# The window icon is the 128px file embedded in the binary; the desktop entry
# needs the whole hicolor ladder for panels, task switchers and file managers.
for size in 16 32 48 64 128 256 512; do
    install -Dm644 "$icons/hicolor/$size.png" \
        "$prefix/share/icons/hicolor/${size}x${size}/apps/strata.png"
done

# Both caches are optional; a distribution's packages usually update them.
command -v update-desktop-database >/dev/null 2>&1 &&
    update-desktop-database "$prefix/share/applications" || true
command -v gtk-update-icon-cache >/dev/null 2>&1 &&
    gtk-update-icon-cache -q "$prefix/share/icons/hicolor" || true

echo "installed Strata into $prefix"
