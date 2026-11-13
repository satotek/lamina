# Application icon

`strata.svg` is the master. The small sizes are separate drawings, not
downscales: `strata-64.svg` keeps one commit, and `strata-32.svg` /
`strata-16.svg` drop the history line so the bands stay legible.

`strata.ico` (Windows), `Strata.icns` (macOS bundle) and `hicolor/*.png`
(Linux, and the X11 window icon embedded in the app) are generated from them
and checked in, so no build step needs an image tool. Run `./generate.sh` after
changing an SVG; it renders with `resvg` from Nix and needs macOS for the
`iconutil` step that writes the `.icns`.

`wordmark.py` regenerates `assets/icons/wordmark.svg`, the title bar's "STRATA"
wordmark, as outlines so it looks the same on every platform. Its docstring
has the command.
