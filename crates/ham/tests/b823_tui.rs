//! In the terminal app: the run screen says when the last copy of the run's work was saved, and a run that ended blocked says where
//! its work is: in the retry panel, in the hint of its message box, and on the keys that need a zip.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::*;
use crossterm::event::KeyCode;
use ham::tui::App;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use serde_json::{Value, json};

const HEAD: &str = "ad2645e679230000000000000000000000000000";
const REASON: &str = "bundle validation failed: workflow-file .github/workflows/ci.yml: refused";

fn event(
    seq: u64,
    event_type: EventType,
    authority: Authority,
    agent: Option<&str>,
    value: Value,
) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{seq:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: agent.map(str::to_string),
        seq,
        ts: T0.into(),
        event_type,
        authority,
        payload: Payload::Inline { value },
    }
}

/// The event agentd records after it stored a copy: no `kind`, a `savedCopy` member.
fn saved(seq: u64, at: &str, count: u32, last: bool) -> Envelope {
    let mut copy = json!({"at": at, "count": count, "headSha": HEAD, "session": 1});
    if last {
        copy["final"] = json!(true);
    }
    event(
        seq,
        EventType::TurnProgress,
        Authority::Agent,
        Some("direct"),
        json!({"turn": 1, "savedCopy": copy}),
    )
}

fn blocked_tail() -> Vec<Envelope> {
    vec![
        event(
            20,
            EventType::GoalBlocked,
            Authority::Harness,
            None,
            json!({"reason": REASON}),
        ),
        event(
            21,
            EventType::GoalStatus,
            Authority::Harness,
            None,
            json!({"status": "blocked", "phase": "delivering"}),
        ),
        event(
            22,
            EventType::RunCompleted,
            Authority::Harness,
            None,
            json!({"status": "blocked", "outcome": "reported_done", "label": "Cline reported done"}),
        ),
    ]
}

fn blocked(with_copy: bool) -> App {
    let mut events = base_events();
    if with_copy {
        events.push(saved(15, "2026-10-16T09:47:40Z", 9, true));
    }
    events.extend(blocked_tail());
    ready(events)
}

fn row_with<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no row has {needle:?}:\n{text}"))
}

#[test]
fn b823_the_run_screen_says_when_the_last_copy_was_saved() {
    // A run that works: the time of the copy and how long ago. The clock of the tests writes UTC.
    let mut events = base_events();
    events.push(saved(15, "2026-10-16T08:40:22Z", 8, false));
    let mut app = ready(events);
    // Two minutes and ten seconds after the copy.
    app.set_now(harness_protocol::common::parse_utc_unix("2026-10-16T08:40:22Z").unwrap() + 130);
    let text = screen(&app, 120, 40);
    let line = row_with(&text, "Saved");
    assert!(line.contains("Saved  08:40 UTC, 2 min ago"), "{text}");

    // A run that never saved a copy (or a service older than the event) has no such line.
    let app = ready(base_events());
    assert!(
        !screen(&app, 120, 40).contains("Saved  "),
        "no copy, no line"
    );
}

#[test]
fn b823_a_blocked_run_shows_that_its_work_was_saved_when_the_engine_stopped() {
    let mut app = blocked(true);
    app.set_now(harness_protocol::common::parse_utc_unix("2026-10-16T12:00:00Z").unwrap());
    let text = screen(&app, 120, 40);
    assert!(
        row_with(&text, "Saved").contains("Saved  09:47 UTC, when Cline stopped"),
        "{text}"
    );
    assert!(text.contains("blocked"), "{text}");
}

#[test]
fn b823_the_retry_panel_says_where_the_work_of_the_blocked_run_is() {
    let mut app = blocked(true);
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    press(&mut app, &mut clock, KeyCode::Char('r'), 50);
    let text = screen(&app, 140, 44);
    assert!(text.contains("Retry blocked run?"), "{text}");
    assert!(text.contains("This run's work stays saved:"), "{text}");
    assert!(
        text.contains(&format!("ham pull {RUN} --snapshot downloads it")),
        "{text}"
    );

    // Without a copy in the run's events the panel says what the command does and promises nothing.
    let mut app = blocked(false);
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    press(&mut app, &mut clock, KeyCode::Char('r'), 50);
    let text = screen(&app, 140, 44);
    assert!(!text.contains("stays saved"), "{text}");
    assert!(
        text.contains("downloads this run's last saved copy, if it saved one"),
        "{text}"
    );
}

#[test]
fn b823_the_hint_of_a_blocked_run_names_the_command_for_its_work() {
    let mut app = blocked(true);
    let mut clock = Clock::new();
    type_text(&mut app, &mut clock, "please try again");
    let text = screen(&app, 140, 44);
    assert!(
        text.contains("This run is blocked: it ended without a result"),
        "{text}"
    );
    assert!(text.contains("This run's work stays saved:"), "{text}");
}

#[test]
fn b823_the_download_key_of_a_blocked_run_says_where_its_work_is() {
    let mut app = blocked(true);
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    press(&mut app, &mut clock, KeyCode::Char('d'), 50);
    let text = screen(&app, 140, 44);
    assert!(
        text.contains("this run ended blocked and delivered no zip; /pull --snapshot downloads the latest saved copy of its work"),
        "{text}"
    );
    assert!(
        !text.contains("nothing is delivered yet"),
        "a run that ended is not told to wait: {text}"
    );
}
