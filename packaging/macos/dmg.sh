#!/bin/sh
# Turns dist/Strata.app (from bundle.sh) into the disk image the release
# publishes: dist/Strata-<version>-macos-<arch>.dmg, with Strata.app and an
# /Applications symlink so the image can be dragged into place.
#
#     direnv exec . packaging/macos/dmg.sh
#     packaging/macos/dmg.sh --target aarch64-apple-darwin
#
# The app is ad hoc signed and not notarised, so a downloaded copy still needs
# the Finder's "Open" bypass on first launch.
set -eu

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
. "$root/packaging/lib/common.sh"

usage() {
    cat <<'EOF'
Turns dist/Strata.app (from bundle.sh) into the disk image the release
publishes: dist/Strata-<version>-macos-<arch>.dmg, with Strata.app and an
/Applications symlink so the image can be dragged into place.

    direnv exec . packaging/macos/dmg.sh
    packaging/macos/dmg.sh --target aarch64-apple-darwin

The app is ad hoc signed and not notarised, so a downloaded copy still needs
the Finder's "Open" bypass on first launch.

Options:
    --target <triple>  name the image after that target's architecture
EOF
}

target=
while [ $# -gt 0 ]; do
    case $1 in
    --target)
        target=${2:?--target needs a triple}
        shift 2
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    *)
        echo "dmg.sh: unknown argument: $1" >&2
        exit 2
        ;;
    esac
done

app="$root/dist/Strata.app"
version=$(strata_version "$root")
arch=$(strata_arch "$target")

if [ ! -d "$app" ]; then
    echo "dmg.sh: $app does not exist; run packaging/macos/bundle.sh first" >&2
    exit 1
fi
command -v hdiutil >/dev/null 2>&1 || {
    echo "dmg.sh: hdiutil not found; this script only runs on macOS" >&2
    exit 1
}

dmg="$root/dist/Strata-$version-macos-$arch.dmg"
staging=$(mktemp -d "$root/dist/dmg.XXXXXX")
trap 'rm -rf "$staging"' EXIT

# ditto rather than cp: it keeps the bundle's symlinks and metadata intact.
ditto "$app" "$staging/Strata.app"
ln -s /Applications "$staging/Applications"

# The bundle already carries these, but they are also worth seeing without
# opening it; this is what recipients expect of a licence notice.
for doc in LICENSE-MIT LICENSE-APACHE THIRD-PARTY-NOTICES.md; do
    if [ -f "$app/Contents/Resources/$doc" ]; then
        cp "$app/Contents/Resources/$doc" "$staging/$doc"
    fi
done

rm -f "$dmg"
hdiutil create -quiet -volname Strata -srcfolder "$staging" -ov -format UDZO "$dmg"

echo "wrote $dmg"
