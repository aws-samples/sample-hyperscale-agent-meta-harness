# docs/images

## Purpose

The pictures that `README.md` shows: the architecture picture under "Architecture" (`architecture.svg`, with `architecture.png` as the fallback for viewers that do not draw the SVG) and the ham mascot above the title (`ham-mascot.svg`). `SOURCES.md` records where the architecture icons come from and how those files were made.

## Ownership

Owned by `docs/AGENTS.md`. The architecture picture is built with the official AWS Architecture Icons. Their terms of use apply to those files, and `SOURCES.md` names the icon package, its release and the icons used. The architecture SVG is generated, so nobody edits it by hand. The ham mascot is artwork of this project and uses no AWS icon: it is an original pixel drawing with no logo or text.

## Local Contracts

- `architecture.svg` is one self-contained file: every icon is embedded, so it renders on GitLab and GitHub with no other file.
- The numbers in the picture match the numbered steps under it in `README.md` ("How it works in 8 steps"). A change to the picture and a change to those steps happen in the same commit.
- The icons stay unaltered. Group colours and service names follow the official package.
- `architecture.png` is rendered from `architecture.svg`. A new SVG comes with a new PNG, and the byte counts and sha256 values in `SOURCES.md` change with them.
- No secret, no account id and no endpoint URL appears in the picture.
- `ham-mascot.svg` is the ham mascot of the terminal app (`crates/ham/src/tui/mascot.rs`, the large form, frame 0) as an SVG of plain rectangles, 14 by 24 units, one unit for one pixel of the terminal drawing: the character in the first 14 rows, a short flame under each foot in rows 14 to 17 and grey smoke in rows 18 to 23. It is an original pixel drawing with no logo or text: it is not copied or traced from any existing artwork. It uses exactly the twelve colours of `PALETTE` in `mascot.rs` (`#ff8700` orange, `#ffffff` eye white, `#000000` pupils, `#5f5f5f` shirt, `#af875f` shorts, `#ffd700`, `#ff5f00` and `#d70000` for the flame, `#6c6c6c`, `#8a8a8a`, `#b2b2b2` and `#d0d0d0` for the smoke), so it reads on a light and on a dark page. It is at most 4096 bytes, has the `<title>` "The ham mascot, rocket flame and smoke" and a `<desc>` that says it has no mouth and names the flame and the smoke, and holds no script, style sheet, font, text, image, curve or reference to another file or address (its one address is the XML namespace). `crates/ham/tests/mascot_svg.rs` rasterises its paths and compares them with the large form pixel for pixel, and enforces the rest. It is a still picture: the smoke moves only in the terminal app. There is no PNG of it in the repository. A change to the art changes `mascot.rs`, this file and the user guide's section "The mascot" together.

## Work Guidance

- Regenerate the architecture SVG with the steps in `SOURCES.md`, then render the PNG from it, and replace both files together.
- Look at the rendered PNG before committing: no clipped text, no overlapping labels, every arrow readable. For the ham mascot, render the SVG once on a white and once on a dark background and look at both.

## Verification

```bash
python3 scripts/dox_lint.py
cargo nextest run -p ham --test mascot_svg
```

## Child DOX Index

None.
