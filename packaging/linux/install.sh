#!/bin/sh
# Installs a Linux build into a prefix: the binary, the hicolor icons, the
# desktop entry, (when they ship next to this script) the licences, and the
# remote helpers. Helpers go in <prefix>/bin/remote-helpers/ so they stay
# beside the installed binary, which is where the app looks.
#
# From the release archive, which needs no toolchain:
#
#     ./install.sh
#     PREFIX=$HOME/.local ./install.sh
#
# From a checkout, which builds the release binary first:
#
#     direnv exec . packaging/linux/install.sh
#     PREFIX=$HOME/.local direnv exec . packaging/linux/install.sh
#
# This installs an archive or a build made in the Nix dev shell, which links the
# shell's native libraries; a distribution package is a separate job.
set -eu

here=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)
prefix=${PREFIX:-/usr/local}

command -v install >/dev/null 2>&1 || {
    echo "install.sh: the 'install' utility is missing (coreutils)" >&2
    exit 1
}

if [ -x "$here/strata" ] && [ -d "$here/icons" ]; then
    # Release archive: the binary, desktop entry and icons sit next to this
    # script, so nothing has to be built.
    bin=$here/strata
    desktop=$here/strata.desktop
    icons=$here/icons
    docs=$here
else
    root=$(CDPATH='' cd -- "$here/../.." && pwd)
    . "$root/packaging/lib/common.sh"
    command -v cargo >/dev/null 2>&1 || {
        echo "install.sh: cargo not found; run through the dev shell: direnv exec . $0" >&2
        exit 1
    }
    (cd "$root" && cargo build --release --locked -p strata-app)
    bin=$(strata_release_dir "$root" "")/strata
    desktop=$root/packaging/linux/strata.desktop
    icons=$root/crates/strata-app/resources/icon/hicolor
    docs=$root
fi

install -Dm755 "$bin" "$prefix/bin/strata"
install -Dm644 "$desktop" "$prefix/share/applications/strata.desktop"

# Only a release archive carries the helpers. They have to stay in the same
# directory as the installed binary: the app looks next to current_exe().
if [ -d "$here/remote-helpers" ]; then
    for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
        helper=$here/remote-helpers/strata-remote-$target
        if [ ! -f "$helper" ]; then
            echo "install.sh: missing remote helper $helper" >&2
            exit 1
        fi
        install -Dm755 "$helper" "$prefix/bin/remote-helpers/strata-remote-$target"
    done
fi

# The window icon is the 128px file embedded in the binary; the desktop entry
# needs the whole hicolor ladder for panels, task switchers and file managers.
for size in 16 32 48 64 128 256 512; do
    install -Dm644 "$icons/$size.png" \
        "$prefix/share/icons/hicolor/${size}x${size}/apps/strata.png"
done

# Licences and third-party notices are required next to a distributed binary;
# a fresh checkout has them at the root as well.
for doc in LICENSE-MIT LICENSE-APACHE THIRD-PARTY-NOTICES.md; do
    if [ -f "$docs/$doc" ]; then
        install -Dm644 "$docs/$doc" "$prefix/share/doc/strata/$doc"
    fi
done

# Both caches are optional; a distribution's packages usually update them.
command -v update-desktop-database >/dev/null 2>&1 &&
    update-desktop-database "$prefix/share/applications" || true
command -v gtk-update-icon-cache >/dev/null 2>&1 &&
    gtk-update-icon-cache -q "$prefix/share/icons/hicolor" || true

echo "installed Strata into $prefix"
