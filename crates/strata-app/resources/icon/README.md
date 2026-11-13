# Application icon

`strata.svg` is the master. The small sizes are separate drawings, not
downscales: `strata-64.svg` keeps one commit, and `strata-32.svg` /
`strata-16.svg` drop the history line so the bands stay legible.

`strata.ico` is generated from them and checked in, so a Windows build needs
no image tools. Run `./generate.sh` after changing an SVG.
