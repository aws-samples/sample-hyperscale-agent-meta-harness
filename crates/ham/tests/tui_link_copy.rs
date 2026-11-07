//! `y` on the delivery card: with a pull request on the card, `y` copies its link (the key is
//! named in the footer); a selected block still wins, and a run without a pull request keeps `y` for the last answer. The clipboard is
//! written only on the person's own key: events arriving, polls and ticks never copy anything. The link of the zip is a bearer link and is
//! never copied.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::*;
use crossterm::event::KeyCode;
use ham::tui::keys::Mode;
use ham::tui::{App, Effect, Msg};
use harness_protocol::envelope::{Envelope, EventType};
use serde_json::json;

const PR: &str = "https://example.test/pr/7";

fn delivered(pr: bool) -> Vec<Envelope> {
    let mut v = vec![env(
        10,
        EventType::TurnProgress,
        Some("direct"),
        json!({"turn": 1, "kind": "message", "text": "All done: the tests pass."}),
    )];
    if pr {
        v.push(env(11, EventType::DeliveryPr, None, json!({"url": PR})));
    }
    v.push(env(
        12,
        EventType::GoalStatus,
        None,
        json!({"status": "complete", "phase": "delivering"}),
    ));
    v.push(env(
        13,
        EventType::RunCompleted,
        None,
        json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
    ));
    v
}

fn browsing(extra: Vec<Envelope>) -> (App, Clock) {
    let (mut app, mut clock) = ready_with(extra);
    press(&mut app, &mut clock, KeyCode::Esc, 60);
    assert_eq!(app.mode(), Mode::Browse);
    (app, clock)
}

fn copies(effects: &[Effect]) -> Vec<String> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Copy(p) => Some(p.text().to_string()),
            _ => None,
        })
        .collect()
}

fn footer(app: &App) -> String {
    screen(app).lines().last().unwrap_or_default().to_string()
}

#[test]
fn y_copies_the_pull_request_link_when_the_card_has_one() {
    let (mut app, mut clock) = browsing(delivered(true));
    let effects = press(&mut app, &mut clock, KeyCode::Char('y'), 400);
    assert_eq!(copies(&effects), vec![PR.to_string()], "{effects:?}");
}

#[test]
fn a_selected_block_still_wins_over_the_link() {
    let (mut app, mut clock) = browsing(delivered(true));
    // Tab selects the first block, the prompt.
    press(&mut app, &mut clock, KeyCode::Tab, 400);
    let effects = press(&mut app, &mut clock, KeyCode::Char('y'), 400);
    assert_eq!(copies(&effects), vec![PROMPT.to_string()], "{effects:?}");
}

#[test]
fn without_a_pull_request_y_keeps_copying_the_last_answer() {
    let (mut app, mut clock) = browsing(delivered(false));
    let effects = press(&mut app, &mut clock, KeyCode::Char('y'), 400);
    assert_eq!(
        copies(&effects),
        vec!["All done: the tests pass.".to_string()],
        "{effects:?}"
    );
}

#[test]
fn the_footer_names_y_when_the_card_has_a_link_to_copy_and_not_otherwise() {
    let (app, _) = browsing(delivered(true));
    let f = footer(&app);
    assert!(
        f.contains("o open PR") && f.contains("y copy PR link"),
        "{f}"
    );
    let (app, _) = browsing(delivered(false));
    assert!(!footer(&app).contains("y copy"), "{}", footer(&app));
}

#[test]
fn nothing_is_copied_unless_the_person_presses_y() {
    let (mut app, mut clock) = ready();
    let mut all = Vec::new();
    all.extend(app.update(pages(delivered(true))));
    all.extend(app.update(detail()));
    all.extend(app.tick());
    all.extend(app.tick());
    all.extend(app.update(Msg::Copied(Ok("Copied 25 bytes with OSC 52.".into()))));
    clock.after(5_000);
    assert!(
        !all.iter().any(|e| matches!(e, Effect::Copy(_))),
        "a copy without a key: {all:?}"
    );
}

#[test]
fn y_in_the_message_box_is_a_letter_even_on_a_delivered_run() {
    let (mut app, mut clock) = ready_with(delivered(true));
    let effects = press(&mut app, &mut clock, KeyCode::Char('y'), 60);
    assert!(copies(&effects).is_empty(), "{effects:?}");
    assert_eq!(app.chat_text().as_deref(), Some("y"));
}

#[test]
fn the_link_of_the_zip_is_never_what_y_copies() {
    // A delivered zip without a pull request: the only link is the bearer link of the zip, which is not copied; y copies the answer.
    let (mut app, mut clock) = browsing({
        let mut v = delivered(false);
        v.push(env(
            14,
            EventType::DeliveryReady,
            None,
            json!({"artifactId": "01K6A00000000000000000ZP01", "sha256": "ab".repeat(32), "sizeBytes": 2048}),
        ));
        v
    });
    let effects = press(&mut app, &mut clock, KeyCode::Char('y'), 400);
    let got = copies(&effects);
    assert_eq!(got.len(), 1, "{effects:?}");
    assert!(
        !got[0].contains("amazonaws") && !got[0].contains("X-Amz"),
        "{got:?}"
    );
}
