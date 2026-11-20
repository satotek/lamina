#!/bin/sh
# Packages the Linux build into dist/lamina-<version>-linux-<arch>.tar.gz: the
# binary, the desktop entry, the hicolor icons, install.sh, both licences and
# the third-party notices. The archive unpacks into one directory and needs no
# build tools on the machine that installs it.
#
#     direnv exec . packaging/linux/package.sh
#     packaging/linux/package.sh --target aarch64-unknown-linux-gnu
set -eu

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
. "$root/packaging/lib/common.sh"

usage() {
    cat <<'EOF'
Packages the Linux build into dist/lamina-<version>-linux-<arch>.tar.gz: the
binary, the desktop entry, the hicolor icons, install.sh, both licences and
the third-party notices. The archive unpacks into one directory and needs no
build tools on the machine that installs it.

    direnv exec . packaging/linux/package.sh
    packaging/linux/package.sh --target aarch64-unknown-linux-gnu

Options:
    --target <triple>  build and package that target instead of the host
    --no-build         package a release binary that already exists

Environment:
    NOTICES  notices file to embed (default: dist/THIRD-PARTY-NOTICES.md)
EOF
}

target=
build=1
while [ $# -gt 0 ]; do
    case $1 in
    --target)
        target=${2:?--target needs a triple}
        shift 2
        ;;
    --no-build) build=0; shift ;;
    -h | --help)
        usage
        exit 0
        ;;
    *)
        echo "package.sh: unknown argument: $1" >&2
        exit 2
        ;;
    esac
done

cd "$root"

notices=${NOTICES:-$root/dist/THIRD-PARTY-NOTICES.md}
version=$(lamina_version "$root")
# The archive name uses the plain machine architecture (x86_64, aarch64), the
# same spelling the target triples use.
if [ -n "$target" ]; then
    arch=${target%%-*}
else
    arch=$(uname -m)
fi
release=$(lamina_release_dir "$root" "$target")
icons=crates/lamina-app/resources/icon/hicolor

if [ -z "$version" ]; then
    echo "package.sh: could not read [workspace.package] version from Cargo.toml" >&2
    exit 1
fi
if [ ! -f "$notices" ]; then
    echo "package.sh: $notices is missing; run packaging/third-party/generate.sh first" >&2
    exit 1
fi

if [ "$build" = 1 ]; then
    command -v cargo >/dev/null 2>&1 || {
        echo "package.sh: cargo not found; run through the dev shell: direnv exec . $0" >&2
        exit 1
    }
    set -- --release --locked -p lamina-app
    if [ -n "$target" ]; then
        set -- "$@" --target "$target"
    fi
    cargo build "$@"
fi

if [ ! -f "$release/lamina" ]; then
    echo "package.sh: no binary at $release/lamina; build it or drop --no-build" >&2
    exit 1
fi

name="lamina-$version-linux-$arch"
dist="$root/dist"
stage="$dist/$name"
archive="$dist/$name.tar.gz"

rm -rf "$stage"
mkdir -p "$stage/icons"
cp "$release/lamina" "$stage/lamina"
chmod 755 "$stage/lamina"
cp packaging/linux/lamina.desktop "$stage/lamina.desktop"
cp packaging/linux/install.sh "$stage/install.sh"
chmod 755 "$stage/install.sh"
cp LICENSE-MIT LICENSE-APACHE "$stage/"
cp "$notices" "$stage/THIRD-PARTY-NOTICES.md"

# The flat <size>.png names match the repository layout, so install.sh can use
# one path expression for both a checkout and this archive.
for size in 16 32 48 64 128 256 512; do
    cp "$icons/$size.png" "$stage/icons/$size.png"
done

rm -f "$archive"
tar -czf "$archive" -C "$dist" "$name"
rm -rf "$stage"

echo "wrote $archive"
