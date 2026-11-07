//! `o` on the delivery card opens the link or falls back to copying it. When no browser
//! opens the pull request link (no opener on the machine, a headless host), the link is copied on the same key press and the notice says
//! so; the person pastes it where a browser is. The link of the zip is a bearer link: it is never copied, so a failed open names `d`
//! instead. A failed open that no `o` key asked for copies nothing, and so does every event, poll and tick.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::*;
use crossterm::event::KeyCode;
use ham::tui::keys::Mode;
use ham::tui::{App, Effect, Msg};
use harness_protocol::envelope::{Envelope, EventType};
use serde_json::json;

const PR: &str = "https://example.test/pr/7";
const ARTIFACT: &str = "01K6A00000000000000000ZP01";

/// The delivered run: the answer, optionally the pull request, and the zip.
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
        EventType::DeliveryReady,
        None,
        json!({"artifactId": ARTIFACT, "sha256": "ab".repeat(32), "sizeBytes": 2048}),
    ));
    v.push(env(
        14,
        EventType::RunCompleted,
        None,
        json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
    ));
    v
}

fn browsing(pr: bool) -> (App, Clock) {
    let (mut app, mut clock) = ready_with(delivered(pr));
    app.set_open_command(Some("/stand/in".into()));
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

fn squashed(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

const NO_OPENER: &str = "could not run /stand/in: No such file or directory (set HAMH_OPEN_COMMAND to a program that opens a URL)";

#[test]
fn o_falls_back_to_copying_the_pull_request_link_when_no_browser_opens() {
    let (mut app, mut clock) = browsing(true);
    let first = press(&mut app, &mut clock, KeyCode::Char('o'), 400);
    assert!(
        matches!(
            first.as_slice(),
            [Effect::OpenUrl {
                what: "PR link",
                ..
            }]
        ),
        "{first:?}"
    );
    let after = app.update(Msg::Opened(Err(NO_OPENER.into())));
    assert_eq!(copies(&after), vec![PR.to_string()], "{after:?}");
    let text = squashed(&screen(&app));
    assert!(
        text.contains("could not open a browser") && text.contains("copying the PR link"),
        "{text}"
    );
    // The copy's own answer keeps the reason in front of it.
    app.update(Msg::Copied(Ok("Copied 25 bytes with OSC 52.".into())));
    let text = squashed(&screen(&app));
    assert!(
        text.contains("could not open a browser") && text.contains("Copied 25 bytes with OSC 52."),
        "{text}"
    );
    // A later copy answer carries no stale reason.
    app.update(Msg::Copied(Ok("Copied 7 bytes with OSC 52.".into())));
    let text = squashed(&screen(&app));
    assert!(
        text.contains("Copied 7 bytes with OSC 52.") && !text.contains("could not open a browser"),
        "{text}"
    );
}

#[test]
fn a_copy_that_fails_after_a_failed_open_says_both() {
    let (mut app, mut clock) = browsing(true);
    press(&mut app, &mut clock, KeyCode::Char('o'), 400);
    app.update(Msg::Opened(Err(NO_OPENER.into())));
    app.update(Msg::Copied(Err("no clipboard program answered".into())));
    let text = squashed(&screen(&app));
    assert!(
        text.contains("could not open a browser, and the link could not be copied: no clipboard program answered"),
        "{text}"
    );
}

#[test]
fn an_open_that_works_copies_nothing() {
    let (mut app, mut clock) = browsing(true);
    press(&mut app, &mut clock, KeyCode::Char('o'), 400);
    let after = app.update(Msg::Opened(Ok(ham::tui::Opened {
        what: "PR link",
        expires_at: None,
    })));
    assert!(copies(&after).is_empty(), "{after:?}");
    assert!(
        squashed(&screen(&app)).contains("opened the PR link in your browser"),
        "{}",
        screen(&app)
    );
}

#[test]
fn the_download_link_is_a_bearer_link_and_is_never_copied_when_the_open_fails() {
    let (mut app, mut clock) = browsing(false);
    let first = press(&mut app, &mut clock, KeyCode::Char('o'), 400);
    assert!(
        matches!(first.as_slice(), [Effect::OpenDownload { .. }]),
        "{first:?}"
    );
    let after = app.update(Msg::Opened(Err(NO_OPENER.into())));
    assert!(copies(&after).is_empty(), "{after:?}");
    let text = squashed(&screen(&app));
    assert!(text.contains("could not open the link"), "{text}");
    // The way on is on the card: `d download` stays on its Link line and in the footer.
    assert!(text.contains("d download"), "{text}");
}

#[test]
fn a_failed_open_that_no_o_key_asked_for_copies_nothing() {
    let (mut app, mut clock) = browsing(true);
    let after = app.update(Msg::Opened(Err(NO_OPENER.into())));
    assert!(copies(&after).is_empty(), "{after:?}");
    // And the fallback is spent by one failure: the next failed open (nobody pressed o) copies nothing either.
    press(&mut app, &mut clock, KeyCode::Char('o'), 400);
    app.update(Msg::Opened(Err(NO_OPENER.into())));
    let again = app.update(Msg::Opened(Err(NO_OPENER.into())));
    assert!(copies(&again).is_empty(), "{again:?}");
}
