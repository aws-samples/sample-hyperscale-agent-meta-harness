//! The mouse on the update notice: the pure part. The notice on the first row is a region of the hit map (`Target::Update`). A press
//! on it waits for the button: when it comes up anywhere on the notice, the pointer gives `Action::Update` (the screen opens the page of
//! the update source); when it comes up elsewhere, nothing happens. The notice has no pane, so a move over it selects nothing. The rule
//! that the first press only clears a selection stays in front of it. The module reads no terminal; the clock is passed in.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ham::tui::mouse::{Action, BoxId, HitMap, PaneId, Pointer, Target, WheelSettings};
use ratatui::layout::Rect;
use std::time::Instant;

const FRAME: Rect = Rect {
    x: 0,
    y: 0,
    width: 100,
    height: 30,
};
const PROMPT: BoxId = BoxId(1);
const CHAT: PaneId = PaneId(1);
/// The cells of the notice on the first row: columns 58 to 72.
const NOTICE: Rect = Rect {
    x: 58,
    y: 0,
    width: 15,
    height: 1,
};

fn ev(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

fn down(x: u16, y: u16) -> MouseEvent {
    ev(MouseEventKind::Down(MouseButton::Left), x, y)
}
fn up(x: u16, y: u16) -> MouseEvent {
    ev(MouseEventKind::Up(MouseButton::Left), x, y)
}
fn drag(x: u16, y: u16) -> MouseEvent {
    ev(MouseEventKind::Drag(MouseButton::Left), x, y)
}

/// A screen with a text box, a pane of selectable text and, drawn last, the notice on the first row.
fn screen() -> HitMap {
    let mut m = HitMap::new();
    m.begin(FRAME);
    m.push(Rect::new(0, 3, 40, 3), Target::Box(PROMPT), "Tab");
    m.push(Rect::new(0, 10, 100, 15), Target::Text(CHAT), "y");
    m.push(NOTICE, Target::Update, "");
    m
}

fn pointer() -> Pointer {
    Pointer::new(WheelSettings::default())
}

fn t0() -> Instant {
    Instant::now()
}

#[test]
fn b513_a_click_acts_when_the_button_comes_up_on_the_notice() {
    let m = screen();
    let mut p = pointer();
    // Down and up on the same cell.
    assert_eq!(
        p.handle(&m, down(60, 0), t0()),
        vec![],
        "the press alone does nothing"
    );
    assert_eq!(p.handle(&m, up(60, 0), t0()), vec![Action::Update]);
    // Down on one cell of the notice and up on another: still a click on the notice.
    assert_eq!(p.handle(&m, down(58, 0), t0()), vec![]);
    assert_eq!(p.handle(&m, up(72, 0), t0()), vec![Action::Update]);
    // The pointer moved over the notice while the button was held: nothing is selected, and the click acts.
    assert_eq!(p.handle(&m, down(59, 0), t0()), vec![]);
    assert_eq!(p.handle(&m, drag(65, 0), t0()), vec![]);
    assert_eq!(p.handle(&m, up(65, 0), t0()), vec![Action::Update]);
}

#[test]
fn b513_a_press_on_the_notice_that_comes_up_elsewhere_does_nothing() {
    let m = screen();
    for (x, y, what) in [
        (57, 0, "one cell left of the notice"),
        (73, 0, "one cell right of the notice"),
        (60, 1, "the row under the notice"),
        (5, 4, "a text box"),
        (5, 12, "the text pane"),
    ] {
        let mut p = pointer();
        assert_eq!(p.handle(&m, down(60, 0), t0()), vec![], "{what}");
        assert_eq!(
            p.handle(&m, drag(x, y), t0()),
            vec![],
            "{what}: no selection starts"
        );
        assert_eq!(p.handle(&m, up(x, y), t0()), vec![], "{what}");
        // The gesture is over: a release with no press does nothing either.
        assert_eq!(p.handle(&m, up(60, 0), t0()), vec![], "{what}");
    }
}

#[test]
fn b513_a_press_elsewhere_that_comes_up_on_the_notice_does_nothing() {
    let m = screen();
    // From a text box.
    let mut p = pointer();
    assert_eq!(p.handle(&m, down(5, 4), t0()), vec![Action::Focus(PROMPT)]);
    assert_eq!(p.handle(&m, up(60, 0), t0()), vec![]);
    // From a cell that holds nothing.
    let mut p = pointer();
    assert_eq!(p.handle(&m, down(90, 0), t0()), vec![]);
    assert_eq!(p.handle(&m, up(60, 0), t0()), vec![]);
    // From the text pane: that is a selection of text, as before, and never the notice's click.
    let mut p = pointer();
    assert_eq!(p.handle(&m, down(5, 12), t0()), vec![]);
    let actions = p.handle(&m, up(60, 0), t0());
    assert!(!actions.contains(&Action::Update), "{actions:?}");
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::SelectionEnd { pane, .. } if *pane == CHAT)),
        "{actions:?}"
    );
}

#[test]
fn b513_while_text_is_selected_the_first_press_only_clears_the_selection() {
    let m = screen();
    let mut p = pointer();
    p.handle(&m, down(5, 12), t0());
    p.handle(&m, drag(20, 12), t0());
    let made = p.handle(&m, up(20, 12), t0());
    assert!(
        made.iter()
            .any(|a| matches!(a, Action::SelectionEnd { .. })),
        "{made:?}"
    );
    // The first click on the notice takes the selection away and does nothing else.
    assert_eq!(
        p.handle(&m, down(60, 0), t0()),
        vec![Action::ClearSelection]
    );
    assert_eq!(p.handle(&m, up(60, 0), t0()), vec![]);
    // The next click acts.
    assert_eq!(p.handle(&m, down(60, 0), t0()), vec![]);
    assert_eq!(p.handle(&m, up(60, 0), t0()), vec![Action::Update]);
}

#[test]
fn b513_a_resize_between_press_and_release_drops_the_click() {
    let mut m = screen();
    let mut p = pointer();
    assert_eq!(p.handle(&m, down(60, 0), t0()), vec![]);
    m.resized();
    assert_eq!(
        p.handle(&m, up(60, 0), t0()),
        vec![],
        "a stale map names no target"
    );
    // After the next draw a new click acts.
    m.begin(FRAME);
    m.push(NOTICE, Target::Update, "");
    assert_eq!(p.handle(&m, down(60, 0), t0()), vec![]);
    assert_eq!(p.handle(&m, up(60, 0), t0()), vec![Action::Update]);
}

#[test]
fn b513_the_notice_is_the_one_region_without_a_key() {
    // Every other click has a key that does the same. The notice has none in this release: its click opens a web page, and the update
    // itself is `ham update` typed in a shell.
    let m = screen();
    let unkeyed = m.unkeyed();
    assert_eq!(unkeyed.len(), 1, "{unkeyed:?}");
    assert_eq!(unkeyed[0].target, Target::Update);
    assert_eq!(unkeyed[0].rect, NOTICE);
}
