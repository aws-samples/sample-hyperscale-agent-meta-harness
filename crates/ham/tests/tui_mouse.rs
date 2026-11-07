//! The pure part of the mouse. `tui::mouse` holds the hit map every draw records (rectangles for boxes, rows,
//! folds, key chips, links and text panes, later draws over earlier ones) and the pointer state machine that turns crossterm mouse
//! events into actions: a press on a box focuses it, on a run row selects it (a second click opens it), on a fold toggles it; a click
//! does nothing while text is selected; the wheel scrolls with a configurable step and a small acceleration. The module reads no
//! terminal and draws nothing, and the clock is injected, so these tests count and compare, they never sleep.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ham::tui::mouse::{
    ACCEL_GAP, AUTOSCROLL_EVERY, Action, BoxId, DEFAULT_WHEEL_STEP, DOUBLE_CLICK, HitMap, ListId,
    MAX_WHEEL_STEP, PaneId, Pointer, Target, WheelSettings, stable_id,
};
use proptest::prelude::*;
use ratatui::layout::Rect;
use std::time::{Duration, Instant};

const FRAME: Rect = Rect {
    x: 0,
    y: 0,
    width: 100,
    height: 30,
};
const PROMPT: BoxId = BoxId(1);
const REPO: BoxId = BoxId(2);
const RUNS: ListId = ListId(1);
const CHAT: PaneId = PaneId(1);

/// The stable identity the screen gives run `i` (a hash of the run id in the app).
fn rid(i: usize) -> u64 {
    100 + i as u64
}

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
fn wheel_down(x: u16, y: u16) -> MouseEvent {
    ev(MouseEventKind::ScrollDown, x, y)
}
fn wheel_up(x: u16, y: u16) -> MouseEvent {
    ev(MouseEventKind::ScrollUp, x, y)
}

/// A screen like the composer: two boxes, a list of three runs, a chat pane with one foldable block head on its second row, a key chip
/// and a link.
fn screen() -> HitMap {
    let mut m = HitMap::new();
    m.begin(FRAME);
    m.push(Rect::new(0, 0, 40, 3), Target::Box(PROMPT), "Tab");
    m.push(Rect::new(0, 3, 40, 3), Target::Box(REPO), "Tab");
    for i in 0..3 {
        m.push(
            Rect::new(50, 2 + i as u16, 40, 1),
            Target::Row {
                list: RUNS,
                index: i,
                id: rid(i),
            },
            "Enter",
        );
    }
    m.push(Rect::new(0, 10, 100, 15), Target::Text(CHAT), "y");
    m.push(
        Rect::new(7, 11, 60, 1),
        Target::Fold {
            block: "b1".to_string(),
        },
        "Enter",
    );
    m.push(
        Rect::new(60, 27, 10, 1),
        Target::Key(KeyCode::Char('p')),
        "p",
    );
    m.push(Rect::new(75, 27, 10, 1), Target::Link(3), "o");
    m
}

fn pointer() -> Pointer {
    Pointer::new(WheelSettings::default())
}

fn t0() -> Instant {
    Instant::now()
}

// ---- the hit map --------------------------------------------------------------------------------------------------------------------

#[test]
fn a_point_finds_the_region_that_holds_it_and_the_edges_are_half_open() {
    let m = screen();
    let hit = |x, y| m.target_at(x, y).map(|r| r.target.clone());
    assert_eq!(hit(0, 0), Some(Target::Box(PROMPT)));
    assert_eq!(hit(39, 2), Some(Target::Box(PROMPT)));
    assert_eq!(hit(40, 2), None, "the right edge belongs to the next cell");
    assert_eq!(hit(0, 3), Some(Target::Box(REPO)));
    assert_eq!(hit(0, 6), None, "below the second box");
    assert_eq!(
        hit(50, 3),
        Some(Target::Row {
            list: RUNS,
            index: 1,
            id: rid(1)
        })
    );
}

#[test]
fn a_region_drawn_later_wins_where_two_overlap() {
    let mut m = HitMap::new();
    m.begin(FRAME);
    m.push(Rect::new(0, 0, 50, 20), Target::Text(CHAT), "y");
    m.push(
        Rect::new(10, 5, 20, 5),
        Target::Row {
            list: RUNS,
            index: 7,
            id: rid(7),
        },
        "Enter",
    );
    m.push(Rect::new(15, 6, 3, 1), Target::Block, "Esc");
    assert_eq!(m.target_at(16, 6).unwrap().target, Target::Block);
    assert_eq!(
        m.target_at(11, 6).unwrap().target,
        Target::Row {
            list: RUNS,
            index: 7,
            id: rid(7)
        }
    );
    assert_eq!(m.target_at(1, 1).unwrap().target, Target::Text(CHAT));
}

#[test]
fn a_new_draw_forgets_the_old_regions() {
    let mut m = screen();
    assert!(!m.is_empty());
    m.begin(FRAME);
    assert!(m.is_empty());
    assert!(m.target_at(0, 0).is_none());
}

#[test]
fn regions_are_clipped_to_the_frame_and_empty_ones_are_dropped() {
    let mut m = HitMap::new();
    m.begin(Rect::new(0, 0, 20, 10));
    m.push(Rect::new(15, 5, 30, 30), Target::Text(CHAT), "y");
    m.push(Rect::new(3, 3, 0, 4), Target::Box(PROMPT), "Tab");
    m.push(Rect::new(3, 3, 4, 0), Target::Box(REPO), "Tab");
    m.push(Rect::new(40, 40, 5, 5), Target::Link(1), "o");
    assert_eq!(m.len(), 1);
    assert_eq!(m.regions()[0].rect, Rect::new(15, 5, 5, 5));
    assert!(m.target_at(19, 9).is_some());
    assert!(m.target_at(20, 9).is_none());
    assert!(m.target_at(19, 10).is_none());
}

#[test]
fn a_resize_makes_the_map_stale_until_the_next_draw() {
    let mut m = screen();
    assert!(!m.is_stale());
    m.resized();
    assert!(m.is_stale());
    assert!(m.target_at(0, 0).is_none(), "a stale map names no target");
    m.begin(Rect::new(0, 0, 60, 20));
    assert!(!m.is_stale());
    m.push(Rect::new(0, 0, 10, 3), Target::Box(PROMPT), "Tab");
    assert!(m.target_at(1, 1).is_some());
}

#[test]
fn every_region_names_the_key_that_does_the_same() {
    let mut m = screen();
    assert!(m.unkeyed().is_empty());
    m.push(Rect::new(0, 28, 5, 1), Target::Link(9), "");
    let bad = m.unkeyed();
    assert_eq!(bad.len(), 1);
    assert_eq!(bad[0].target, Target::Link(9));
}

#[test]
fn a_text_pane_is_found_under_the_regions_drawn_over_it() {
    let m = screen();
    let pane = m.pane_at(8, 11).unwrap();
    assert_eq!(pane.target, Target::Text(CHAT));
    assert_eq!(m.text_at(8, 11).unwrap().target, Target::Text(CHAT));
    assert!(m.pane_at(2, 1).is_none(), "a box is not a pane");
    assert!(m.text_at(2, 1).is_none());
    // A scroll region is a pane for the wheel and not text to select.
    let mut m = HitMap::new();
    m.begin(FRAME);
    m.push(Rect::new(0, 0, 10, 10), Target::Text(CHAT), "y");
    m.push(Rect::new(2, 2, 4, 4), Target::Scroll(PaneId(9)), "j");
    assert_eq!(m.pane_at(3, 3).unwrap().target, Target::Scroll(PaneId(9)));
    assert_eq!(m.text_at(3, 3).unwrap().target, Target::Text(CHAT));
}

// ---- press ------------------------------------------------------------------------------------------------------------------------

#[test]
fn a_press_on_a_box_focuses_it() {
    let m = screen();
    let mut p = pointer();
    assert_eq!(p.handle(&m, down(5, 1), t0()), vec![Action::Focus(PROMPT)]);
    assert_eq!(p.handle(&m, up(5, 1), t0()), vec![]);
    assert_eq!(p.handle(&m, down(5, 4), t0()), vec![Action::Focus(REPO)]);
}

#[test]
fn a_press_on_a_key_chip_is_the_key() {
    let m = screen();
    let mut p = pointer();
    assert_eq!(
        p.handle(&m, down(62, 27), t0()),
        vec![Action::Press(KeyCode::Char('p'))]
    );
}

#[test]
fn a_press_on_a_link_opens_it() {
    let m = screen();
    let mut p = pointer();
    assert_eq!(p.handle(&m, down(76, 27), t0()), vec![Action::OpenLink(3)]);
}

#[test]
fn a_press_on_nothing_does_nothing() {
    let m = screen();
    let mut p = pointer();
    assert_eq!(p.handle(&m, down(45, 8), t0()), vec![]);
}

#[test]
fn a_run_row_selects_and_a_second_click_inside_the_window_opens_it() {
    let m = screen();
    let mut p = pointer();
    let t = t0();
    let row = |i| Action::Select {
        list: RUNS,
        index: i,
        id: rid(i),
    };
    assert_eq!(p.handle(&m, down(60, 3), t), vec![row(1)]);
    p.handle(&m, up(60, 3), t);
    assert_eq!(
        p.handle(&m, down(60, 3), t + Duration::from_millis(200)),
        vec![Action::Open {
            list: RUNS,
            index: 1,
            id: rid(1)
        }]
    );
    // The chain is spent: a third click selects again.
    assert_eq!(
        p.handle(&m, down(60, 3), t + Duration::from_millis(300)),
        vec![row(1)]
    );
}

#[test]
fn a_second_click_after_the_window_only_selects_again() {
    let m = screen();
    let mut p = pointer();
    let t = t0();
    p.handle(&m, down(60, 2), t);
    let late = t + DOUBLE_CLICK + Duration::from_millis(1);
    assert_eq!(
        p.handle(&m, down(60, 2), late),
        vec![Action::Select {
            list: RUNS,
            index: 0,
            id: rid(0)
        }]
    );
}

#[test]
fn the_window_includes_its_last_instant() {
    let m = screen();
    let mut p = pointer();
    let t = t0();
    p.handle(&m, down(60, 2), t);
    assert_eq!(
        p.handle(&m, down(60, 2), t + DOUBLE_CLICK),
        vec![Action::Open {
            list: RUNS,
            index: 0,
            id: rid(0)
        }]
    );
}

#[test]
fn a_click_on_another_row_selects_it_and_breaks_the_chain() {
    let m = screen();
    let mut p = pointer();
    let t = t0();
    p.handle(&m, down(60, 2), t);
    let a = p.handle(&m, down(60, 3), t + Duration::from_millis(50));
    assert_eq!(
        a,
        vec![Action::Select {
            list: RUNS,
            index: 1,
            id: rid(1)
        }]
    );
    let b = p.handle(&m, down(60, 2), t + Duration::from_millis(100));
    assert_eq!(
        b,
        vec![Action::Select {
            list: RUNS,
            index: 0,
            id: rid(0)
        }]
    );
}

#[test]
fn a_key_press_between_two_clicks_breaks_the_chain() {
    let m = screen();
    let mut p = pointer();
    let t = t0();
    p.handle(&m, down(60, 2), t);
    p.note_key();
    assert_eq!(
        p.handle(&m, down(60, 2), t + Duration::from_millis(50)),
        vec![Action::Select {
            list: RUNS,
            index: 0,
            id: rid(0)
        }]
    );
}

#[test]
fn a_press_on_a_box_between_two_row_clicks_breaks_the_chain() {
    let m = screen();
    let mut p = pointer();
    let t = t0();
    p.handle(&m, down(60, 2), t);
    p.handle(&m, down(5, 1), t + Duration::from_millis(10));
    assert_eq!(
        p.handle(&m, down(60, 2), t + Duration::from_millis(20)),
        vec![Action::Select {
            list: RUNS,
            index: 0,
            id: rid(0)
        }]
    );
}

#[test]
fn a_clock_that_goes_backwards_never_opens() {
    let m = screen();
    let mut p = pointer();
    let t = t0() + Duration::from_secs(10);
    p.handle(&m, down(60, 2), t);
    let earlier = t - Duration::from_secs(5);
    assert_eq!(
        p.handle(&m, down(60, 2), earlier),
        vec![Action::Select {
            list: RUNS,
            index: 0,
            id: rid(0)
        }]
    );
}

// ---- folds and text ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_click_on_a_fold_toggles_it_when_the_button_comes_up_on_the_same_cell() {
    let m = screen();
    let mut p = pointer();
    assert_eq!(p.handle(&m, down(20, 11), t0()), vec![]);
    assert_eq!(
        p.handle(&m, up(20, 11), t0()),
        vec![Action::ToggleFold {
            block: "b1".to_string()
        }]
    );
}

#[test]
fn a_drag_that_starts_on_a_fold_selects_text_and_does_not_toggle() {
    let m = screen();
    let mut p = pointer();
    p.handle(&m, down(20, 11), t0());
    let a = p.handle(&m, drag(30, 12), t0());
    assert_eq!(
        a,
        vec![
            Action::SelectionStart {
                pane: CHAT,
                x: 20,
                y: 11
            },
            Action::SelectionExtend {
                pane: CHAT,
                x: 30,
                y: 12
            },
        ]
    );
    let b = p.handle(&m, drag(32, 12), t0());
    assert_eq!(
        b,
        vec![Action::SelectionExtend {
            pane: CHAT,
            x: 32,
            y: 12
        }]
    );
    let c = p.handle(&m, up(33, 12), t0());
    assert_eq!(
        c,
        vec![Action::SelectionEnd {
            pane: CHAT,
            x: 33,
            y: 12
        }]
    );
    assert!(p.has_selection());
}

#[test]
fn a_release_elsewhere_with_no_drag_report_still_selects() {
    let m = screen();
    let mut p = pointer();
    p.handle(&m, down(10, 14), t0());
    let a = p.handle(&m, up(25, 16), t0());
    assert_eq!(
        a,
        vec![
            Action::SelectionStart {
                pane: CHAT,
                x: 10,
                y: 14
            },
            Action::SelectionExtend {
                pane: CHAT,
                x: 25,
                y: 16
            },
            Action::SelectionEnd {
                pane: CHAT,
                x: 25,
                y: 16
            },
        ]
    );
}

#[test]
fn a_click_on_plain_text_without_a_drag_selects_nothing() {
    let m = screen();
    let mut p = pointer();
    assert_eq!(p.handle(&m, down(10, 14), t0()), vec![]);
    assert_eq!(p.handle(&m, up(10, 14), t0()), vec![]);
    assert!(!p.has_selection());
}

#[test]
fn a_drag_past_the_pane_is_clamped_into_it_and_scrolls_it() {
    let m = screen();
    let mut p = pointer();
    p.handle(&m, down(10, 14), t0());
    let a = p.handle(&m, drag(130, 4), t0());
    assert_eq!(
        a,
        vec![
            Action::SelectionStart {
                pane: CHAT,
                x: 10,
                y: 14
            },
            Action::SelectionExtend {
                pane: CHAT,
                x: 99,
                y: 10
            },
            Action::Autoscroll {
                pane: CHAT,
                lines: -(DEFAULT_WHEEL_STEP as i32)
            },
        ]
    );
    let b = p.handle(&m, drag(5, 29), t0());
    assert_eq!(
        b,
        vec![
            Action::SelectionExtend {
                pane: CHAT,
                x: 5,
                y: 24
            },
            Action::Autoscroll {
                pane: CHAT,
                lines: DEFAULT_WHEEL_STEP as i32
            },
        ]
    );
}

#[test]
fn a_drag_with_no_press_does_nothing() {
    let m = screen();
    let mut p = pointer();
    assert_eq!(p.handle(&m, drag(10, 14), t0()), vec![]);
    assert_eq!(p.handle(&m, up(10, 14), t0()), vec![]);
}

// ---- a click does nothing while text is selected ------------------------------------------------------------------------------------

fn with_selection(m: &HitMap, p: &mut Pointer) {
    p.handle(m, down(10, 14), t0());
    p.handle(m, drag(20, 14), t0());
    p.handle(m, up(21, 14), t0());
    assert!(p.has_selection());
}

#[test]
fn while_text_is_selected_the_next_click_only_clears_the_selection() {
    let m = screen();
    let mut p = pointer();
    with_selection(&m, &mut p);
    assert_eq!(
        p.handle(&m, down(5, 1), t0()),
        vec![Action::ClearSelection],
        "no focus"
    );
    assert!(!p.has_selection());
    assert_eq!(p.handle(&m, up(5, 1), t0()), vec![]);
    assert_eq!(p.handle(&m, down(5, 1), t0()), vec![Action::Focus(PROMPT)]);
}

#[test]
fn while_text_is_selected_a_row_click_does_not_select_the_row() {
    let m = screen();
    let mut p = pointer();
    with_selection(&m, &mut p);
    assert_eq!(
        p.handle(&m, down(60, 2), t0()),
        vec![Action::ClearSelection]
    );
}

#[test]
fn while_text_is_selected_a_press_on_text_clears_it_and_starts_a_new_gesture() {
    let m = screen();
    let mut p = pointer();
    with_selection(&m, &mut p);
    assert_eq!(
        p.handle(&m, down(40, 20), t0()),
        vec![Action::ClearSelection]
    );
    assert!(!p.has_selection());
    let a = p.handle(&m, drag(45, 20), t0());
    assert_eq!(
        a,
        vec![
            Action::SelectionStart {
                pane: CHAT,
                x: 40,
                y: 20
            },
            Action::SelectionExtend {
                pane: CHAT,
                x: 45,
                y: 20
            },
        ]
    );
}

#[test]
fn the_screen_can_clear_the_selection_by_key() {
    let m = screen();
    let mut p = pointer();
    with_selection(&m, &mut p);
    p.clear_selection();
    assert!(!p.has_selection());
    assert_eq!(p.handle(&m, down(5, 1), t0()), vec![Action::Focus(PROMPT)]);
}

#[test]
fn a_drag_that_comes_back_to_where_it_started_is_a_click() {
    let m = screen();
    let mut p = pointer();
    p.handle(&m, down(20, 11), t0());
    p.handle(&m, drag(30, 12), t0());
    p.handle(&m, drag(20, 11), t0());
    assert_eq!(
        p.handle(&m, up(20, 11), t0()),
        vec![
            Action::ClearSelection,
            Action::ToggleFold {
                block: "b1".to_string()
            }
        ]
    );
    assert!(!p.has_selection());
    // The same on plain text: nothing is left selected.
    p.handle(&m, down(10, 14), t0());
    p.handle(&m, drag(20, 14), t0());
    p.handle(&m, drag(10, 14), t0());
    assert_eq!(p.handle(&m, up(10, 14), t0()), vec![Action::ClearSelection]);
    assert!(!p.has_selection());
}

#[test]
fn a_fold_toggles_only_when_the_release_is_still_on_that_fold() {
    let m = screen();
    let mut p = pointer();
    p.handle(&m, down(20, 11), t0());
    // The screen is drawn again without the fold (the block scrolled away) before the button comes up.
    let mut m2 = HitMap::new();
    m2.begin(FRAME);
    m2.push(Rect::new(0, 10, 100, 15), Target::Text(CHAT), "y");
    assert_eq!(p.handle(&m2, up(20, 11), t0()), vec![]);
    // Another fold at the same place is another block: no toggle either.
    p.handle(&m, down(20, 11), t0());
    let mut m3 = HitMap::new();
    m3.begin(FRAME);
    m3.push(Rect::new(0, 10, 100, 15), Target::Text(CHAT), "y");
    m3.push(
        Rect::new(7, 11, 60, 1),
        Target::Fold {
            block: "b2".to_string(),
        },
        "Enter",
    );
    assert_eq!(p.handle(&m3, up(20, 11), t0()), vec![]);
}

#[test]
fn holding_a_drag_outside_the_pane_keeps_scrolling_on_the_tick() {
    let m = screen();
    let mut p = pointer();
    let t = t0();
    p.handle(&m, down(10, 14), t);
    let first = p.handle(&m, drag(10, 4), t);
    assert!(first.contains(&Action::Autoscroll {
        pane: CHAT,
        lines: -3
    }));
    assert_eq!(p.tick(&m, t + AUTOSCROLL_EVERY / 2), vec![], "too soon");
    assert_eq!(
        p.tick(&m, t + AUTOSCROLL_EVERY),
        vec![
            Action::Autoscroll {
                pane: CHAT,
                lines: -3
            },
            Action::SelectionExtend {
                pane: CHAT,
                x: 10,
                y: 10
            },
        ]
    );
    assert_eq!(
        p.tick(&m, t + AUTOSCROLL_EVERY),
        vec![],
        "once per interval"
    );
    // Back inside the pane: nothing to repeat.
    p.handle(&m, drag(10, 15), t + AUTOSCROLL_EVERY);
    assert_eq!(p.tick(&m, t + AUTOSCROLL_EVERY * 5), vec![]);
    // Below the pane it scrolls the other way, and a release ends it.
    p.handle(&m, drag(10, 29), t + AUTOSCROLL_EVERY * 6);
    let down_again = p.tick(&m, t + AUTOSCROLL_EVERY * 8);
    assert_eq!(
        down_again[0],
        Action::Autoscroll {
            pane: CHAT,
            lines: 3
        }
    );
    p.handle(&m, up(10, 29), t + AUTOSCROLL_EVERY * 8);
    assert_eq!(p.tick(&m, t + AUTOSCROLL_EVERY * 20), vec![]);
}

#[test]
fn a_tick_with_no_drag_does_nothing() {
    let m = screen();
    let mut p = pointer();
    assert_eq!(p.tick(&m, t0()), vec![]);
    p.handle(&m, down(10, 14), t0());
    assert_eq!(p.tick(&m, t0() + Duration::from_secs(1)), vec![]);
}

#[test]
fn the_second_click_must_be_on_the_same_run_not_the_same_row_number() {
    let t = t0();
    let mut p = pointer();
    p.handle(&screen(), down(60, 2), t);
    // The list was sorted again between the clicks: row 0 is another run now.
    let mut m2 = HitMap::new();
    m2.begin(FRAME);
    m2.push(
        Rect::new(50, 2, 40, 1),
        Target::Row {
            list: RUNS,
            index: 0,
            id: 555,
        },
        "Enter",
    );
    assert_eq!(
        p.handle(&m2, down(60, 2), t + Duration::from_millis(100)),
        vec![Action::Select {
            list: RUNS,
            index: 0,
            id: 555
        }]
    );
}

#[test]
fn a_wheel_between_two_clicks_breaks_the_chain() {
    let m = screen();
    let mut p = pointer();
    let t = t0();
    p.handle(&m, down(60, 2), t);
    p.handle(&m, wheel_down(60, 2), t + Duration::from_millis(10));
    assert_eq!(
        p.handle(&m, down(60, 2), t + Duration::from_millis(20)),
        vec![Action::Select {
            list: RUNS,
            index: 0,
            id: rid(0)
        }]
    );
}

#[test]
fn a_resize_between_two_clicks_breaks_the_chain_even_after_the_next_draw() {
    let mut m = screen();
    let mut p = pointer();
    let t = t0();
    p.handle(&m, down(60, 2), t);
    m.resized();
    m.begin(FRAME);
    for i in 0..3 {
        m.push(
            Rect::new(50, 2 + i as u16, 40, 1),
            Target::Row {
                list: RUNS,
                index: i,
                id: rid(i),
            },
            "Enter",
        );
    }
    assert_eq!(
        p.handle(&m, down(60, 2), t + Duration::from_millis(20)),
        vec![Action::Select {
            list: RUNS,
            index: 0,
            id: rid(0)
        }]
    );
}

#[test]
fn a_resize_during_a_drag_ends_it_even_after_the_next_draw() {
    let mut m = screen();
    let mut p = pointer();
    p.handle(&m, down(10, 14), t0());
    p.handle(&m, drag(20, 14), t0());
    m.resized();
    m.begin(FRAME);
    m.push(Rect::new(0, 10, 100, 15), Target::Text(CHAT), "y");
    // The half-made selection is dropped once, and the rest of the gesture is ignored.
    assert_eq!(
        p.handle(&m, drag(30, 14), t0()),
        vec![Action::ClearSelection]
    );
    assert_eq!(p.handle(&m, drag(31, 14), t0()), vec![]);
    assert_eq!(p.handle(&m, up(31, 14), t0()), vec![]);
    assert!(!p.has_selection());
}

#[test]
fn stable_ids_come_from_the_text_of_the_run_id() {
    assert_eq!(stable_id("01M5287J"), stable_id("01M5287J"));
    assert_ne!(stable_id("01M5287J"), stable_id("01M5287K"));
    assert_ne!(stable_id(""), stable_id("a"));
}

// ---- the wheel --------------------------------------------------------------------------------------------------------------------

#[test]
fn the_wheel_scrolls_the_pane_under_the_pointer_by_the_step() {
    let m = screen();
    let mut p = pointer();
    assert_eq!(
        p.handle(&m, wheel_down(10, 14), t0()),
        vec![Action::Wheel {
            pane: Some(CHAT),
            lines: DEFAULT_WHEEL_STEP as i32
        }]
    );
    let later = t0() + Duration::from_secs(5);
    assert_eq!(
        p.handle(&m, wheel_up(10, 14), later),
        vec![Action::Wheel {
            pane: Some(CHAT),
            lines: -(DEFAULT_WHEEL_STEP as i32)
        }]
    );
}

#[test]
fn the_wheel_over_no_pane_goes_to_the_screen() {
    let m = screen();
    let mut p = pointer();
    assert_eq!(
        p.handle(&m, wheel_down(45, 8), t0()),
        vec![Action::Wheel {
            pane: None,
            lines: 3
        }]
    );
}

#[test]
fn the_wheel_over_a_scroll_region_names_it() {
    let mut m = HitMap::new();
    m.begin(FRAME);
    m.push(Rect::new(0, 0, 40, 10), Target::Scroll(PaneId(4)), "j");
    let mut p = pointer();
    assert_eq!(
        p.handle(&m, wheel_down(3, 3), t0()),
        vec![Action::Wheel {
            pane: Some(PaneId(4)),
            lines: 3
        }]
    );
}

#[test]
fn the_step_is_configurable_and_clamped() {
    assert_eq!(WheelSettings::new(5).step, 5);
    assert_eq!(WheelSettings::new(0).step, 1);
    assert_eq!(WheelSettings::new(500).step, MAX_WHEEL_STEP);
    let m = screen();
    let mut p = Pointer::new(WheelSettings::new(7));
    assert_eq!(
        p.handle(&m, wheel_down(10, 14), t0()),
        vec![Action::Wheel {
            pane: Some(CHAT),
            lines: 7
        }]
    );
}

#[test]
fn the_step_setting_parses_a_number_and_refuses_the_rest() {
    let cases: [(&str, Option<u16>); 9] = [
        ("3", Some(3)),
        (" 12 ", Some(12)),
        ("1", Some(1)),
        ("30", Some(30)),
        ("0", None),
        ("31", None),
        ("-2", None),
        ("many", None),
        ("", None),
    ];
    for (text, want) in cases {
        assert_eq!(WheelSettings::parse(text).map(|s| s.step), want, "{text:?}");
    }
}

fn lines_of(a: &[Action]) -> i32 {
    match a {
        [Action::Wheel { lines, .. }] => *lines,
        other => panic!("not one wheel action: {other:?}"),
    }
}

#[test]
fn a_fast_run_of_notches_in_one_direction_speeds_up_to_a_cap() {
    let m = screen();
    let mut p = Pointer::new(WheelSettings::new(2));
    let t = t0();
    let gap = ACCEL_GAP - Duration::from_millis(1);
    let seen: Vec<i32> = (0..20)
        .map(|i| lines_of(&p.handle(&m, wheel_down(10, 14), t + gap * i)))
        .collect();
    assert_eq!(seen[0], 2);
    assert!(
        seen.windows(2).all(|w| w[1] >= w[0]),
        "never slows down inside a run: {seen:?}"
    );
    assert!(seen.iter().any(|l| *l > 2), "it speeds up: {seen:?}");
    assert_eq!(*seen.last().unwrap(), 8, "capped at four times the step");
}

#[test]
fn a_pause_or_a_change_of_direction_ends_the_acceleration() {
    let m = screen();
    let mut p = Pointer::new(WheelSettings::new(2));
    let t = t0();
    let gap = ACCEL_GAP - Duration::from_millis(1);
    let mut at = t;
    for _ in 0..12 {
        p.handle(&m, wheel_down(10, 14), at);
        at += gap;
    }
    let fast = lines_of(&p.handle(&m, wheel_down(10, 14), at));
    assert!(fast > 2);
    // Opposite direction right away: back to one step.
    at += gap;
    assert_eq!(lines_of(&p.handle(&m, wheel_up(10, 14), at)), -2);
    // Same direction after a pause: back to one step.
    for _ in 0..12 {
        at += gap;
        p.handle(&m, wheel_up(10, 14), at);
    }
    at += ACCEL_GAP + Duration::from_millis(1);
    assert_eq!(lines_of(&p.handle(&m, wheel_up(10, 14), at)), -2);
}

#[test]
fn acceleration_can_be_switched_off() {
    let m = screen();
    let mut p = Pointer::new(WheelSettings {
        step: 3,
        accelerate: false,
    });
    let t = t0();
    for i in 0..30 {
        let a = p.handle(&m, wheel_down(10, 14), t + Duration::from_millis(i));
        assert_eq!(lines_of(&a), 3);
    }
}

#[test]
fn the_wheel_works_while_text_is_selected_and_does_not_clear_it() {
    let m = screen();
    let mut p = pointer();
    with_selection(&m, &mut p);
    let a = p.handle(&m, wheel_down(10, 14), t0());
    assert_eq!(lines_of(&a), 3);
    assert!(p.has_selection());
}

#[test]
fn the_wheel_never_reaches_a_pane_that_a_popup_covers() {
    let mut m = HitMap::new();
    m.begin(FRAME);
    m.push(Rect::new(0, 0, 40, 20), Target::Text(CHAT), "y");
    m.push(Rect::new(5, 5, 10, 5), Target::Block, "Esc");
    let mut p = pointer();
    assert_eq!(p.handle(&m, wheel_down(6, 6), t0()), vec![]);
    assert_eq!(
        p.handle(&m, wheel_down(30, 15), t0() + Duration::from_secs(1)),
        vec![Action::Wheel {
            pane: Some(CHAT),
            lines: 3
        }]
    );
}

#[test]
fn the_wheel_passes_through_rows_and_folds_to_the_pane_beneath() {
    let mut m = HitMap::new();
    m.begin(FRAME);
    m.push(Rect::new(0, 0, 40, 10), Target::Scroll(PaneId(4)), "j");
    m.push(
        Rect::new(2, 2, 20, 1),
        Target::Row {
            list: RUNS,
            index: 0,
            id: 1,
        },
        "Enter",
    );
    let mut p = pointer();
    assert_eq!(
        p.handle(&m, wheel_down(3, 2), t0()),
        vec![Action::Wheel {
            pane: Some(PaneId(4)),
            lines: 3
        }]
    );
}

// ---- what is ignored --------------------------------------------------------------------------------------------------------------

#[test]
fn other_buttons_motion_and_sideways_wheels_do_nothing() {
    let m = screen();
    let mut p = pointer();
    for kind in [
        MouseEventKind::Down(MouseButton::Right),
        MouseEventKind::Down(MouseButton::Middle),
        MouseEventKind::Up(MouseButton::Right),
        MouseEventKind::Drag(MouseButton::Middle),
        MouseEventKind::Moved,
        MouseEventKind::ScrollLeft,
        MouseEventKind::ScrollRight,
    ] {
        assert_eq!(p.handle(&m, ev(kind, 5, 1), t0()), vec![], "{kind:?}");
    }
    assert!(!p.has_selection());
}

#[test]
fn modifiers_do_not_change_an_action() {
    let m = screen();
    let mut p = pointer();
    for mods in [
        KeyModifiers::SHIFT,
        KeyModifiers::CONTROL,
        KeyModifiers::ALT,
    ] {
        let mut e = down(5, 1);
        e.modifiers = mods;
        assert_eq!(p.handle(&m, e, t0()), vec![Action::Focus(PROMPT)]);
    }
}

// ---- resize -----------------------------------------------------------------------------------------------------------------------

#[test]
fn a_click_between_a_resize_and_the_next_draw_is_ignored() {
    let mut m = screen();
    let mut p = pointer();
    m.resized();
    assert_eq!(p.handle(&m, down(5, 1), t0()), vec![]);
    assert_eq!(
        p.handle(&m, wheel_down(5, 1), t0()),
        vec![Action::Wheel {
            pane: None,
            lines: 3
        }]
    );
    m.begin(FRAME);
    m.push(Rect::new(0, 0, 40, 3), Target::Box(PROMPT), "Tab");
    assert_eq!(p.handle(&m, down(5, 1), t0()), vec![Action::Focus(PROMPT)]);
}

#[test]
fn a_resize_in_the_middle_of_a_drag_ends_the_gesture() {
    let mut m = screen();
    let mut p = pointer();
    p.handle(&m, down(10, 14), t0());
    p.handle(&m, drag(20, 14), t0());
    m.resized();
    assert_eq!(
        p.handle(&m, drag(30, 14), t0()),
        vec![Action::ClearSelection]
    );
    assert_eq!(p.handle(&m, up(30, 14), t0()), vec![]);
    assert!(!p.has_selection());
}

#[test]
fn reset_forgets_every_gesture_and_the_selection() {
    let m = screen();
    let mut p = pointer();
    with_selection(&m, &mut p);
    p.handle(&m, down(60, 2), t0());
    p.reset();
    assert!(!p.has_selection());
    assert_eq!(p.handle(&m, down(5, 1), t0()), vec![Action::Focus(PROMPT)]);
}

// ---- properties -------------------------------------------------------------------------------------------------------------------

fn arb_rect() -> impl Strategy<Value = Rect> {
    (0u16..120, 0u16..40, 0u16..60, 0u16..30).prop_map(|(x, y, w, h)| Rect::new(x, y, w, h))
}

fn arb_target() -> impl Strategy<Value = Target> {
    prop_oneof![
        (0u16..4).prop_map(|i| Target::Box(BoxId(i))),
        (0u16..3, 0usize..5).prop_map(|(l, i)| Target::Row {
            list: ListId(l),
            index: i,
            id: i as u64
        }),
        "[a-z]{1,3}".prop_map(|b| Target::Fold { block: b }),
        Just(Target::Key(KeyCode::Enter)),
        (0u16..4).prop_map(Target::Link),
        (0u16..3).prop_map(|i| Target::Text(PaneId(i))),
        (0u16..3).prop_map(|i| Target::Scroll(PaneId(i))),
        Just(Target::Block),
    ]
}

proptest! {
    #[test]
    fn target_at_is_the_last_region_that_contains_the_point(
        regions in prop::collection::vec((arb_rect(), arb_target()), 0..12),
        x in 0u16..130, y in 0u16..50,
    ) {
        let frame = Rect::new(0, 0, 100, 30);
        let mut m = HitMap::new();
        m.begin(frame);
        let mut kept: Vec<(Rect, Target)> = Vec::new();
        for (r, t) in regions {
            m.push(r, t.clone(), "k");
            let c = r.intersection(frame);
            if c.width > 0 && c.height > 0 {
                kept.push((c, t));
            }
        }
        let want = kept.iter().rev().find(|(r, _)| {
            x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
        });
        let got = m.target_at(x, y);
        prop_assert_eq!(got.map(|g| &g.target), want.map(|(_, t)| t));
        if let Some(g) = got {
            prop_assert!(g.rect.x >= frame.x && g.rect.right() <= frame.right());
            prop_assert!(g.rect.y >= frame.y && g.rect.bottom() <= frame.bottom());
        }
    }

    #[test]
    fn any_event_sequence_is_handled_and_every_coordinate_stays_in_its_pane(
        regions in prop::collection::vec((arb_rect(), arb_target()), 0..10),
        events in prop::collection::vec((0u8..9, 0u16..130, 0u16..50), 0..60),
    ) {
        let frame = Rect::new(0, 0, 100, 30);
        let mut m = HitMap::new();
        m.begin(frame);
        for (r, t) in regions { m.push(r, t, "k"); }
        let mut p = Pointer::new(WheelSettings::default());
        let t = Instant::now();
        let mut starts = 0usize;
        let mut ends = 0usize;
        for (i, (k, x, y)) in events.into_iter().enumerate() {
            let kind = match k {
                0 => MouseEventKind::Down(MouseButton::Left),
                1 => MouseEventKind::Up(MouseButton::Left),
                2 => MouseEventKind::Drag(MouseButton::Left),
                3 => MouseEventKind::ScrollDown,
                4 => MouseEventKind::ScrollUp,
                5 => MouseEventKind::Moved,
                6 => MouseEventKind::Down(MouseButton::Right),
                7 => MouseEventKind::Up(MouseButton::Right),
                _ => MouseEventKind::ScrollLeft,
            };
            let acts = p.handle(&m, ev(kind, x, y), t + Duration::from_millis(i as u64 * 30));
            for a in acts {
                match a {
                    Action::SelectionStart { pane, x, y }
                    | Action::SelectionExtend { pane, x, y }
                    | Action::SelectionEnd { pane, x, y } => {
                        if matches!(a, Action::SelectionStart { .. }) { starts += 1; }
                        if matches!(a, Action::SelectionEnd { .. }) { ends += 1; }
                        let r = m
                            .regions()
                            .iter()
                            .rev()
                            .find(|r| r.target == Target::Text(pane) && x >= r.rect.x && x < r.rect.right()
                                && y >= r.rect.y && y < r.rect.bottom());
                        prop_assert!(r.is_some(), "{a:?} is outside every pane of its id");
                    }
                    Action::Wheel { lines, .. } | Action::Autoscroll { lines, .. } => {
                        prop_assert!(lines != 0 && lines.unsigned_abs() <= u32::from(MAX_WHEEL_STEP) * 4);
                    }
                    _ => {}
                }
            }
        }
        prop_assert!(ends <= starts, "a selection ends only after it started");
    }

    #[test]
    fn a_row_is_opened_only_straight_after_the_click_that_selected_it(
        clicks in prop::collection::vec((0usize..3, 0u64..900), 1..30),
    ) {
        let m = screen();
        let mut p = pointer();
        let t = Instant::now();
        let mut at = t;
        let mut last: Option<(usize, Instant)> = None;
        for (row, gap) in clicks {
            at += Duration::from_millis(gap);
            let acts = p.handle(&m, down(60, 2 + row as u16), at);
            match acts.as_slice() {
                [Action::Open { index, .. }] => {
                    let (lr, lt) = last.expect("an open needs an earlier click");
                    prop_assert_eq!(*index, lr);
                    prop_assert_eq!(*index, row);
                    prop_assert!(at.duration_since(lt) <= DOUBLE_CLICK);
                    last = None;
                }
                [Action::Select { index, .. }] => {
                    prop_assert_eq!(*index, row);
                    if let Some((lr, lt)) = last {
                        prop_assert!(
                            lr != row || at.duration_since(lt) > DOUBLE_CLICK,
                            "a second click inside the window must open"
                        );
                    }
                    last = Some((row, at));
                }
                other => prop_assert!(false, "not one select or open: {other:?}"),
            }
        }
    }
}
