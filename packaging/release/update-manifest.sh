#!/bin/sh
# Signs the release archives and writes latest.json, the manifest the app's
# updater reads to decide whether a newer release exists. The publish job runs
# this after every build artifact has landed in one directory and before
# SHA256SUMS.txt is written, so the sums cover the signatures and the manifest.
#
#     packaging/release/update-manifest.sh --key ~/lamina-update.key dist 0.1.7 v0.1.7
#
# Without --key nothing is signed and no manifest is written; that keeps forks
# and runs without the signing secret working.
set -eu

usage() {
    cat <<'EOF'
Signs the release archives and writes latest.json for the app's updater.

    packaging/release/update-manifest.sh --key ~/lamina-update.key dist 0.1.7 v0.1.7

Each archive is signed into <archive>.minisig, and latest.json is signed into
latest.json.minisig. A platform with no archive in the directory is left out of
the manifest.

Options:
    --key <file>  unencrypted minisign secret key, as made by `minisign -G -W`

Arguments:
    <dist-dir>    directory holding the archives to sign
    <version>     version without the tag prefix, e.g. 0.1.7
    <tag>         release tag, e.g. v0.1.7
EOF
}

key=
while [ $# -gt 0 ]; do
    case $1 in
    --key)
        key=${2:?--key needs a file}
        shift 2
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    -*)
        echo "update-manifest.sh: unknown argument: $1" >&2
        exit 2
        ;;
    *) break ;;
    esac
done

if [ $# -ne 3 ]; then
    usage >&2
    exit 2
fi
dist=$1
version=$2
tag=$3

if [ -z "$version" ]; then
    echo "update-manifest.sh: version is empty" >&2
    exit 1
fi
# The manifest names the version and links the tag, so a disagreement would
# point the updater at a release that does not match the files beside it.
if [ "$tag" != "v$version" ]; then
    echo "update-manifest.sh: tag $tag does not match version $version" >&2
    exit 1
fi
if [ ! -d "$dist" ]; then
    echo "update-manifest.sh: $dist is not a directory" >&2
    exit 1
fi
if [ -z "$key" ]; then
    echo "update-manifest.sh: no key given, leaving the assets unsigned and skipping latest.json" >&2
    exit 0
fi
if [ ! -f "$key" ]; then
    echo "update-manifest.sh: key file $key is missing" >&2
    exit 1
fi
if ! command -v minisign >/dev/null 2>&1; then
    echo "update-manifest.sh: minisign not found on PATH" >&2
    exit 1
fi

# sha256sum is on every Linux runner; macOS only ships shasum, and this script
# is meant to be run by hand there too.
sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

# Maps an archive name to its platform key in latest.json.
asset_key() {
    case $1 in
    Lamina-*-macos-arm64.dmg) echo macos-aarch64 ;;
    Lamina-*-windows-x64-setup.exe) echo windows-x86_64 ;;
    Lamina-*-windows-x64.zip) echo windows-x86_64-portable ;;
    lamina-*-linux-x86_64.tar.gz) echo linux-x86_64 ;;
    *) return 1 ;;
    esac
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
found="$tmp/found"
: >"$found"

for file in "$dist"/*; do
    [ -f "$file" ] || continue
    name=$(basename "$file")
    case $name in
    # Written next to the assets by this script or by the workflow; they are
    # never inputs to it.
    SHA256SUMS.txt | latest.json | *.minisig) continue ;;
    esac
    if ! asset=$(asset_key "$name"); then
        echo "update-manifest.sh: $name matches no asset pattern" >&2
        exit 1
    fi
    # An archive from another release would otherwise be signed and published
    # under this version, which the updater cannot make sense of.
    case $name in
    "Lamina-$version"-* | "lamina-$version"-*) ;;
    *)
        echo "update-manifest.sh: $name does not belong to version $version" >&2
        exit 1
        ;;
    esac
    printf '%s\t%s\n' "$asset" "$name" >>"$found"
done

if [ ! -s "$found" ]; then
    echo "update-manifest.sh: no release archives in $dist" >&2
    exit 1
fi

# Two archives for one platform would make the updater pick whichever happened
# to sort last, so refuse instead of guessing.
duplicate=$(cut -f1 "$found" | sort | uniq -d)
if [ -n "$duplicate" ]; then
    echo "update-manifest.sh: more than one asset for $duplicate:" >&2
    awk -F'\t' -v want="$duplicate" '$1 == want { print "  " $2 }' "$found" >&2
    exit 1
fi

notes_url="https://github.com/satotek/lamina/releases/tag/$tag"
assets='{}'
# Fixed order keeps the manifest stable between runs; a key with no archive is
# simply absent, which is how a platform without an installer drops out.
for asset in macos-aarch64 windows-x86_64 windows-x86_64-portable linux-x86_64; do
    name=$(awk -F'\t' -v want="$asset" '$1 == want { print $2 }' "$found")
    if [ -z "$name" ]; then
        echo "update-manifest.sh: no archive for $asset, leaving it out of latest.json" >&2
        continue
    fi

    minisign -S -q -s "$key" -t "file:$name version:$version" \
        -m "$dist/$name" -x "$dist/$name.minisig"

    entry=$(jq --indent 2 -n \
        --arg name "$name" \
        --arg sha256 "$(sha256 "$dist/$name")" \
        --rawfile minisig "$dist/$name.minisig" \
        '{name: $name, sha256: $sha256, minisig: $minisig}')
    assets=$(printf '%s' "$assets" | jq --arg key "$asset" --argjson entry "$entry" '. + {($key): $entry}')
done

jq --indent 2 -n \
    --arg version "$version" \
    --arg tag "$tag" \
    --arg notes_url "$notes_url" \
    --argjson assets "$assets" \
    '{version: $version, tag: $tag, notes_url: $notes_url, assets: $assets}' \
    >"$dist/latest.json"

# Signing the manifest covers the hashes and the asset signatures in one go, so
# the app can trust the whole document with a single verification.
minisign -S -q -s "$key" -t "file:latest.json version:$version" \
    -m "$dist/latest.json" -x "$dist/latest.json.minisig"

echo "wrote $dist/latest.json and its signature"
