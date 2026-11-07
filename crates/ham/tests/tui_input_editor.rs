//! What the editor core of `ChatBox` gives beyond a caret and a scroll: a selection made with Shift and a movement
//! key, undo (Ctrl-Z) and redo (Alt-Z), moves and deletes by word, and soft wrap at word boundaries, with the keys the app owns left
//! alone. These are the behaviours the in-house core of the first merge does not have; `tests/tui_input_box.rs` holds what both share.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::input::{ChatBox, Kind};
use ratatui::Terminal;
use ratatui::backend::{Backend, TestBackend};
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Borders};
use std::time::{Duration, Instant};

fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}

fn plain(code: KeyCode) -> KeyEvent {
    key(code, KeyModifiers::NONE)
}

fn press(b: &mut ChatBox, code: KeyCode, mods: KeyModifiers) -> bool {
    b.handle_key(key(code, mods))
}

fn drawn(b: &ChatBox, w: u16, h: u16) -> (Vec<String>, (u16, u16)) {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| {
        b.render(
            f,
            Rect::new(0, 0, w, h),
            Block::default().borders(Borders::ALL),
            true,
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
fn shift_with_the_arrows_selects_and_a_delete_removes_the_selection() {
    let mut b = ChatBox::with_text(Kind::Multi, "build a todo api");
    press(&mut b, KeyCode::Home, KeyModifiers::NONE);
    assert_eq!(b.selection(), None);
    for _ in 0..5 {
        assert!(press(&mut b, KeyCode::Right, KeyModifiers::SHIFT));
    }
    assert_eq!(b.selection().as_deref(), Some("build"));
    assert!(press(&mut b, KeyCode::Backspace, KeyModifiers::NONE));
    assert_eq!(b.text(), " a todo api");
    assert_eq!(b.selection(), None);
}

#[test]
fn a_selection_can_span_lines_and_select_all_takes_everything() {
    let mut b = ChatBox::with_text(Kind::Multi, "one\ntwo\nthree");
    b.select_all();
    assert_eq!(b.selection().as_deref(), Some("one\ntwo\nthree"));
    let mut b = ChatBox::with_text(Kind::Multi, "ab\ncd");
    press(&mut b, KeyCode::Up, KeyModifiers::NONE);
    press(&mut b, KeyCode::Home, KeyModifiers::NONE);
    press(&mut b, KeyCode::Right, KeyModifiers::NONE);
    press(&mut b, KeyCode::Down, KeyModifiers::SHIFT);
    assert_eq!(b.selection().as_deref(), Some("b\nc"));
}

#[test]
fn alt_with_b_and_f_move_by_word() {
    let mut b = ChatBox::with_text(Kind::Multi, "build a todo api");
    press(&mut b, KeyCode::Home, KeyModifiers::NONE);
    press(&mut b, KeyCode::Char('f'), KeyModifiers::ALT);
    assert_eq!(b.cursor(), (0, 6), "the start of `a`");
    press(&mut b, KeyCode::Char('f'), KeyModifiers::ALT);
    assert_eq!(b.cursor(), (0, 8), "the start of `todo`");
    press(&mut b, KeyCode::Char('b'), KeyModifiers::ALT);
    assert_eq!(b.cursor(), (0, 6), "back to the start of `a`");
}

#[test]
fn ctrl_w_and_alt_d_delete_a_word() {
    let mut b = ChatBox::with_text(Kind::Multi, "build a todo");
    press(&mut b, KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert_eq!(b.text(), "build a ");
    let mut b = ChatBox::with_text(Kind::Multi, "build a todo");
    press(&mut b, KeyCode::Home, KeyModifiers::NONE);
    press(&mut b, KeyCode::Char('d'), KeyModifiers::ALT);
    assert_eq!(b.text(), " a todo");
}

#[test]
fn ctrl_z_undoes_and_alt_z_redoes() {
    let mut b = ChatBox::new(Kind::Multi);
    b.insert_str("hello");
    press(&mut b, KeyCode::Char(' '), KeyModifiers::NONE);
    press(&mut b, KeyCode::Char('w'), KeyModifiers::NONE);
    let typed = b.text();
    assert_eq!(typed, "hello w");
    assert!(press(&mut b, KeyCode::Char('z'), KeyModifiers::CONTROL));
    assert_ne!(b.text(), typed, "Ctrl-Z changed nothing");
    assert!(press(&mut b, KeyCode::Char('z'), KeyModifiers::ALT));
    assert_eq!(b.text(), typed, "Alt-Z did not bring the edit back");
    // Undo reaches back to the empty box, and further presses do nothing.
    for _ in 0..10 {
        press(&mut b, KeyCode::Char('z'), KeyModifiers::CONTROL);
    }
    assert_eq!(b.text(), "");
}

#[test]
fn the_keys_the_app_owns_are_never_taken() {
    let mut b = ChatBox::with_text(Kind::Multi, "text");
    for c in ['j', 'r', 'u', 'c', 's', 'g', 'm'] {
        assert!(
            !press(&mut b, KeyCode::Char(c), KeyModifiers::CONTROL),
            "Ctrl-{c} was taken by the editor"
        );
    }
    for code in [KeyCode::Enter, KeyCode::Tab, KeyCode::Esc, KeyCode::F(2)] {
        assert!(!b.handle_key(plain(code)), "{code:?}");
    }
    assert_eq!(b.text(), "text", "an app key changed the text");
}

#[test]
fn a_long_line_wraps_at_word_boundaries_and_the_height_follows() {
    let b = ChatBox::with_text(Kind::Multi, "alpha beta gamma");
    // Ten columns inside the border: `alpha beta` then `gamma`.
    assert_eq!(b.rows_at(10), 2);
    let (rows, (x, y)) = drawn(&b, 12, 6);
    assert!(rows[1].starts_with("│alpha beta"), "{rows:?}");
    // The space that ended the first row leads the second one (the crate wraps after a space).
    assert!(rows[2].starts_with("│ gamma"), "{rows:?}");
    assert_eq!((x, y), (1 + 6, 2));
}

#[test]
fn a_word_longer_than_the_box_is_split_at_the_edge() {
    let b = ChatBox::with_text(Kind::Multi, &format!("ab {}", "c".repeat(25)));
    // 10 columns: `ab` then the long word cut at the edge, 10 + 10 + 5.
    assert_eq!(b.rows_at(10), 4);
}

#[test]
fn a_one_line_box_scrolls_sideways_instead_of_wrapping() {
    let b = ChatBox::with_text(Kind::Line, &format!("start {}", "x".repeat(40)));
    let (rows, (x, y)) = drawn(&b, 12, 3);
    // The tail is shown with the caret at its end, inside the box.
    assert!(
        rows[1].contains("xxxx") && !rows[1].contains("start"),
        "{rows:?}"
    );
    assert_eq!(y, 1);
    assert!((1..=10).contains(&x), "{x}");
    assert_eq!(b.rows_at(10), 1);
}

#[test]
fn a_paste_of_several_lines_is_one_undo_step_or_few_and_keeps_its_lines() {
    let mut b = ChatBox::new(Kind::Multi);
    b.insert_str("one\ntwo\nthree");
    assert_eq!(b.lines(), ["one", "two", "three"]);
    assert_eq!(b.cursor(), (2, 5));
    press(&mut b, KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert!(b.text().len() < "one\ntwo\nthree".len());
}

#[test]
fn ctrl_a_and_e_go_to_the_line_ends_and_ctrl_k_with_ctrl_y_cut_and_put_back() {
    let mut b = ChatBox::with_text(Kind::Multi, "one two\nthree");
    press(&mut b, KeyCode::Up, KeyModifiers::NONE);
    press(&mut b, KeyCode::Char('a'), KeyModifiers::CONTROL);
    assert_eq!(b.cursor(), (0, 0));
    press(&mut b, KeyCode::Char('e'), KeyModifiers::CONTROL);
    assert_eq!(b.cursor(), (0, 7));
    press(&mut b, KeyCode::Char('a'), KeyModifiers::CONTROL);
    press(&mut b, KeyCode::Right, KeyModifiers::NONE);
    press(&mut b, KeyCode::Right, KeyModifiers::NONE);
    press(&mut b, KeyCode::Right, KeyModifiers::NONE);
    press(&mut b, KeyCode::Right, KeyModifiers::NONE);
    press(&mut b, KeyCode::Char('k'), KeyModifiers::CONTROL);
    assert_eq!(b.text(), "one \nthree");
    press(&mut b, KeyCode::Down, KeyModifiers::NONE);
    press(&mut b, KeyCode::Char('y'), KeyModifiers::CONTROL);
    assert!(b.text().contains("two"), "{:?}", b.text());
}

/// The crate lays the whole text out again after every edit, so an edit costs time in proportion to the text. A prompt of the size
/// the service accepts (256 KiB) gets into the box by a paste, which is one edit, and typing into it afterwards stays quick. Typing a
/// quarter of a million characters one key at a time is not something a person does and is not measured here.
#[test]
fn a_prompt_of_the_size_limit_is_pasted_in_one_edit_and_typing_into_it_stays_quick() {
    let limit = 256 * 1024 + 1;
    let mut b = ChatBox::new(Kind::Multi);
    let t = Instant::now();
    b.insert_str(&"x".repeat(limit));
    let paste = t.elapsed();
    let t = Instant::now();
    for _ in 0..50 {
        assert!(press(&mut b, KeyCode::Char('y'), KeyModifiers::NONE));
    }
    let typed = t.elapsed();
    let t = Instant::now();
    let rows = b.rows_at(118);
    let (screen, _) = drawn(&b, 120, 40);
    let draw = t.elapsed();
    eprintln!("PERF paste {paste:?}, 50 keys {typed:?}, rows+draw {draw:?}, rows {rows}");
    assert_eq!(b.text().len(), limit + 50);
    assert!(rows > 2000, "{rows}");
    assert!(screen.iter().any(|l| l.contains("xxxx")));
    let bound = Duration::from_secs(10);
    assert!(paste < bound, "a paste of the limit took {paste:?}");
    assert!(
        typed < bound,
        "50 keys in a limit-sized prompt took {typed:?}"
    );
    assert!(draw < bound, "a draw of a limit-sized prompt took {draw:?}");
}
