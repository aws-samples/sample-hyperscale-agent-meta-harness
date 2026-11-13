//! `/mouse on|off` and the one-line native-selection hint: `/mouse off` turns the terminal's mouse reports off for this
//! session and `/mouse on` turns them back on, without writing the config file; `HAMH_NO_MOUSE=1` wins over `/mouse on`; the first screen of a session
//! that asks for the mouse says in one line how to select text with the terminal itself, and the line goes with the first key.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::*;
use crossterm::event::KeyCode;
use ham::tui::{App, DirectSettings, Effect, MouseState, Screen};

const HINT: &str = "mouse is on: hold Option to select text with the terminal · /mouse off";

/// The run on screen in typing mode, with the session's mouse state set.
fn with_mouse(on: bool, forced_off: bool) -> (App, Clock) {
    let (mut app, clock) = ready();
    app.set_mouse(MouseState { on, forced_off }, None);
    (app, clock)
}

#[test]
fn mouse_off_turns_the_reports_off_for_this_session_and_says_so() {
    let (mut app, mut clock) = with_mouse(true, false);
    let effects = send(&mut app, &mut clock, "/mouse off");
    assert_eq!(effects, vec![Effect::SetMouse { on: false }]);
    let text = screen(&app);
    assert!(text.contains("off for this session"), "{text}");
    assert!(
        text.contains("config"),
        "it says the config file is untouched:\n{text}"
    );
    assert_eq!(app.chat_text().as_deref(), Some(""));
    // Saying it again changes nothing.
    let effects = send(&mut app, &mut clock, "/mouse off");
    assert!(effects.is_empty(), "{effects:?}");
    assert!(screen(&app).contains("already off"), "{}", screen(&app));
}

#[test]
fn mouse_on_turns_the_reports_back_on() {
    let (mut app, mut clock) = with_mouse(false, false);
    let effects = send(&mut app, &mut clock, "/mouse on");
    assert_eq!(effects, vec![Effect::SetMouse { on: true }]);
    assert!(
        screen(&app).contains("on for this session"),
        "{}",
        screen(&app)
    );
    let effects = send(&mut app, &mut clock, "/mouse on");
    assert!(effects.is_empty(), "{effects:?}");
    assert!(screen(&app).contains("already on"), "{}", screen(&app));
}

#[test]
fn the_word_alone_says_where_the_mouse_stands_and_changes_nothing() {
    for (on, word) in [(true, "on"), (false, "off")] {
        let (mut app, mut clock) = with_mouse(on, false);
        let effects = send(&mut app, &mut clock, "/mouse");
        assert!(effects.is_empty(), "{effects:?}");
        let text = screen(&app);
        assert!(
            text.contains(&format!("The mouse is {word} for this session")),
            "{text}"
        );
        assert!(text.contains("/mouse on|off"), "{text}");
    }
}

#[test]
fn ham_no_mouse_keeps_the_mouse_off_whatever_the_session_asks() {
    let (mut app, mut clock) = with_mouse(false, true);
    let effects = send(&mut app, &mut clock, "/mouse on");
    assert!(effects.is_empty(), "{effects:?}");
    let text = screen(&app);
    assert!(text.contains("HAMH_NO_MOUSE"), "{text}");
    assert!(text.contains("stays off"), "{text}");
    // The word alone names the variable too.
    send(&mut app, &mut clock, "/mouse");
    assert!(screen(&app).contains("HAMH_NO_MOUSE"), "{}", screen(&app));
}

#[test]
fn any_other_word_is_refused_with_the_usage_and_nothing_changes() {
    for line in ["/mouse maybe", "/mouse on off", "/mouse 1"] {
        let (mut app, mut clock) = with_mouse(true, false);
        let effects = send(&mut app, &mut clock, line);
        assert!(effects.is_empty(), "{line}: {effects:?}");
        let text = screen(&app);
        assert!(text.contains("usage: /mouse [on|off]"), "{line}:\n{text}");
        assert_eq!(app.chat_text().as_deref(), Some(line), "the text stays");
    }
}

#[test]
fn mouse_is_a_built_command_in_the_popup_and_in_help() {
    let (mut app, mut clock) = with_mouse(true, false);
    type_text(&mut app, &mut clock, "/mou");
    let text = screen(&app);
    assert!(text.contains("/mouse"), "the popup lists it:\n{text}");
    let (mut app, mut clock) = with_mouse(true, false);
    send(&mut app, &mut clock, "/help mouse");
    let text = screen(&app);
    assert!(text.contains("/mouse [on|off]"), "{text}");
    assert!(!text.contains("not available in this release"), "{text}");
}

#[test]
fn a_mouse_command_never_answers_the_open_question() {
    let (mut app, mut clock) = ready_with(vec![question()]);
    app.set_mouse(
        MouseState {
            on: true,
            forced_off: false,
        },
        None,
    );
    let effects = send(&mut app, &mut clock, "/mouse off");
    assert_eq!(effects, vec![Effect::SetMouse { on: false }]);
}

#[test]
fn the_first_screen_says_how_to_select_text_natively_and_the_first_key_takes_it_away() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_mouse(
        MouseState {
            on: true,
            forced_off: false,
        },
        Some(HINT.into()),
    );
    assert_eq!(app.screen(), Screen::Runs);
    assert!(
        screen(&app).contains(HINT),
        "the Runs screen shows the hint:\n{}",
        screen(&app)
    );
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Char('j'), 100);
    assert!(!screen(&app).contains(HINT), "{}", screen(&app));
}

#[test]
fn a_run_opened_first_shows_the_hint_on_its_notice_row() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_mouse(
        MouseState {
            on: true,
            forced_off: false,
        },
        Some(HINT.into()),
    );
    app.open_run(RUN);
    app.update(pages(start_events()));
    assert!(
        screen(&app).contains(HINT),
        "the run screen shows the hint:\n{}",
        screen(&app)
    );
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Char('k'), 100);
    assert!(!screen(&app).contains(HINT), "{}", screen(&app));
}

#[test]
fn another_notice_takes_the_place_of_the_hint_and_no_hint_is_shown_with_the_mouse_off() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_mouse(
        MouseState {
            on: false,
            forced_off: false,
        },
        None,
    );
    assert!(!screen(&app).contains("hold"), "{}", screen(&app));
    // With the mouse on, a hint that was set is the first thing shown and a later notice replaces it.
    let (mut app, mut clock) = ready();
    app.set_mouse(
        MouseState {
            on: true,
            forced_off: false,
        },
        Some(HINT.into()),
    );
    assert!(screen(&app).contains(HINT));
    type_text(&mut app, &mut clock, "/mouse");
    assert!(
        !screen(&app).contains(HINT),
        "a key was pressed: {}",
        screen(&app)
    );
}
