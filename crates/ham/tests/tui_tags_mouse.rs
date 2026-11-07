//! The mouse in the tag explorer. The overlay is the list `ListId(2)` beside the Runs list and the model list: a press on
//! a tag row moves the cursor, a second press on the same tag inside 500 ms applies it, the wheel moves one row, a press anywhere else under the
//! overlay does nothing (it is shielded from the Runs list), and the key chips of its help line press their keys, `Space` included. With the
//! mouse off the keys do everything.
#![allow(clippy::unwrap_used)]

mod tagsfx;

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ham::tui::{App, Effect, MouseState, Screen};
use std::time::{Duration, Instant};
use tagsfx::*;

fn text_of(app: &App) -> String {
    screen_text(app, 120, 40)
}

/// The column and row where `needle` starts.
fn pos_of(text: &str, needle: &str) -> (u16, u16) {
    for (y, line) in text.lines().enumerate() {
        if let Some(at) = line.find(needle) {
            return (line[..at].chars().count() as u16, y as u16);
        }
    }
    panic!("{needle:?} is not on the screen:\n{text}");
}

fn mouse(app: &mut App, kind: MouseEventKind, x: u16, y: u16, at: Instant) -> Vec<Effect> {
    app.handle_mouse_at(
        MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        },
        at,
    )
}

fn click(app: &mut App, x: u16, y: u16, at: Instant) -> Vec<Effect> {
    let mut effects = mouse(app, MouseEventKind::Down(MouseButton::Left), x, y, at);
    effects.extend(mouse(app, MouseEventKind::Up(MouseButton::Left), x, y, at));
    effects
}

fn opened() -> (App, [String; 4]) {
    let (mut app, ids) = small();
    ch(&mut app, 't');
    (app, ids)
}

#[test]
fn a_press_moves_the_cursor_and_a_second_press_on_the_same_tag_applies() {
    let (mut app, ids) = opened();
    let text = text_of(&app);
    assert_eq!(cursor_tag(&text).as_deref(), Some("tech:rust"));
    let (x, y) = pos_of(&text, "topic:alpha");
    let t0 = Instant::now();
    assert!(click(&mut app, x + 2, y, t0).is_empty());
    let text = text_of(&app);
    assert_eq!(cursor_tag(&text).as_deref(), Some("topic:alpha"), "{text}");
    assert!(
        text.starts_with("ham \u{b7} Tags"),
        "one press only moves: {text}"
    );
    // The second press on the same tag, inside 500 ms, applies it: the Runs list holds the runs of `topic:alpha` and the overlay is gone.
    assert!(click(&mut app, x + 2, y, t0 + Duration::from_millis(200)).is_empty());
    assert_eq!(app.screen(), Screen::Runs);
    let text = text_of(&app);
    assert!(text.starts_with("ham \u{b7} Runs"), "{text}");
    assert_eq!(
        listed(&text, &ids),
        [ids[0].clone(), ids[1].clone()],
        "{text}"
    );
    assert!(
        bottom_two(&text)
            .0
            .starts_with("2 of 4 runs match +topic:alpha"),
        "{text}"
    );
}

#[test]
fn a_second_press_after_the_double_click_time_only_moves_the_cursor_again() {
    let (mut app, _) = opened();
    let text = text_of(&app);
    let (x, y) = pos_of(&text, "topic:beta");
    let t0 = Instant::now();
    click(&mut app, x + 2, y, t0);
    click(&mut app, x + 2, y, t0 + Duration::from_millis(900));
    let text = text_of(&app);
    assert!(text.starts_with("ham \u{b7} Tags"), "{text}");
    assert_eq!(cursor_tag(&text).as_deref(), Some("topic:beta"));
}

#[test]
fn the_wheel_moves_one_row() {
    let (mut app, _) = opened();
    let text = text_of(&app);
    let (x, y) = pos_of(&text, "tech:rust");
    let t0 = Instant::now();
    mouse(&mut app, MouseEventKind::ScrollDown, x + 4, y, t0);
    assert_eq!(cursor_tag(&text_of(&app)).as_deref(), Some("topic:alpha"));
    mouse(
        &mut app,
        MouseEventKind::ScrollDown,
        x + 4,
        y + 1,
        t0 + Duration::from_secs(2),
    );
    assert_eq!(cursor_tag(&text_of(&app)).as_deref(), Some("topic:beta"));
    mouse(
        &mut app,
        MouseEventKind::ScrollUp,
        x + 40,
        y,
        t0 + Duration::from_secs(4),
    );
    assert_eq!(cursor_tag(&text_of(&app)).as_deref(), Some("topic:alpha"));
}

#[test]
fn a_click_under_the_overlay_does_nothing() {
    let (mut app, ids) = opened();
    let text = text_of(&app);
    assert!(listed(&text, &ids).is_empty());
    // Three tag rows are drawn; the fourth row of the table is empty here, and the Runs list under it has a run on that row. Two quick
    // presses there would open that run on the Runs list; the overlay shields it.
    let (_, y) = pos_of(&text, "topic:beta");
    let t0 = Instant::now();
    let mut effects = click(&mut app, 30, y + 1, t0);
    effects.extend(click(&mut app, 30, y + 1, t0 + Duration::from_millis(100)));
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.screen(), Screen::Runs);
    let after = text_of(&app);
    assert!(after.starts_with("ham \u{b7} Tags"), "{after}");
    assert_eq!(cursor_tag(&after).as_deref(), Some("tech:rust"), "{after}");
    // The frame's border and the title row do nothing either.
    assert!(click(&mut app, 0, 0, t0 + Duration::from_secs(3)).is_empty());
    assert!(click(&mut app, 119, 20, t0 + Duration::from_secs(4)).is_empty());
    assert!(text_of(&app).starts_with("ham \u{b7} Tags"));
}

#[test]
fn the_space_chip_presses_space_and_the_other_chips_press_theirs() {
    let (mut app, _) = opened();
    let text = text_of(&app);
    let (x, y) = pos_of(&text, "Space pick");
    let t0 = Instant::now();
    assert!(click(&mut app, x + 2, y, t0).is_empty());
    let text = text_of(&app);
    assert_eq!(picked(&text), ["tech:rust"], "{text}");
    // The `/` chip opens the find line, the `Esc` chip closes the overlay.
    let (fx, fy) = pos_of(&text, "/ find");
    click(&mut app, fx + 1, fy, t0 + Duration::from_secs(3));
    let text = text_of(&app);
    assert!(bottom_two(&text).1.starts_with('/'), "{text}");
    key(&mut app, KeyCode::Esc);
    let text = text_of(&app);
    let (ex, ey) = pos_of(&text, "Esc back");
    click(&mut app, ex + 1, ey, t0 + Duration::from_secs(6));
    assert!(text_of(&app).starts_with("ham \u{b7} Runs"));
}

#[test]
fn every_region_of_the_overlay_names_the_key_that_does_the_same() {
    let (app, _) = opened();
    let _ = text_of(&app);
    assert!(
        app.hit_map().unkeyed().is_empty(),
        "{:?}",
        app.hit_map().unkeyed()
    );
}

#[test]
fn with_the_mouse_off_the_keys_work_alone() {
    let (mut app, ids) = small();
    app.set_mouse(
        MouseState {
            on: false,
            forced_off: true,
        },
        None,
    );
    ch(&mut app, 't');
    ch(&mut app, 'j');
    ch(&mut app, ' ');
    key(&mut app, KeyCode::Enter);
    let text = screen(&app);
    assert_eq!(
        listed(&text, &ids),
        [ids[0].clone(), ids[1].clone()],
        "{text}"
    );
}
