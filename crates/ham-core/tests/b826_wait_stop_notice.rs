//! What the client shows when the engine's guard ended a prompt on a wait that was sent again. The worker resumes the prompt and
//! does not count the stop (`turn.progress`, kind `engine-stop`, hint `wait-repeated`, no try number).
#![allow(clippy::unwrap_used)]

use ham_core::notice::notice_of;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use serde_json::{Value, json};

fn event(payload: Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: "01J9ZZZZZZ0000000000000002".into(),
        run_id: "01JABCDEFGHJKMNPQRSTVWXYZ0".into(),
        task_id: None,
        agent_id: Some("direct".into()),
        seq: 2,
        ts: "2026-11-16T07:00:00Z".into(),
        event_type: EventType::TurnProgress,
        authority: Authority::Agent,
        payload: Payload::Inline { value: payload },
    }
}

#[test]
fn b826_a_stop_on_a_repeated_wait_reads_as_not_counted() {
    let forgiven = event(
        json!({"turn": 1, "kind": "engine-stop", "hint": "wait-repeated", "backoffS": 5,
        "detail": "the engine stopped the run itself (5 identical tool calls in a row) while it waited for a command that ran long"}),
    );
    assert_eq!(
        notice_of(&forgiven).unwrap().text,
        "The engine stopped its own prompt (repeated a wait on a command that ran long; not counted): another try follows in 5 s."
    );
    // A counted stop reads as before.
    let counted = event(
        json!({"turn": 1, "kind": "engine-stop", "hint": "loop-guard", "attempt": 1, "of": 1, "backoffS": 5}),
    );
    assert_eq!(
        notice_of(&counted).unwrap().text,
        "The engine stopped its own prompt (repeated the same tool call): try 1 of 1 follows in 5 s."
    );
}
