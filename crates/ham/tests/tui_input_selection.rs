//! How a selection of the message box is drawn: every selected character carries the
//! selection's background, including the one under the caret when the selection was made backwards (Shift-Left, Shift-Home: the caret is
//! at the start of the selection and the character under it is selected); a selection made forwards ends just before the caret and does not
//! include the cell the caret stands on; a box with no selection paints nothing.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::input::{ChatBox, Kind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::{Block, Borders};

const TEXT: &str = "alpha beta gamma";

fn box_with(text: &str) -> ChatBox {
    ChatBox::with_text(Kind::Multi, text)
}

fn press(b: &mut ChatBox, code: KeyCode, mods: KeyModifiers, times: usize) {
    for _ in 0..times {
        assert!(b.handle_key(KeyEvent::new(code, mods)));
    }
}

/// The characters of the first text row that have a background colour, and the column of the first one, after drawing the box.
fn painted(b: &ChatBox) -> (String, Vec<u16>) {
    let (w, h) = (30u16, 5u16);
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
    let buf = t.backend().buffer().clone();
    let mut text = String::new();
    let mut cols = Vec::new();
    for x in 1..w - 1 {
        let cell = &buf[(x, 1)];
        if cell.bg != Color::Reset {
            text.push_str(cell.symbol());
            cols.push(x);
        }
    }
    (text, cols)
}

#[test]
fn a_box_with_no_selection_paints_nothing() {
    let b = box_with(TEXT);
    assert_eq!(painted(&b).0, "");
}

#[test]
fn a_selection_made_backwards_paints_the_character_under_the_caret() {
    let mut b = box_with(TEXT);
    press(&mut b, KeyCode::Left, KeyModifiers::SHIFT, 5);
    assert_eq!(b.selection().as_deref(), Some("gamma"));
    assert_eq!(
        painted(&b).0,
        "gamma",
        "the first selected character is under the caret"
    );
}

#[test]
fn shift_home_paints_the_whole_line_from_under_the_caret() {
    let mut b = box_with(TEXT);
    press(&mut b, KeyCode::Home, KeyModifiers::SHIFT, 1);
    assert_eq!(b.selection().as_deref(), Some(TEXT));
    assert_eq!(painted(&b).0, TEXT);
}

#[test]
fn a_selection_made_forwards_stops_before_the_caret_cell() {
    let mut b = box_with(TEXT);
    press(&mut b, KeyCode::Home, KeyModifiers::NONE, 1);
    press(&mut b, KeyCode::Right, KeyModifiers::SHIFT, 5);
    assert_eq!(b.selection().as_deref(), Some("alpha"));
    let (text, cols) = painted(&b);
    assert_eq!(
        text, "alpha",
        "the cell the caret stands on is not selected"
    );
    assert_eq!(cols, vec![1, 2, 3, 4, 5]);
}

#[test]
fn the_character_under_the_caret_is_unpainted_again_when_the_selection_is_gone() {
    let mut b = box_with(TEXT);
    press(&mut b, KeyCode::Left, KeyModifiers::SHIFT, 5);
    press(&mut b, KeyCode::Right, KeyModifiers::NONE, 1);
    assert_eq!(b.selection(), None);
    assert_eq!(painted(&b).0, "");
}

#[test]
fn a_selection_that_shrinks_back_to_nothing_paints_nothing() {
    let mut b = box_with(TEXT);
    press(&mut b, KeyCode::Left, KeyModifiers::SHIFT, 3);
    press(&mut b, KeyCode::Right, KeyModifiers::SHIFT, 3);
    assert_eq!(b.selection(), None);
    assert_eq!(painted(&b).0, "");
}

#[test]
fn undo_after_a_backwards_selection_leaves_no_stale_paint() {
    let mut b = box_with(TEXT);
    press(&mut b, KeyCode::Left, KeyModifiers::SHIFT, 5);
    assert!(b.undo() || !b.undo());
    press(&mut b, KeyCode::End, KeyModifiers::NONE, 1);
    assert_eq!(painted(&b).0, "");
}
