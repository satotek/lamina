#!/bin/sh
# Builds the musl `lamina-remote` binaries the app uploads to a Linux host,
# and writes `lamina-remote-<target>` into the directory named by the first
# argument. Further arguments limit the build to those targets; with none, both
# targets the client knows how to upload are built.
#
#     scripts/build-remote-helpers.sh dist/remote-helpers
#     scripts/build-remote-helpers.sh dist/remote-helpers x86_64-unknown-linux-musl
#
# The app and these binaries must be compiled with the same LAMINA_BUILD_ID.
# The release workflow sets that to the commit it checked out. When it is
# unset, both sides embed "dev".
#
# Zig 0.17.1 has to be on PATH (the dev shell provides it). `cargo zigbuild`
# does too; the dev shell does not include it.
set -eu

root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
# shellcheck disable=SC1091
. "$root/packaging/lib/common.sh"

usage() {
    cat <<'EOF'
Builds the musl lamina-remote binaries the app uploads, and writes
lamina-remote-<target> into the given directory.

    scripts/build-remote-helpers.sh dist/remote-helpers
    scripts/build-remote-helpers.sh dist/remote-helpers x86_64-unknown-linux-musl

With no targets, builds x86_64-unknown-linux-musl and
aarch64-unknown-linux-musl. LAMINA_BUILD_ID must match the app build; when
unset, the binaries embed "dev".

Needs Zig 0.17.1 and cargo-zigbuild on PATH. The dev shell provides Zig only.
A one-off `nix shell nixpkgs#cargo-zigbuild` supplies the cross driver. The
dev shell's rustc is the host target only, so a cross build uses rustup:

    rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
    CARGO_TARGET_DIR=$PWD/dist/helper-target \
        PATH="$HOME/.cargo/bin:$PATH" \
        scripts/build-remote-helpers.sh dist/remote-helpers

The dev shell's CARGO_TARGET_DIR was filled by its own rustc. rustup cannot
reuse those host build scripts, so point CARGO_TARGET_DIR somewhere else.

On macOS the script sets MACOSX_DEPLOYMENT_TARGET to 11.0, replacing any
inherited value. A target of 12 or newer makes a host compiler from before
the Mach-O alignment fix emit proc-macro dylibs dyld will not load.
EOF
}

if [ $# -lt 1 ]; then
    usage >&2
    exit 2
fi

case ${1:-} in
-h | --help)
    usage
    exit 0
    ;;
esac

dest=$1
shift
case $dest in
/*) ;;
*) dest=$(pwd)/$dest ;;
esac

if [ $# -eq 0 ]; then
    # The names have no whitespace; the helper prints one triple per line.
    # shellcheck disable=SC2046
    set -- $(lamina_remote_targets)
fi

for target in "$@"; do
    known=0
    for allowed in $(lamina_remote_targets); do
        if [ "$target" = "$allowed" ]; then
            known=1
        fi
    done
    if [ "$known" = 0 ]; then
        echo "build-remote-helpers.sh: unsupported target: $target" >&2
        exit 2
    fi
done

command -v cargo >/dev/null 2>&1 || {
    echo "build-remote-helpers.sh: cargo not found" >&2
    exit 1
}
command -v zig >/dev/null 2>&1 || {
    echo "build-remote-helpers.sh: zig not found; run through the dev shell" >&2
    exit 1
}
zig_version=$(zig version)
if [ "$zig_version" != "0.17.1" ]; then
    echo "build-remote-helpers.sh: zig $zig_version found; this repository uses 0.17.1" >&2
    exit 1
fi
command -v cargo-zigbuild >/dev/null 2>&1 || {
    echo "build-remote-helpers.sh: cargo-zigbuild is not on PATH." >&2
    echo "build-remote-helpers.sh: the dev shell does not include it. For one build:" >&2
    echo "build-remote-helpers.sh:   nix shell nixpkgs#cargo-zigbuild --command $0 $dest" >&2
    exit 1
}

cd "$root"
mkdir -p "$dest"

# An ambient macOS deployment target of 12 or newer (Zig's default, or 14.0
# in some dev shells) makes a host compiler from before the Mach-O alignment
# fix emit proc-macro dylibs dyld will not load, so the build dies in a host
# build script. The Linux binaries do not use this version. Override whatever
# was inherited; 11.0 is the host rustc default for Apple Silicon.
if [ "$(uname -s)" = "Darwin" ]; then
    export MACOSX_DEPLOYMENT_TARGET=11.0
fi

for target in "$@"; do
    sysroot=$(rustc --print sysroot)
    if [ ! -d "$sysroot/lib/rustlib/$target" ]; then
        case $sysroot in
        */.rustup/toolchains/*)
            rustup target add "$target"
            ;;
        *)
            echo "build-remote-helpers.sh: rustc ($sysroot) has no standard library for $target." >&2
            echo "build-remote-helpers.sh: the dev shell rustc is host-only. Add the target with rustup and put that cargo first:" >&2
            echo "build-remote-helpers.sh:   rustup target add $target" >&2
            echo "build-remote-helpers.sh:   CARGO_TARGET_DIR=\$PWD/dist/helper-target PATH=\"\$HOME/.cargo/bin:\$PATH\" $0 $dest $target" >&2
            exit 1
            ;;
        esac
    fi

    CARGO_PROFILE_RELEASE_STRIP=symbols \
        cargo zigbuild --release --locked -p lamina-remote --target "$target"
    release=$(lamina_release_dir "$root" "$target")
    src=$release/lamina-remote
    if [ ! -f "$src" ]; then
        echo "build-remote-helpers.sh: no binary at $src" >&2
        exit 1
    fi
    cp "$src" "$dest/lamina-remote-$target"
    chmod 755 "$dest/lamina-remote-$target"
    echo "wrote $dest/lamina-remote-$target"
done
