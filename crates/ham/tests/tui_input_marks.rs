//! The caret and mark parts of `ChatBox` that the pasted-text placeholders and the draft editor need: the caret
//! as a character offset in the whole text (a line break counts one), the text and caret set together, a removal that is one edit of the undo
//! history, and `render_marked`, which draws the characters of given ranges in a style, on the crate's wrapped rows and scrolled rows.
#![allow(clippy::unwrap_used)]

use ham::tui::input::{ChatBox, Kind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders};

const MARK: Style = Style::new().fg(Color::Cyan);

/// A range of characters; a function, so that a one-element array of it is not a `single_range_in_vec_init`.
fn span(from: usize, to: usize) -> std::ops::Range<usize> {
    from..to
}

/// The characters of the box that carry the mark colour, row by row, after drawing `b` in a bordered box of `w` x `h`.
fn marked_cells(b: &ChatBox, marks: &[std::ops::Range<usize>], w: u16, h: u16) -> Vec<String> {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| {
        b.render_marked(
            f,
            Rect::new(0, 0, w, h),
            Block::default().borders(Borders::ALL),
            true,
            marks,
            MARK,
        );
    })
    .unwrap();
    let buf = t.backend().buffer().clone();
    (0..h)
        .map(|y| {
            (0..w)
                .filter(|x| buf[(*x, y)].fg == Color::Cyan)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect()
}

#[test]
fn the_caret_index_counts_a_line_break_as_one_character() {
    let mut b = ChatBox::with_text(Kind::Multi, "ab\ncd");
    assert_eq!(b.caret_index(), 5, "the caret is at the end");
    b.set_caret_index(4);
    assert_eq!(b.cursor(), (1, 1));
    assert_eq!(b.caret_index(), 4);
    b.set_caret_index(2);
    assert_eq!(
        b.cursor(),
        (0, 2),
        "index 2 is the end of the first line, before the break"
    );
    b.set_caret_index(3);
    assert_eq!(
        b.cursor(),
        (1, 0),
        "index 3 is the start of the second line"
    );
}

#[test]
fn an_index_past_the_end_puts_the_caret_at_the_end() {
    let mut b = ChatBox::with_text(Kind::Multi, "ab\ncd");
    b.set_caret_index(0);
    b.set_caret_index(999);
    assert_eq!(b.cursor(), (1, 2));
    assert_eq!(b.caret_index(), 5);
}

#[test]
fn the_index_counts_characters_and_not_bytes() {
    let mut b = ChatBox::with_text(Kind::Multi, "h\u{e9}llo \u{1f600} x");
    b.set_caret_index(8);
    assert_eq!(b.cursor(), (0, 8));
    assert_eq!(b.caret_index(), 8);
    b.insert_char('!');
    assert_eq!(b.text(), "h\u{e9}llo \u{1f600} !x");
}

#[test]
fn set_text_caret_replaces_the_text_and_puts_the_caret() {
    let mut b = ChatBox::with_text(Kind::Multi, "old text");
    b.set_text_caret("new\ntext", 5);
    assert_eq!(b.text(), "new\ntext");
    assert_eq!(b.cursor(), (1, 1));
    b.set_text_caret("x", 40);
    assert_eq!(b.caret_index(), 1, "a caret past the end is the end");
}

#[test]
fn remove_chars_takes_a_range_across_a_line_break_and_undo_brings_it_back() {
    let mut b = ChatBox::with_text(Kind::Multi, "ab\ncd");
    b.remove_chars(1..4);
    assert_eq!(b.text(), "ad");
    assert_eq!(
        b.caret_index(),
        1,
        "the caret goes to where the characters were"
    );
    assert!(b.undo(), "the removal is an edit of the history");
    assert_eq!(b.text(), "ab\ncd");
}

#[test]
fn remove_chars_with_an_empty_or_late_range_changes_nothing() {
    let mut b = ChatBox::with_text(Kind::Multi, "abc");
    b.remove_chars(1..1);
    assert_eq!(b.text(), "abc");
    b.remove_chars(3..9);
    assert_eq!(b.text(), "abc");
}

#[test]
fn render_marked_draws_the_marked_characters_in_the_style_and_no_others() {
    let b = ChatBox::with_text(Kind::Multi, "see [Pasted 3 lines] ok");
    // "[Pasted 3 lines]" is characters 4 to 19.
    let cells = marked_cells(&b, &[span(4, 20)], 40, 3);
    assert_eq!(cells[1], "[Pasted 3 lines]", "{cells:?}");
    assert_eq!(cells[0], "");
    assert_eq!(cells[2], "");
}

#[test]
fn render_marked_follows_a_mark_over_wrapped_rows() {
    let b = ChatBox::with_text(Kind::Multi, "aaaa bbbbbbbb cccc");
    // A box 8 columns wide inside the border (10 outside): the crate wraps at word boundaries and the mark covers `bbbbbbbb`.
    let cells = marked_cells(&b, &[span(5, 13)], 10, 5);
    let joined: String = cells.concat();
    assert_eq!(joined, "bbbbbbbb", "{cells:?}");
}

#[test]
fn render_marked_follows_a_mark_after_a_line_break() {
    let b = ChatBox::with_text(Kind::Multi, "one\ntwo three");
    let cells = marked_cells(&b, &[span(4, 7)], 20, 4);
    assert_eq!(cells[1], "");
    assert_eq!(cells[2], "two", "{cells:?}");
}

#[test]
fn render_marked_skips_a_mark_that_is_scrolled_out_of_view() {
    let text: Vec<String> = (0..12).map(|i| format!("line{i:02}")).collect();
    let b = ChatBox::with_text(Kind::Multi, &text.join("\n"));
    // The caret is at the end, so the box shows the last rows; the mark on the first line is off screen and nothing else is marked.
    let cells = marked_cells(&b, &[span(0, 6)], 20, 5);
    assert!(cells.iter().all(String::is_empty), "{cells:?}");
}

#[test]
fn render_marked_with_no_marks_is_render() {
    let b = ChatBox::with_text(Kind::Multi, "plain text");
    let cells = marked_cells(&b, &[], 20, 3);
    assert!(cells.iter().all(String::is_empty), "{cells:?}");
}
