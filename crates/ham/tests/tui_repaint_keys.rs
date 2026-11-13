//! What asks for a full repaint of the terminal (the key and focus half of `tests/tui_repaint.rs`): Ctrl-L on every screen and in both
//! key modes, and the terminal reporting that the window has focus again. Nothing else does: not a focus loss, not a letter, not the
//! other Ctrl keys. `App::take_repaint` hands the request to the event loop once.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::{Clock, press, ready, screen};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::keys::Mode;
use ham::tui::{App, DirectSettings, Screen};

fn new_app() -> App {
    App::new_direct("n".into(), None, DirectSettings::default())
}

fn ctrl(app: &mut App, c: char) {
    app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
}

#[test]
fn a_new_app_asks_for_no_repaint_and_the_request_is_taken_once() {
    let mut app = new_app();
    assert!(
        !app.take_repaint(),
        "the first draw paints every cell anyway"
    );
    ctrl(&mut app, 'l');
    assert!(app.take_repaint(), "Ctrl-L asks");
    assert!(!app.take_repaint(), "the loop took the request");
}

#[test]
fn ctrl_l_asks_for_a_repaint_on_the_runs_screen_and_in_its_filter_line() {
    let mut app = new_app();
    assert_eq!(app.screen(), Screen::Runs);
    ctrl(&mut app, 'l');
    assert!(app.take_repaint(), "Runs, browsing the list");

    // `/` starts the filter line, where letters are text.
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    ctrl(&mut app, 'l');
    assert!(app.take_repaint(), "Runs, typing in the filter line");
}

#[test]
fn ctrl_l_asks_for_a_repaint_in_the_composer() {
    let mut app = new_app();
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(app.screen(), Screen::Composer);
    ctrl(&mut app, 'l');
    assert!(app.take_repaint(), "the composer");
}

#[test]
fn ctrl_l_asks_for_a_repaint_in_the_chat_in_both_modes_and_keeps_its_other_job() {
    let mut app = ready(vec![]);
    let mut clock = Clock::new();
    assert_eq!(
        (app.screen(), app.mode()),
        (Screen::Dashboard, Mode::Typing)
    );
    assert!(!app.take_repaint());
    let at = clock.after(100);
    app.handle_key_at(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL), at);
    assert!(app.take_repaint(), "the message box, typing mode");

    press(&mut app, &mut clock, KeyCode::Esc, 100);
    assert_eq!(app.mode(), Mode::Browse);
    let at = clock.after(100);
    app.handle_key_at(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL), at);
    assert!(app.take_repaint(), "the run screen, browse mode");

    // Browse mode keeps its letters: `l` alone is no repaint and no command.
    press(&mut app, &mut clock, KeyCode::Char('l'), 100);
    assert!(!app.take_repaint(), "a plain l is not Ctrl-L");
}

#[test]
fn other_keys_do_not_ask_for_a_repaint() {
    let mut app = ready(vec![]);
    let mut clock = Clock::new();
    for c in ['k', 'j', 'u', 'z', 'a', 'w'] {
        let at = clock.after(100);
        app.handle_key_at(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL), at);
        assert!(!app.take_repaint(), "Ctrl-{c}");
    }
    for code in [
        KeyCode::Enter,
        KeyCode::Tab,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Char('l'),
        KeyCode::Char('L'),
    ] {
        press(&mut app, &mut Clock::early(), code, 100);
        assert!(!app.take_repaint(), "{code:?}");
    }
}

#[test]
fn the_terminal_reporting_focus_again_asks_for_a_repaint_and_losing_it_does_not() {
    let mut app = new_app();
    app.focus_lost();
    assert!(
        !app.take_repaint(),
        "a window that lost focus has nothing to repaint"
    );
    app.focus_gained();
    assert!(
        app.take_repaint(),
        "a window that has focus again may have lost cells"
    );
    assert!(!app.take_repaint());
    // A second report of the same thing asks again: a window can be covered and uncovered any number of times.
    app.focus_gained();
    assert!(app.take_repaint());
}

#[test]
fn outside_a_prepared_follow_up_ctrl_l_says_it_repainted_and_what_else_it_is_for() {
    // The message box of a run that is not a prepared follow-up: the key repaints, changes no text, and the notice says both.
    let mut app = ready(vec![]);
    let mut clock = Clock::new();
    let at = clock.after(100);
    app.handle_key_at(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL), at);
    assert!(app.take_repaint());
    assert_eq!(app.chat_text().as_deref(), Some(""), "no text was added");
    let shown = screen(&app, 160, 40)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        shown.contains(
            "repainted the screen · Ctrl-L adds Cline's last message to a follow-up draft"
        ),
        "{shown}"
    );
}
