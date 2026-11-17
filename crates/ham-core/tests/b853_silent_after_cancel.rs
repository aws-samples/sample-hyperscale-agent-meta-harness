//! With: what the client says about a silent session once the user's cancel is recorded.
//!
//! After a recorded cancel the service starts no further session: when it calls the silent session lost, the run ends and delivers by
//! the rule of the cancel (`ham_core::cancel_note::DELIVERS`). The notice of a silent session and the sentence about an open question
//! of a session that was asked to end must say the same. Before, both still promised a new session, right under the line that says
//! the run starts no further one.
#![allow(clippy::unwrap_used)]

use ham_core::cancel_note::{DELIVERS, KIND};
use ham_core::lost_session::{Silence, notice_after_s};
use ham_core::state::RunState;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use serde_json::{Value, json};

const RUN: &str = "01JABCDEFGHJKMNPQRSTVWXYZ0";
/// 2026-11-14T07:00:00Z.
const T0: i64 = 1_794_639_600;
const HEAD: &str = "29a235e2ea242b5e494b8dca989a4ea841132055";
/// The cancel is recorded at 07:05:00.
const CANCEL: i64 = 300;

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

/// A run whose first session started at 07:03:10. `service` is what the service's `session.started` event adds to its payload.
fn begun(service: Value, saved: bool) -> Vec<Envelope> {
    let mut started =
        json!({"agentId": "direct", "engine": "cline", "modelId": "us.anthropic.claude-opus-5-5"});
    for (k, x) in service.as_object().unwrap() {
        started[k] = x.clone();
    }
    let mut events = vec![
        ev(
            1,
            188,
            EventType::RunCreated,
            Authority::Harness,
            json!({"target": "s3-only"}),
        ),
        ev(
            2,
            189,
            EventType::GoalStatus,
            Authority::Harness,
            json!({"status": "active", "phase": "green"}),
        ),
        ev(
            3,
            189,
            EventType::SessionStarted,
            Authority::Harness,
            started,
        ),
        ev(
            4,
            190,
            EventType::TurnStarted,
            Authority::Agent,
            json!({"turn": 1, "engine": "cline", "model": "us.anthropic.claude-opus-5-5", "role": "direct", "session": 1, "beatS": 60}),
        ),
    ];
    if saved {
        events.push(ev(
            5,
            211,
            EventType::TurnProgress,
            Authority::Agent,
            json!({"turn": 1, "savedCopy": {"at": at(211), "count": 1, "headSha": HEAD, "session": 1}}),
        ));
    }
    events
}

/// The service's word that the user's cancel is recorded: the run's session is asked to stop.
fn cancel_recorded(n: u64) -> Envelope {
    ev(
        n,
        CANCEL,
        EventType::RunWaiting,
        Authority::Harness,
        json!({
            "kind": KIND, "requestedAt": at(CANCEL), "session": 1, "sessionLive": true,
            "lastHeartbeatAt": at(CANCEL - 50), "lostAfter": at(CANCEL + 550)
        }),
    )
}

fn state_of(events: Vec<Envelope>) -> RunState {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    s
}

/// The notice of the silent session of the run, two beats and half a minute (and five seconds) after its newest event at `newest`.
fn silence_of(events: Vec<Envelope>, newest: i64) -> Silence {
    let quiet = i64::try_from(notice_after_s(60)).unwrap();
    state_of(events)
        .view_at(Some(T0 + newest + quiet + 5))
        .silence()
        .expect("TRIGGER PRESENT: the session is silent")
        .clone()
}

const WHEN: &str = "No event from the run's session since 2026-11-14 07:05 UTC (2 min). When the service receives no heartbeat from a \
                    session for 600 seconds, the service calls the session lost. You stopped the run at 2026-11-14 07:05 UTC, so no \
                    new session starts:";

fn promises_no_session(said: &str) {
    for promise in [
        "starts a new session",
        "a new session starts and asks again",
    ] {
        assert!(
            !said.contains(promise),
            "a run whose cancel is recorded starts no further session: {said}"
        );
    }
}

#[test]
fn b853_the_notice_of_a_silent_session_after_a_recorded_cancel_promises_no_new_session() {
    // TRIGGER PRESENT: the same silence without the cancel is the notice that the service starts a new session.
    let before = silence_of(begun(json!({"storageReadBack": true}), true), 211);
    assert!(
        before.sentence().contains("starts a new session"),
        "TRIGGER PRESENT: {}",
        before.sentence()
    );

    // The cancel is recorded and the session stays silent. The service's event is the run's newest event.
    let mut events = begun(json!({"storageReadBack": true}), true);
    events.push(cancel_recorded(6));
    let state = state_of(events.clone());
    assert!(
        state.view().cancel_recorded().is_some(),
        "TRIGGER PRESENT: the view holds the recorded cancel"
    );
    let quiet = silence_of(events, CANCEL);
    promises_no_session(&quiet.sentence());
    // The copy the run delivers is told by the one rule of the cancel lines.
    assert_eq!(
        quiet.sentence(),
        format!("{WHEN} the run ends and the service {DELIVERS}.")
    );

    // The same for a run that saved no copy: the rule's last clause covers it.
    let mut events = begun(json!({"storageReadBack": true}), false);
    events.push(cancel_recorded(6));
    let quiet = silence_of(events, CANCEL);
    assert_eq!(
        quiet.sentence(),
        format!("{WHEN} the run ends and the service {DELIVERS}.")
    );

    // Losing the session would have ended the run anyway: the cancel is still the reason the notice names.
    let mut events = begun(json!({"storageReadBack": true}), true);
    events.push(cancel_recorded(6));
    let mut ending = silence_of(events, CANCEL);
    ending.would_end = Some(3);
    assert_eq!(
        ending.sentence(),
        format!("{WHEN} the run ends and the service {DELIVERS}.")
    );
}

/// A run for which the service makes no read of a stopped session's storage (the read is off, the machine keeps no storage, or the
/// stage is of an earlier release) is told of no files read back.
#[test]
fn b853_the_notice_after_a_recorded_cancel_names_no_files_read_back_when_the_service_reads_none() {
    let mut events = begun(json!({}), true);
    events.push(cancel_recorded(6));
    let quiet = silence_of(events, CANCEL);
    assert!(!quiet.reads_back, "TRIGGER PRESENT: no read for this run");
    promises_no_session(&quiet.sentence());
    assert_eq!(
        quiet.sentence(),
        format!(
            "{WHEN} the run ends and the service delivers the last saved copy (saved 2026-11-14 07:03 UTC)."
        )
    );
    assert!(
        !quiet.sentence().contains("read back"),
        "{}",
        quiet.sentence()
    );

    let mut events = begun(json!({}), false);
    events.push(cancel_recorded(6));
    let quiet = silence_of(events, CANCEL);
    assert_eq!(
        quiet.sentence(),
        format!("{WHEN} the run ends, and it has saved no copy that the service could deliver.")
    );
}

/// An open question of a session that was asked to end: after a recorded cancel no new session asks again.
#[test]
fn b853_an_open_question_of_an_ending_session_after_a_recorded_cancel_promises_no_new_session() {
    let question = |n: u64, secs: i64| {
        ev(
            n,
            secs,
            EventType::TurnProgress,
            Authority::Agent,
            json!({
                "turn": 1, "kind": "needs-input", "text": "Needs your input: Cline ended without changing files",
                "requestId": format!("01J9ZZZZZZ00000000000000{n:02}"), "n": 1, "reason": "no_changes",
                "title": "Cline ended without changing files", "stopOutcome": "stopped_no_changes",
                "evidence": {"attempts": 1, "toolCalls": 0, "filesChanged": 0, "identicalCalls": null, "failedTurns": null,
                             "lastError": null, "runS": 30, "sessions": 1},
                "lastMessage": "Which design do you want?", "files": [], "filesTotal": 0, "blocking": true,
                "options": ["continue_with_prompt", "finish"], "models": [], "recommended": null, "canContinue": true,
                "waitS": 1800, "expiresAt": at(secs + 1800)
            }),
        )
    };
    let asked_to_end = |n: u64, secs: i64| {
        ev(
            n,
            secs,
            EventType::TurnProgress,
            Authority::Agent,
            json!({"turn": 1, "kind": "notify", "reason": "copy_not_saved", "outageMinutes": null,
                   "detail": "The session was asked to end, and a copy of the work could not be saved."}),
        )
    };
    let whose =
        "The open question was asked by that session: once it has ended, no answer reaches it.";

    // TRIGGER PRESENT: without a cancel the note promises that a new session asks again.
    let mut events = begun(json!({}), false);
    events.push(question(5, 220));
    events.push(asked_to_end(6, 280));
    let note = state_of(events.clone()).view().end_request_note().unwrap();
    assert!(
        note.ends_with(&format!(
            "{whose} When the run has time left, a new session starts and asks again when it needs your answer."
        )),
        "TRIGGER PRESENT: {note}"
    );

    events.push(cancel_recorded(7));
    let v = state_of(events).view();
    assert!(
        v.input.waiting().is_some(),
        "TRIGGER PRESENT: the question is still open"
    );
    let note = v.end_request_note().unwrap();
    promises_no_session(&note);
    assert!(
        note.ends_with(&format!(
            "{whose} You stopped the run, so no new session starts and nobody asks again."
        )),
        "{note}"
    );
}
