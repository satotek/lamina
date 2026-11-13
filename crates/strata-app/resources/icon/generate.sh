#!/bin/sh
# Regenerates the platform icon files from the SVGs: strata.ico for Windows,
# Strata.icns for the macOS bundle and hicolor/*.png for Linux. All of them are
# checked in, so no build step needs an image tool; run this only after
# changing an SVG.
set -eu
cd "$(dirname "$0")"
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT

# resvg comes from Nix so every machine renders the same pixels. Each small
# size comes from the drawing made for it; for the intermediate sizes that
# have no drawing of their own, the next smaller one scales up cleanly enough
# at 2x scales.
render() { nix shell nixpkgs#resvg --command resvg -w "$2" -h "$2" "$1" "$out/$2.png"; }
render strata-16.svg 16
render strata-32.svg 24
render strata-32.svg 32
render strata-64.svg 48
render strata-64.svg 64
render strata.svg 128
render strata.svg 256
render strata.svg 512
render strata.svg 1024

# Every size is stored as PNG, which Windows has accepted since Vista and
# keeps the file a fraction of the BMP size.
python3 - "$out" <<'PY'
import struct, sys
sizes = [16, 24, 32, 48, 64, 128, 256]
images = [open(f"{sys.argv[1]}/{s}.png", "rb").read() for s in sizes]
header = struct.pack("<HHH", 0, 1, len(images))
offset = len(header) + 16 * len(images)
entries = b""
for size, data in zip(sizes, images):
    # A width or height of 0 means 256.
    entries += struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(data), offset)
    offset += len(data)
with open("strata.ico", "wb") as f:
    f.write(header + entries + b"".join(images))
PY

# iconutil ships with macOS only. The bundle uses the checked-in .icns, so a
# missing iconutil just means this one output has to be regenerated on a Mac.
if command -v iconutil >/dev/null 2>&1; then
    iconset="$out/Strata.iconset"
    mkdir -p "$iconset"
    cp "$out/16.png" "$iconset/icon_16x16.png"
    cp "$out/32.png" "$iconset/icon_16x16@2x.png"
    cp "$out/32.png" "$iconset/icon_32x32.png"
    cp "$out/64.png" "$iconset/icon_32x32@2x.png"
    cp "$out/128.png" "$iconset/icon_128x128.png"
    cp "$out/256.png" "$iconset/icon_128x128@2x.png"
    cp "$out/256.png" "$iconset/icon_256x256.png"
    cp "$out/512.png" "$iconset/icon_256x256@2x.png"
    cp "$out/512.png" "$iconset/icon_512x512.png"
    cp "$out/1024.png" "$iconset/icon_512x512@2x.png"
    iconutil -c icns -o Strata.icns "$iconset"
else
    echo "generate.sh: iconutil not found (macOS only); leaving Strata.icns as it is" >&2
fi

# Flat files rather than a hicolor tree: package.ps1 and install.sh copy them
# into the size-suffixed directories, and the app embeds one of them.
mkdir -p hicolor
for size in 16 32 48 64 128 256 512; do
    cp "$out/$size.png" "hicolor/$size.png"
done
