//! `ham watch` in line mode and the card of a goal-loop brake: the card prints once when the brake is read, in plain lines, and
//! names the follow-up command. It never says that a run needs to be cancelled (the harness already ended it) and never calls the work
//! verified, tested, gated or evaluated.
#![allow(clippy::unwrap_used)]

use ham::watch::InputMemo;
use ham_core::state::{RunState, RunView};
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";

fn goal(n: u32, phase: &str, extra: Value) -> Envelope {
    let mut v = json!({"turn": 1, "kind": "goal-loop", "phase": phase});
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": format!("01M5A2R7K3C9XJ4D8E6F{n:06}"), "runId": RUN, "agentId": "direct", "seq": n,
        "ts": "2026-10-16T08:15:00Z", "type": "turn.progress", "authority": "agent",
        "payload": {"kind": "inline", "value": v}
    }))
    .unwrap()
}

fn view(events: Vec<Envelope>) -> RunView {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    s.view()
}

fn braked() -> RunView {
    view(vec![
        goal(1, "round", json!({"rounds": 4, "status": "completed"})),
        goal(
            2,
            "brake",
            json!({"rule": "blocked-twice", "text": "The harness ended the run: Cline ended 2 rounds in a row with the same last line. Its words: \"BLOCKED: need the API key\""}),
        ),
    ])
}

#[test]
fn the_card_prints_once_with_the_rule_the_rounds_and_the_follow_up_command() {
    let mut memo = InputMemo::new();
    let v = braked();
    let first = memo.lines(&v).join("\n");
    assert!(first.contains("The harness ended the run:"), "{first}");
    assert!(first.contains("4 rounds"), "{first}");
    assert!(first.contains("BLOCKED: need the API key"), "{first}");
    assert!(first.contains(&format!("ham continue {RUN}")), "{first}");
    assert!(!first.contains("ham cancel"), "{first}");
    for w in ["verified", "tested", "gated", "evaluated"] {
        assert!(!first.to_lowercase().contains(w), "{first}");
    }
    assert!(memo.lines(&v).is_empty(), "the card prints once");
    println!("WATCH-GOAL-BRAKE TRIGGER PRESENT: card printed once, follow-up command named");
}

#[test]
fn a_run_without_a_brake_prints_nothing() {
    let mut memo = InputMemo::new();
    let v = view(vec![goal(
        1,
        "round",
        json!({"rounds": 2, "status": "completed"}),
    )]);
    assert!(memo.lines(&v).is_empty());
}
