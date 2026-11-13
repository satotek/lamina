#!/bin/sh
# Regenerates strata.ico from the SVGs. Every size is stored as PNG, which
# Windows has accepted since Vista and keeps the file a fraction of the BMP size.
set -eu
cd "$(dirname "$0")"
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT

render() { nix shell nixpkgs#resvg --command resvg -w "$2" -h "$2" "$1" "$out/$2.png"; }
render strata-16.svg 16
render strata-32.svg 24
render strata-32.svg 32
render strata-64.svg 48
render strata-64.svg 64
render strata.svg 128
render strata.svg 256

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
