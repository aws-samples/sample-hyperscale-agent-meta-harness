//! The question of a session that was lost is over. No engine knows its request id any more, so the fold of the correction path
//! closes it: on the service's word that the session was lost (a harness `run.waiting` event of kind `session-lost`, which a reader of
//! all shards holds) and on the start of a later session (`turn.started` of the agent, which a reader of the agent's own events holds).
#![allow(clippy::unwrap_used)]

use harness_protocol::direct_input::fold_input;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use serde_json::{Value, json};

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const BEFORE: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT0A";
const AFTER: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT2A";

fn event(id: &str, t: EventType, authority: Authority, value: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: id.into(),
        run_id: RUN.into(),
        task_id: None,
        agent_id: (authority == Authority::Agent).then(|| "direct".to_string()),
        seq: 1,
        ts: "2026-10-15T13:41:07Z".into(),
        event_type: t,
        authority,
        payload: Payload::Inline { value },
    }
}

fn question() -> Envelope {
    event(
        REQ,
        EventType::TurnProgress,
        Authority::Agent,
        json!({"turn": 1, "kind": "needs-input", "requestId": REQ, "reason": "engine_stop", "title": "t",
               "blocking": true, "options": ["finish"], "canContinue": true, "waitS": 1800,
               "expiresAt": "2026-10-15T14:11:07Z"}),
    )
}

fn session_lost(id: &str) -> Envelope {
    event(
        id,
        EventType::RunWaiting,
        Authority::Harness,
        json!({"kind": "session-lost", "session": 1, "decidedAt": "2026-10-15T13:52:00Z", "cause": "no-heartbeat",
               "silenceS": 600, "lostInARow": 1, "limit": 3, "then": "restart", "nextSession": 2, "from": "start"}),
    )
}

fn started(id: &str, session: u32) -> Envelope {
    event(
        id,
        EventType::TurnStarted,
        Authority::Agent,
        json!({"turn": 1, "engine": "cline", "model": "m", "role": "direct", "session": session, "beatS": 60}),
    )
}

#[test]
fn b842_the_services_word_that_a_session_was_lost_closes_the_question_that_session_asked() {
    let events = [question(), session_lost(AFTER)];
    let fold = fold_input(&events);
    assert!(
        fold.open.is_none(),
        "the question of a lost session is still open: {:?}",
        fold.open
    );
    let closed = fold.closed.last().expect("the question is closed");
    assert_eq!(closed.request_id, REQ);
    assert_eq!(closed.how, "session-lost");
    // A lost session before the question: the question is a later session's and stays open.
    let events = [session_lost(BEFORE), question()];
    assert!(fold_input(&events).open.is_some());
    // Only the service's word counts: the same payload from an agent, and a `run.waiting` of another kind, close nothing.
    let mut forged = session_lost(AFTER);
    forged.authority = Authority::Agent;
    forged.agent_id = Some("direct".into());
    assert!(fold_input(&[question(), forged]).open.is_some());
    let other = event(
        AFTER,
        EventType::RunWaiting,
        Authority::Harness,
        json!({"kind": "something-else"}),
    );
    assert!(fold_input(&[question(), other]).open.is_some());
}

#[test]
fn b842_the_start_of_a_later_session_closes_the_question_of_the_session_before() {
    // The run list reads the agent's own events: the start of the next session is among them, the service's word is not.
    let events = [started(BEFORE, 1), question(), started(AFTER, 2)];
    let fold = fold_input(&events);
    assert!(
        fold.open.is_none(),
        "a new session started and the question of the session before is still open: {:?}",
        fold.open
    );
    assert_eq!(fold.closed.last().unwrap().how, "session-lost");
    // The start of the session that asked changes nothing.
    assert!(fold_input(&[started(BEFORE, 1), question()]).open.is_some());
}
