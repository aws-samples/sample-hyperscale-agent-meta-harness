//! The ham mascot: a small orange character of the terminal app, an original pixel drawing with no logo or text. It has two big eyes that
//! look up, a plain dark shirt with small arms and light shorts, and no mouth. "ham" is the name of the app; the character is its face. In the
//! empty Runs table it stands like a rocket, with a short flame under each foot and grey smoke below the flames, and so does its head in the
//! title band of a tall terminal.
//!
//! - Three sizes: [`Form::Head`] (8 columns by 5 rows: the character's head and shirt, a flame under its legs and a puff of smoke), which
//!   stands beside the app name in the title band of a terminal of 33 rows or more (`tui/head.rs`) and is the fallback in an empty Runs table
//!   that has no room for the large form; [`Form::HeadShort`] (8 by 3: the same character without flame and smoke), the band of a 31 or
//!   32-row terminal and the fallback of a table too short for the head; and [`Form::Large`] (14 columns by 12 rows: the character, then the
//!   flames, then the smoke), which fills the empty Runs table ([`draw_runs_empty`]).
//! - Two looks: [`Look::Blocks`] draws half blocks (`▀ ▄ █`) in the twelve colours of [`PALETTE`]; [`Look::Ascii`] is plain ASCII with no style
//!   at all, for `NO_COLOR`, `TERM=dumb` and a locale that is not UTF-8 ([`look_from_env`]). Both looks have the same size and the same
//!   cells, so the choice never moves anything. No look draws a letter or a digit: the eyes of the ASCII look are `@`.
//! - Three frames of the large form and of the head ([`FRAMES`]): the character and the flames never change, only the smoke moves, and
//!   mildly. Each frame of the large form moves four to six smoke pixels by one cell from the one before (a puff drifts, a tail pixel moves on),
//!   each frame of the head changes four to six pixels of its last pixel row, and the last frame leads back to the first. Frame 0 is the still
//!   picture of the README. The frame is an argument of the draw: the app counts the ticks of its timer and picks it (`App::mascot_frame`) for
//!   the band and the picture alike, so they move in step, and `ui.mascot_motion = false` keeps it at 0. The ASCII look moves the same smoke.
//!   The short head has one frame.
//! - The head's flame is three pixels wide under each leg: an orange edge on each side of a yellow core, and a red tip under the core, so the
//!   core is enclosed like the large form's. Its smoke is two pixel rows: the first (dark and mid grey) is fastened to the flame, the second
//!   (light and mid grey) moves. The two puffs never touch. The head draws eleven of the twelve colours; the palest grey is the tail of the
//!   large form's puff.
//! - Colours: twelve xterm-256 indexes, so no terminal theme changes them and none depends on a 24-bit terminal. The orange is xterm 208
//!   (`#ff8700`), the one nearest the orange of the picture the character follows (CIE76 distance 9.3, against 27.3 for xterm 202) that still
//!   reads on a black and on a white terminal: 8.7:1 on black and 2.4:1 on white (the decorative body), where the shirt (`#5f5f5f`, 3.3:1 and
//!   6.4:1) and the shorts (`#af875f`, 6.5:1 and 3.3:1) keep 3:1 on both. The eye white and the ink sit inside the orange and are judged
//!   against it. The flame is xterm 202 (`#ff5f00`, 6.9:1 on black and 3.0:1 on white) at the edge and xterm 160 (`#d70000`, 3.9:1 and 5.4:1)
//!   at the tip, both at 3:1 or better on both backgrounds; its yellow core, xterm 220 (`#ffd700`), is enclosed by the edge and judged against it
//!   (2.2:1). The smoke is four steps of the xterm grey ramp: 242 (`#6c6c6c`, 4.0:1 on black and 5.2:1 on white) and 245 (`#8a8a8a`, 6.1:1 and
//!   3.5:1) keep 3:1 on both, and 249 (`#b2b2b2`, 9.9:1 and 2.1:1) and 252 (`#d0d0d0`, 13.6:1 and 1.5:1) are the fading end of the puff, decorative
//!   like the orange body, so the palest tail pixel is the faintest on a white terminal. A test computes all of this ([`PALETTE`]).
//! - Pure: the drawing, the choice of look from an environment ([`look_from_env`], given the environment as a function) and the placement take
//!   their input as arguments, and the look reaches a draw as a value of the app (`head.rs`), never from the process. `docs/images/ham-mascot.svg`
//!   is frame 0 of the large form for the README, pixel for pixel (a test compares them).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// The two sizes of the drawing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// [`HEAD_MAX`]: the head with a flame and a puff of smoke, for the title band of a terminal of 33 rows or more and for a short empty table.
    Head,
    /// [`HEAD_SHORT_MAX`]: the head alone, for the title band of a 31 or 32-row terminal and for an empty table too short for [`Form::Head`].
    HeadShort,
    /// [`LARGE_MAX`]: the whole character with its flames and smoke, for the empty Runs table.
    Large,
}

/// How the mascot is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    /// Half blocks in the twelve colours of [`PALETTE`].
    Blocks,
    /// Plain ASCII with no style: `NO_COLOR`, `TERM=dumb`, a locale that is not UTF-8, or `ui.mascot = ascii`.
    Ascii,
}

/// The size of the head with its flame and smoke: rows, columns.
pub const HEAD_MAX: (u16, u16) = (5, 8);
/// The size of the short head: rows, columns.
pub const HEAD_SHORT_MAX: (u16, u16) = (3, 8);
/// The size of the large form: rows, columns.
pub const LARGE_MAX: (u16, u16) = (12, 14);
/// The frames of the large form. Frame 0 is the still picture; a frame number past the last wraps.
pub const FRAMES: usize = 3;

/// Empty rows kept above and below the picture in the empty table, and empty columns kept on each side.
const MARGIN_ROWS: u16 = 1;
const MARGIN_COLS: u16 = 2;

/// The colours as (name, xterm-256 index, the same colour as hex). The README's SVG uses the hex. The orange, the shirt, the shorts, the flame's
/// edge and tip and the smoke border the terminal's background; the eye white, the ink and the flame's core sit inside the orange.
pub const PALETTE: [(&str, u8, &str); 12] = [
    ("orange", 208, "#ff8700"),
    ("eyewhite", 231, "#ffffff"),
    ("ink", 16, "#000000"),
    ("shirt", 59, "#5f5f5f"),
    ("shorts", 137, "#af875f"),
    ("flame_yellow", 220, "#ffd700"),
    ("flame_orange", 202, "#ff5f00"),
    ("flame_red", 160, "#d70000"),
    ("smoke_dark", 242, "#6c6c6c"),
    ("smoke_mid", 245, "#8a8a8a"),
    ("smoke_light", 249, "#b2b2b2"),
    ("smoke_pale", 252, "#d0d0d0"),
];
const ORANGE: Color = Color::Indexed(PALETTE[0].1);
const EYEWHITE: Color = Color::Indexed(PALETTE[1].1);
const INK: Color = Color::Indexed(PALETTE[2].1);
const SHIRT: Color = Color::Indexed(PALETTE[3].1);
const SHORTS: Color = Color::Indexed(PALETTE[4].1);
const FLAME_YELLOW: Color = Color::Indexed(PALETTE[5].1);
const FLAME_ORANGE: Color = Color::Indexed(PALETTE[6].1);
const FLAME_RED: Color = Color::Indexed(PALETTE[7].1);
const SMOKE_DARK: Color = Color::Indexed(PALETTE[8].1);
const SMOKE_MID: Color = Color::Indexed(PALETTE[9].1);
const SMOKE_LIGHT: Color = Color::Indexed(PALETTE[10].1);
const SMOKE_PALE: Color = Color::Indexed(PALETTE[11].1);

// The large form as pixels: one character for each pixel, 14 pixels wide, two pixel rows for each terminal row. `.` is the terminal's own
// background. O orange, W eye white, K ink, G shirt, T shorts, Y flame core, F flame edge, R flame tip, and D M L P the smoke from the darkest
// (next to the flame) to the palest (the tail).

/// The character, pixel rows 0 to 13. It never changes.
const BODY: [&str; 14] = [
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

/// The flame under each foot, pixel rows 14 to 17: a yellow core, an orange edge and a red tip. It never changes.
const FLAME: [&str; 4] = [
    ".FYYF....FYYF.",
    ".FYYF....FYYF.",
    "..FF......FF..",
    "..RR......RR..",
];

/// The smoke, pixel rows 18 to 23, in each frame. Frame 0 is the still picture. From one frame to the next four to six pixels move by one cell
/// (the lower outer edge, the lower inner edge and the second tail pixel of each puff, and in the first two steps the tip of each tail), and the
/// first three rows, next to the flame, stay: the smoke is fastened to the flame and the two plumes never touch.
const SMOKE: [[&str; 6]; FRAMES] = [
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

/// The flame under the legs of the head, two pixel rows of 8 pixels (one terminal row): an orange edge on each side of a yellow core, and a red
/// tip under the core. The core of each flame is under the outer cell of its leg. It never changes.
const HEAD_FLAME: [&str; 2] = ["FYF..FYF", ".R....R."];

/// The smoke under the head's flame, two pixel rows (one terminal row), in each frame. Frame 0 is the still picture. The first row, next to the
/// flame, stays; the second row moves, four to six pixels from one frame to the next. The two puffs never touch (columns 3 and 4 are empty).
const HEAD_SMOKE: [[&str; 2]; FRAMES] = [
    ["DM....MD", "LML..LML"],
    ["DM....MD", ".LM..ML."],
    ["DM....MD", "L......L"],
];

/// The head: per row the glyphs and, one letter per glyph, the tone. A cell is a column wide and two pixels tall. A lower-case letter is one
/// colour: ` ` none, `o` orange, `t` shorts; the glyph says which half of the cell it fills (`█` both, `▀` the top, `▄` the bottom). An
/// upper-case letter is a `▀` over a different colour, foreground over background: `A` orange over ink, `B` orange over eye white, `C` eye white
/// over shirt, `D` orange over shirt.
const HEAD_SHORT: [(&str, &str); 3] = [
    ("▄▀▀██▀▀▄", "oABooABo"),
    ("█▀▀▀▀▀▀█", "oCCDDCCo"),
    (" ██  ██ ", " tt  tt "),
];

/// The ASCII look of the large form from the hat to the flames: nine rows. The eyes are `@`. The smoke below is [`ascii_smoke`].
const LARGE_ASCII: [&str; 9] = [
    "  .'------'.  ",
    " |/@@\\  /@@\\| ",
    " |\\__/  \\__/| ",
    "(############)",
    " ############ ",
    " |::::::::::| ",
    " |:::/  \\:::| ",
    " (**)    (**) ",
    "  \\/      \\/  ",
];

/// The ASCII look of the short head, which is the first three rows of the head.
const HEAD_ASCII: [&str; 3] = [" ,----. ", "(@    @)", "(##  ##)"];

/// The ASCII look of the flame under the head's legs (one row). The smoke below it is [`ascii_pairs`] of [`HEAD_SMOKE`].
const HEAD_FLAME_ASCII: &str = "\\*/  \\*/";

/// Whether this environment wants the plain ASCII mascot. `NO_COLOR` set to anything but the empty string (no-color.org), `TERM=dumb` and
/// every locale that is not UTF-8 give [`Look::Ascii`]. The locale is the first of `LC_ALL`, `LC_CTYPE` and `LANG` that is not empty;
/// none set is the C locale, which is not UTF-8.
pub fn look_from_env(get: impl Fn(&str) -> Option<String>) -> Look {
    if get("NO_COLOR").is_some_and(|v| !v.is_empty()) || get("TERM").is_some_and(|t| t == "dumb") {
        return Look::Ascii;
    }
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|key| get(key).filter(|v| !v.is_empty()));
    match locale {
        Some(l) if l.to_ascii_lowercase().replace('-', "").contains("utf8") => Look::Blocks,
        _ => Look::Ascii,
    }
}

fn tone_style(tone: char) -> Style {
    let on = |fg| Style::new().fg(fg);
    match tone {
        'o' => on(ORANGE),
        't' => on(SHORTS),
        'A' => on(ORANGE).bg(INK),
        'B' => on(ORANGE).bg(EYEWHITE),
        'C' => on(EYEWHITE).bg(SHIRT),
        'D' => on(ORANGE).bg(SHIRT),
        _ => Style::new(),
    }
}

/// Spans from cells: runs of cells that share a style are one span.
fn spans_of(cells: impl Iterator<Item = (char, Style)>) -> Line<'static> {
    let mut spans = Vec::new();
    let mut run = String::new();
    let mut run_style = Style::new();
    for (glyph, style) in cells {
        if style != run_style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_style = style;
        run.push(glyph);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style));
    }
    Line::from(spans)
}

/// One row of the head: runs of glyphs that share a tone are one span.
fn tone_line(glyphs: &str, tones: &str) -> Line<'static> {
    spans_of(
        glyphs
            .chars()
            .zip(tones.chars())
            .map(|(glyph, tone)| (glyph, tone_style(tone))),
    )
}

/// The colour of a pixel of the large form, none for the terminal's own background.
fn pixel(letter: char) -> Option<Color> {
    match letter {
        'O' => Some(ORANGE),
        'W' => Some(EYEWHITE),
        'K' => Some(INK),
        'G' => Some(SHIRT),
        'T' => Some(SHORTS),
        'Y' => Some(FLAME_YELLOW),
        'F' => Some(FLAME_ORANGE),
        'R' => Some(FLAME_RED),
        'D' => Some(SMOKE_DARK),
        'M' => Some(SMOKE_MID),
        'L' => Some(SMOKE_LIGHT),
        'P' => Some(SMOKE_PALE),
        _ => None,
    }
}

/// The glyph and the style of a cell that holds two pixels, the upper one and the lower one: `█` when both are the same colour, otherwise
/// a half block in the colour of the pixel that is drawn, with the other pixel as the background of a `▀` when both are drawn.
fn block_cell(top: Option<Color>, bottom: Option<Color>) -> (char, Style) {
    match (top, bottom) {
        (None, None) => (' ', Style::new()),
        (Some(t), Some(b)) if t == b => ('█', Style::new().fg(t)),
        (Some(t), Some(b)) => ('▀', Style::new().fg(t).bg(b)),
        (Some(t), None) => ('▀', Style::new().fg(t)),
        (None, Some(b)) => ('▄', Style::new().fg(b)),
    }
}

/// The pixel rows of the large form in a frame: the character, the flames and that frame's smoke.
fn large_pixel_rows(frame: usize) -> Vec<&'static str> {
    BODY.iter()
        .chain(FLAME.iter())
        .chain(SMOKE[frame % FRAMES].iter())
        .copied()
        .collect()
}

/// One terminal row of the block look from two pixel rows: the upper one and the lower one.
fn pixel_line(top: &str, bottom: &str) -> Line<'static> {
    spans_of(
        top.chars()
            .zip(bottom.chars())
            .map(|(t, b)| block_cell(pixel(t), pixel(b))),
    )
}

/// The block look of the large form in a frame: two pixel rows for each terminal row.
fn large_block_lines(frame: usize) -> Vec<Line<'static>> {
    large_pixel_rows(frame)
        .chunks(2)
        .map(|pair| pixel_line(pair[0], pair[1]))
        .collect()
}

/// The block look of the head in a frame: the three rows of the short head, then the flame, then the smoke.
fn head_block_lines(frame: usize) -> Vec<Line<'static>> {
    let smoke = &HEAD_SMOKE[frame % FRAMES];
    let mut lines = short_head_block_lines();
    lines.push(pixel_line(HEAD_FLAME[0], HEAD_FLAME[1]));
    lines.push(pixel_line(smoke[0], smoke[1]));
    lines
}

/// The block look of the short head.
fn short_head_block_lines() -> Vec<Line<'static>> {
    HEAD_SHORT.iter().map(|(g, t)| tone_line(g, t)).collect()
}

/// The ASCII glyph of a smoke pixel and its weight: the darker the smoke, the denser the mark.
fn smoke_mark(letter: char) -> (u8, char) {
    match letter {
        'D' => (4, '%'),
        'M' => (3, '='),
        'L' => (2, '~'),
        'P' => (1, '.'),
        _ => (0, ' '),
    }
}

/// The ASCII look of pairs of pixel rows of smoke: one row for each pair, the denser mark of the two pixels in a cell.
fn ascii_pairs(rows: &[&str]) -> Vec<String> {
    rows.chunks(2)
        .map(|pair| {
            pair[0]
                .chars()
                .zip(pair[1].chars())
                .map(|(top, bottom)| smoke_mark(top).max(smoke_mark(bottom)).1)
                .collect()
        })
        .collect()
}

/// The ASCII look of the large form's smoke in a frame.
fn ascii_smoke(frame: usize) -> Vec<String> {
    ascii_pairs(&SMOKE[frame % FRAMES])
}

/// The lines of a form in a look in a frame. Every line of a form has the same width. The short head has one frame; a frame past the last of
/// the head and of the large form wraps.
pub fn lines_at(form: Form, look: Look, frame: usize) -> Vec<Line<'static>> {
    match (look, form) {
        (Look::Blocks, Form::Large) => large_block_lines(frame),
        (Look::Blocks, Form::Head) => head_block_lines(frame),
        (Look::Blocks, Form::HeadShort) => short_head_block_lines(),
        (Look::Ascii, Form::Large) => LARGE_ASCII
            .iter()
            .map(|l| (*l).to_string())
            .chain(ascii_smoke(frame))
            .map(Line::from)
            .collect(),
        (Look::Ascii, Form::Head) => HEAD_ASCII
            .iter()
            .map(|l| (*l).to_string())
            .chain([HEAD_FLAME_ASCII.to_string()])
            .chain(ascii_pairs(&HEAD_SMOKE[frame % FRAMES]))
            .map(Line::from)
            .collect(),
        (Look::Ascii, Form::HeadShort) => HEAD_ASCII.iter().map(|l| Line::from(*l)).collect(),
    }
}

/// The lines of a form in a look: the still picture (frame 0).
pub fn lines(form: Form, look: Look) -> Vec<Line<'static>> {
    lines_at(form, look, 0)
}

/// The text of the lines of a form in a frame, without styles.
pub fn plain_at(form: Form, look: Look, frame: usize) -> Vec<String> {
    lines_at(form, look, frame)
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

/// The text of the lines of a form, without styles: the still picture (frame 0).
pub fn plain(form: Form, look: Look) -> Vec<String> {
    plain_at(form, look, 0)
}

/// Columns and rows of a form in a look.
pub fn size(form: Form, look: Look) -> (u16, u16) {
    let lines = lines(form, look);
    let cols = lines.first().map_or(0, Line::width);
    (
        u16::try_from(cols).unwrap_or(u16::MAX),
        u16::try_from(lines.len()).unwrap_or(u16::MAX),
    )
}

/// The form that fits `room` with its margin (the large form first, then the head with its flame and smoke, then the short head), or none
/// when not even the short head fits.
pub fn form_for(room: Rect) -> Option<Form> {
    let fits = |(rows, cols): (u16, u16)| {
        room.height >= rows + 2 * MARGIN_ROWS && room.width >= cols + 2 * MARGIN_COLS
    };
    if fits(LARGE_MAX) {
        Some(Form::Large)
    } else if fits(HEAD_MAX) {
        Some(Form::Head)
    } else if fits(HEAD_SHORT_MAX) {
        Some(Form::HeadShort)
    } else {
        None
    }
}

/// The empty area of the Runs table: inside its border and below its header row.
pub fn room(body: Rect) -> Rect {
    Rect::new(
        body.x.saturating_add(1),
        body.y.saturating_add(2),
        body.width.saturating_sub(2),
        body.height.saturating_sub(3),
    )
}

/// The form that fits the Runs body and the rectangle it is drawn in, centred in the empty table.
pub fn placement(body: Rect, look: Look) -> Option<(Form, Rect)> {
    let room = room(body);
    let form = form_for(room)?;
    let (cols, rows) = size(form, look);
    let x = room.x + room.width.saturating_sub(cols) / 2;
    let y = room.y + room.height.saturating_sub(rows) / 2;
    Some((form, Rect::new(x, y, cols, rows)))
}

/// Draws the mascot in the empty table of the Runs body in `look`, the large form in `frame`. It writes only inside the rectangle it returns,
/// which lies in the table's empty area, and nothing when the area has no room. The same rectangle is drawn in every frame.
pub fn draw_with(f: &mut Frame, body: Rect, look: Look, frame: usize) -> Option<Rect> {
    let (form, at) = placement(body, look)?;
    f.render_widget(Paragraph::new(lines_at(form, look, frame)), at);
    Some(at)
}

/// The one call of the Runs screen: draws the mascot when `empty` (the service has answered and the user has no runs) in `look`, the large
/// form in `frame` (the app counts its timer ticks and picks it).
pub fn draw_runs_empty(f: &mut Frame, body: Rect, empty: bool, look: Look, frame: usize) {
    if empty {
        draw_with(f, body, look, frame);
    }
}

/// Draws the head `form` ([`Form::Head`] in five rows or [`Form::HeadShort`] in three, eight columns wide) at the top left of `at` (the title
/// band), in `look` and in `frame` (the short head has one). The rectangle is cut to the head's size and to what `f` has: a head that does not
/// fit whole is not drawn.
pub fn draw_head(f: &mut Frame, at: Rect, form: Form, look: Look, frame: usize) {
    let (cols, rows) = size(form, look);
    let area = Rect::new(at.x, at.y, cols, rows).intersection(f.area());
    if area.width == cols && area.height == rows {
        f.render_widget(Paragraph::new(lines_at(form, look, frame)), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    /// The Runs body of an 80x24 terminal: the title row above it, the status and help rows below.
    const BODY_80X24: Rect = Rect::new(0, 1, 80, 21);

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
        }
    }

    fn rows(buf: &Buffer) -> Vec<String> {
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn drawn(width: u16, height: u16, f: impl FnOnce(&mut Frame)) -> Buffer {
        let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
        t.draw(f).unwrap();
        t.backend().buffer().clone()
    }

    #[test]
    fn the_environment_picks_the_look() {
        type Pairs<'a> = &'a [(&'a str, &'a str)];
        let utf8 = ("LANG", "en_US.UTF-8");
        let cases: [(Pairs, Look, &str); 11] = [
            (&[utf8], Look::Blocks, "a UTF-8 locale"),
            (&[("LC_ALL", "C.utf8")], Look::Blocks, "LC_ALL utf8"),
            (&[("LC_CTYPE", "en_GB.UTF-8")], Look::Blocks, "LC_CTYPE"),
            (&[], Look::Ascii, "no locale at all"),
            (&[("LANG", "C")], Look::Ascii, "the C locale"),
            (&[("LANG", "en_US.ISO-8859-1")], Look::Ascii, "Latin-1"),
            (
                &[("LC_ALL", "C"), utf8],
                Look::Ascii,
                "LC_ALL overrides LANG",
            ),
            (&[utf8, ("NO_COLOR", "1")], Look::Ascii, "NO_COLOR"),
            (
                &[utf8, ("NO_COLOR", "")],
                Look::Blocks,
                "an empty NO_COLOR is not set",
            ),
            (&[utf8, ("TERM", "dumb")], Look::Ascii, "TERM=dumb"),
            (
                &[utf8, ("TERM", "xterm-256color")],
                Look::Blocks,
                "a real terminal",
            ),
        ];
        for (pairs, expected, why) in cases {
            assert_eq!(look_from_env(env(pairs)), expected, "{why}");
        }
    }

    #[test]
    fn the_room_is_the_table_below_its_header() {
        assert_eq!(room(BODY_80X24), Rect::new(1, 3, 78, 18));
        for tiny in [
            Rect::new(0, 0, 0, 0),
            Rect::new(3, 4, 1, 1),
            Rect::new(0, 0, 2, 2),
        ] {
            assert_eq!(room(tiny).area(), 0, "{tiny:?} leaves no room");
        }
    }

    #[test]
    fn the_large_form_is_drawn_in_the_middle_of_the_empty_table() {
        for look in [Look::Blocks, Look::Ascii] {
            let mut at = None;
            let buf = drawn(80, 24, |f| at = draw_with(f, BODY_80X24, look, 0));
            let at = at.unwrap();
            assert_eq!(at, Rect::new(33, 6, 14, 12), "{look:?}");
            let screen = rows(&buf);
            for (i, line) in plain(Form::Large, look).iter().enumerate() {
                let got: String = screen[usize::from(at.y) + i]
                    .chars()
                    .skip(usize::from(at.x))
                    .take(usize::from(at.width))
                    .collect();
                assert_eq!(&got, line, "{look:?} row {i}");
            }
            for (y, row) in screen.iter().enumerate() {
                let inside = y >= usize::from(at.y) && y < usize::from(at.bottom());
                if !inside {
                    assert!(row.trim().is_empty(), "{look:?}: row {y} holds {row:?}");
                }
            }
        }
    }

    #[test]
    fn every_frame_is_drawn_whole_in_the_same_rectangle() {
        for look in [Look::Blocks, Look::Ascii] {
            let (_, rect) = placement(BODY_80X24, look).unwrap();
            for frame in 0..FRAMES {
                let mut at = None;
                let buf = drawn(80, 24, |f| at = draw_with(f, BODY_80X24, look, frame));
                assert_eq!(at, Some(rect), "{look:?} frame {frame}");
                let screen = rows(&buf);
                for (i, line) in plain_at(Form::Large, look, frame).iter().enumerate() {
                    let got: String = screen[usize::from(rect.y) + i]
                        .chars()
                        .skip(usize::from(rect.x))
                        .take(usize::from(rect.width))
                        .collect();
                    assert_eq!(&got, line, "{look:?} frame {frame} row {i}");
                }
            }
        }
    }

    #[test]
    fn nothing_is_drawn_unless_the_screen_asks() {
        let buf = drawn(80, 24, |f| {
            draw_runs_empty(f, BODY_80X24, false, Look::Blocks, 0);
        });
        assert!(rows(&buf).iter().all(|r| r.trim().is_empty()));
        let buf = drawn(80, 24, |f| {
            draw_runs_empty(f, BODY_80X24, true, Look::Blocks, 0);
        });
        assert!(rows(&buf).iter().any(|r| !r.trim().is_empty()));
    }

    #[test]
    fn the_placement_matches_what_is_drawn() {
        for look in [Look::Blocks, Look::Ascii] {
            let (form, rect) = placement(BODY_80X24, look).unwrap();
            let mut drew = None;
            drawn(80, 24, |f| drew = draw_with(f, BODY_80X24, look, 0));
            assert_eq!(drew, Some(rect));
            assert_eq!((rect.width, rect.height), size(form, look));
        }
    }

    #[test]
    fn the_head_is_drawn_whole_or_not_at_all() {
        for look in [Look::Blocks, Look::Ascii] {
            for form in [Form::Head, Form::HeadShort] {
                for frame in 0..FRAMES {
                    let (cols, high) = size(form, look);
                    let buf = drawn(20, 8, |f| {
                        draw_head(f, Rect::new(2, 1, cols, high), form, look, frame);
                    });
                    let screen = rows(&buf);
                    for (i, line) in plain_at(form, look, frame).iter().enumerate() {
                        let got: String = screen[1 + i].chars().skip(2).take(8).collect();
                        assert_eq!(&got, line, "{form:?} {look:?} frame {frame} row {i}");
                    }
                    // Cut by the frame: nothing is drawn rather than half a head.
                    let buf = drawn(8, high - 1, |f| {
                        draw_head(f, Rect::new(0, 0, cols, high), form, look, frame);
                    });
                    assert!(
                        rows(&buf).iter().all(|r| r.trim().is_empty()),
                        "{form:?} {look:?}"
                    );
                }
            }
        }
    }
}
