//! The Outcome line of a card drawn after `ham watch` gave up waiting for `run.completed`. Only the watch
//! sets `FinalCard::label_missing_after`; a card drawn while the label may still come, or one that has its label, is unchanged.

#![allow(clippy::unwrap_used)]

use ham_core::render::{FinalCard, final_card, wait_text};
use ham_core::state::{RunState, RunView};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use serde_json::{Value, json};
use std::time::Duration;

fn harness(n: u8, t: EventType, v: Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: format!("01J9ZZZZZZ00000000000000{n:02}"),
        run_id: "R1".into(),
        task_id: None,
        agent_id: None,
        seq: u64::from(n),
        ts: "2026-10-15T02:00:00Z".into(),
        event_type: t,
        authority: Authority::Harness,
        payload: Payload::Inline { value: v },
    }
}

/// A delivered run: the session, `complete`, the zip announced; `completed` is `run.completed` when the harness wrote it.
fn view(completed: Option<Value>) -> RunView {
    let mut s = RunState::new("R1");
    s.apply(harness(1, EventType::RunCreated, json!({})));
    s.apply(harness(
        2,
        EventType::SessionStarted,
        json!({"agentId": "direct", "engine": "cline", "modelId": "global.anthropic.claude-sonnet-5-5"}),
    ));
    s.apply(harness(
        3,
        EventType::GoalStatus,
        json!({"status": "complete", "phase": "delivering"}),
    ));
    s.apply(harness(
        4,
        EventType::DeliveryReady,
        json!({"artifactId": "ART1", "sha256": "ab".repeat(32), "sizeBytes": 2048}),
    ));
    if let Some(c) = completed {
        s.apply(harness(5, EventType::RunCompleted, c));
    }
    s.view()
}

fn card(wait: Option<Duration>) -> FinalCard {
    FinalCard {
        local_path: "/downloads/s3-only-R1.zip".into(),
        label_missing_after: wait,
        ..FinalCard::default()
    }
}

#[test]
fn the_wait_is_written_in_whole_or_tenths_of_seconds() {
    assert_eq!(wait_text(Duration::from_secs(30)), "30 s");
    assert_eq!(wait_text(Duration::from_millis(600)), "0.6 s");
    assert_eq!(wait_text(Duration::from_millis(1500)), "1.5 s");
    assert_eq!(wait_text(Duration::ZERO), "0 s");
}

#[test]
fn a_card_after_a_wait_with_no_label_says_it_was_not_received() {
    let text = final_card(&view(None), &card(Some(Duration::from_secs(30))));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[1],
        "Outcome   not received: run.completed did not arrive within 30 s; ham watch R1 shows it once it does",
        "{text}"
    );
    assert!(lines[0].starts_with("Run       R1 complete"), "{text}");
}

#[test]
fn a_card_with_the_label_or_without_a_wait_has_no_such_line() {
    let done =
        json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"});
    let with_label = final_card(&view(Some(done)), &card(Some(Duration::from_secs(30))));
    assert!(
        with_label
            .lines()
            .any(|l| l == "Outcome   Cline reported done")
    );
    assert!(!with_label.contains("not received"), "{with_label}");
    // The TUI draws the card while the label may still come: no Outcome line yet.
    let waiting = final_card(&view(None), &card(None));
    assert!(!waiting.contains("Outcome"), "{waiting}");
}
