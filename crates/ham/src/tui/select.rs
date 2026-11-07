//! The geometry of an in-app text selection over the transcript. The module draws nothing and reads no
//! terminal: a [`Selection`] is two [`Pos`]itions (an absolute transcript line and a display cell), the text comes from
//! [`TextLines`] (one entry per laid-out row, content only: no gutter, role word or scrollbar), and a [`Viewport`] turns a screen cell
//! into a position. Painting works on a ratatui `Buffer`.
//!
//! * **Cells.** A position's column is a display cell. Both end cells are in the selection. A character takes the cells a terminal gives
//!   it ([`cell_width`], [`text_cells`]); a wide character, a letter with its combining marks and an emoji joined by zero-width joiners
//!   are one unit that is taken whole when either of its cells is in range.
//! * **Rows.** A row's [`Join`] says how it continues the row before it: a new line, a soft wrap at a space, or a soft wrap inside a
//!   word (a long URL cut at the width). The selected text puts back `\n`, a space or nothing, so a wrapped paragraph copies as it was
//!   written and a cut URL copies without a space.
//! * **Missing rows.** A selection that needs a row `TextLines` does not hold gives `None`, never a shorter text.
//! * **Folds.** A folded block contributes the rows that are on screen, so a selection across a fold copies the head row and not the
//!   hidden body (copying a whole block is a separate action).
//! * **Still current.** The rows of a selection change when the width changes, a fold opens above it or a block grows. A screen keeps
//!   [`Selection::fingerprint`] from when the selection was made and drops the selection when the fingerprint of the same rows differs.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use std::ops::Range;

/// The cells one character takes on its own: 0 for a control, a combining mark, a joiner or another zero-width character; 1 or 2.
pub fn cell_width(c: char) -> usize {
    if c.is_ascii() {
        return usize::from(!c.is_ascii_control());
    }
    let mut b = [0u8; 4];
    Span::raw(&*c.encode_utf8(&mut b)).width()
}

/// The display unit boundaries of `s`: `(first byte, end byte, first cell, cells)` for each unit. A unit is a character with the zero-width
/// characters after it, a joiner with the character it joins, and, at the start of a row, zero-width characters with the first real
/// character. Widths come from the same function ratatui draws with, so a unit's cells equal what the screen shows.
fn units(s: &str) -> Vec<(usize, usize, usize, usize)> {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    // (start byte, every character so far is zero-width)
    let mut cur: Option<(usize, bool)> = None;
    let mut after_joiner = false;
    for (i, c) in s.char_indices() {
        let zero = cell_width(c) == 0;
        cur = match cur {
            None => Some((i, zero)),
            Some((start, only_zero)) => {
                if zero || after_joiner || only_zero {
                    Some((start, only_zero && zero))
                } else {
                    spans.push((start, i));
                    Some((i, false))
                }
            }
        };
        after_joiner = c == '\u{200d}';
    }
    if let Some((start, _)) = cur {
        spans.push((start, s.len()));
    }
    let mut cell = 0usize;
    spans
        .into_iter()
        .map(|(a, b)| {
            let w = if s[a..b].is_ascii() {
                s[a..b].chars().map(cell_width).sum()
            } else {
                Span::raw(&s[a..b]).width()
            };
            let u = (a, b, cell, w);
            cell += w;
            u
        })
        .collect()
}

/// The display units of `s` as `(first byte, end byte, cells)`: what [`Selection`] takes whole and what a layout may not cut.
pub fn display_units(s: &str) -> Vec<(usize, usize, usize)> {
    units(s).into_iter().map(|(a, b, _, w)| (a, b, w)).collect()
}

/// The cells `s` takes on a terminal.
pub fn text_cells(s: &str) -> usize {
    if s.is_ascii() {
        return s.chars().map(cell_width).sum();
    }
    units(s).last().map_or(0, |(_, _, cell, w)| cell + w)
}

/// A place in the transcript: an absolute line (counted from the first row of the whole conversation) and a display cell of that row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

/// How a row continues the row before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Join {
    /// A new line of the text (also the first row).
    Hard,
    /// The text was wrapped at a space; the space is not in either row.
    Space,
    /// A long word was cut at the width; nothing was between the rows.
    Cut,
}

impl Join {
    fn separator(self) -> &'static str {
        match self {
            Join::Hard => "\n",
            Join::Space => " ",
            Join::Cut => "",
        }
    }
}

/// The laid-out rows of the transcript, content only. Row `n` of the transcript is `line(n)`; the first row held is [`TextLines::base`].
#[derive(Clone, Debug, Default)]
pub struct TextLines {
    base: usize,
    rows: Vec<(String, Join)>,
}

impl TextLines {
    /// No rows; the first one pushed is line `base`.
    pub fn new(base: usize) -> Self {
        Self {
            base,
            rows: Vec::new(),
        }
    }

    /// The rows `rows`, the first of them line `base`.
    pub fn from_rows<S: Into<String>>(
        base: usize,
        rows: impl IntoIterator<Item = (S, Join)>,
    ) -> Self {
        Self {
            base,
            rows: rows.into_iter().map(|(t, j)| (t.into(), j)).collect(),
        }
    }

    pub fn push(&mut self, text: impl Into<String>, join: Join) {
        self.rows.push((text.into(), join));
    }

    /// The rows on a drawn buffer: each row of `area` (cut to the buffer) is one line, starting at line `first_line`, read cell by cell
    /// (the cells behind a wide character are skipped) with trailing spaces cut. Every row is [`Join::Hard`], because the screen does not
    /// say how a row was wrapped; only the rows on screen are held, so a selection that reaches past them gives `None`. A transcript that
    /// can hand over its laid-out rows gives exact text and joins and should be preferred.
    pub fn from_buffer(buf: &Buffer, area: Rect, first_line: usize) -> Self {
        let area = area.intersection(buf.area);
        let mut rows = Vec::with_capacity(usize::from(area.height));
        for y in area.y..area.bottom() {
            let mut text = String::new();
            let mut x = area.x;
            while x < area.right() {
                let sym = buf[(x, y)].symbol();
                let w = if sym.is_ascii() {
                    sym.chars().map(cell_width).sum::<usize>()
                } else {
                    Span::raw(sym).width()
                };
                text.push_str(sym);
                x = x.saturating_add(u16::try_from(w.max(1)).unwrap_or(1));
            }
            rows.push((text.trim_end_matches(' ').to_string(), Join::Hard));
        }
        Self {
            base: first_line,
            rows,
        }
    }

    /// The number of the first row held.
    pub fn base(&self) -> usize {
        self.base
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn get(&self, line: usize) -> Option<&(String, Join)> {
        self.rows.get(line.checked_sub(self.base)?)
    }

    /// The text of line `line`, if held.
    pub fn line(&self, line: usize) -> Option<&str> {
        self.get(line).map(|(t, _)| t.as_str())
    }

    /// How line `line` continues the one before it, if held.
    pub fn join(&self, line: usize) -> Option<Join> {
        self.get(line).map(|(_, j)| *j)
    }

    /// Every row, joined by the way each continues the one before (the first row's join is not used).
    pub fn all_text(&self) -> String {
        let mut out = String::new();
        for (i, (t, j)) in self.rows.iter().enumerate() {
            if i > 0 {
                out.push_str(j.separator());
            }
            out.push_str(t);
        }
        out
    }

    /// A hash of lines `from..=to`: their text, how each joins the one before, and which are missing. Equal when nothing in those rows
    /// changed, whatever happened elsewhere in the transcript.
    pub fn fingerprint(&self, from: usize, to: usize) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut feed = |bytes: &[u8]| {
            for b in bytes {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        };
        let mut line = from;
        while line <= to {
            match self.get(line) {
                Some((t, j)) => {
                    feed(&[1, *j as u8]);
                    feed(t.as_bytes());
                }
                None => feed(&[0]),
            }
            feed(&[0x1f]);
            let Some(next) = line.checked_add(1) else {
                break;
            };
            line = next;
        }
        h
    }
}

/// The part of `s` that cells `from..=to` select: the units whose cells meet the range, from the first to the last of them.
fn slice_cells(s: &str, from: usize, to: usize) -> (usize, usize, Option<(usize, usize)>) {
    let us = units(s);
    let mut first: Option<usize> = None;
    let mut last: Option<usize> = None;
    for (i, (_, _, cell, w)) in us.iter().enumerate() {
        let hit = if *w > 0 {
            *cell <= to && cell.saturating_add(*w) > from
        } else {
            *cell >= from && *cell <= to
        };
        if hit {
            first.get_or_insert(i);
            last = Some(i);
        }
    }
    match (first, last) {
        (Some(a), Some(b)) => {
            let cells = (us[a].2, us[b].2 + us[b].3);
            (us[a].0, us[b].1, Some(cells))
        }
        _ => (0, 0, None),
    }
}

/// Two ends of a selection; both end cells are in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    anchor: Pos,
    head: Pos,
}

impl Selection {
    /// A selection of the one cell `anchor`.
    pub fn new(anchor: Pos) -> Self {
        Self {
            anchor,
            head: anchor,
        }
    }

    /// The selection a drag from `a` to `b` makes.
    pub fn from_points(a: Pos, b: Pos) -> Self {
        Self { anchor: a, head: b }
    }

    /// Moves the end the pointer holds.
    pub fn extend(&mut self, to: Pos) {
        self.head = to;
    }

    pub fn anchor(&self) -> Pos {
        self.anchor
    }

    pub fn head(&self) -> Pos {
        self.head
    }

    /// The ends in reading order.
    pub fn bounds(&self) -> (Pos, Pos) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Whether both ends are the same cell.
    pub fn is_single_cell(&self) -> bool {
        self.anchor == self.head
    }

    /// The selected text, or `None` when a row it needs is not in `lines`.
    pub fn text(&self, lines: &TextLines) -> Option<String> {
        let (s, e) = self.bounds();
        if lines.get(s.line).is_none() || lines.get(e.line).is_none() {
            return None;
        }
        let mut out = String::new();
        for line in s.line..=e.line {
            let (text, join) = lines.get(line)?;
            if line > s.line {
                out.push_str(join.separator());
            }
            let from = if line == s.line { s.col } else { 0 };
            let to = if line == e.line { e.col } else { usize::MAX };
            let (a, b, hit) = slice_cells(text, from, to);
            if hit.is_some() {
                out.push_str(&text[a..b]);
            }
        }
        Some(out)
    }

    /// The cells of row `line` the selection covers, for painting; `None` when it covers none (an empty row, a row outside it).
    pub fn cells_on(&self, lines: &TextLines, line: usize) -> Option<Range<usize>> {
        let (s, e) = self.bounds();
        if line < s.line || line > e.line {
            return None;
        }
        let (text, _) = lines.get(line)?;
        let from = if line == s.line { s.col } else { 0 };
        let to = if line == e.line { e.col } else { usize::MAX };
        let (_, _, cells) = slice_cells(text, from, to);
        cells.filter(|(a, b)| b > a).map(|(a, b)| a..b)
    }

    /// Reverses the selected cells that are on screen: row `i` of `content` shows line `first_line + i`. Nothing outside `content` is
    /// touched.
    pub fn paint(&self, lines: &TextLines, buf: &mut Buffer, content: Rect, first_line: usize) {
        let content = content.intersection(buf.area);
        let reverse = Style::default().add_modifier(Modifier::REVERSED);
        for i in 0..content.height {
            let Some(r) = self.cells_on(lines, first_line + usize::from(i)) else {
                continue;
            };
            let width = usize::from(content.width);
            let (a, b) = (r.start.min(width), r.end.min(width));
            if b > a {
                let w = u16::try_from(b - a).unwrap_or(u16::MAX);
                let x = content.x + u16::try_from(a).unwrap_or(u16::MAX);
                buf.set_style(Rect::new(x, content.y + i, w, 1), reverse);
            }
        }
    }

    /// The fingerprint of the rows this selection covers (see [`TextLines::fingerprint`]).
    pub fn fingerprint(&self, lines: &TextLines) -> u64 {
        let (s, e) = self.bounds();
        lines.fingerprint(s.line, e.line)
    }
}

/// Where the transcript's text sits on screen: `content` is the rectangle of the text columns (right of the gutter, left of the
/// scrollbar) and `first_line` the line shown on its first row.
#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    pub content: Rect,
    pub first_line: usize,
}

impl Viewport {
    /// The position under the screen cell `(x, y)`; a cell outside the rectangle is taken to its nearest edge (a drag past the pane
    /// still has an end), and a cell left of the text is column 0.
    pub fn pos(&self, x: u16, y: u16) -> Pos {
        let col =
            usize::from(x.saturating_sub(self.content.x)).min(usize::from(self.content.width));
        let row = y
            .saturating_sub(self.content.y)
            .min(self.content.height.saturating_sub(1));
        Pos {
            line: self.first_line + usize::from(row),
            col,
        }
    }
}
