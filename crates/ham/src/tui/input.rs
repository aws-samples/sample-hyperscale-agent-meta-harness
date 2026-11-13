//! The text-editing box of the TUI: `ChatBox`, the one widget behind the composer's
//! Prompt, Repository, Base ref and Options boxes and the chat input box of a run. It owns the text, the caret, the scroll position and
//! the drawing, so a screen never keeps a bare `String` for something the user types into. The signatures are frozen in `AGENTS.md`.
//!
//! This is the only file that names the `ratatui_textarea` crate. The crate gives the caret, selection (Shift with a
//! movement key), undo and redo, word moves and word delete, and soft wrap at word boundaries; this file keeps the keys the app owns
//! (Enter, Tab, Esc, Ctrl-J, Ctrl-R, Ctrl-U, Ctrl-C, Ctrl-S, Ctrl-G) out of the crate's hands, moves undo and redo to Ctrl-Z and Alt-Z,
//! hides the crate's own cursor cell (the terminal's cursor is the caret) and measures the wrapped height through the caret.
//! `tests/tui_input_box.rs` pins the behaviour that the earlier in-house core shared with this one.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, Widget};
use ratatui_textarea::{CursorMove, DataCursor, Input, TextArea, WrapMode};
use std::ops::Range;

/// What a box accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Any text; Enter is not handled by the box (the screen decides what Enter does) and a new line is inserted with
    /// [`ChatBox::newline`].
    Multi,
    /// One line of text: no line breaks.
    Line,
    /// One line without whitespace (a repository name, a git ref).
    Token,
}

/// A text box: the editor of the crate and the kind of text it takes.
#[derive(Clone, Debug)]
pub struct ChatBox {
    kind: Kind,
    ta: TextArea<'static>,
}

impl PartialEq for ChatBox {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.ta.lines() == other.ta.lines()
    }
}

impl Eq for ChatBox {}

impl Default for ChatBox {
    fn default() -> Self {
        Self::new(Kind::Multi)
    }
}

/// The number of edits that undo can go back through.
const UNDO_DEPTH: usize = 50;

fn editor(kind: Kind) -> TextArea<'static> {
    let mut ta = TextArea::default();
    // The terminal's cursor is the caret: the crate neither paints a cursor cell nor underlines the cursor's line.
    ta.set_cursor_style(Style::default());
    ta.set_cursor_line_style(Style::default());
    ta.set_max_histories(UNDO_DEPTH);
    ta.set_wrap_mode(match kind {
        Kind::Multi => WrapMode::WordOrGlyph,
        Kind::Line | Kind::Token => WrapMode::None,
    });
    ta
}

/// The control keys the app owns; the box never takes them (the key table, `keys.rs`, gives each its meaning).
const APP_CTRL: &[char] = &['j', 'r', 'u', 'c', 's', 'g', 'm'];

impl ChatBox {
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            ta: editor(kind),
        }
    }

    /// A box that holds `text`, the caret at its end.
    pub fn with_text(kind: Kind, text: &str) -> Self {
        let mut b = Self::new(kind);
        b.insert_str(text);
        b
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The text, lines joined with `\n`.
    pub fn text(&self) -> String {
        self.ta.lines().join("\n")
    }

    /// The lines of the text.
    pub fn lines(&self) -> &[String] {
        self.ta.lines()
    }

    pub fn is_empty(&self) -> bool {
        self.ta.is_empty()
    }

    /// The caret as (line, column in characters), both from 0.
    pub fn cursor(&self) -> (usize, usize) {
        let DataCursor(row, col) = self.ta.cursor();
        (row, col)
    }

    /// The character just before the caret on its line, if there is one.
    pub fn char_before_cursor(&self) -> Option<char> {
        let (row, col) = self.cursor();
        col.checked_sub(1)
            .and_then(|i| self.ta.lines()[row].chars().nth(i))
    }

    /// The caret as a number of characters from the start of the whole text (each line break counts as one).
    pub fn caret_index(&self) -> usize {
        let (row, col) = self.cursor();
        let before: usize = self.ta.lines()[..row]
            .iter()
            .map(|l| l.chars().count() + 1)
            .sum();
        before + col
    }

    /// The (line, column) of the character `index` of the whole text; an index past the end gives the end of the last line.
    fn position_of(&self, index: usize) -> (usize, usize) {
        let lines = self.ta.lines();
        let mut left = index;
        for (row, line) in lines.iter().enumerate() {
            let n = line.chars().count();
            if left <= n {
                return (row, left);
            }
            left -= n + 1;
        }
        let last = lines.len().saturating_sub(1);
        (last, lines.get(last).map_or(0, |l| l.chars().count()))
    }

    /// Puts the caret `index` characters from the start of the whole text (clamped to the end). A selection is dropped.
    pub fn set_caret_index(&mut self, index: usize) {
        let (row, col) = self.position_of(index);
        self.ta.cancel_selection();
        self.ta.move_cursor(CursorMove::Jump(
            u16::try_from(row).unwrap_or(u16::MAX),
            u16::try_from(col).unwrap_or(u16::MAX),
        ));
    }

    /// Replaces the text with `text`, the caret `caret` characters from its start.
    pub fn set_text_caret(&mut self, text: &str, caret: usize) {
        self.set_text(text);
        self.set_caret_index(caret);
    }

    /// Removes the characters `range` of the whole text (line breaks count as one each); the caret goes to where they were. The
    /// removal is one edit of the undo history.
    pub fn remove_chars(&mut self, range: Range<usize>) {
        self.set_caret_index(range.start);
        self.ta.delete_str(range.end.saturating_sub(range.start));
    }

    /// Removes all text and puts the caret at the start. The undo history goes with it.
    pub fn clear(&mut self) {
        self.ta = editor(self.kind);
    }

    /// Replaces the text with `text`, the caret at its end.
    pub fn set_text(&mut self, text: &str) {
        self.clear();
        self.insert_str(text);
    }

    /// Inserts one character at the caret. A box that takes no line breaks or no whitespace drops them.
    pub fn insert_char(&mut self, c: char) {
        match self.kind {
            Kind::Multi => {}
            Kind::Line if c == '\n' => return,
            Kind::Token if c.is_whitespace() => return,
            Kind::Line | Kind::Token => {}
        }
        if c == '\n' {
            self.newline();
        } else {
            self.ta.insert_char(c);
            self.sync_cursor_style();
        }
    }

    /// Inserts a new line at the caret (a box of one line ignores it).
    pub fn newline(&mut self) {
        if self.kind == Kind::Multi {
            self.ta.insert_newline();
            self.sync_cursor_style();
        }
    }

    /// Inserts pasted text at the caret: a line break starts a line in a box of many lines and is dropped in the others, as is white
    /// space in a token box. The crate drops a paste event; this is the route.
    pub fn insert_str(&mut self, text: &str) {
        match self.kind {
            Kind::Multi => {
                self.ta.insert_str(text);
            }
            Kind::Line => {
                let line: String = text.chars().filter(|c| *c != '\n').collect();
                self.ta.insert_str(line);
            }
            Kind::Token => {
                let token: String = text.chars().filter(|c| !c.is_whitespace()).collect();
                self.ta.insert_str(token);
            }
        }
        self.sync_cursor_style();
    }

    /// Goes back one edit; true when there was one.
    pub fn undo(&mut self) -> bool {
        let done = self.ta.undo();
        self.sync_cursor_style();
        done
    }

    /// Goes forward again after an undo; true when there was an edit to redo.
    pub fn redo(&mut self) -> bool {
        let done = self.ta.redo();
        self.sync_cursor_style();
        done
    }

    /// Selects the whole text.
    pub fn select_all(&mut self) {
        self.ta.select_all();
        self.sync_cursor_style();
    }

    /// The selected text, if a selection is active and not empty.
    pub fn selection(&self) -> Option<String> {
        let ((r1, c1), (r2, c2)) = self.ta.selection_range()?;
        if (r1, c1) == (r2, c2) {
            return None;
        }
        let lines = self.ta.lines();
        let take = |row: usize, from: usize, to: Option<usize>| -> String {
            let chars = lines[row].chars().skip(from);
            match to {
                Some(end) => chars.take(end.saturating_sub(from)).collect(),
                None => chars.collect(),
            }
        };
        if r1 == r2 {
            return Some(take(r1, c1, Some(c2)));
        }
        let mut out = take(r1, c1, None);
        for line in &lines[r1 + 1..r2] {
            out.push('\n');
            out.push_str(line);
        }
        out.push('\n');
        out.push_str(&take(r2, 0, Some(c2)));
        Some(out)
    }

    /// Applies one key press; true when the box used it. Enter, Tab, Esc, function keys and the control keys the app owns are the
    /// screen's. Undo is Ctrl-Z and redo Alt-Z; every other key goes to the crate (movement, Shift for a selection, word moves with
    /// Ctrl or Alt and the arrows or B and F, Backspace, Delete, Ctrl-W and Alt-D for words).
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('z') if ctrl && !alt => {
                self.undo();
                return true;
            }
            KeyCode::Char('z') if alt && !ctrl => {
                self.redo();
                return true;
            }
            KeyCode::Char(c) if ctrl && APP_CTRL.contains(&c) => return false,
            KeyCode::Char(c) if !ctrl && !alt => {
                self.insert_char(c);
                return true;
            }
            KeyCode::Char(_)
            | KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown => {}
            _ => return false,
        }
        self.ta.input(Input::from(key));
        self.sync_cursor_style();
        true
    }

    /// How many display rows the text takes at `width` columns (at least one), measured the way says: the text is laid
    /// out once at that width and the caret, moved to the end, reports its row.
    pub fn rows_at(&self, width: usize) -> usize {
        let w = u16::try_from(width).unwrap_or(u16::MAX).max(1);
        let mut probe = self.ta.clone();
        let area = Rect::new(0, 0, w, 1);
        let mut buf = Buffer::empty(area);
        (&probe).render(area, &mut buf);
        probe.move_cursor(CursorMove::Bottom);
        probe.move_cursor(CursorMove::End);
        let mut rows = probe.screen_cursor().row + 1;
        // A caret after a last row that is exactly full has no column: it takes a row of its own, so the next character has a place.
        // (A box that does not wrap scrolls sideways instead.)
        if self.wraps() && probe.screen_cursor().col >= usize::from(w) {
            rows += 1;
        }
        rows.max(1)
    }

    fn wraps(&self) -> bool {
        self.kind == Kind::Multi
    }

    /// The terminal's cursor is the caret and the crate paints no cursor cell, with one exception: a selection made backwards starts under the
    /// caret, and that character is part of the selection, so the cell takes the selection's style. (A selection made forwards ends just
    /// before the caret and does not include the cell it stands on.) Called after every edit that can move the caret or the selection.
    fn sync_cursor_style(&mut self) {
        let DataCursor(row, col) = self.ta.cursor();
        let under_the_caret = self
            .ta
            .selection_range()
            .is_some_and(|(start, end)| start != end && start == (row, col));
        let style = if under_the_caret {
            self.ta.selection_style()
        } else {
            Style::default()
        };
        self.ta.set_cursor_style(style);
    }

    /// [`ChatBox::render`] with the characters in `marks` (ranges over the whole text, line breaks counting one each) drawn in `mark_style`:
    /// the placeholders of pasted text. The text is laid out by the crate; each marked character is found by moving a copy of the editor's
    /// caret onto it and reading where the crate put that caret on the screen.
    pub fn render_marked(
        &self,
        f: &mut Frame,
        area: Rect,
        block: Block<'_>,
        focused: bool,
        marks: &[Range<usize>],
        mark_style: Style,
    ) {
        let inner = block.inner(area);
        self.render(f, area, block, focused);
        if marks.is_empty() || inner.width == 0 || inner.height == 0 {
            return;
        }
        let (top_row, top_col) = self.ta.scroll_offset();
        let (top_row, top_col) = (usize::from(top_row), usize::from(top_col));
        let mut probe = self.ta.clone();
        for index in marks.iter().flat_map(|m| m.clone()) {
            let (row, col) = self.position_of(index);
            // The line break at the end of a line is a character of the text and has no cell.
            if col >= self.ta.lines()[row].chars().count() {
                continue;
            }
            probe.move_cursor(CursorMove::Jump(
                u16::try_from(row).unwrap_or(u16::MAX),
                u16::try_from(col).unwrap_or(u16::MAX),
            ));
            let at = probe.screen_cursor();
            let (Some(y), Some(x)) = (at.row.checked_sub(top_row), at.col.checked_sub(top_col))
            else {
                continue;
            };
            if x < usize::from(inner.width) && y < usize::from(inner.height) {
                let cell = &mut f.buffer_mut()[(
                    inner.x + u16::try_from(x).unwrap_or(0),
                    inner.y + u16::try_from(y).unwrap_or(0),
                )];
                cell.set_style(mark_style);
            }
        }
    }

    /// Draws the box in `area` inside `block`. The text is wrapped to the box's width, scrolled so the caret row is shown (the rows
    /// around it stay in view), and, when `focused`, the terminal's cursor is put at the caret.
    pub fn render(&self, f: &mut Frame, area: Rect, block: Block<'_>, focused: bool) {
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(&self.ta, inner);
        if focused && inner.width > 0 && inner.height > 0 {
            let at = self.ta.screen_cursor();
            let (top_row, top_col) = self.ta.scroll_offset();
            let mut col = u16::try_from(at.col)
                .unwrap_or(u16::MAX)
                .saturating_sub(top_col);
            let mut row = u16::try_from(at.row)
                .unwrap_or(u16::MAX)
                .saturating_sub(top_row);
            // The caret after a full last row goes to the start of the row below (`rows_at` made room for it); a box that cannot
            // grow any more keeps it in the last column.
            if self.wraps() && col >= inner.width && row + 1 < inner.height {
                col = 0;
                row += 1;
            }
            f.set_cursor_position((
                inner.x + col.min(inner.width - 1),
                inner.y + row.min(inner.height - 1),
            ));
        }
    }
}
