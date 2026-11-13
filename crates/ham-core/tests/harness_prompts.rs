//! The count of prompts the harness sent to the engine on its own. The run's
//! own events are the only source the client holds, so the rule is stated by what the events record: a goal-loop continue
//! (`goal-loop`, phase `kick`), a provider retry (agentd sends a fixed resume sentence after each) and a fresh session (its first prompt is
//! harness-written). It counts events once each, whatever the delivery order, and it counts nothing the engine or the person wrote.
#![allow(clippy::unwrap_used)]

use ham_core::state::{RunState, RunView};
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";

fn id(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn progress(n: u32, value: Value) -> Envelope {
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": id(n), "runId": RUN, "agentId": "direct", "seq": n, "ts": "2026-10-16T08:15:00Z",
        "type": "turn.progress", "authority": "agent", "payload": {"kind": "inline", "value": value}
    }))
    .unwrap()
}

fn kick(n: u32) -> Envelope {
    progress(n, json!({"turn": 1, "kind": "goal-loop", "phase": "kick"}))
}

fn round(n: u32) -> Envelope {
    progress(
        n,
        json!({"turn": 1, "kind": "goal-loop", "phase": "round", "rounds": 2, "status": "completed"}),
    )
}

fn retry(n: u32) -> Envelope {
    progress(
        n,
        json!({"turn": 1, "kind": "provider-retry", "attempt": 2, "of": 4, "class": "transient", "backoffS": 12, "detail": "x"}),
    )
}

fn fresh(n: u32, session: u32) -> Envelope {
    progress(
        n,
        json!({"turn": 1, "kind": "new-session", "text": "Restarted from the work tree: fresh Cline session.", "requestId": null,
               "session": session, "trigger": "harness", "reason": "replay_rejected", "restartedProcess": false,
               "digestBytes": 9210, "promptBytes": 11540}),
    )
}

fn message(n: u32) -> Envelope {
    progress(n, json!({"turn": 1, "kind": "message", "text": "hello"}))
}

fn view_of(events: &[Envelope]) -> RunView {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e.clone());
    }
    s.view()
}

#[test]
fn a_run_with_no_automatic_prompt_counts_none() {
    let v = view_of(&[message(1), round(2)]);
    assert_eq!(v.harness_prompts, 0);
}

#[test]
fn continues_retries_and_fresh_sessions_are_counted_one_each() {
    let v = view_of(&[
        kick(1),
        message(2),
        kick(3),
        retry(4),
        retry(5),
        round(6),
        fresh(7, 2),
    ]);
    assert_eq!(
        v.harness_prompts, 5,
        "2 continues + 2 retries + 1 fresh session"
    );
    assert_eq!(v.sessions, 2);
}

#[test]
fn a_round_counter_a_message_and_the_first_session_are_not_prompts() {
    let v = view_of(&[round(1), round(2), message(3)]);
    assert_eq!(v.harness_prompts, 0);
    assert_eq!(
        v.sessions, 1,
        "the first session's prompt is the person's own text"
    );
}

#[test]
fn an_event_delivered_twice_or_out_of_order_counts_once() {
    let events = [kick(3), retry(1), kick(3), retry(1), fresh(2, 2)];
    let v = view_of(&events);
    assert_eq!(v.harness_prompts, 3);
    let reversed: Vec<Envelope> = events.iter().rev().cloned().collect();
    assert_eq!(view_of(&reversed).harness_prompts, 3);
}
