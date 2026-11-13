//! `ChatBox`, the text box behind the composer and the chat input: what a person sees of an editor
//! (the text, the caret, a wrapped long line, a scrolled long text, a paste) and what a screen needs of it (which keys it takes, the
//! height it wants). These tests hold for the small core that ships first and for the `ratatui-textarea` core that replaces it, so
//! they use only the frozen API of `crates/ham/src/tui/AGENTS.md`.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::input::{ChatBox, Kind};
use ratatui::Terminal;
use ratatui::backend::{Backend, TestBackend};
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Borders};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn typed(b: &mut ChatBox, text: &str) {
    for c in text.chars() {
        let used = if c == '\n' {
            b.newline();
            true
        } else {
            b.handle_key(key(KeyCode::Char(c)))
        };
        assert!(used || c.is_whitespace(), "the box refused {c:?}");
    }
}

/// Draws `b` in a bordered box of `w` x `h` and returns the rows of text and the terminal's cursor.
fn drawn(b: &ChatBox, w: u16, h: u16, focused: bool) -> (Vec<String>, (u16, u16)) {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| {
        b.render(
            f,
            Rect::new(0, 0, w, h),
            Block::default().borders(Borders::ALL).title(" box "),
            focused,
        );
    })
    .unwrap();
    let pos = t.backend_mut().get_cursor_position().unwrap();
    let buf = t.backend().buffer().clone();
    let rows = (0..h)
        .map(|y| {
            (0..w)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect();
    (rows, (pos.x, pos.y))
}

#[test]
fn a_new_box_is_empty_with_the_caret_at_the_start() {
    let b = ChatBox::new(Kind::Multi);
    assert!(b.is_empty());
    assert_eq!(b.text(), "");
    assert_eq!(b.cursor(), (0, 0));
    assert_eq!(b.lines(), [String::new()]);
    assert_eq!(b.kind(), Kind::Multi);
}

#[test]
fn text_with_line_breaks_makes_lines_and_the_caret_follows_the_end() {
    let b = ChatBox::with_text(Kind::Multi, "one\ntwo\nthree");
    assert_eq!(b.lines(), ["one", "two", "three"]);
    assert_eq!(b.text(), "one\ntwo\nthree");
    assert_eq!(b.cursor(), (2, 5));
    assert!(!b.is_empty());
}

#[test]
fn a_one_line_box_drops_line_breaks_and_a_token_box_drops_whitespace_too() {
    let mut line = ChatBox::new(Kind::Line);
    line.insert_str("a\nb c");
    assert_eq!(line.text(), "ab c");
    line.newline();
    assert_eq!(line.text(), "ab c", "newline is ignored in a one-line box");
    let mut token = ChatBox::new(Kind::Token);
    token.insert_str(" cc:my \n repo\t");
    assert_eq!(token.text(), "cc:myrepo");
}

#[test]
fn keys_move_the_caret_and_edit_in_the_middle() {
    let mut b = ChatBox::new(Kind::Multi);
    typed(&mut b, "build a todo api");
    for _ in 0..8 {
        assert!(b.handle_key(key(KeyCode::Left)));
    }
    typed(&mut b, "small ");
    assert_eq!(b.text(), "build a small todo api");
    assert_eq!(b.cursor(), (0, 14));
    b.handle_key(key(KeyCode::Home));
    assert_eq!(b.cursor(), (0, 0));
    b.handle_key(key(KeyCode::End));
    assert_eq!(b.cursor(), (0, 22));
    b.handle_key(key(KeyCode::Backspace));
    assert_eq!(b.text(), "build a small todo ap");
    b.handle_key(key(KeyCode::Home));
    b.handle_key(key(KeyCode::Delete));
    assert_eq!(b.text(), "uild a small todo ap");
}

#[test]
fn backspace_and_delete_join_lines_and_arrows_cross_them() {
    let mut b = ChatBox::with_text(Kind::Multi, "ab\ncd");
    b.handle_key(key(KeyCode::Home));
    b.handle_key(key(KeyCode::Backspace));
    assert_eq!((b.text().as_str(), b.cursor()), ("abcd", (0, 2)));
    let mut b = ChatBox::with_text(Kind::Multi, "ab\ncd");
    b.handle_key(key(KeyCode::Up));
    b.handle_key(key(KeyCode::End));
    b.handle_key(key(KeyCode::Delete));
    assert_eq!(b.text(), "abcd");
    let mut b = ChatBox::with_text(Kind::Multi, "ab\ncd");
    b.handle_key(key(KeyCode::Home));
    b.handle_key(key(KeyCode::Left));
    assert_eq!(
        b.cursor(),
        (0, 2),
        "left at a line start goes to the end of the line above"
    );
    b.handle_key(key(KeyCode::Right));
    assert_eq!(
        b.cursor(),
        (1, 0),
        "right at a line end goes to the start of the next"
    );
}

#[test]
fn up_and_down_keep_the_column_as_far_as_the_line_allows() {
    let mut b = ChatBox::with_text(Kind::Multi, "abcdef\nab\nabcdef");
    b.handle_key(key(KeyCode::Up));
    assert_eq!(b.cursor(), (1, 2));
    b.handle_key(key(KeyCode::Up));
    assert_eq!(b.cursor(), (0, 2));
    b.handle_key(key(KeyCode::Down));
    b.handle_key(key(KeyCode::Down));
    assert_eq!(b.cursor(), (2, 2));
}

#[test]
fn the_keys_that_belong_to_the_screen_are_not_taken() {
    let mut b = ChatBox::new(Kind::Multi);
    for code in [KeyCode::Enter, KeyCode::Tab, KeyCode::Esc, KeyCode::F(1)] {
        assert!(!b.handle_key(key(code)), "{code:?} was taken by the box");
    }
    for c in ['s', 'c', 'j', 'r', 'g'] {
        let ctrl = KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert!(!b.handle_key(ctrl), "Ctrl-{c} was taken by the box");
    }
    assert_eq!(b.text(), "");
}

#[test]
fn the_character_before_the_caret_is_known_for_the_backslash_enter_rule() {
    let mut b = ChatBox::with_text(Kind::Multi, "ab\\");
    assert_eq!(b.char_before_cursor(), Some('\\'));
    b.handle_key(key(KeyCode::Home));
    assert_eq!(b.char_before_cursor(), None);
    b.handle_key(key(KeyCode::Right));
    assert_eq!(b.char_before_cursor(), Some('a'));
}

#[test]
fn clear_and_set_text_reset_the_text_and_put_the_caret_at_the_end() {
    let mut b = ChatBox::with_text(Kind::Multi, "one\ntwo");
    b.clear();
    assert!(b.is_empty());
    assert_eq!(b.cursor(), (0, 0));
    b.set_text("x\ny");
    assert_eq!(b.cursor(), (1, 1));
    assert_eq!(b.text(), "x\ny");
}

#[test]
fn boxes_are_equal_when_their_kind_and_text_are() {
    assert_eq!(
        ChatBox::with_text(Kind::Multi, "a"),
        ChatBox::with_text(Kind::Multi, "a")
    );
    assert_ne!(
        ChatBox::with_text(Kind::Multi, "a"),
        ChatBox::with_text(Kind::Line, "a")
    );
}

#[test]
fn rows_at_counts_the_wrapped_rows_and_the_row_the_caret_needs_after_a_full_line() {
    let b = ChatBox::new(Kind::Multi);
    assert_eq!(b.rows_at(10), 1);
    assert_eq!(ChatBox::with_text(Kind::Multi, "abc").rows_at(10), 1);
    assert_eq!(ChatBox::with_text(Kind::Multi, "a\nb\nc").rows_at(10), 3);
    // 25 characters at width 10: three rows (10 + 10 + 5).
    assert_eq!(
        ChatBox::with_text(Kind::Multi, &"x".repeat(25)).rows_at(10),
        3
    );
    // A last line of exactly the width leaves the caret no room: it takes one more row.
    assert_eq!(
        ChatBox::with_text(Kind::Multi, &"x".repeat(20)).rows_at(10),
        3
    );
}

#[test]
fn a_wide_character_takes_two_columns() {
    let b = ChatBox::with_text(Kind::Multi, "日本語日");
    // Width 5 holds two wide characters (4 columns) per row.
    assert_eq!(b.rows_at(5), 2);
    let (rows, _) = drawn(&b, 7, 4, false);
    // The cell after a wide character is blank in the buffer, so the spaces are dropped before the comparison.
    let squeezed: Vec<String> = rows.iter().map(|r| r.replace(' ', "")).collect();
    assert!(squeezed[1].contains("日本"), "{rows:?}");
    assert!(squeezed[2].contains("語日"), "{rows:?}");
}

#[test]
fn render_draws_the_text_in_its_block_and_puts_the_cursor_at_the_caret() {
    let b = ChatBox::with_text(Kind::Multi, "hello\nworld");
    let (rows, (x, y)) = drawn(&b, 20, 6, true);
    assert!(rows[0].contains("box"), "{rows:?}");
    assert!(rows[1].starts_with("│hello"), "{rows:?}");
    assert!(rows[2].starts_with("│world"), "{rows:?}");
    // The caret is after `world`: inside the border, on the second text row.
    assert_eq!((x, y), (1 + 5, 2));
}

#[test]
fn a_long_line_wraps_at_the_inner_width_and_the_caret_is_on_the_last_row() {
    let b = ChatBox::with_text(Kind::Multi, &"w".repeat(25));
    let (rows, (x, y)) = drawn(&b, 12, 6, true);
    // Inner width 10: 10 + 10 + 5.
    assert!(rows[1].contains(&"w".repeat(10)), "{rows:?}");
    assert!(
        rows[3].contains(&"w".repeat(5)) && !rows[3].contains(&"w".repeat(6)),
        "{rows:?}"
    );
    assert_eq!((x, y), (1 + 5, 3));
}

#[test]
fn the_caret_after_a_full_last_line_sits_on_a_new_row() {
    let b = ChatBox::with_text(Kind::Multi, &"w".repeat(10));
    let (_, (x, y)) = drawn(&b, 12, 6, true);
    assert_eq!((x, y), (1, 2));
}

#[test]
fn a_text_longer_than_the_box_scrolls_to_the_caret_and_back() {
    let text: String = (1..=20)
        .map(|i| format!("line {i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut b = ChatBox::with_text(Kind::Multi, &text);
    // Three text rows (a box of five with its border): the end of the text, the caret on the last.
    let (rows, (x, y)) = drawn(&b, 20, 5, true);
    let joined = rows.join("\n");
    assert!(
        joined.contains("line 20") && joined.contains("line 18"),
        "{joined}"
    );
    assert!(!joined.contains("line 01"), "{joined}");
    assert_eq!((x, y), (1 + 7, 3));
    for _ in 0..19 {
        b.handle_key(key(KeyCode::Up));
    }
    let (rows, (x, y)) = drawn(&b, 20, 5, true);
    let joined = rows.join("\n");
    assert!(
        joined.contains("line 01") && joined.contains("line 03"),
        "{joined}"
    );
    assert!(!joined.contains("line 20"), "{joined}");
    assert_eq!((x, y), (1 + 7, 1));
}

#[test]
fn moving_the_caret_inside_the_view_does_not_scroll_it() {
    let text: String = (1..=20)
        .map(|i| format!("line {i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut b = ChatBox::with_text(Kind::Multi, &text);
    drawn(&b, 20, 5, true);
    b.handle_key(key(KeyCode::Up));
    let (rows, (_, y)) = drawn(&b, 20, 5, true);
    let joined = rows.join("\n");
    // Lines 18 to 20 stay in view; the caret is on line 19.
    assert!(
        joined.contains("line 18") && joined.contains("line 20"),
        "{joined}"
    );
    assert_eq!(y, 2);
}

#[test]
fn an_unfocused_box_asks_for_no_cursor_and_a_tiny_area_does_not_panic() {
    let b = ChatBox::with_text(Kind::Multi, "abc");
    let (rows, _) = drawn(&b, 12, 4, false);
    assert!(rows[1].contains("abc"));
    for (w, h) in [(1, 1), (2, 2), (3, 3), (4, 1)] {
        drawn(&b, w, h, true);
    }
}

#[test]
fn a_default_box_is_an_empty_box_of_many_lines() {
    let b = ChatBox::default();
    assert!(b.is_empty());
    assert_eq!(b.kind(), Kind::Multi);
}

#[test]
fn up_on_the_first_line_and_down_on_the_last_stay_in_the_text() {
    let mut b = ChatBox::with_text(Kind::Multi, "abc\ndef");
    for _ in 0..3 {
        b.handle_key(key(KeyCode::Down));
    }
    assert_eq!(b.cursor().0, 1, "Down left the last line");
    for _ in 0..4 {
        b.handle_key(key(KeyCode::Up));
    }
    assert_eq!(b.cursor().0, 0, "Up left the first line");
}
