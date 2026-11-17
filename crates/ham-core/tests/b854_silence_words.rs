//! What the notice of a silent session says about the service's read of the session's storage. The client promises a read
//! only when the service said that it makes one: its `session.started` event carries `storageReadBack: true` for a run whose stopped
//! sessions it reads back. Three cases: a stage of this release with the read on (the notice says that the storage is read back and
//! calls nothing lost), the read off for the run (the notice of release 5.22: the run goes on from the last saved copy and file
//! changes after it are lost), and a stage of an earlier release (the same notice: its service makes no read and says nothing).
#![allow(clippy::unwrap_used)]

use ham_core::lost_session::{Silence, notice_after_s};
use ham_core::state::RunState;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use serde_json::{Value, json};

const RUN: &str = "01JABCDEFGHJKMNPQRSTVWXYZ0";
/// 2026-11-14T07:00:00Z.
const T0: i64 = 1_794_639_600;
const HEAD: &str = "29a235e2ea242b5e494b8dca989a4ea841132055";

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

fn silence_of(events: Vec<Envelope>) -> Silence {
    let newest = if events.len() > 4 { 211 } else { 190 };
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    let quiet = i64::try_from(notice_after_s(60)).unwrap();
    s.view_at(Some(T0 + newest + quiet + 5))
        .silence()
        .expect("TRIGGER PRESENT: the session is silent")
        .clone()
}

const WHEN: &str = "No event from the run's session since 2026-11-14 07:03 UTC (2 min). When the service receives no heartbeat from a \
                    session for 600 seconds,";
/// The notice of release 5.22 for a run that saved a copy.
const AS_BEFORE: &str = "it starts a new session from the last saved copy (saved 2026-11-14 07:03 UTC) when the run has time left; file \
                         changes after that copy are lost.";

/// A stage of this release, the read on: the service said so, and the notice says what the service does and calls nothing lost.
#[test]
fn b854_the_notice_of_a_silent_session_says_that_the_storage_is_read_back_when_the_service_said_so()
{
    let quiet = silence_of(begun(json!({"storageReadBack": true}), true));
    assert_eq!(
        quiet.sentence(),
        format!(
            "{WHEN} it reads the session's storage back and starts a new session when the run has time left: from the files it read \
             back when its checks leave no doubt, and from the last saved copy (saved 2026-11-14 07:03 UTC) otherwise."
        )
    );
    assert!(
        !quiet.sentence().contains("are lost"),
        "{}",
        quiet.sentence()
    );
    // No copy was saved yet.
    let quiet = silence_of(begun(json!({"storageReadBack": true}), false));
    assert_eq!(
        quiet.sentence(),
        format!(
            "{WHEN} it reads the session's storage back and starts a new session when the run has time left: from the files it read \
             back when its checks leave no doubt, and from the files the run started with otherwise, because the run has saved no copy \
             yet."
        )
    );
    // Losing the session would end the run.
    let mut ending = silence_of(begun(json!({"storageReadBack": true}), true));
    ending.would_end = Some(3);
    assert_eq!(
        ending.sentence(),
        format!(
            "{WHEN} the session is lost; it would be lost session 3 in a row, so the run would end. The service would read the \
             session's storage back and deliver those files when its checks leave no doubt, and the last saved copy otherwise."
        )
    );
    ending.saved_at = None;
    assert_eq!(
        ending.sentence(),
        format!(
            "{WHEN} the session is lost; it would be lost session 3 in a row, so the run would end. The service would read the \
             session's storage back and deliver those files when its checks leave no doubt; the run has saved no copy that it could \
             deliver otherwise."
        )
    );
}

/// The read is off for the run, or its machine keeps no storage: the service's event has no such member, and the notice is the one of
/// release 5.22, which is true for such a run.
#[test]
fn b854_the_notice_of_a_silent_session_promises_no_read_for_a_run_without_one() {
    for service in [json!({}), json!({"storageReadBack": false})] {
        let quiet = silence_of(begun(service.clone(), true));
        assert_eq!(quiet.sentence(), format!("{WHEN} {AS_BEFORE}"), "{service}");
        assert!(!quiet.sentence().contains("read"), "{service}");
    }
}

/// A stage of an earlier release writes no such member, and its service reads nothing back: the notice is the one of release 5.22,
/// byte for byte, in every form. A word of the worker is not the service's word.
#[test]
fn b854_the_notice_of_a_silent_session_on_a_stage_of_an_earlier_release_is_the_notice_of_that_release()
 {
    let quiet = silence_of(begun(json!({}), true));
    assert_eq!(quiet.sentence(), format!("{WHEN} {AS_BEFORE}"));
    let none = silence_of(begun(json!({}), false));
    assert_eq!(
        none.sentence(),
        format!(
            "{WHEN} it starts a new session from the files the run started with when the run has time left, because the run has saved \
             no copy yet."
        )
    );
    let mut ending = quiet.clone();
    ending.would_end = Some(3);
    assert_eq!(
        ending.sentence(),
        format!(
            "{WHEN} the session is lost; it would be lost session 3 in a row, so the run would end and the service would deliver the \
             last saved copy."
        )
    );
    // The member in an event that is not the service's is not believed.
    let mut events = begun(json!({}), true);
    events.insert(
        3,
        ev(
            9,
            189,
            EventType::SessionStarted,
            Authority::Agent,
            json!({"agentId": "direct", "engine": "cline", "modelId": "us.anthropic.claude-opus-5-5", "storageReadBack": true}),
        ),
    );
    let quiet = silence_of(events);
    assert_eq!(quiet.sentence(), format!("{WHEN} {AS_BEFORE}"));
}
