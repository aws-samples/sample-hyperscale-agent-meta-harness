//! The note under the Runs table says what the service says about the session of the selected run: `cancel recorded` when
//! the user's cancel is recorded, else `session silent since HH:MM UTC`. The STATUS cell is 14 columns wide and keeps the status word; the
//! note is the row that already says what the selected run waits for. The client's clock is not asked.
#![allow(clippy::unwrap_used)]

mod tagsfx;

use crossterm::event::KeyCode;
use ham::tui::{App, Screen};
use harness_protocol::api::v1::RunSummary;
use serde_json::{Value, json};
use tagsfx::*;

/// An active run in phase green, created at 07:0`n` (a later one stands first in the list), with the service's members in `extra`.
fn active(n: usize, extra: Value) -> RunSummary {
    let mut v = serde_json::to_value(s3_only(&id(n), &format!("2026-10-21T07:0{n}:00Z"))).unwrap();
    v["status"] = json!("active");
    v["phase"] = json!("green");
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    serde_json::from_value(v).unwrap()
}

/// Three active runs: the first with a recorded cancel and a silent session, the second with a silent session, the third with a
/// session that answers.
fn three(tags: &[&str]) -> App {
    app_with(
        vec![
            active(
                3,
                json!({"cancelRequestedAt": "2026-10-21T07:06:40Z", "sessionSilentSince": "2026-10-21T07:05:22Z"}),
            ),
            active(2, json!({"sessionSilentSince": "2026-10-21T07:05:22Z"})),
            active(1, json!({})),
        ],
        vec![
            entry(&id(3), Some(tags)),
            entry(&id(2), Some(&[])),
            entry(&id(1), Some(&[])),
        ],
    )
}

/// The note row at 200 columns: the table shows every column there, so the note holds the count and the words about the selected run.
fn note(app: &App) -> String {
    bottom_two(&screen_text(app, 200, 24)).0
}

#[test]
fn b853_the_runs_note_says_what_the_service_says_about_the_selected_runs_session() {
    let mut app = three(&[]);
    // Both members are sent for the first run: the cancel is what the row says.
    assert_eq!(note(&app), "3 runs \u{b7} cancel recorded");
    key(&mut app, KeyCode::Down);
    assert_eq!(note(&app), "3 runs \u{b7} session silent since 07:05 UTC");
    key(&mut app, KeyCode::Down);
    assert_eq!(
        note(&app),
        "3 runs",
        "TRIGGER PRESENT: a run whose session answers adds nothing"
    );
}

#[test]
fn b853_the_note_of_a_narrowed_list_says_it_too_before_the_tags() {
    let mut app = three(&["tech:go"]);
    // The tag explorer: its first tag is the only one, and Enter narrows the list to the runs that carry it.
    ch(&mut app, 't');
    ch(&mut app, ' ');
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.screen(), Screen::Runs);
    let text = note(&app);
    assert!(text.starts_with("1 of 3 runs match"), "{text}");
    assert!(
        text.ends_with(" \u{b7} cancel recorded \u{b7} tags tech:go"),
        "{text}"
    );
}
