//! The version tag: `ham v4.01 | backend
//! v4.01`, right-aligned on the top row of every screen, drawn last, over whatever the screen put there.
//!
//! - The pure formatter: [`tag`] picks the longest of three forms that fits a width, [`compose`] returns the same with its parts for styling, and
//!   [`for_frame`] chooses for a terminal width, keeping [`TITLE_KEEP`] columns for the title (`ham · Run ` and a whole run id).
//!   The forms are `ham v4.01 | backend v4.01`, `h v4.01 | b v4.01` and `4.01|4.01`. A backend that has not answered, or reports no valid
//!   version, is `v?.??` in the same slot, so the tag does not move when the backend answers.
//! - The style: the backend half is bold yellow when it differs from the client or is unknown, and everything else is dim. Bold and dim are
//!   style attributes, so the difference shows on a terminal with no colour.
//! - [`overlay`]: writes the tag on row 0 of an area over a ratatui buffer and clears the cells under it. A title that would touch the tag is
//!   clipped with an ellipsis one cell before the gap; the tag is never hidden. Box-drawing glyphs on the row are decoration (a border
//!   around an overlay) and are replaced under the tag without an ellipsis. `App::draw` calls it once, after the screen and the open-anything
//!   box are drawn, so the titles in `ui.rs` stay as they are.
//! - [`reserve`]: the columns the tag and its gap take at a width, for the run screen's header, which fits itself to the room it has and gives
//!   up its tail (the age, the cost, the tokens, the time, the model) before it would be clipped.
//! - [`split_tag`]: reads a tag back off the end of a screen row by its exact shape, for the live drivers that compare titles.
//! - The update notice: a short text one blank left of the tag, bold and reversed so that it reads as a button without colour,
//!   drawn and registered for the mouse together with the tag. [`row0_for_frame`] chooses the forms of both for a width: the title keeps
//!   its [`TITLE_KEEP`] columns, and the tag takes a shorter form before the notice does, and the notice a shorter form before it is
//!   dropped. [`overlay_with`] draws both and returns the notice's cells, [`reserve_with`] is the room both take, and [`split_row0`] reads
//!   a row back into title, notice and tag. Without a notice every function gives what the tag alone gave.

use super::select::text_cells;
use crate::version;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

const GAP_CELLS: u16 = 1;

/// Blank columns between a title and the tag.
pub const GAP: usize = GAP_CELLS as usize;

/// The columns of the title row that [`for_frame`] keeps for the title before it lets the tag grow: `ham · Run ` and a whole 26-character run
/// id. A terminal narrower than that plus the shortest tag cuts the title, never the tag.
pub const TITLE_KEEP: usize = 36;

/// The columns of the long form of the tag, `ham v4.01 | backend v4.01`. The title band (`head.rs`) is drawn from the terminal width at which
/// its prefix, [`TITLE_KEEP`], the gap and this many columns fit, so the band never makes the tag shorter than it is without it.
pub const LONG_TAG: usize = 25;

/// What stands for a backend version that is not known.
const UNKNOWN: &str = "?.??";

/// One tag, in the form that fitted: its text and where the backend half starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    text: String,
    /// Byte offset in `text` of the backend half (`backend v4.01`, `b v4.01` or `4.01`).
    split: usize,
    alert: bool,
}

impl Tag {
    /// The whole tag, one line of ASCII.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Columns the tag takes (it is ASCII).
    pub fn width(&self) -> usize {
        self.text.chars().count()
    }

    /// The backend differs from the client or is unknown: its half is drawn bold yellow.
    pub fn alert(&self) -> bool {
        self.alert
    }

    /// The tag as two styled spans: the client half with the separator (dim) and the backend half.
    pub fn spans(&self) -> Vec<Span<'static>> {
        let dim = Style::default().add_modifier(Modifier::DIM);
        let backend = if self.alert {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        vec![
            Span::styled(self.text[..self.split].to_string(), dim),
            Span::styled(self.text[self.split..].to_string(), backend),
        ]
    }
}

/// The backend differs from the client, or is unknown.
pub fn differs(client: &str, backend: Option<&str>) -> bool {
    backend != Some(client)
}

/// The three forms, longest first.
fn forms(client: &str, backend: Option<&str>) -> [Tag; 3] {
    let shown = backend.unwrap_or(UNKNOWN);
    let alert = differs(client, backend);
    let make = |head: String, tail: String| {
        let split = head.len();
        Tag {
            text: format!("{head}{tail}"),
            split,
            alert,
        }
    };
    // Several backends that report different versions read `mixed`; the word has the width of `v5.05`, so no form moves.
    if backend == Some(version::MIXED) {
        return [
            make(format!("ham v{client} | "), format!("backend {shown}")),
            make(format!("h v{client} | "), format!("b {shown}")),
            make(format!("{client}|"), shown.to_string()),
        ];
    }
    [
        make(format!("ham v{client} | "), format!("backend v{shown}")),
        make(format!("h v{client} | "), format!("b v{shown}")),
        make(format!("{client}|"), shown.to_string()),
    ]
}

/// The longest form that is at most `avail_width` columns, with its parts. `None` when even the shortest form does not fit.
pub fn compose(client: &str, backend: Option<&str>, avail_width: usize) -> Option<Tag> {
    forms(client, backend)
        .into_iter()
        .find(|t| t.width() <= avail_width)
}

/// The text of [`compose`]: `ham v4.01 | backend v4.01`, `h v4.01 | b v4.01` or `4.01|4.01`, and `v?.??` in the backend's slot while it is
/// not known.
pub fn tag(client: &str, backend: Option<&str>, avail_width: usize) -> Option<String> {
    compose(client, backend, avail_width).map(|t| t.text)
}

/// The tag for a terminal `frame_width` columns wide: the longest form that leaves [`TITLE_KEEP`] columns and the gap for the title, and when
/// none does, the shortest form if it fits the terminal at all. A terminal too narrow for that has no tag.
pub fn for_frame(client: &str, backend: Option<&str>, frame_width: usize) -> Option<Tag> {
    let room = frame_width.saturating_sub(TITLE_KEEP + GAP);
    let [full, short, bare] = forms(client, backend);
    [full, short]
        .into_iter()
        .find(|t| t.width() <= room)
        .or_else(|| (bare.width() <= frame_width).then_some(bare))
}

/// [`for_frame`] for a frame whose first `left` columns hold something that is not title (the mascot's head and its gap in the title band):
/// the title keeps [`TITLE_KEEP`] columns of the rest. For every width at which the band is drawn the result is the one of [`for_frame`].
pub fn for_frame_at(
    client: &str,
    backend: Option<&str>,
    frame_width: usize,
    left: usize,
) -> Option<Tag> {
    for_frame(client, backend, frame_width.saturating_sub(left))
}

/// The two texts of the update notice: the long one names the version (`v5.25 available`), the
/// short one does not (`update`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoticeWords {
    pub long: String,
    pub short: String,
}

/// What the end of the first row holds: the tag in the form that fitted and, left of it, the notice in the form that fitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row0 {
    pub tag: Tag,
    pub notice: Option<String>,
}

/// The tag and the notice for a frame `frame_width` columns wide whose first `left` columns are not title. With no notice it is exactly
/// [`for_frame_at`]. With one, the first of these that leaves the title [`TITLE_KEEP`] columns, with one blank on each side of the notice:
/// the long tag and the long notice; the short tag and the long notice; the long tag and the short notice; the short tag and the short
/// notice; the bare tag and the short notice. When none fits, the tag of [`for_frame_at`] and no notice: the tag is never given up for it.
pub fn row0_for_frame(
    client: &str,
    backend: Option<&str>,
    notice: Option<&NoticeWords>,
    frame_width: usize,
    left: usize,
) -> Option<Row0> {
    let alone =
        || for_frame_at(client, backend, frame_width, left).map(|tag| Row0 { tag, notice: None });
    let Some(words) = notice else {
        return alone();
    };
    let [full, short, bare] = forms(client, backend);
    let order = [
        (&full, &words.long),
        (&short, &words.long),
        (&full, &words.short),
        (&short, &words.short),
        (&bare, &words.short),
    ];
    order
        .into_iter()
        .find(|(tag, text)| {
            left + TITLE_KEEP + GAP + text_cells(text) + GAP + tag.width() <= frame_width
        })
        .map(|(tag, text)| Row0 {
            tag: tag.clone(),
            notice: Some(text.clone()),
        })
        .or_else(alone)
}

/// The columns the tag and its gap take at `frame_width` with this client's version and the backend version last seen, 0 when there is no
/// tag. The run screen's header fits itself to the width minus this.
pub fn reserve(frame_width: u16) -> usize {
    reserve_at(frame_width, 0)
}

/// [`reserve`] for a frame whose first `left` columns are not title: the tag and its gap only; the caller takes `left` off the width once.
pub fn reserve_at(frame_width: u16, left: u16) -> usize {
    reserve_with(frame_width, left, None)
}

/// [`reserve_at`] with the update notice: the columns the tag, the notice and the blank before each take, so the run screen's header gives
/// up its tail for the notice as it does for the tag.
pub fn reserve_with(frame_width: u16, left: u16, notice: Option<&NoticeWords>) -> usize {
    row0_for_frame(
        version::CLIENT,
        version::backend().as_deref(),
        notice,
        usize::from(frame_width),
        usize::from(left),
    )
    .map_or(0, |row| {
        row.tag.width() + GAP + row.notice.map_or(0, |text| text_cells(&text) + GAP)
    })
}

/// Box-drawing and block glyphs: the border of an overlay, not the title.
fn decoration(symbol: &str) -> bool {
    symbol
        .chars()
        .all(|c| ('\u{2500}'..='\u{259f}').contains(&c))
}

/// Writes the tag on the first row of `area` and clears what is under it. See the module docs for what happens to a title that reaches it.
pub fn overlay(buf: &mut Buffer, area: Rect, client: &str, backend: Option<&str>) {
    overlay_at(buf, area, 0, client, backend);
}

/// [`overlay`] for a frame whose first `left` columns hold the mascot's head and its gap: the row is read and clipped from column `left` on,
/// so the head is never taken for title text, and the tag is the form [`for_frame_at`] chooses.
pub fn overlay_at(buf: &mut Buffer, area: Rect, left: u16, client: &str, backend: Option<&str>) {
    let _ = overlay_with(buf, area, left, client, backend, None);
}

/// [`overlay_at`] with the update notice: when [`row0_for_frame`] gives one, it is written one blank left of the tag, bold and
/// reversed, and the title is clipped one cell before the blank in front of it. Returns the cells of the notice (one row) when one was
/// drawn, for the mouse.
pub fn overlay_with(
    buf: &mut Buffer,
    area: Rect,
    left: u16,
    client: &str,
    backend: Option<&str>,
    notice: Option<&NoticeWords>,
) -> Option<Rect> {
    if area.height == 0 {
        return None;
    }
    let row = row0_for_frame(
        client,
        backend,
        notice,
        usize::from(area.width),
        usize::from(left),
    )?;
    let tag = row.tag;
    let tag_width = u16::try_from(tag.width()).ok()?;
    let y = area.y;
    let right = area.x.saturating_add(area.width);
    // The row from the title's first column: the head and its gap are not read and not cleared.
    let start = area.x.saturating_add(left).min(right);
    let tag_x = right.saturating_sub(tag_width).max(start);
    // The notice stands one blank left of the tag. `row0_for_frame` gave one only where the title keeps its columns, so it never reaches
    // the head.
    let notice = row.notice.and_then(|text| {
        let width = u16::try_from(text_cells(&text)).ok()?;
        let x = tag_x.checked_sub(GAP_CELLS)?.checked_sub(width)?;
        (x >= start).then_some((text, x, width))
    });
    // The title may use the cells left of the gap before the notice, or before the tag when there is no notice.
    let first = notice.as_ref().map_or(tag_x, |(_, x, _)| *x);
    let limit = first.saturating_sub(GAP_CELLS).max(start);

    // Walk the row by glyph: where each starts and how far the text on it reaches.
    let mut glyphs: Vec<(u16, u16)> = Vec::new();
    let mut text_end = start;
    let mut x = start;
    while x < right {
        let symbol = buf[(x, y)].symbol();
        let width = u16::try_from(text_cells(symbol)).unwrap_or(1).max(1);
        if !symbol.trim().is_empty() && !decoration(symbol) {
            text_end = text_end.max(x.saturating_add(width));
        }
        glyphs.push((x, width));
        x = x.saturating_add(width);
    }

    // A title that reaches the gap ends in an ellipsis in the last cell before it.
    if text_end > limit && limit > start {
        let cut = limit - 1;
        let covering = glyphs
            .iter()
            .copied()
            .find(|&(start, width)| start <= cut && cut < start.saturating_add(width));
        let style = covering.map_or_else(Style::default, |(start, _)| buf[(start, y)].style());
        if let Some((start, _)) = covering {
            // A wide character that reaches the cut cell goes whole.
            for cell_x in start..cut {
                buf[(cell_x, y)].reset();
            }
        }
        let cell = &mut buf[(cut, y)];
        cell.reset();
        cell.set_symbol("\u{2026}");
        cell.set_style(style);
    }
    for cell_x in limit..right {
        buf[(cell_x, y)].reset();
    }

    let mut cell_x = tag_x;
    for span in tag.spans() {
        for c in span.content.chars() {
            let cell = &mut buf[(cell_x, y)];
            cell.reset();
            cell.set_char(c);
            cell.set_style(span.style);
            cell_x += 1;
        }
    }
    let (text, x, width) = notice?;
    let style = Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED);
    for (cell_x, c) in (x..).zip(text.chars()) {
        let cell = &mut buf[(cell_x, y)];
        cell.reset();
        cell.set_char(c);
        cell.set_style(style);
    }
    Some(Rect::new(x, y, width, 1))
}

/// The two numbers of a tag: a valid client version and a valid backend version or `?.??`.
fn versions_ok(client: &str, backend: &str) -> bool {
    version::is_valid(client)
        && (version::is_valid(backend) || backend == UNKNOWN || backend == version::MIXED)
}

/// Whether the whole of `text` is one of the three forms of a tag.
fn is_tag(text: &str) -> bool {
    let pair = |head: &str, mid: &str| {
        text.strip_prefix(head)
            .and_then(|rest| rest.split_once(mid))
            .is_some_and(|(client, backend)| versions_ok(client, backend))
    };
    let mixed = |head: &str, mid: &str| {
        text.strip_prefix(head)
            .and_then(|rest| rest.split_once(mid))
            .is_some_and(|(client, backend)| version::is_valid(client) && backend == version::MIXED)
    };
    pair("ham v", " | backend v")
        || pair("h v", " | b v")
        || mixed("ham v", " | backend ")
        || mixed("h v", " | b ")
        || text
            .split_once('|')
            .is_some_and(|(client, backend)| versions_ok(client, backend))
}

/// Splits a screen row into its title and the tag at its end, by the exact shape of the three forms: the tag starts the row or follows white
/// space and runs to the end of the row. The title is returned without the white space before the tag. A row with no tag comes back whole.
pub fn split_tag(row: &str) -> (&str, Option<&str>) {
    let trimmed = row.trim_end();
    let mut before_blank = true;
    for (at, c) in trimmed.char_indices() {
        if before_blank && is_tag(&trimmed[at..]) {
            return (trimmed[..at].trim_end(), Some(&trimmed[at..]));
        }
        before_blank = c.is_whitespace();
    }
    (row, None)
}

/// Whether the whole of `text` is one of the texts of the update notice (`ham_core::update::notice_words`): `v<version> available` or
/// `update`.
fn is_notice(text: &str) -> bool {
    text == "update"
        || text
            .strip_prefix('v')
            .and_then(|rest| rest.strip_suffix(" available"))
            .is_some_and(version::is_valid)
}

/// Splits a screen row into its title, the update notice and the tag, by their exact shapes, for drivers that compare titles: the tag as
/// [`split_tag`] reads it and, before it, a notice that starts the rest of the row or follows white space. A notice is read only in front
/// of a tag, because it is never drawn without one. The title comes back without the white space before the notice or the tag.
pub fn split_row0(row: &str) -> (&str, Option<&str>, Option<&str>) {
    let (title, tag) = split_tag(row);
    if tag.is_none() {
        return (title, None, None);
    }
    let mut before_blank = true;
    for (at, c) in title.char_indices() {
        if before_blank && is_notice(&title[at..]) {
            return (title[..at].trim_end(), Some(&title[at..]), tag);
        }
        before_blank = c.is_whitespace();
    }
    (title, None, tag)
}
