//! The art of the ham mascot, `crates/ham/src/tui/mascot.rs`: an original pixel drawing, orange, with two big
//! eyes that look up, a plain shirt and shorts, and a short rocket flame with grey smoke under each foot. It has no mouth, no logo and no
//! letter. The drawing exists at three sizes: the head (8 columns by 5 rows: the character, a flame under the legs and a puff of smoke, beside
//! the app name on a terminal of 33 rows or more), the short head (8 by 3, the character alone, for the band of a 31 or 32-row terminal) and the
//! large form (14 by 12, in the empty Runs table). Each size has a block look (half blocks in twelve named xterm colours) and an ASCII look
//! (no style). The large form and the head have three frames: the character and the flames never change, only the smoke moves.
//!
//! These tests decode what the module draws back to the pixel grids the art was designed on, so a change to a glyph, a tone letter or a
//! colour that moves one pixel fails here, and they pin the rules of the palette (contrast on a black and on a white terminal), of the
//! frames (what moves, how far) and of the glyph rows (no letter and no digit anywhere in the drawing).
#![allow(clippy::unwrap_used)]

use ham::tui::mascot::{self, Form, Look, PALETTE};
use ratatui::style::{Color, Style};
use ratatui::text::Line;

/// The pixel grids the art was designed on: one character per pixel, `.` is the terminal's own background.
/// O orange, W eye white, K pupil (ink), G shirt, T shorts, Y flame yellow, F flame orange, R flame red, D M L P the four greys of the smoke
/// from the darkest (next to the flame) to the palest (the tail).
const HEAD_GRID: [&str; 6] = [
    ".OOOOOO.", //
    "OKWOOKWO", //
    "OWWOOWWO", //
    "OGGGGGGO", //
    ".TT..TT.", //
    ".TT..TT.",
];

/// The flame under the legs of the head, pixel rows 6 and 7: an orange edge on each side of a yellow core, a red tip under the core. It never
/// changes.
const HEAD_FLAME_GRID: [&str; 2] = ["FYF..FYF", ".R....R."];

/// The smoke of the head, pixel rows 8 and 9, in each of the three frames. Frame 0 is the still picture. The first row stays; the second row
/// moves. The two puffs never touch (columns 3 and 4 are empty).
const HEAD_SMOKE_GRIDS: [[&str; 2]; 3] = [
    ["DM....MD", "LML..LML"],
    ["DM....MD", ".LM..ML."],
    ["DM....MD", "L......L"],
];

/// The whole head in a frame: 10 rows of 8 pixels.
fn head_grid(frame: usize) -> Vec<String> {
    HEAD_GRID
        .iter()
        .chain(HEAD_FLAME_GRID.iter())
        .chain(HEAD_SMOKE_GRIDS[frame].iter())
        .map(|r| (*r).to_string())
        .collect()
}

/// The character, rows 0 to 13 of the large form. It never changes.
const BODY_GRID: [&str; 14] = [
    "...OOOOOOOO...",
    "..OOOOOOOOOO..",
    ".OOKKOOOOKKOO.",
    ".OWKKWOOWKKWO.",
    ".OWWWWOOWWWWO.",
    ".OOWWOOOOWWOO.",
    ".OOOOOOOOOOOO.",
    "OGGGGGGGGGGGGO",
    "OGGGGGGGGGGGGO",
    "OGGGGGGGGGGGGO",
    ".GGGGGGGGGGGG.",
    ".TTTTTTTTTTTT.",
    ".TTTTT..TTTTT.",
    ".TTTT....TTTT.",
];

/// The flame under each foot, rows 14 to 17: a yellow core, an orange edge and a red tip. It never changes.
const FLAME_GRID: [&str; 4] = [
    ".FYYF....FYYF.",
    ".FYYF....FYYF.",
    "..FF......FF..",
    "..RR......RR..",
];

/// The smoke, rows 18 to 23, in each of the three frames. Frame 0 is the still picture of the README.
const SMOKE_GRIDS: [[&str; 6]; 3] = [
    [
        ".DMMD....DMMD.",
        ".MMLM....MMLM.",
        "MMLLMM..MMLMLM",
        ".MLLLM..MLLLM.",
        "..LML....MLL..",
        "...L......P...",
    ],
    [
        ".DMMD....DMMD.",
        ".MMLM....MMLM.",
        "MMLLMM..MMLMLM",
        "M.LLLM..MLLL.M",
        "..LM.L..M.LL..",
        "..L........P..",
    ],
    [
        ".DMMD....DMMD.",
        ".MMLM....MMLM.",
        "MMLLMM..MMLMLM",
        "..LLLM..MLLL..",
        ".MLM......LLM.",
        "...LL....MP...",
    ],
];

/// The whole large form in a frame: 24 rows of 14 pixels.
fn large_grid(frame: usize) -> Vec<String> {
    BODY_GRID
        .iter()
        .chain(FLAME_GRID.iter())
        .chain(SMOKE_GRIDS[frame].iter())
        .map(|r| (*r).to_string())
        .collect()
}

/// The letter of each palette colour in the grids.
const LETTERS: [(char, &str); 12] = [
    ('O', "orange"),
    ('W', "eyewhite"),
    ('K', "ink"),
    ('G', "shirt"),
    ('T', "shorts"),
    ('Y', "flame_yellow"),
    ('F', "flame_orange"),
    ('R', "flame_red"),
    ('D', "smoke_dark"),
    ('M', "smoke_mid"),
    ('L', "smoke_light"),
    ('P', "smoke_pale"),
];

/// Every size of the drawing.
const FORMS: [Form; 3] = [Form::Head, Form::HeadShort, Form::Large];

fn text_of(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn index_of(name: &str) -> u8 {
    PALETTE.iter().find(|(n, _, _)| *n == name).unwrap().1
}

/// The pixel letter of a colour, `.` for none.
fn letter(color: Option<Color>) -> char {
    match color {
        None => '.',
        Some(Color::Indexed(i)) => {
            LETTERS
                .iter()
                .find(|(_, name)| index_of(name) == i)
                .unwrap_or_else(|| panic!("colour {i} is not in the palette"))
                .0
        }
        Some(other) => panic!("{other:?} is not a named xterm colour"),
    }
}

/// The two pixel rows of every terminal row of the block look: `▀` is the foreground over the background, `▄` the reverse, `█` the
/// foreground twice, a space nothing.
fn decode_lines(lines: &[Line<'_>]) -> Vec<String> {
    let mut pixels = Vec::new();
    for line in lines {
        let (mut top, mut bottom) = (String::new(), String::new());
        for span in &line.spans {
            for glyph in span.content.chars() {
                let (fg, bg) = (span.style.fg, span.style.bg);
                let (t, b) = match glyph {
                    ' ' => (None, None),
                    '█' => (fg, fg),
                    '▀' => (fg, bg),
                    '▄' => (bg, fg),
                    other => panic!("{other:?} is not a block glyph"),
                };
                top.push(letter(t));
                bottom.push(letter(b));
            }
        }
        pixels.push(top);
        pixels.push(bottom);
    }
    pixels
}

fn decode(form: Form) -> Vec<String> {
    decode_lines(&mascot::lines(form, Look::Blocks))
}

fn decode_at(form: Form, frame: usize) -> Vec<String> {
    decode_lines(&mascot::lines_at(form, Look::Blocks, frame))
}

#[test]
fn the_short_head_decodes_to_its_pixel_grid() {
    assert_eq!(decode(Form::HeadShort), HEAD_GRID);
    // The short head has one frame.
    for frame in 0..mascot::FRAMES {
        assert_eq!(decode_at(Form::HeadShort, frame), HEAD_GRID);
    }
}

#[test]
fn the_head_decodes_to_its_pixel_grid_in_every_frame() {
    assert_eq!(decode(Form::Head), head_grid(0));
    for frame in 0..mascot::FRAMES {
        assert_eq!(
            decode_at(Form::Head, frame),
            head_grid(frame),
            "frame {frame}"
        );
        // The character is the short head, pixel for pixel.
        assert_eq!(
            decode_at(Form::Head, frame)[..6],
            HEAD_GRID,
            "frame {frame}"
        );
    }
}

#[test]
fn the_large_form_decodes_to_its_pixel_grid() {
    assert_eq!(decode(Form::Large), large_grid(0));
    assert_eq!(decode_at(Form::Large, 0), large_grid(0));
}

#[test]
fn the_other_two_frames_decode_to_their_pixel_grids() {
    assert_eq!(mascot::FRAMES, 3);
    for frame in 1..mascot::FRAMES {
        assert_eq!(
            decode_at(Form::Large, frame),
            large_grid(frame),
            "frame {frame}"
        );
    }
}

#[test]
fn the_head_rows_are_these_glyphs() {
    // `crates/ham-e2e` finds the head on a screen by its first three rows (`snapshot::head_present`), so they are pinned as text.
    assert_eq!(
        mascot::plain(Form::HeadShort, Look::Blocks),
        ["▄▀▀██▀▀▄", "█▀▀▀▀▀▀█", " ██  ██ "]
    );
    assert_eq!(
        mascot::plain(Form::HeadShort, Look::Ascii),
        [" ,----. ", "(@    @)", "(##  ##)"]
    );
    // The head: those three rows, then the flame, then the smoke of frame 0 (the second smoke row of the pixel grid decides the glyphs).
    assert_eq!(
        mascot::plain(Form::Head, Look::Blocks),
        ["▄▀▀██▀▀▄", "█▀▀▀▀▀▀█", " ██  ██ ", "▀▀▀  ▀▀▀", "▀█▄  ▄█▀"]
    );
    assert_eq!(
        mascot::plain(Form::Head, Look::Ascii),
        [" ,----. ", "(@    @)", "(##  ##)", "\\*/  \\*/", "%=~  ~=%"]
    );
}

#[test]
fn the_sizes_are_eight_by_five_eight_by_three_and_fourteen_by_twelve() {
    assert_eq!(mascot::HEAD_MAX, (5, 8));
    assert_eq!(mascot::HEAD_SHORT_MAX, (3, 8));
    assert_eq!(mascot::LARGE_MAX, (12, 14));
    for look in [Look::Blocks, Look::Ascii] {
        assert_eq!(mascot::size(Form::Head, look), (8, 5), "{look:?}");
        assert_eq!(mascot::size(Form::HeadShort, look), (8, 3), "{look:?}");
        assert_eq!(mascot::size(Form::Large, look), (14, 12), "{look:?}");
        for form in FORMS {
            for frame in 0..mascot::FRAMES {
                let widths: Vec<usize> = mascot::lines_at(form, look, frame)
                    .iter()
                    .map(Line::width)
                    .collect();
                assert!(
                    widths.windows(2).all(|w| w[0] == w[1]),
                    "{form:?} {look:?} frame {frame}: {widths:?}"
                );
                assert_eq!(widths.len(), usize::from(mascot::size(form, look).1));
            }
        }
    }
}

#[test]
fn no_letter_and_no_digit_is_drawn_in_any_look() {
    // The mascot carries no text, in the block rows or in the ASCII rows (the flame and the smoke included): the eyes of the ASCII look are
    // `@`, not `o`.
    for form in FORMS {
        for look in [Look::Blocks, Look::Ascii] {
            for frame in 0..mascot::FRAMES {
                for row in mascot::plain_at(form, look, frame) {
                    for c in row.chars() {
                        assert!(
                            !c.is_alphanumeric(),
                            "{form:?} {look:?} frame {frame}: {c:?} in {row:?} is a letter or a digit"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn the_block_look_uses_only_single_width_half_blocks() {
    for form in FORMS {
        for frame in 0..mascot::FRAMES {
            let lines = mascot::lines_at(form, Look::Blocks, frame);
            let all: String = lines.iter().map(text_of).collect();
            for c in all.chars() {
                assert!(
                    matches!(c, ' ' | '▀' | '▄' | '█'),
                    "{form:?}: {c:?} (U+{:04X}) is not allowed",
                    u32::from(c)
                );
            }
            for line in &lines {
                assert_eq!(
                    line.width(),
                    text_of(line).chars().count(),
                    "{form:?}: a character is not one cell wide"
                );
            }
        }
    }
}

#[test]
fn the_ascii_look_is_plain_ascii_with_no_style() {
    for form in FORMS {
        for frame in 0..mascot::FRAMES {
            let lines = mascot::lines_at(form, Look::Ascii, frame);
            for line in &lines {
                assert!(
                    text_of(line)
                        .chars()
                        .all(|c| c == ' ' || c.is_ascii_graphic()),
                    "{form:?}: {:?}",
                    text_of(line)
                );
                assert_eq!(line.style, Style::default());
                for span in &line.spans {
                    assert_eq!(span.style, Style::default(), "{form:?}");
                }
            }
            assert!(
                mascot::plain_at(form, Look::Ascii, frame)
                    .join("")
                    .contains('@'),
                "{form:?}: the eyes are drawn as @"
            );
        }
        // The size of the ASCII twin is the size of the block form.
        assert_eq!(
            mascot::size(form, Look::Ascii),
            mascot::size(form, Look::Blocks)
        );
    }
}

#[test]
fn the_block_look_names_every_colour_so_no_theme_changes_them() {
    let palette: Vec<u8> = PALETTE.iter().map(|(_, index, _)| *index).collect();
    for form in FORMS {
        for frame in 0..mascot::FRAMES {
            let coloured = mascot::lines_at(form, Look::Blocks, frame)
                .iter()
                .flat_map(|l| l.spans.iter())
                .filter(|s| s.style.fg.is_some())
                .count();
            assert!(coloured >= 5, "{form:?} has {coloured} coloured spans");
            for line in mascot::lines_at(form, Look::Blocks, frame) {
                for span in &line.spans {
                    for c in [span.style.fg, span.style.bg].into_iter().flatten() {
                        let Color::Indexed(i) = c else {
                            panic!("{form:?}: {span:?} is not a named xterm colour")
                        };
                        assert!(palette.contains(&i), "{form:?}: {i} is not in the palette");
                        assert!(i >= 16, "{form:?}: {i} is one of the 16 theme colours");
                    }
                }
            }
        }
    }
}

// ---- the frames -------------------------------------------------------------------------------------------------------------------

/// The pixels that differ between two frames of the large form: (row, column, letter before, letter after).
fn changes(from: usize, to: usize) -> Vec<(usize, usize, char, char)> {
    let (a, b) = (decode_at(Form::Large, from), decode_at(Form::Large, to));
    let mut out = Vec::new();
    for (y, (ra, rb)) in a.iter().zip(&b).enumerate() {
        for (x, (ca, cb)) in ra.chars().zip(rb.chars()).enumerate() {
            if ca != cb {
                out.push((y, x, ca, cb));
            }
        }
    }
    out
}

/// The first row of the smoke.
const SMOKE_FIRST_ROW: usize = 18;

#[test]
fn only_the_smoke_changes_from_one_frame_to_the_next() {
    for from in 0..mascot::FRAMES {
        let to = (from + 1) % mascot::FRAMES;
        let diff = changes(from, to);
        assert!(!diff.is_empty(), "frames {from} and {to} are the same");
        for (y, x, _, _) in &diff {
            assert!(
                *y >= SMOKE_FIRST_ROW,
                "frames {from} to {to}: the pixel ({x},{y}) is not smoke"
            );
        }
    }
    // The character and the flames are the same pixels in every frame.
    for frame in 1..mascot::FRAMES {
        assert_eq!(
            decode_at(Form::Large, frame)[..SMOKE_FIRST_ROW],
            decode_at(Form::Large, 0)[..SMOKE_FIRST_ROW],
            "frame {frame}"
        );
    }
    // The first three rows of the smoke, next to the flames, are the same too: the puff stays fastened to the flame.
    for frame in 1..mascot::FRAMES {
        assert_eq!(
            decode_at(Form::Large, frame)[..SMOKE_FIRST_ROW + 3],
            decode_at(Form::Large, 0)[..SMOKE_FIRST_ROW + 3],
            "frame {frame}"
        );
    }
}

#[test]
fn the_two_plumes_never_touch() {
    // The middle two columns of the picture are the gap between the plumes: empty in every frame, so a frame never joins the two puffs.
    for frame in 0..mascot::FRAMES {
        for (y, row) in decode_at(Form::Large, frame)
            .iter()
            .enumerate()
            .skip(SMOKE_FIRST_ROW)
        {
            assert_eq!(&row[6..8], "..", "frame {frame} row {y}: {row}");
        }
    }
}

#[test]
fn each_step_moves_four_to_eight_smoke_pixels_by_one_cell() {
    for from in 0..mascot::FRAMES {
        let to = (from + 1) % mascot::FRAMES;
        let diff = changes(from, to);
        // A pixel that moves empties one place and fills another: 4 to 8 pixels are 8 to 16 places.
        assert!(
            (8..=16).contains(&diff.len()),
            "frames {from} to {to} change {} places",
            diff.len()
        );
        let gained: Vec<_> = diff.iter().filter(|(_, _, a, _)| *a == '.').collect();
        let mut lost: Vec<_> = diff.iter().filter(|(_, _, _, b)| *b == '.').collect();
        assert_eq!(
            gained.len() + lost.len(),
            diff.len(),
            "frames {from} to {to}: a pixel changes colour in place"
        );
        assert_eq!(gained.len(), lost.len(), "frames {from} to {to}");
        for (y, x, _, shade) in gained {
            let at = lost.iter().position(|(ly, lx, was, _)| {
                was == shade && ly.abs_diff(*y) <= 1 && lx.abs_diff(*x) <= 1
            });
            let at = at.unwrap_or_else(|| {
                panic!("frames {from} to {to}: the pixel ({x},{y}) did not move one cell")
            });
            lost.remove(at);
        }
    }
}

#[test]
fn a_frame_number_past_the_last_wraps_and_the_short_head_has_one_frame() {
    for look in [Look::Blocks, Look::Ascii] {
        for frame in 0..mascot::FRAMES {
            for form in [Form::Large, Form::Head] {
                assert_eq!(
                    mascot::plain_at(form, look, frame + mascot::FRAMES),
                    mascot::plain_at(form, look, frame),
                    "{form:?} {look:?}"
                );
            }
            assert_eq!(
                mascot::plain_at(Form::HeadShort, look, frame),
                mascot::plain(Form::HeadShort, look),
                "{look:?}"
            );
        }
        assert_eq!(
            mascot::plain(Form::Head, look),
            mascot::plain_at(Form::Head, look, 0)
        );
        assert_eq!(
            mascot::plain(Form::Large, look),
            mascot::plain_at(Form::Large, look, 0)
        );
    }
}

#[test]
fn the_ascii_look_moves_the_same_smoke_and_nothing_else() {
    // Rows 0 to 8 (the character and the flames) are the same text in every frame; the three rows of smoke change from each frame to the next.
    let smoke_cells = usize::from(mascot::size(Form::Large, Look::Ascii).1) - SMOKE_FIRST_ROW / 2;
    assert_eq!(smoke_cells, 3);
    let still = SMOKE_FIRST_ROW / 2;
    for from in 0..mascot::FRAMES {
        let to = (from + 1) % mascot::FRAMES;
        let (a, b) = (
            mascot::plain_at(Form::Large, Look::Ascii, from),
            mascot::plain_at(Form::Large, Look::Ascii, to),
        );
        assert_eq!(a[..still], b[..still], "frames {from} and {to}");
        assert_ne!(a[still..], b[still..], "frames {from} and {to}");
    }
}

#[test]
fn the_ascii_look_draws_a_flame_under_each_foot_and_smoke_below_it() {
    let rows = mascot::plain(Form::Large, Look::Ascii);
    // The flames stand under the feet, which are columns 1 to 4 and 9 to 12 of the pixel grid.
    for row in &rows[7..9] {
        assert!(!row.trim().is_empty(), "no flame in {row:?}");
        assert!(row.chars().skip(5).take(4).all(|c| c == ' '), "{row:?}");
    }
    for row in &rows[9..] {
        assert!(!row.trim().is_empty(), "no smoke in {row:?}");
    }
}

// ---- the frames of the head ---------------------------------------------------------------------------------------------------------

/// The pixels that differ between two frames of the head: (row, column, letter before, letter after).
fn head_changes(from: usize, to: usize) -> Vec<(usize, usize, char, char)> {
    let (a, b) = (decode_at(Form::Head, from), decode_at(Form::Head, to));
    let mut out = Vec::new();
    for (y, (ra, rb)) in a.iter().zip(&b).enumerate() {
        for (x, (ca, cb)) in ra.chars().zip(rb.chars()).enumerate() {
            if ca != cb {
                out.push((y, x, ca, cb));
            }
        }
    }
    out
}

/// The last pixel row of the head: the only one that moves.
const HEAD_MOVING_ROW: usize = 9;

#[test]
fn only_the_last_smoke_row_of_the_head_moves() {
    for from in 0..mascot::FRAMES {
        let to = (from + 1) % mascot::FRAMES;
        let diff = head_changes(from, to);
        assert!(!diff.is_empty(), "frames {from} and {to} are the same");
        for (y, x, _, _) in &diff {
            assert_eq!(
                *y, HEAD_MOVING_ROW,
                "frames {from} to {to}: the pixel ({x},{y}) is not on the moving row"
            );
        }
        // A step changes four to eight places, so the puff changes mildly.
        assert!(
            (4..=8).contains(&diff.len()),
            "frames {from} to {to} change {} places",
            diff.len()
        );
    }
}

#[test]
fn the_two_puffs_of_the_head_never_touch() {
    for frame in 0..mascot::FRAMES {
        for (y, row) in decode_at(Form::Head, frame).iter().enumerate().skip(6) {
            assert_eq!(&row[3..5], "..", "frame {frame} row {y}: {row}");
        }
    }
}

#[test]
fn each_frame_of_the_head_reads_differently_in_both_looks() {
    // The rows the user (and a test that reads the screen as text) can tell apart: the last row of the head differs from frame to frame in
    // the glyphs of the block look and in the marks of the ASCII look.
    for look in [Look::Blocks, Look::Ascii] {
        let last: Vec<String> = (0..mascot::FRAMES)
            .map(|f| mascot::plain_at(Form::Head, look, f)[4].clone())
            .collect();
        for i in 0..last.len() {
            for j in (i + 1)..last.len() {
                assert_ne!(last[i], last[j], "{look:?}: frames {i} and {j}");
            }
        }
        // The first four rows are the same text in every frame.
        for frame in 1..mascot::FRAMES {
            assert_eq!(
                mascot::plain_at(Form::Head, look, frame)[..4],
                mascot::plain_at(Form::Head, look, 0)[..4],
                "{look:?} frame {frame}"
            );
        }
    }
}

#[test]
fn the_ascii_head_has_a_flame_under_each_leg_and_smoke_below_it() {
    let rows = mascot::plain(Form::Head, Look::Ascii);
    for row in &rows[3..] {
        assert!(!row.trim().is_empty(), "nothing in {row:?}");
        assert!(row.chars().skip(3).take(2).all(|c| c == ' '), "{row:?}");
    }
}

#[test]
fn the_head_flame_core_is_enclosed_and_the_head_carries_no_mark_of_the_eyes_below_them() {
    // Like the large form: the yellow core never touches the terminal's background, in any frame.
    for frame in 0..mascot::FRAMES {
        assert_enclosed(&head_grid(frame), &['Y']);
        for row in &head_grid(frame)[6..] {
            assert!(!row.contains('K') && !row.contains('W'), "{row:?}");
        }
    }
}

// ---- the palette ------------------------------------------------------------------------------------------------------------------

/// The xterm-256 colour cube and grey ramp as sRGB.
fn rgb(index: u8) -> (u8, u8, u8) {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match index {
        16..=231 => {
            let n = usize::from(index - 16);
            (LEVELS[n / 36], LEVELS[(n / 6) % 6], LEVELS[n % 6])
        }
        232..=255 => {
            let v = 8 + 10 * (index - 232);
            (v, v, v)
        }
        _ => panic!("only the cube and the grey ramp are used, got {index}"),
    }
}

/// WCAG relative luminance.
fn luminance((r, g, b): (u8, u8, u8)) -> f64 {
    let lin = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

fn colour(name: &str) -> (u8, u8, u8) {
    rgb(index_of(name))
}

#[test]
fn the_palette_has_twelve_named_colours_and_the_hex_of_each_index() {
    let names: Vec<&str> = PALETTE.iter().map(|(n, _, _)| *n).collect();
    assert_eq!(
        names,
        [
            "orange",
            "eyewhite",
            "ink",
            "shirt",
            "shorts",
            "flame_yellow",
            "flame_orange",
            "flame_red",
            "smoke_dark",
            "smoke_mid",
            "smoke_light",
            "smoke_pale"
        ]
    );
    let mut indexes: Vec<u8> = PALETTE.iter().map(|(_, i, _)| *i).collect();
    indexes.sort_unstable();
    indexes.dedup();
    assert_eq!(indexes.len(), 12, "two colours share an index");
    for (name, index, hex) in PALETTE {
        let (r, g, b) = rgb(index);
        assert_eq!(hex, format!("#{r:02x}{g:02x}{b:02x}"), "{name}");
    }
    // Every colour of the palette is in the still picture, and every letter of the grids names one. The head, with its flame and its smoke,
    // draws all of them but the palest grey, which is the tail of the large form's puff.
    let head_drawn: String = head_grid(0).concat();
    for (letter, name) in LETTERS {
        if letter != 'P' {
            assert!(head_drawn.contains(letter), "{name} is not in the head");
        }
    }
    let drawn: String = large_grid(0).concat();
    for (letter, name) in LETTERS {
        assert!(drawn.contains(letter), "{name} is not in the still picture");
    }
    assert!(
        drawn
            .chars()
            .all(|c| c == '.' || LETTERS.iter().any(|(l, _)| *l == c)),
        "a letter of the grid names no colour"
    );
}

#[test]
fn the_orange_is_the_one_nearest_the_picture_that_still_reads() {
    // Decided with the numbers in the build record: xterm 208 is 9.3 CIE76 units from the picture's orange, 202 is 27.3. Changing it is a
    // colour decision, so this pin fails and says so.
    assert_eq!(index_of("orange"), 208, "the orange is a decided colour");
}

#[test]
fn the_flame_and_the_smoke_are_the_xterm_colours_nearest_the_picture() {
    // The picture's flame is #ffd700, #ff5f00 and #d70000 and its smoke #6e6e6e, #8c8c8c, #b0b0b0 and #d4d4d4. The first three are
    // colours of the xterm cube; the greys are the nearest steps of the xterm grey ramp (a step of the ramp is 10 levels).
    for (name, index, hex) in [
        ("flame_yellow", 220, "#ffd700"),
        ("flame_orange", 202, "#ff5f00"),
        ("flame_red", 160, "#d70000"),
        ("smoke_dark", 242, "#6c6c6c"),
        ("smoke_mid", 245, "#8a8a8a"),
        ("smoke_light", 249, "#b2b2b2"),
        ("smoke_pale", 252, "#d0d0d0"),
    ] {
        assert_eq!(index_of(name), index, "{name}");
        let (_, _, h) = PALETTE.iter().find(|(n, _, _)| *n == name).unwrap();
        assert_eq!(*h, hex, "{name}");
    }
}

#[test]
fn every_colour_that_borders_the_background_reads_on_a_black_and_on_a_white_terminal() {
    let black = (0, 0, 0);
    let white = (255, 255, 255);
    // The decorative orange body: 3:1 on black, 2:1 on white (8.7 and 2.4). Compared unrounded.
    assert!(contrast(colour("orange"), black) >= 3.0);
    assert!(contrast(colour("orange"), white) >= 2.0);
    // The shirt and the shorts keep 3:1 on both (shirt 3.3 and 6.4, shorts 6.5 and 3.3).
    for name in ["shirt", "shorts"] {
        assert!(contrast(colour(name), black) >= 3.0, "{name} on black");
        assert!(contrast(colour(name), white) >= 3.0, "{name} on white");
    }
    // The flame's edge and tip border the background and keep 3:1 on both (orange 6.9 and 3.0, red 3.9 and 5.4).
    for name in ["flame_orange", "flame_red"] {
        assert!(contrast(colour(name), black) >= 3.0, "{name} on black");
        assert!(contrast(colour(name), white) >= 3.0, "{name} on white");
    }
    // The two darker greys of the smoke keep 3:1 on both (dark 4.0 and 5.2, mid 6.1 and 3.5).
    for name in ["smoke_dark", "smoke_mid"] {
        assert!(contrast(colour(name), black) >= 3.0, "{name} on black");
        assert!(contrast(colour(name), white) >= 3.0, "{name} on white");
    }
    // The two lighter greys are the fading end of the puff, decorative like the orange body: 3:1 on black, and on white at least 2:1 for the
    // light grey (9.9 and 2.1) and 1.5:1 for the palest, a single tail pixel (13.6 and 1.5).
    assert!(contrast(colour("smoke_light"), black) >= 3.0);
    assert!(contrast(colour("smoke_light"), white) >= 2.0);
    assert!(contrast(colour("smoke_pale"), black) >= 3.0);
    assert!(contrast(colour("smoke_pale"), white) >= 1.5);
}

#[test]
fn the_flame_core_reads_against_the_flame_it_sits_in() {
    // The yellow core never touches the terminal's background (every yellow pixel is enclosed, in every frame), so it is judged against the
    // orange edge (2.2:1), like the eye white against the body.
    assert!(contrast(colour("flame_yellow"), colour("flame_orange")) >= 2.0);
    for frame in 0..mascot::FRAMES {
        let grid = large_grid(frame);
        assert_enclosed(&grid, &['Y']);
    }
}

#[test]
fn the_eyes_read_against_the_orange_they_sit_in() {
    // Eye white and ink never touch the terminal's background (every white and ink pixel is enclosed), so they are judged on the orange.
    assert!(contrast(colour("eyewhite"), colour("orange")) >= 2.0);
    assert!(contrast(colour("ink"), colour("orange")) >= 4.5);
    assert_enclosed(&HEAD_GRID.map(String::from), &['W', 'K']);
    assert_enclosed(&large_grid(0), &['W', 'K']);
}

/// Every pixel of these letters has four neighbours that are drawn.
fn assert_enclosed(grid: &[String], letters: &[char]) {
    let h = grid.len();
    let w = grid[0].len();
    for (y, row) in grid.iter().enumerate() {
        for (x, c) in row.chars().enumerate() {
            if !letters.contains(&c) {
                continue;
            }
            for (dx, dy) in [(0i32, 1i32), (0, -1), (1, 0), (-1, 0)] {
                let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                assert!(
                    nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h,
                    "a pixel {c} ({x},{y}) touches the edge of the grid"
                );
                assert_ne!(
                    grid[ny as usize].as_bytes()[nx as usize],
                    b'.',
                    "a pixel {c} ({x},{y}) touches the terminal background"
                );
            }
        }
    }
}

#[test]
fn the_drawing_has_no_mouth() {
    // The face below the eyes is plain orange: no ink and no eye white below the eye rows, in either size.
    for (grid, eye_rows_end, rows) in [(&HEAD_GRID[..], 3, 6), (&BODY_GRID[..], 6, 14)] {
        for row in &grid[eye_rows_end..rows] {
            assert!(
                !row.contains('K') && !row.contains('W'),
                "a mark below the eyes: {row:?}"
            );
        }
    }
    // Nor does the flame or the smoke carry a mark of the eyes.
    for frame in 0..mascot::FRAMES {
        for row in &large_grid(frame)[14..] {
            assert!(!row.contains('K') && !row.contains('W'), "{row:?}");
        }
    }
}
