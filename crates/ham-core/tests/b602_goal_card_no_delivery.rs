//! The card of a goal-loop brake on a run that changed no file says that nothing was delivered and what to do next: the
//! T2 rerun changed zero files, made no pull request, and its card said "To go on from the code this run delivered:". The brake event carries the
//! count of changed files the harness measured (`filesChanged`); a count of 0 changes the words, and a card without the count reads as before.

#![allow(clippy::unwrap_used)]

use ham_core::state::{RunState, RunView};
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";

fn id(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn event(n: u32, ty: &str, authority: &str, value: Value) -> Envelope {
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": id(n), "runId": RUN, "agentId": "direct", "seq": n, "ts": "2026-11-11T08:15:00Z",
        "type": ty, "authority": authority, "payload": {"kind": "inline", "value": value}
    }))
    .unwrap()
}

fn view_of(files: Option<u64>) -> RunView {
    let mut brake = json!({"turn": 1, "kind": "goal-loop", "phase": "brake", "rule": "no-progress",
                           "text": "The harness ended the run: 3 rounds in a row changed no file."});
    if let Some(n) = files {
        brake["filesChanged"] = json!(n);
    }
    let mut s = RunState::new(RUN);
    for e in [
        event(
            1,
            "session.started",
            "harness",
            json!({"agentId": "direct", "engine": "cline", "modelId": "m"}),
        ),
        event(
            2,
            "turn.progress",
            "agent",
            json!({"turn": 1, "kind": "goal-loop", "phase": "round", "rounds": 3, "status": "completed"}),
        ),
        event(3, "turn.progress", "agent", brake),
    ] {
        s.apply(e);
    }
    s.view()
}

fn card(files: Option<u64>) -> String {
    view_of(files)
        .goal_brake
        .expect("a brake card")
        .lines(RUN, 200)
        .join("\n")
}

#[test]
fn b602_a_brake_on_a_run_that_changed_no_file_says_nothing_was_delivered() {
    let text = card(Some(0));
    assert!(!text.contains("the code this run delivered"), "{text}");
    assert!(
        text.contains("Cline changed no file, so this run delivered no code."),
        "{text}"
    );
    assert!(text.contains("To try again from the same start:"), "{text}");
    assert!(
        text.contains(&format!("ham continue {RUN} '<what Cline should do next>'")),
        "{text}"
    );
}

#[test]
fn b602_a_brake_on_a_run_that_changed_files_reads_as_before() {
    let text = card(Some(4));
    assert!(
        text.contains("To go on from the code this run delivered:"),
        "{text}"
    );
    assert!(!text.contains("delivered no code"), "{text}");
}

#[test]
fn b602_a_brake_event_without_the_count_reads_as_before() {
    let text = card(None);
    assert!(
        text.contains("To go on from the code this run delivered:"),
        "{text}"
    );
}

#[test]
fn b602_the_card_keeps_the_count_it_was_given() {
    assert_eq!(view_of(Some(0)).goal_brake.unwrap().files_changed, Some(0));
    assert_eq!(view_of(None).goal_brake.unwrap().files_changed, None);
}
