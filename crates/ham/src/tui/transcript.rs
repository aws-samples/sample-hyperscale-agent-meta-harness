//! The transcript widget of the run chat view: draws a [`ChatModel`] into a `Rect` and keeps what a long conversation needs.
//!
//! * **Layout.** A role gutter of [`GUTTER`] columns, a fold-marker column, then the text from column [`CONTENT_X`]; the last column of the
//!   area is the scrollbar's. The gutter word comes from the block's structure ([`ChatModel::role_word`]); the text is content and is
//!   drawn only right of the gutter, so nothing in it can draw a role word, a border or a prompt.
//! * **Cache.** Each block is wrapped once per (block id, width, fold state, revision); a scroll lays out nothing, an appended or grown
//!   block lays out itself only ([`TranscriptState::wrap_computations`] counts the work, so tests count instead of timing). Prefix sums
//!   of the block heights and a `usize` scroll offset make any offset of a 100,000-line transcript a binary search.
//! * **Follow mode.** The newest block stays in view until the user scrolls up; then a `[ Jump to bottom ↓ n new ]` row shows how many
//!   blocks arrived since; reaching the end by hand follows again.
//! * **Safety.** Block text is cleaned again at layout (the model's constructors already did it): controls, escape sequences and bidi
//!   marks never reach the buffer, tabs become spaces, and nothing is written outside the given area.
//!
//! The widget takes the model by reference and the state by `&mut`, draws into the `Rect` it is given, and edits no other file of the
//! TUI: the chat view that owns the area wires keys, clicks and the area itself.

use super::markdown;
use super::select::{Join, Pos, Selection, TextLines, Viewport};
use ham_core::chat::{Block, BlockKind, ChatModel, Role};
use ham_core::notice::NoticeKind;
use ham_core::sanitize::{display_line, display_text};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::StatefulWidget;
use std::collections::HashMap;

/// Columns of the role word (six letters and a space).
pub const GUTTER: u16 = 7;
/// The column where text starts: the gutter and the two-column fold marker.
pub const CONTENT_X: u16 = 9;
const TAB_STOP: usize = 4;
/// The id of the pseudo-block that carries the "progress feed only" label.
const LABEL_ID: &str = "\u{b7}label";

/// The look of a search match, and of the match the search stands on.
pub const OTHER_MARK: Style = Style::new().fg(Color::Black).bg(Color::Yellow);
pub const CURRENT_MARK: Style = Style::new().fg(Color::Black).bg(Color::LightRed);

/// A stretch of a block's text to paint as a search match. The text is counted in non-blank characters, head first: wrapping drops
/// blanks and nothing else, so the n-th non-blank character is the same character in the block's text and in its laid-out rows, whatever
/// the width. Marks of one block are in order and do not overlap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    /// Non-blank characters before the match.
    pub nb_start: usize,
    /// Non-blank characters in the match.
    pub nb_len: usize,
    /// The match the search stands on.
    pub current: bool,
}

// ---- width and wrapping ------------------------------------------------------------------------------------------------------------

/// The columns a character takes.
fn char_width(c: char) -> usize {
    if c.is_ascii() {
        return 1;
    }
    let mut b = [0u8; 4];
    Span::raw(&*c.encode_utf8(&mut b)).width()
}

fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// `s` cut to `max` columns, with an ellipsis in place of what is cut.
fn truncate_width(s: &str, max: usize) -> String {
    if str_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if w + cw + 1 > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('\u{2026}');
    out
}

fn expand_tabs(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut col = 0usize;
    for c in line.chars() {
        if c == '\t' {
            let n = TAB_STOP - (col % TAB_STOP);
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(c);
            col += char_width(c);
        }
    }
    out
}

fn wrap_line(line: &str, width: usize, out: &mut Vec<(String, Join)>) {
    let expanded = expand_tabs(line);
    let body = expanded.trim();
    if body.is_empty() {
        out.push((String::new(), Join::Hard));
        return;
    }
    let indent = expanded
        .chars()
        .take_while(|c| *c == ' ')
        .count()
        .min(width / 2);
    let pad = " ".repeat(indent);
    let mut cur = pad.clone();
    let mut cur_w = indent;
    // The byte index in `cur` of the last space that follows a word: where a row may break.
    let mut brk: Option<usize> = None;
    // How the row being built continues the one before it: the first row starts a line; a row after a break at a space follows a space;
    // a row after a cut inside a word follows nothing.
    let mut join = Join::Hard;
    for c in body.chars() {
        let w = char_width(c);
        if c == ' ' {
            if cur.trim_start().is_empty() {
                // A space at the start of a continuation row is the break itself.
                continue;
            }
            if cur_w + w > width {
                // The row is full and this space ends it.
                out.push((cur.trim_end().to_string(), join));
                join = Join::Space;
                cur = pad.clone();
                cur_w = indent;
                brk = None;
                continue;
            }
        }
        while cur_w + w > width && !cur.trim_start().is_empty() {
            match brk.take() {
                Some(i) => {
                    let rest = cur[i + 1..].to_string();
                    cur.truncate(i);
                    out.push((cur.trim_end().to_string(), join));
                    join = Join::Space;
                    cur = format!("{pad}{rest}");
                }
                None => {
                    out.push((std::mem::take(&mut cur), join));
                    join = Join::Cut;
                    cur = pad.clone();
                }
            }
            cur_w = str_width(&cur);
            brk = cur[pad.len()..].rfind(' ').map(|i| i + pad.len());
        }
        if c == ' ' && !cur.trim_start().is_empty() {
            brk = Some(cur.len());
        }
        cur.push(c);
        cur_w += w;
    }
    out.push((cur.trim_end().to_string(), join));
}

/// `text` wrapped to `width` columns: broken at spaces (a longer word is cut at the width), the indentation of a line kept on its
/// continuation rows, tabs expanded to four-column stops, trailing spaces cut. Every `\n` starts a row; an empty line is an empty row.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    wrap_text_joined(text, width)
        .into_iter()
        .map(|(row, _)| row)
        .collect()
}

/// [`wrap_text`] with the way each row continues the one before it: a new line of the text, a wrap at a space, or a cut inside a word.
/// A selection puts that back when it copies.
pub fn wrap_text_joined(text: &str, width: usize) -> Vec<(String, Join)> {
    if width == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in text.split('\n') {
        wrap_line(line, width, &mut out);
    }
    out
}

// ---- rows -------------------------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Plain,
    Dim,
    Head,
    Added,
    Removed,
    Hunk,
    Done,
    Failed,
    Running,
}

impl Tone {
    fn style(self) -> Style {
        match self {
            Tone::Plain => Style::default(),
            Tone::Dim => Style::default().fg(Color::DarkGray),
            Tone::Head => Style::default().add_modifier(Modifier::BOLD),
            Tone::Added => Style::default().fg(Color::Green),
            Tone::Removed => Style::default().fg(Color::Red),
            Tone::Hunk => Style::default().fg(Color::Cyan),
            Tone::Done => Style::default().fg(Color::Green),
            Tone::Failed => Style::default().fg(Color::Red),
            Tone::Running => Style::default().fg(Color::Yellow),
        }
    }
}

#[derive(Clone, Debug)]
struct Row {
    /// The row as plain text: what a search counts in, what a selection copies.
    text: String,
    tone: Tone,
    join: Join,
    /// The row styled by the Markdown renderer; `None` for a row drawn in the one `tone` of its block.
    styled: Option<Line<'static>>,
}

/// The characters of a styled line, spans joined.
fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// An engine message is Markdown and a diff block is a diff; every other block is the text as written (the person's words, tool output,
/// thoughts, notices and cards).
fn is_markdown(b: &Block) -> bool {
    b.kind == BlockKind::Diff
        || (b.kind == BlockKind::Message
            && b.effective_role() == Role::Engine
            && !plain_prose(&b.body))
}

/// Text with nothing in it that Markdown would read: no marker character, no line that starts with a space, a tab, a bullet, a rule or a
/// number and its dot. Such a message is drawn as wrapped text, which is what its Markdown rendering would be, at a fraction of the cost: a
/// conversation of ten thousand plain messages lays out within the draw budget.
fn plain_prose(text: &str) -> bool {
    if text.chars().any(|c| {
        matches!(
            c,
            '#' | '*'
                | '_'
                | '`'
                | '['
                | ']'
                | '<'
                | '>'
                | '~'
                | '|'
                | '\\'
                | '\t'
                | '&'
                | '\u{1b}'
        ) || c.is_control() && c != '\n'
    }) {
        return false;
    }
    text.lines().all(|l| {
        let t = l.trim_end();
        let digits = t.chars().take_while(char::is_ascii_digit).count();
        !t.starts_with([' ', '-', '+', '=', ':', '!'])
            && !(digits > 0 && t[digits..].starts_with(['.', ')']))
    })
}

/// The rows of the body of `b` at `cw` columns. A Markdown block goes through `tui::markdown` unless `plain` (a search is marking the block:
/// its offsets count the characters of the text as written, so the rows must be that text); the rest is wrapped as written, a diff line
/// in the colour of its first character.
fn body_rows(b: &Block, body: &str, cw: usize, tone: Tone, plain: bool) -> Vec<Row> {
    if !plain && is_markdown(b) {
        let rows = markdown::render_block(b, cw);
        if !rows.is_empty() {
            return rows
                .into_iter()
                .map(|r| Row {
                    text: line_text(&r.line),
                    tone: Tone::Plain,
                    join: r.join,
                    styled: Some(r.line),
                })
                .collect();
        }
    }
    wrap_text_joined(body, cw)
        .into_iter()
        .map(|(text, join)| {
            let tone = if b.kind == BlockKind::Diff {
                match text.chars().next() {
                    Some('+') => Tone::Added,
                    Some('-') => Tone::Removed,
                    Some('@') => Tone::Hunk,
                    _ => tone,
                }
            } else {
                tone
            };
            Row {
                text,
                tone,
                join,
                styled: None,
            }
        })
        .collect()
}

/// One block laid out at one width and fold state.
#[derive(Clone, Debug)]
struct Laid {
    width: usize,
    folded: bool,
    rev: u64,
    foldable: bool,
    /// The block was laid out as written even though it is Markdown, because a search marks it.
    plain: bool,
    rows: Vec<Row>,
    /// Non-blank characters in the rows before each row.
    nb_before: Vec<usize>,
    status: Option<(String, Tone)>,
}

/// The right-aligned word of a tool call.
fn tool_status(b: &Block) -> Option<(String, Tone)> {
    if b.kind != BlockKind::ToolCall {
        return None;
    }
    let s = b.status.as_deref()?;
    Some(match s {
        "completed" => ("done".to_string(), Tone::Done),
        "failed" => ("failed".to_string(), Tone::Failed),
        "in_progress" => ("running".to_string(), Tone::Running),
        "pending" => ("pending".to_string(), Tone::Dim),
        other => (truncate_width(&display_line(other), 12), Tone::Dim),
    })
}

fn is_foldable(b: &Block) -> bool {
    !b.head.is_empty() && !b.body.is_empty()
}

/// Lays `b` out in `cw` columns. The text is cleaned again here: a block built without the constructors is still safe to draw.
fn layout_block(b: &Block, folded: bool, cw: usize, plain: bool) -> Laid {
    let head = display_line(&b.head);
    let body = display_text(&b.body);
    let foldable = !head.is_empty() && !body.is_empty();
    let dim = matches!(
        b.kind,
        BlockKind::Thought
            | BlockKind::Usage
            | BlockKind::Gap(_)
            | BlockKind::Unknown
            | BlockKind::Notice(NoticeKind::GoalQuiet)
    );
    // The engine's BLOCKED line stands out; the quiet notes of the goal loop are dim.
    let body_tone = if matches!(b.kind, BlockKind::Notice(NoticeKind::GoalBlocked)) {
        Tone::Head
    } else if dim {
        Tone::Dim
    } else {
        Tone::Plain
    };
    let status = tool_status(b);
    let mut rows: Vec<Row> = Vec::new();
    if head.is_empty() {
        rows.extend(body_rows(b, &body, cw, body_tone, plain));
    } else {
        let reserve = status.as_ref().map_or(0, |(s, _)| str_width(s) + 1);
        let avail = cw.saturating_sub(reserve);
        let open = foldable && !folded;
        let head_tone = if matches!(b.kind, BlockKind::ToolCall) {
            Tone::Plain
        } else {
            Tone::Head
        };
        if open {
            rows.extend(
                wrap_text_joined(&head, avail)
                    .into_iter()
                    .map(|(text, join)| Row {
                        text,
                        tone: head_tone,
                        join,
                        styled: None,
                    }),
            );
        } else {
            rows.push(Row {
                text: truncate_width(&head, avail),
                tone: head_tone,
                join: Join::Hard,
                styled: None,
            });
        }
        if open {
            rows.extend(body_rows(b, &body, cw, body_tone, plain));
        }
    }
    if rows.is_empty() {
        rows.push(Row {
            text: String::new(),
            tone: Tone::Plain,
            join: Join::Hard,
            styled: None,
        });
    }
    let mut seen = 0usize;
    let nb_before = rows
        .iter()
        .map(|r| {
            let before = seen;
            seen += nonblank(&r.text);
            before
        })
        .collect();
    Laid {
        width: cw,
        folded,
        rev: b.rev,
        foldable,
        plain,
        rows,
        nb_before,
        status,
    }
}

/// Characters of `text` that are not blank: the count a search mark is measured in.
fn nonblank(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count()
}

/// The row that holds the `k`-th (from 0) non-blank character, given the non-blank characters before each row; the last row when `k` is
/// past the end.
fn row_of_nonblank(nb_before: &[usize], k: usize) -> usize {
    nb_before.partition_point(|&n| n <= k).saturating_sub(1)
}

/// Paints the cells of `text` (drawn from column `x0` of row `y`, `cw` columns wide, the first character being the `nb_before + 1`-th
/// non-blank one of its block) that belong to a mark. A blank inside a mark is painted too, so a match across words reads as one.
/// `marks` are in order and do not overlap, so each character finds its mark by bisection.
fn paint_marks(
    buf: &mut Buffer,
    x0: u16,
    y: u16,
    cw: usize,
    text: &str,
    nb_before: usize,
    marks: &[Mark],
) {
    let mut col = 0usize;
    let mut nb = nb_before;
    for c in text.chars() {
        let w = char_width(c);
        if col + w > cw {
            break;
        }
        let blank = c.is_whitespace();
        let at = marks.partition_point(|m| m.nb_start + m.nb_len <= nb);
        let hit = marks.get(at).filter(|m| {
            if blank {
                m.nb_start < nb
            } else {
                m.nb_start <= nb
            }
        });
        if let Some(m) = hit
            && w > 0
        {
            let style = if m.current { CURRENT_MARK } else { OTHER_MARK };
            let x = x0.saturating_add(u16::try_from(col).unwrap_or(u16::MAX));
            let w16 = u16::try_from(w).unwrap_or(1);
            buf.set_style(Rect::new(x, y, w16, 1), style);
        }
        if !blank {
            nb += 1;
        }
        col += w;
    }
}

// ---- state ------------------------------------------------------------------------------------------------------------------------

/// Where a pointer press landed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub block_id: String,
    /// The index of the block in the model.
    pub index: usize,
    /// The row inside the block (0 is its first).
    pub row: usize,
    /// The press is on the fold marker of a foldable block.
    pub on_fold_marker: bool,
}

#[derive(Clone, Debug)]
struct Slot {
    id: String,
    /// Index in the model's blocks; `None` for the label row.
    index: Option<usize>,
    /// The first line of the block.
    start: usize,
    height: usize,
    foldable: bool,
}

/// A text selection held by the transcript: the two ends, and the proof that the rows under it are still the ones that were selected.
#[derive(Clone, Copy, Debug)]
struct Chosen {
    sel: Selection,
    /// The fingerprint of the selected rows when the selection was last looked at; `None` until the next draw takes it.
    finger: Option<u64>,
    /// The layout count (`TranscriptState::wraps`) when `finger` was taken: the rows can only have changed when it moved.
    wraps: u64,
}

/// What the transcript remembers between frames: the scroll position, follow mode, the fold choices of the user, the selected block and
/// the wrap cache.
#[derive(Clone, Debug)]
pub struct TranscriptState {
    top: usize,
    follow: bool,
    max_top: usize,
    height: usize,
    total: usize,
    /// How many blocks the model had when follow mode paused.
    paused_at: usize,
    /// How many blocks the model had at the last draw.
    seen_blocks: usize,
    folds: HashMap<String, bool>,
    selected: Option<String>,
    reveal: Option<String>,
    /// A search jump: the block and the non-blank character to bring to the middle of the view at the next draw.
    reveal_text: Option<(String, usize)>,
    marks: HashMap<String, Vec<Mark>>,
    /// The block at the top of the view and how many lines of it are above the view, kept across a change of the model that adds blocks
    /// above it (older history loaded): the next draw puts the view back on that block.
    anchor: Option<(String, usize)>,
    cache: HashMap<String, Laid>,
    slots: Vec<Slot>,
    area: Rect,
    wraps: u64,
    selection: Option<Chosen>,
}

impl Default for TranscriptState {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptState {
    pub fn new() -> Self {
        Self {
            top: 0,
            follow: true,
            max_top: 0,
            height: 0,
            total: 0,
            paused_at: 0,
            seen_blocks: 0,
            folds: HashMap::new(),
            selected: None,
            reveal: None,
            reveal_text: None,
            marks: HashMap::new(),
            anchor: None,
            cache: HashMap::new(),
            slots: Vec::new(),
            area: Rect::default(),
            wraps: 0,
            selection: None,
        }
    }

    // ---- selection -----------------------------------------------------------------------------------------------

    /// Where the text of the transcript sits on screen at the last draw: the columns right of the gutter and left of the scrollbar, and the
    /// line shown on the first row.
    pub fn viewport(&self) -> Viewport {
        let a = self.area;
        let x = a.x + CONTENT_X.min(a.width);
        Viewport {
            content: Rect::new(x, a.y, a.width.saturating_sub(CONTENT_X + 1), a.height),
            first_line: self.top,
        }
    }

    /// The laid-out rows `from..=to` (absolute lines), content only, each with the way it continues the one before it. Rows past the end are
    /// not held.
    pub fn text_lines(&self, from: usize, to: usize) -> TextLines {
        let mut lines = TextLines::new(from);
        if self.total == 0 || from > to {
            return lines;
        }
        let to = to.min(self.total - 1);
        let mut i = self
            .slots
            .partition_point(|s| s.start <= from)
            .saturating_sub(1);
        let mut line = from;
        while line <= to && i < self.slots.len() {
            let slot = &self.slots[i];
            let Some(laid) = self.cache.get(&slot.id) else {
                break;
            };
            for row in laid.rows.iter().skip(line.saturating_sub(slot.start)) {
                if line > to {
                    break;
                }
                lines.push(row.text.clone(), row.join);
                line += 1;
            }
            i += 1;
        }
        lines
    }

    /// A selection begins at the cell `(x, y)` of the last draw.
    pub fn select_start(&mut self, x: u16, y: u16) {
        let at = self.viewport().pos(x, y);
        self.selection = Some(Chosen {
            sel: Selection::new(at),
            finger: None,
            wraps: self.wraps,
        });
    }

    /// The end the pointer holds moves to the cell `(x, y)`.
    pub fn select_extend(&mut self, x: u16, y: u16) {
        let at: Pos = self.viewport().pos(x, y);
        if let Some(c) = self.selection.as_mut() {
            c.sel.extend(at);
            c.finger = None;
        }
    }

    /// The selection is dropped.
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    pub fn has_selection(&self) -> bool {
        self.selection.is_some()
    }

    /// The selected text as it was written, or `None` when there is no selection or a row it needs is no longer held.
    pub fn selection_text(&self) -> Option<String> {
        let sel = self.selection.as_ref()?.sel;
        let (a, b) = sel.bounds();
        sel.text(&self.text_lines(a.line, b.line))
    }

    /// Looks at the selection after a layout: the rows under it must be the ones that were selected, else it is dropped; then paints it.
    fn settle_selection(&mut self, buf: &mut Buffer) {
        let Some(c) = self.selection else {
            return;
        };
        let (a, b) = c.sel.bounds();
        if a.line >= self.total {
            self.selection = None;
            return;
        }
        if c.finger.is_none() || c.wraps != self.wraps {
            let finger = c.sel.fingerprint(&self.text_lines(a.line, b.line));
            if c.finger.is_some_and(|f| f != finger) {
                self.selection = None;
                return;
            }
            if let Some(held) = self.selection.as_mut() {
                held.finger = Some(finger);
                held.wraps = self.wraps;
            }
        }
        let visible = self.text_lines(self.top, self.top + self.height.saturating_sub(1));
        let view = self.viewport();
        c.sel.paint(&visible, buf, view.content, view.first_line);
    }

    /// Blocks laid out since the state was made: a block counts each time it is wrapped again (new, changed, resized, folded).
    pub fn wrap_computations(&self) -> u64 {
        self.wraps
    }

    pub fn is_following(&self) -> bool {
        self.follow
    }

    /// Blocks that arrived while follow mode was paused.
    pub fn new_blocks(&self) -> usize {
        if self.follow {
            0
        } else {
            self.seen_blocks.saturating_sub(self.paused_at)
        }
    }

    /// The first visible line (0-based) at the last draw.
    pub fn top_line(&self) -> usize {
        self.top
    }

    /// Lines in the whole transcript at the last draw.
    pub fn total_lines(&self) -> usize {
        self.total
    }

    /// The whole transcript fits in the area at the last draw: nothing can be scrolled.
    pub fn fits(&self) -> bool {
        self.height > 0 && self.total <= self.height
    }

    /// Remembers the block at the top of the view, for a change of the model that puts older blocks above it (the next draw puts the view
    /// back on that block, so history that loads above does not move what the person is reading). Nothing is remembered in follow mode,
    /// which stays at the end.
    pub fn keep_view(&mut self) {
        if self.follow {
            self.anchor = None;
            return;
        }
        let top = self.top;
        self.anchor = self
            .slots
            .iter()
            .rev()
            .find(|s| s.start <= top)
            .map(|s| (s.id.clone(), top - s.start));
    }

    /// `lines 41-64 of 480`, the range on show; `no lines` for an empty transcript.
    pub fn range_text(&self) -> String {
        if self.total == 0 {
            return "no lines".to_string();
        }
        let last = (self.top + self.height).min(self.total);
        format!("lines {}-{} of {}", self.top + 1, last, self.total)
    }

    fn pause(&mut self) {
        if self.follow {
            self.follow = false;
            self.top = self.max_top;
            self.paused_at = self.seen_blocks;
        }
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.pause();
        self.top = self.top.saturating_sub(lines);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        if self.follow {
            return;
        }
        self.top = self.top.saturating_add(lines);
    }

    pub fn page_up(&mut self) {
        self.scroll_up(self.height.saturating_sub(1).max(1));
    }

    pub fn page_down(&mut self) {
        self.scroll_down(self.height.saturating_sub(1).max(1));
    }

    pub fn to_top(&mut self) {
        self.pause();
        self.top = 0;
    }

    /// The end of the transcript, and follow mode again.
    pub fn to_bottom(&mut self) {
        self.follow = true;
    }

    /// Shows the transcript from `line` (clamped at the next draw; the end of it is follow mode).
    pub fn scroll_to_line(&mut self, line: usize) {
        self.pause();
        self.top = line;
    }

    fn effective_fold(&self, b: &Block) -> bool {
        self.folds.get(&b.id).copied().unwrap_or(b.folded)
    }

    /// Opens or folds the block `id`; a block without a head and a body has nothing to fold.
    pub fn toggle_fold(&mut self, model: &ChatModel, id: &str) {
        if let Some(b) = model.blocks.iter().find(|b| b.id == id)
            && is_foldable(b)
        {
            let now = self.effective_fold(b);
            self.folds.insert(b.id.clone(), !now);
        }
    }

    pub fn toggle_selected_fold(&mut self, model: &ChatModel) {
        if let Some(id) = self.selected.clone() {
            self.toggle_fold(model, &id);
        }
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Whether block `id` is shown folded; `None` when there is no such block or it has nothing to fold.
    pub fn is_folded(&self, model: &ChatModel, id: &str) -> Option<bool> {
        let b = model.blocks.iter().find(|b| b.id == id)?;
        is_foldable(b).then(|| self.effective_fold(b))
    }

    /// Opens block `id` if it is folded; `true` when it changed.
    pub fn open_fold(&mut self, model: &ChatModel, id: &str) -> bool {
        self.set_fold(model, id, false)
    }

    /// Folds block `id` if it is open; `true` when it changed.
    pub fn close_fold(&mut self, model: &ChatModel, id: &str) -> bool {
        self.set_fold(model, id, true)
    }

    fn set_fold(&mut self, model: &ChatModel, id: &str, folded: bool) -> bool {
        if self.is_folded(model, id) == Some(!folded) {
            self.folds.insert(id.to_string(), folded);
            return true;
        }
        false
    }

    /// At the next draw, shows the row that holds the `nonblank_before + 1`-th non-blank character of block `id` (its head first) in the
    /// middle of the view, pausing follow mode. A block that is not in the model is ignored; a count past the end shows the last row.
    pub fn reveal_text(&mut self, id: &str, nonblank_before: usize) {
        self.reveal_text = Some((id.to_string(), nonblank_before));
    }

    /// Forgets a jump that [`reveal_text`](Self::reveal_text) queued and the next draw has not made yet (a cancel that follows the jump
    /// before any draw must not be undone by it).
    pub fn cancel_reveal_text(&mut self) {
        self.reveal_text = None;
    }

    /// Replaces the search marks: the matches to paint, by block id.
    pub fn set_marks(&mut self, marks: HashMap<String, Vec<Mark>>) {
        self.marks = marks;
    }

    pub fn clear_marks(&mut self) {
        self.marks.clear();
    }

    /// Moves the selection to the next block (the first when none is selected); the next draw scrolls it into view.
    pub fn select_next(&mut self, model: &ChatModel) {
        let n = model.blocks.len();
        if n == 0 {
            return;
        }
        let at = self
            .selected
            .as_deref()
            .and_then(|id| model.blocks.iter().position(|b| b.id == id));
        let next = match at {
            Some(i) => (i + 1).min(n - 1),
            None => 0,
        };
        self.select(model.blocks[next].id.clone());
    }

    /// Moves the selection to the previous block (the last when none is selected).
    pub fn select_prev(&mut self, model: &ChatModel) {
        let n = model.blocks.len();
        if n == 0 {
            return;
        }
        let at = self
            .selected
            .as_deref()
            .and_then(|id| model.blocks.iter().position(|b| b.id == id));
        let prev = match at {
            Some(i) => i.saturating_sub(1),
            None => n - 1,
        };
        self.select(model.blocks[prev].id.clone());
    }

    fn select(&mut self, id: String) {
        self.selected = Some(id.clone());
        self.reveal = Some(id);
    }

    /// The slot and the row inside it under the point `(x, y)` of the last draw.
    fn locate(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        let a = self.area;
        if x < a.x || x >= a.right() || y < a.y || y >= a.bottom() {
            return None;
        }
        let line = self.top + usize::from(y - a.y);
        if line >= self.total {
            return None;
        }
        let i = self
            .slots
            .partition_point(|s| s.start <= line)
            .checked_sub(1)?;
        Some((i, line - self.slots[i].start))
    }

    /// The block under the point `(x, y)` of the last draw, if the point is on a block.
    pub fn hit(&self, x: u16, y: u16) -> Option<Hit> {
        let (i, row) = self.locate(x, y)?;
        let slot = &self.slots[i];
        Some(Hit {
            block_id: slot.id.clone(),
            index: slot.index?,
            row,
            on_fold_marker: row == 0 && slot.foldable && x == self.area.x + GUTTER,
        })
    }

    /// The head rows of the foldable blocks that are on screen, as the rectangle a press toggles them from (the marker and the text of the head,
    /// right of the gutter) and the block id. A block whose head has scrolled out of view has none.
    pub fn fold_heads(&self) -> Vec<(Rect, String)> {
        let a = self.area;
        let mut out = Vec::new();
        for slot in &self.slots {
            if slot.index.is_none() || !slot.foldable {
                continue;
            }
            let Some(row) = slot.start.checked_sub(self.top) else {
                continue;
            };
            if row >= usize::from(a.height) {
                continue;
            }
            let y = a.y + u16::try_from(row).unwrap_or(u16::MAX);
            out.push((
                Rect::new(a.x + GUTTER, y, a.width.saturating_sub(GUTTER), 1),
                slot.id.clone(),
            ));
        }
        out
    }

    /// A press at `(x, y)`: it folds or opens a foldable block when it lands on the block's head row (the marker or the text of it).
    /// `true` when something changed.
    pub fn click(&mut self, model: &ChatModel, x: u16, y: u16) -> bool {
        let Some((i, row)) = self.locate(x, y) else {
            return false;
        };
        let slot = &self.slots[i];
        if slot.index.is_some() && slot.foldable && row == 0 && x >= self.area.x + GUTTER {
            let id = slot.id.clone();
            self.toggle_fold(model, &id);
            return true;
        }
        false
    }
}

// ---- the widget -------------------------------------------------------------------------------------------------------------------

/// The transcript of a [`ChatModel`], drawn with a [`TranscriptState`].
#[derive(Debug)]
pub struct Transcript<'a> {
    model: &'a ChatModel,
}

impl<'a> Transcript<'a> {
    pub fn new(model: &'a ChatModel) -> Self {
        Self { model }
    }
}

fn role_style(role: Role) -> Style {
    match role {
        Role::You => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        Role::Engine => Style::default().fg(Color::Cyan),
        Role::Tool => Style::default().fg(Color::DarkGray),
        Role::Notice => Style::default().fg(Color::Yellow),
    }
}

impl StatefulWidget for Transcript<'_> {
    type State = TranscriptState;

    fn render(self, area: Rect, buf: &mut Buffer, st: &mut TranscriptState) {
        let area = area.intersection(buf.area);
        st.area = area;
        st.height = usize::from(area.height);
        if area.width <= CONTENT_X + 1 || area.height == 0 {
            st.slots.clear();
            st.total = 0;
            st.max_top = 0;
            return;
        }
        let cw = usize::from(area.width - CONTENT_X - 1);
        let model = self.model;

        // Layout: every block at this width, from the cache when nothing about it changed.
        let label = model
            .label()
            .map(|l| Block::notice(LABEL_ID, NoticeKind::RunStatus, l));
        let entries: Vec<(Option<usize>, &Block)> = label
            .iter()
            .map(|b| (None, b))
            .chain(model.blocks.iter().enumerate().map(|(i, b)| (Some(i), b)))
            .collect();
        st.slots.clear();
        let mut start = 0usize;
        for (index, b) in &entries {
            let folded = st.effective_fold(b);
            // A block a search marks is laid out as written, so that the marks sit on the characters they count.
            let plain = st.marks.contains_key(&b.id);
            let fresh = st.cache.get(&b.id).is_some_and(|l| {
                l.width == cw && l.folded == folded && l.rev == b.rev && l.plain == plain
            });
            if !fresh {
                st.wraps += 1;
                st.cache
                    .insert(b.id.clone(), layout_block(b, folded, cw, plain));
            }
            let laid = &st.cache[&b.id];
            st.slots.push(Slot {
                id: b.id.clone(),
                index: *index,
                start,
                height: laid.rows.len(),
                foldable: laid.foldable,
            });
            start += laid.rows.len();
        }
        if st.cache.len() > entries.len() {
            let keep: std::collections::HashSet<&str> =
                entries.iter().map(|(_, b)| b.id.as_str()).collect();
            st.cache.retain(|id, _| keep.contains(id.as_str()));
        }
        st.total = start;
        st.seen_blocks = model.blocks.len();
        st.max_top = st.total.saturating_sub(st.height);

        // The scroll position: follow mode sits at the end; a revealed block is brought into view; a position at the end follows again.
        if let Some((id, above)) = st.anchor.take()
            && !st.follow
            && let Some(s) = st.slots.iter().find(|s| s.id == id)
        {
            st.top = s.start + above;
        }
        if let Some(id) = st.reveal.take()
            && let Some(s) = st.slots.iter().find(|s| s.id == id)
        {
            let (a, h) = (s.start, s.height);
            if st.follow {
                st.pause();
            }
            if a < st.top {
                st.top = a;
            } else if a + h > st.top + st.height {
                st.top = (a + h).saturating_sub(st.height).min(a);
            }
        }
        if let Some((id, k)) = st.reveal_text.take()
            && let Some(s) = st.slots.iter().find(|s| s.id == id)
        {
            let row = st
                .cache
                .get(&id)
                .map_or(0, |l| row_of_nonblank(&l.nb_before, k));
            let line = s.start + row.min(s.height.saturating_sub(1));
            if st.follow {
                st.pause();
            }
            st.top = line.saturating_sub(st.height / 2);
        }
        if st.follow {
            st.top = st.max_top;
        } else if st.top >= st.max_top {
            st.top = st.max_top;
            st.follow = true;
        }

        // Draw the visible lines.
        let bar_x = area.right() - 1;
        let mut slot_i = st
            .slots
            .partition_point(|s| s.start <= st.top)
            .saturating_sub(1);
        for i in 0..st.height {
            let line = st.top + i;
            if line >= st.total {
                break;
            }
            while slot_i + 1 < st.slots.len() && st.slots[slot_i + 1].start <= line {
                slot_i += 1;
            }
            let slot = &st.slots[slot_i];
            let Some((_, block)) = entries.iter().find(|(_, b)| b.id == slot.id).copied() else {
                continue;
            };
            let Some(laid) = st.cache.get(&slot.id) else {
                continue;
            };
            let row = &laid.rows[line - slot.start];
            let y = area.y + u16::try_from(i).unwrap_or(u16::MAX);
            let first = line == slot.start;
            if slot.id == LABEL_ID {
                buf.set_stringn(
                    area.x,
                    y,
                    &row.text,
                    usize::from(area.width - 1),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                );
                continue;
            }
            if first {
                let role = block.effective_role();
                let word = model.role_word(block);
                buf.set_stringn(area.x, y, &word, usize::from(GUTTER) - 1, role_style(role));
                if st.selected.as_deref() == Some(block.id.as_str()) {
                    buf.set_style(
                        Rect::new(area.x, y, GUTTER, 1),
                        Style::default().add_modifier(Modifier::REVERSED),
                    );
                }
                if laid.foldable {
                    let mark = if laid.folded { "\u{25b8}" } else { "\u{25be}" };
                    buf.set_stringn(area.x + GUTTER, y, mark, 1, Style::default());
                }
            }
            match &row.styled {
                Some(line) => {
                    buf.set_line(
                        area.x + CONTENT_X,
                        y,
                        line,
                        u16::try_from(cw).unwrap_or(u16::MAX),
                    );
                }
                None => {
                    buf.set_stringn(area.x + CONTENT_X, y, &row.text, cw, row.tone.style());
                }
            }
            if let Some(marks) = st.marks.get(&slot.id) {
                let nb = laid.nb_before[line - slot.start];
                paint_marks(buf, area.x + CONTENT_X, y, cw, &row.text, nb, marks);
            }
            if first && let Some((s, tone)) = &laid.status {
                let w = u16::try_from(str_width(s)).unwrap_or(u16::MAX);
                let x = bar_x.saturating_sub(w);
                buf.set_stringn(x, y, s, usize::from(w), tone.style());
            }
        }

        // The selection, reversed over the text it covers.
        st.settle_selection(buf);

        // The scrollbar column.
        if st.total > st.height {
            let h = st.height;
            let thumb_len = (h * h / st.total).max(1).min(h);
            let span = h - thumb_len;
            let thumb_at = (st.top * span).checked_div(st.max_top).unwrap_or(0);
            for i in 0..h {
                let on = i >= thumb_at && i < thumb_at + thumb_len;
                buf.set_stringn(
                    bar_x,
                    area.y + u16::try_from(i).unwrap_or(u16::MAX),
                    if on { "\u{2503}" } else { "\u{2502}" },
                    1,
                    Style::default().fg(Color::DarkGray),
                );
            }
        }

        // The jump row: follow mode is paused and the end is out of view.
        if !st.follow && st.top < st.max_top {
            let n = st.new_blocks();
            let text = if n > 0 {
                format!("[ Jump to bottom \u{2193} {n} new ]")
            } else {
                "[ Jump to bottom \u{2193} ]".to_string()
            };
            let w = u16::try_from(str_width(&text)).unwrap_or(u16::MAX);
            if w <= area.width {
                buf.set_stringn(
                    area.right() - w,
                    area.bottom() - 1,
                    &text,
                    usize::from(w),
                    Style::default().fg(Color::Black).bg(Color::Cyan),
                );
            }
        }
    }
}
