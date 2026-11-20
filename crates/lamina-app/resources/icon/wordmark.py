"""Regenerates assets/icons/wordmark.svg, the title bar's "LAMINA" wordmark.

The wordmark is outlines, not text, so it looks the same on every platform
whatever fonts are installed. Run from this directory:

    nix shell --impure --expr 'with import (builtins.getFlake "nixpkgs") {}; python3.withPackages (p: [ p.fonttools ])' \
      --command python3 -I wordmark.py "$(nix build --no-link --print-out-paths nixpkgs#inter)/share/fonts/truetype/InterVariable.ttf" \
      > ../../assets/icons/wordmark.svg
"""

import sys

from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont

TEXT = "LAMINA"
WEIGHT = 600
TRACKING_EM = 0.2
# Round letters (the S) overshoot the cap height and baseline by about this much.
OVERSHOOT = 30

font = instantiateVariableFont(TTFont(sys.argv[1]), {"wght": WEIGHT, "opsz": 14})
glyphs = font.getGlyphSet()
cmap = font.getBestCmap()
cap = font["OS/2"].sCapHeight
tracking = TRACKING_EM * font["head"].unitsPerEm

x, paths = 0, []
for index, char in enumerate(TEXT):
    name = cmap[ord(char)]
    pen = SVGPathPen(glyphs, ntos=lambda v: str(round(v)))
    # SVG's y axis points down; flip so the cap height is at the top.
    glyphs[name].draw(TransformPen(pen, (1, 0, 0, -1, x, cap)))
    paths.append(pen.getCommands())
    x += glyphs[name].width + (tracking if index < len(TEXT) - 1 else 0)

width, height = round(x), cap + 2 * OVERSHOOT
print(
    f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 {-OVERSHOOT} {width} {height}" '
    f'width="{width * 13 / height:.0f}" height="13">'
    f'<path fill="currentColor" d="{" ".join(paths)}"/></svg>'
)
