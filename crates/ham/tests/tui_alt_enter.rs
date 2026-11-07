//! Alt-Enter in the message box: where the terminal reports the key, Alt-Enter sends the whole box to Cline as
//! chat, exactly as typed. No command is read from it, no chip is offered, a leading space, a `//` and a second line stay, and the people who
//! cannot send the key use `/send <text>` as before. Inside a burst, or with a preview up, it never sends.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::*;
use crossterm::event::{KeyCode, KeyModifiers};
use ham::tui::{App, Effect};
use std::time::Instant;

/// Alt-Enter after `wait_ms`.
fn alt_enter(app: &mut App, clock: &mut Clock, wait_ms: u64) -> Vec<Effect> {
    press_mod(app, clock, KeyCode::Enter, KeyModifiers::ALT, wait_ms)
}

/// The text of the one answer in `effects`.
fn answered(effects: &[Effect]) -> String {
    match effects {
        [Effect::Answer { answer, .. }] => answer.text.clone().unwrap_or_default(),
        other => panic!("expected one answer to the open question, got {other:?}"),
    }
}

#[test]
fn alt_enter_sends_the_whole_box_as_chat_exactly_as_typed() {
    // Each line is one that plain Enter would read as a command, refuse, trim or turn into a chip.
    for typed in [
        "/cancel",
        "//cancel  now ",
        " /cancel",
        "/nonsense word  ",
        "ham push",
        "push my code",
        "  two leading blanks and one after ",
    ] {
        let (mut app, mut clock) = ready_with(vec![question()]);
        type_text(&mut app, &mut clock, typed);
        let effects = alt_enter(&mut app, &mut clock, 10);
        assert_eq!(answered(&effects), typed, "{typed:?}");
        assert_eq!(app.chat_text().as_deref(), Some(""), "{typed:?}");
        let text = screen(&app);
        assert!(
            !text.contains("Confirm") && !text.contains("Command?"),
            "{typed:?} showed a preview or a chip:\n{text}"
        );
    }
}

#[test]
fn alt_enter_sends_two_lines_that_plain_enter_refuses() {
    let (mut app, mut clock) = ready_with(vec![question()]);
    type_text(&mut app, &mut clock, "/cancel");
    press_mod(
        &mut app,
        &mut clock,
        KeyCode::Char('j'),
        KeyModifiers::CONTROL,
        50,
    );
    type_text(&mut app, &mut clock, "and the rest");
    assert_eq!(app.chat_text().as_deref(), Some("/cancel\nand the rest"));
    let effects = alt_enter(&mut app, &mut clock, 10);
    assert_eq!(answered(&effects), "/cancel\nand the rest");
}

#[test]
fn alt_enter_sends_the_text_of_a_pasted_block_not_its_label() {
    let (mut app, mut clock) = ready_with(vec![question()]);
    let pasted: String = (1..=40).map(|i| format!("pasted line {i}\n")).collect();
    let at = clock.after(100);
    app.handle_paste_at(pasted.clone(), at);
    assert!(
        screen(&app).contains("[Pasted"),
        "the paste is shown as a block:\n{}",
        screen(&app)
    );
    // The pasted text is tainted (a command in it would preview); as chat it goes unchanged.
    let effects = alt_enter(&mut app, &mut clock, 400);
    assert_eq!(answered(&effects), pasted);
}

#[test]
fn alt_enter_on_an_empty_box_sends_nothing() {
    let (mut app, mut clock) = ready_with(vec![question()]);
    assert!(alt_enter(&mut app, &mut clock, 10).is_empty());
    type_text(&mut app, &mut clock, "   ");
    assert!(alt_enter(&mut app, &mut clock, 10).is_empty());
}

#[test]
fn alt_enter_inside_a_burst_is_a_line_break_and_sends_nothing() {
    let (mut app, _clock) = ready_with(vec![question()]);
    let t = Instant::now() + std::time::Duration::from_secs(60);
    app.handle_key_at(
        crossterm::event::KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        t,
    );
    let effects = app.handle_key_at(
        crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
        t + std::time::Duration::from_millis(5),
    );
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.chat_text().as_deref(), Some("a\n"));
}

#[test]
fn alt_enter_while_a_preview_is_up_only_cancels_the_preview() {
    let (mut app, mut clock) = ready_with(vec![]);
    send(&mut app, &mut clock, "/cancel");
    assert!(screen(&app).contains("Cancel run"), "{}", screen(&app));
    let effects = alt_enter(&mut app, &mut clock, 400);
    assert!(effects.is_empty(), "{effects:?}");
    assert!(!screen(&app).contains("Cancel run"), "{}", screen(&app));
    assert_eq!(app.chat_text().as_deref(), Some("/cancel"));
}

#[test]
fn plain_enter_still_reads_the_same_text_as_a_command() {
    let (mut app, mut clock) = ready_with(vec![]);
    type_text(&mut app, &mut clock, "/cancel");
    let effects = press(&mut app, &mut clock, KeyCode::Enter, 10);
    assert!(effects.is_empty(), "{effects:?}");
    assert!(
        screen(&app).contains("Cancel run"),
        "plain Enter previews the command:\n{}",
        screen(&app)
    );
}
