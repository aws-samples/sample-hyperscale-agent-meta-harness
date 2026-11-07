//! A run that waits for the person opens with the message box focused (plan 3.3) and must still make the keys that answer it
//! discoverable: the hotkeys p, s, m and f work after Esc, so the footer of the focused box names them next to "Esc browse", offering
//! only the keys the question offers. A run that does not wait keeps the usual typing footer.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::*;
use harness_protocol::envelope::{Envelope, EventType};
use serde_json::{Value, json};

fn footer(app: &ham::tui::App) -> String {
    screen(app).lines().last().unwrap_or_default().to_string()
}

/// The question of `chat_support`, offering only `options`, with two models to pick from when `models` is set.
fn question_offering(options: &[&str], can_continue: bool, models: bool) -> Envelope {
    let mut e = question();
    if let harness_protocol::envelope::Payload::Inline { value } = &mut e.payload {
        let v: &mut Value = value;
        v["options"] = json!(options);
        v["canContinue"] = json!(can_continue);
        if models {
            v["models"] = json!([
                {"id": "global.anthropic.claude-opus-5-5", "name": "Claude Opus 5.5", "contextTokens": 1_000_000},
                {"id": "global.anthropic.claude-sonnet-5-5", "name": "Claude Sonnet 5.5", "contextTokens": 1_000_000}
            ]);
        }
    }
    // The event type and ids stay those of the plain question.
    assert_eq!(e.event_type, EventType::TurnProgress);
    e
}

#[test]
fn a_run_that_waits_for_the_person_names_the_answering_keys_in_the_focused_footer() {
    let all = [
        "continue_with_prompt",
        "new_session_with_prompt",
        "try_model",
        "finish",
    ];
    let (app, _) = ready_with(vec![question_offering(&all, true, true)]);
    let f = footer(&app);
    assert!(f.contains("Esc browse"), "{f}");
    for key in ["p message", "s fresh session", "m model", "f f finish"] {
        assert!(f.contains(key), "{key:?} missing from the footer: {f}");
    }
    // The card names them too.
    assert!(screen(&app).contains("Cline needs you"), "{}", screen(&app));
}

#[test]
fn only_the_keys_the_question_offers_are_named() {
    let (app, _) = ready_with(vec![question_offering(&["finish"], false, false)]);
    let f = footer(&app);
    assert!(f.contains("f f finish") && f.contains("Esc browse"), "{f}");
    assert!(!f.contains("p message") && !f.contains("m model"), "{f}");
    // A question with no model to pick does not offer `m` either.
    let all = [
        "continue_with_prompt",
        "new_session_with_prompt",
        "try_model",
        "finish",
    ];
    let (app, _) = ready_with(vec![question_offering(&all, true, false)]);
    let f = footer(&app);
    assert!(f.contains("p message") && !f.contains("m model"), "{f}");
}

#[test]
fn a_run_that_does_not_wait_keeps_the_usual_typing_footer() {
    let (app, _) = ready();
    let f = footer(&app);
    assert!(f.contains("Enter send") && f.contains("Esc browse"), "{f}");
    assert!(!f.contains("p message") && !f.contains("f f finish"), "{f}");
}

#[test]
fn in_browse_mode_the_footer_is_the_usual_hotkey_line() {
    let (mut app, mut clock) = ready_with(vec![question()]);
    press(&mut app, &mut clock, crossterm::event::KeyCode::Esc, 60);
    let f = footer(&app);
    assert!(f.contains("p message") && f.contains("a artifacts"), "{f}");
    assert!(!f.contains("Esc browse"), "{f}");
}
