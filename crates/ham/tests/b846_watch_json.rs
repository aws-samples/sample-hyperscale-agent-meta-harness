//! What `ham watch --json` carries about a request to end the session and about a recorded cancel.
//!
//! A worker that is asked to end says so in one notice when its copy of that moment could not be saved. The view keeps the time of that
//! word (`RunView::end_asked`). The JSON view had no member for it: a script saw the notice only in `copyNotices`, which a stored copy
//! empties. `endRequestedAt` is that time, and it stays until the service calls the session lost (the fact is then in `lostSessions`) or
//! a later session starts. `cancelRequestedAt` is the time of a cancel that the service recorded, while the run goes on.
#![allow(clippy::unwrap_used)]

use ham::watch::view_json;
use ham_core::state::RunState;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use serde_json::{Value, json};

const RUN: &str = "01JABCDEFGHJKMNPQRSTVWXYZ0";
const STOP: &str = "The session was asked to end, and a copy of the work could not be saved.";
const HEAD: &str = "29a235e2ea242b5e494b8dca989a4ea841132055";

/// `07:MM:SS` on 2026-11-14 as an RFC 3339 UTC time, for a second count after 07:00:00.
fn at(secs: i64) -> String {
    format!("2026-11-14T07:{:02}:{:02}Z", secs / 60, secs % 60)
}

fn ev(n: u64, secs: i64, t: EventType, authority: Authority, payload: Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: format!("01J9ZZZZZZ00000000000000{n:02}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: (authority == Authority::Agent).then(|| "direct".to_string()),
        seq: n,
        ts: at(secs),
        event_type: t,
        authority,
        payload: Payload::Inline { value: payload },
    }
}

fn h(n: u64, secs: i64, t: EventType, p: Value) -> Envelope {
    ev(n, secs, t, Authority::Harness, p)
}

fn a(n: u64, secs: i64, t: EventType, p: Value) -> Envelope {
    ev(n, secs, t, Authority::Agent, p)
}

fn started(n: u64, secs: i64, session: u32) -> Envelope {
    a(
        n,
        secs,
        EventType::TurnStarted,
        json!({"turn": 1, "engine": "cline", "model": "us.anthropic.claude-opus-5-5", "role": "direct", "session": session, "beatS": 60}),
    )
}

fn copy(n: u64, secs: i64, count: u32) -> Envelope {
    a(
        n,
        secs,
        EventType::TurnProgress,
        json!({"turn": 1, "savedCopy": {"at": at(secs), "count": count, "headSha": HEAD, "session": 1}}),
    )
}

fn end_request(n: u64, secs: i64) -> Envelope {
    a(
        n,
        secs,
        EventType::TurnProgress,
        json!({"turn": 1, "kind": "notify", "reason": "copy_not_saved", "outageMinutes": null, "detail": STOP}),
    )
}

fn begun() -> Vec<Envelope> {
    vec![
        h(1, 188, EventType::RunCreated, json!({"target": "s3-only"})),
        h(
            2,
            189,
            EventType::GoalStatus,
            json!({"status": "active", "phase": "green"}),
        ),
        started(3, 190, 1),
    ]
}

fn json_of(events: Vec<Envelope>) -> Value {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    view_json(&s.view())
}

#[test]
fn b846_the_view_keeps_a_request_to_end() {
    // TRIGGER PRESENT: a run whose session works carries neither member.
    let v = json_of(begun());
    assert!(
        v.get("endRequestedAt").is_none() && v.get("cancelRequestedAt").is_none(),
        "{v}"
    );

    // The worker says that its session was asked to end.
    let mut events = begun();
    events.push(end_request(4, 598));
    let v = json_of(events.clone());
    assert_eq!(v["endRequestedAt"], json!(at(598)), "{v}");
    assert_eq!(v["copyNotices"][0]["endRequest"], json!(true), "{v}");

    // The worker's next try stored a copy: the notices are over, and the request to end still stands.
    events.push(copy(5, 604, 2));
    let v = json_of(events.clone());
    assert!(
        v.get("copyNotices").is_none(),
        "a stored copy empties the notices: {v}"
    );
    assert_eq!(
        v["endRequestedAt"],
        json!(at(598)),
        "the time of the worker's word, not of the copy: {v}"
    );

    // The service calls the session lost: its record takes the place of the worker's word.
    let mut lost = events.clone();
    lost.push(h(
        6,
        1_253,
        EventType::RunWaiting,
        json!({
            "kind": "session-lost", "session": 1, "decidedAt": at(1_253), "cause": "no-heartbeat", "silenceS": 600,
            "lostInARow": 1, "limit": 3, "then": "restart", "nextSession": 2, "from": "copy", "savedAt": at(604)
        }),
    ));
    let v = json_of(lost);
    assert!(v.get("endRequestedAt").is_none(), "{v}");
    assert_eq!(v["lostSessions"][0]["session"], json!(1), "{v}");

    // A later session's start: the request was the earlier session's.
    let mut next = events.clone();
    next.push(started(7, 700, 2));
    assert!(json_of(next).get("endRequestedAt").is_none());
}

#[test]
fn b853_the_view_carries_the_time_of_a_recorded_cancel() {
    let mut events = begun();
    events.push(h(
        4,
        322,
        EventType::RunWaiting,
        json!({
            "kind": "cancel-recorded", "requestedAt": at(322), "session": 1, "sessionLive": true,
            "lastHeartbeatAt": at(310), "lostAfter": at(910)
        }),
    ));
    let v = json_of(events.clone());
    assert_eq!(v["cancelRequestedAt"], json!(at(322)), "{v}");
    assert!(
        v.get("lostSessions").is_none(),
        "the cancel event is no lost session: {v}"
    );
    // Once the run has ended the record is history.
    events.push(h(
        5,
        400,
        EventType::GoalStatus,
        json!({"status": "complete", "phase": "delivering"}),
    ));
    assert!(json_of(events).get("cancelRequestedAt").is_none());
}
