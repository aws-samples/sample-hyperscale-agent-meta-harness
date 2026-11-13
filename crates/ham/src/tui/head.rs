//! The title band: the ham mascot's head beside the app name.
//!
//! On a terminal more than 30 rows tall and at least 72 columns wide, every screen has a band at the top. From 33 rows it is five rows
//! ([`BAND_ROWS`]): row 0 holds the head's first row at columns 0 to 7, the screen's title from column 10 and the version tag at the right end;
//! rows 1 to 4 hold the head's other rows (its legs, a flame under them and a puff of smoke) and nothing else; the body of the screen starts on
//! row 5. On a terminal of 31 or 32 rows the band is the three rows it always was ([`SHORT_BAND_ROWS`]), with the short head (no flame and no
//! smoke) and the body from row 3, so nothing gets worse there. On a terminal of 30 rows or fewer, or under 72 columns, nothing changes: one
//! title row and no mascot beside the name (the usual 80x24 and 100x30 windows). The empty Runs table keeps the large form in every mode.
//!
//! - [`Head`] is what the app is told: whether the band may be drawn, whether the empty table may show its picture, and the look. The
//!   constructor of `App` gives the neutral value ([`Head::default`]: no band, the picture on, the block look) so that the library's tests draw
//!   the screens they always drew; `cli::tui_app` gives the user's ([`Head::banded`] or the one `ui.mascot` and `HAMH_NO_MASCOT` ask for).
//!   The look is a value of the app, never read from the process inside a draw.
//! - [`band_rows_for`] is the rule, a pure function of the frame size and the switch, made again at every draw: no hysteresis, no dependence on
//!   the screen, the data, the locale or `NO_COLOR`. 31 rows keep the New run screen whole with a three-line message (it needs 27 below
//!   the band, and the screens keep the 28 rows below a three-row band that 31 rows give them), so the five-row band starts two rows later, at
//!   33 ([`TALL_ROWS`]). 72 columns are the prefix (10), the columns a title keeps ([`version_tag::TITLE_KEEP`]), the gap and the long form of
//!   the version tag, so the band never makes the tag shorter than it is without it.
//! - [`split`] cuts a frame into the title rectangle and the area the screens lay themselves out in. Every screen has always laid out a
//!   one-row title above its body in the whole frame. In band mode that layout runs in the frame less the band's rows but one: its title row
//!   is then the last row of the band (never drawn; the title goes to [`Split::title`]) and its body starts on the first row below the band.
//!   So the body of every screen in band mode is, cell for cell, the body of the same screen `rows - 1` rows shorter without the band (2 for
//!   the three-row band, 4 for the five-row band), and one helper owns the geometry of all seven screens and the tag explorer.

use super::mascot::{self, Look};
use super::version_tag::{GAP, LONG_TAG, TITLE_KEEP};
use ham_core::config::MascotMode;
use ratatui::Frame;
use ratatui::layout::Rect;

/// Rows of the band on a terminal of [`TALL_ROWS`] rows or more: the head with its flame and smoke.
pub const BAND_ROWS: u16 = mascot::HEAD_MAX.0;
/// Rows of the band on a terminal of [`MIN_ROWS`] to [`TALL_ROWS`] less one rows: the short head.
pub const SHORT_BAND_ROWS: u16 = mascot::HEAD_SHORT_MAX.0;
/// The five-row band is drawn on a frame at least this tall: the 31 rows that keep the New run screen whole under a three-row band, and the
/// rows the tall band takes more.
pub const TALL_ROWS: u16 = MIN_ROWS + (BAND_ROWS - SHORT_BAND_ROWS);
/// Columns between the head and the title.
pub const GAP_COLS: u16 = 2;
/// Columns left of the title: the head and the gap.
pub const HEAD_PREFIX: u16 = mascot::HEAD_MAX.1 + GAP_COLS;
/// A band is drawn on a frame at least this tall: more than 30 rows.
pub const MIN_ROWS: u16 = 31;
/// The band is drawn on a frame at least this wide: the prefix, the columns a title keeps, the gap and the long form of the tag (72).
pub const MIN_COLS: u16 = HEAD_PREFIX + (TITLE_KEEP + GAP + LONG_TAG) as u16;

/// What the app is told about the mascot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    /// The band may be drawn (when the frame is tall and wide enough).
    pub band: bool,
    /// The empty Runs table may show its picture.
    pub empty_table: bool,
    /// How the mascot is drawn.
    pub look: Look,
}

impl Default for Head {
    /// The neutral value of a new `App`: no band, the picture in the empty table, the block look.
    fn default() -> Self {
        Head {
            band: false,
            empty_table: true,
            look: Look::Blocks,
        }
    }
}

impl Head {
    /// The band and the picture on, in `look`: what a person who changed nothing gets.
    pub const fn banded(look: Look) -> Head {
        Head {
            band: true,
            empty_table: true,
            look,
        }
    }

    /// No band and no picture: `ui.mascot = off` and `HAMH_NO_MASCOT=1`.
    pub const OFF: Head = Head {
        band: false,
        empty_table: false,
        look: Look::Ascii,
    };

    /// The head a `ui.mascot` setting gives, with `env_look` the look the environment asks for (`mascot::look_from_env`): `auto` is the band
    /// and the picture in that look, `ascii` is the same in the ASCII look whatever the environment says (the cells are the same), `off` is
    /// neither.
    pub const fn for_mode(mode: MascotMode, env_look: Look) -> Head {
        match mode {
            MascotMode::Auto => Head::banded(env_look),
            MascotMode::Ascii => Head::banded(Look::Ascii),
            MascotMode::Off => Head::OFF,
        }
    }
}

/// The rows of the band of a frame `width` columns wide and `height` rows tall when the switch is `head`: 0 for no band, [`SHORT_BAND_ROWS`]
/// from [`MIN_ROWS`] rows and [`BAND_ROWS`] from [`TALL_ROWS`] rows.
pub fn band_rows_for(width: u16, height: u16, head: Head) -> u16 {
    if !head.band || height < MIN_ROWS || width < MIN_COLS {
        0
    } else if height >= TALL_ROWS {
        BAND_ROWS
    } else {
        SHORT_BAND_ROWS
    }
}

/// Whether a frame `width` columns wide and `height` rows tall has a band when the switch is `head`.
pub fn band_for(width: u16, height: u16, head: Head) -> bool {
    band_rows_for(width, height, head) > 0
}

/// A frame cut for drawing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Split {
    /// The frame has the band.
    pub band: bool,
    /// Rows of the band: [`BAND_ROWS`], [`SHORT_BAND_ROWS`], or 0 without the band.
    pub rows: u16,
    /// Columns left of the title: [`HEAD_PREFIX`] with the band, else 0.
    pub left: u16,
    /// Where the screen's title is drawn: row 0 from column `left` to the right edge (the version tag is drawn over its end).
    pub title: Rect,
    /// Everything below the title: row 1 and on, or the first row below the band and on with the band.
    pub rest: Rect,
    /// The area the screens lay themselves out in: the whole frame, or the frame less the band's rows but one with the band (its first row is
    /// the last row of the band, where the one-row title of every layout would be, and its second row is the first row below the band).
    pub area: Rect,
}

impl Split {
    /// The rectangle a screen draws its title in: the band's title with the band, else the title row its own layout made.
    pub fn title_over(&self, own: Rect) -> Rect {
        if self.band { self.title } else { own }
    }

    /// The height the box of the run screen keeps a third of: the rows below the band with it, else the area's own height.
    pub fn cap_base(&self) -> u16 {
        if self.band {
            self.rest.height
        } else {
            self.area.height
        }
    }
}

/// Cuts `frame` for the screens, with the band when [`band_for`] says so.
pub fn split(frame: Rect, head: Head) -> Split {
    let rows = band_rows_for(frame.width, frame.height, head);
    if rows > 0 {
        let left = HEAD_PREFIX;
        Split {
            band: true,
            rows,
            left,
            title: Rect::new(
                frame.x.saturating_add(left),
                frame.y,
                frame.width.saturating_sub(left),
                1,
            ),
            rest: Rect::new(
                frame.x,
                frame.y.saturating_add(rows),
                frame.width,
                frame.height.saturating_sub(rows),
            ),
            area: Rect::new(
                frame.x,
                frame.y.saturating_add(rows - 1),
                frame.width,
                frame.height.saturating_sub(rows - 1),
            ),
        }
    } else {
        let first = Rect::new(frame.x, frame.y, frame.width, frame.height.min(1));
        Split {
            band: false,
            rows: 0,
            left: 0,
            title: first,
            rest: Rect::new(
                frame.x,
                frame.y.saturating_add(1),
                frame.width,
                frame.height.saturating_sub(1),
            ),
            area: frame,
        }
    }
}

impl Split {
    /// The form of the mascot in this band: the head with its flame and smoke in the five-row band, the short head in the three-row band.
    pub fn form(&self) -> mascot::Form {
        if self.rows >= BAND_ROWS {
            mascot::Form::Head
        } else {
            mascot::Form::HeadShort
        }
    }
}

/// Draws the head at the left of the band's rows, in `mascot_frame` (the app's frame of the smoke; the short head has one). Nothing without
/// the band.
pub fn draw_band(f: &mut Frame, frame: Rect, split: &Split, head: Head, mascot_frame: usize) {
    if split.band {
        mascot::draw_head(f, frame, split.form(), head.look, mascot_frame);
    }
}
