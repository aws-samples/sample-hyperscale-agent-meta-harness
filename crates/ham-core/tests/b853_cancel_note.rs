//! What the client says about a cancel that the service recorded.
//!
//! The service answers `ham cancel` with what it recorded and writes the same facts as one `run.waiting` event of the harness (kind
//! `cancel-recorded`). `CancelNote` reads both and words them in three forms: a session that is asked to stop, a run that has no session
//! that answers, and a run that is already delivering. The view keeps the note while the run goes on (`RunView::cancel_recorded`), and the
//! words beside its status are `cancel recorded`. A session that is lost after the cancel is told with the cancel as the reason the run
//! ends (`SessionLost.cancelledAt`); without that member every sentence is the one of release 5.22.
#![allow(clippy::unwrap_used)]

use ham_core::cancel_note::{CancelNote, KIND, RECORDED_WORDS};
use ham_core::lost_session::{self, SessionLost};
use ham_core::state::{RunState, SessionGap};
use harness_protocol::api::v1::CancelRecorded;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use serde_json::{Value, json};

const RUN: &str = "01M7HJNPAZVSDBPKFS6FB44ZY4";

fn ev(n: u64, ts: &str, t: EventType, authority: Authority, payload: Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: format!("01J9ZZZZZZ00000000000000{n:02}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: (authority == Authority::Agent).then(|| "direct".to_string()),
        seq: n,
        ts: ts.into(),
        event_type: t,
        authority,
        payload: Payload::Inline { value: payload },
    }
}

fn waiting(n: u64, ts: &str, payload: Value) -> Envelope {
    ev(n, ts, EventType::RunWaiting, Authority::Harness, payload)
}

/// A session that is asked to stop.
fn live() -> Value {
    json!({
        "kind": KIND, "requestedAt": "2026-11-16T07:05:22Z", "session": 1, "sessionLive": true,
        "lastHeartbeatAt": "2026-11-16T07:05:22Z", "lostAfter": "2026-11-16T07:15:22Z"
    })
}

/// No session that answers.
fn none_answers() -> Value {
    json!({"kind": KIND, "requestedAt": "2026-11-16T07:20:01Z", "session": 1, "sessionLive": false})
}

/// A run that is already delivering.
fn delivering() -> Value {
    json!({"kind": KIND, "requestedAt": "2026-11-16T07:20:01Z", "session": 1, "sessionLive": false, "delivering": true})
}

fn note(payload: Value) -> CancelNote {
    CancelNote::parse(&waiting(9, "2026-11-16T07:05:22Z", payload)).expect("a cancel note")
}

#[test]
fn b853_cancel_note_sentences() {
    let copy = format!(
        "ham pull {RUN} --snapshot downloads the newest saved copy of the work at any time, if the run saved one."
    );
    // A session that is asked to stop: the latest time at which it is called lost, which is no promise of the end.
    let n = note(live());
    assert_eq!(
        n.answer_lines(RUN),
        vec![
            "The cancel is recorded (2026-11-16 07:05:22 UTC). The run's session is asked to stop and the work so far is kept: the run \
             delivers what exists."
                .to_string(),
            "If session 1 does not answer, the service calls it lost at the latest at about 07:15 UTC, starts no further session and \
             delivers the files it read back from the session's storage when its checks leave no doubt, and otherwise the last saved copy, if the run saved one."
                .to_string(),
            copy.clone(),
        ]
    );
    assert_eq!(
        n.watch_line(),
        "Cancel recorded at 2026-11-16 07:05 UTC: the run starts no further session. If session 1 stays silent, the service calls it \
         lost at the latest at about 07:15 UTC and delivers the files it read back from the session's storage when its checks leave no doubt, and otherwise the last saved copy, if the run saved one."
    );
    // A time of another day is said with its day.
    let mut next_day = live();
    next_day["lostAfter"] = json!("2026-11-17T00:02:00Z");
    assert!(
        note(next_day).answer_lines(RUN)[1].contains("at the latest at about 2026-11-17 00:02 UTC"),
    );
    // The service sent no time and no session number: the rule is said instead.
    let bare =
        note(json!({"kind": KIND, "requestedAt": "2026-11-16T07:05:22Z", "sessionLive": true}));
    assert_eq!(
        bare.answer_lines(RUN)[1],
        "If its session does not answer, the service calls it lost when it has received no heartbeat from it for 600 seconds, starts no \
         further session and delivers the files it read back from the session's storage when its checks leave no doubt, and otherwise the last saved copy, if the run saved one."
    );

    // No session that answers: the explicit clear is named.
    let n = note(none_answers());
    assert_eq!(
        n.answer_lines(RUN),
        vec![
            "The cancel is recorded (2026-11-16 07:20:01 UTC). The run has no session that answers: the service starts no further session \
             and delivers the files it read back from the session's storage when its checks leave no doubt, and otherwise the last saved copy, if the run saved one."
                .to_string(),
            format!("{copy} ham cancel {RUN} --discard ends the run now and delivers nothing."),
        ]
    );
    assert_eq!(
        n.watch_line(),
        "Cancel recorded at 2026-11-16 07:20 UTC: the run has no session that answers and starts no further one. The service delivers the files it read back from the session's storage when its checks leave no doubt, and otherwise the last saved copy, if the run saved one."
    );

    // A run that is already delivering: nothing will be called lost.
    let n = note(delivering());
    assert_eq!(
        n.answer_lines(RUN),
        vec![
            "The cancel is recorded (2026-11-16 07:20:01 UTC). The run is already delivering what exists.".to_string(),
            copy.clone(),
        ]
    );
    assert_eq!(
        n.watch_line(),
        "Cancel recorded at 2026-11-16 07:20 UTC: the run is already delivering what exists."
    );
    // Every form names the command that downloads the newest saved copy (root invariant 9).
    for payload in [live(), none_answers(), delivering()] {
        assert!(
            note(payload)
                .answer_lines(RUN)
                .iter()
                .any(|l| l.contains(&format!("ham pull {RUN} --snapshot"))),
        );
    }
}

#[test]
fn b853_a_cancel_note_is_read_from_the_harness_event_of_its_kind_only() {
    let at = "2026-11-16T07:05:22Z";
    // The answer of the route and the event say the same.
    let recorded: CancelRecorded = serde_json::from_value(live()).unwrap();
    assert_eq!(CancelNote::of(&recorded), Some(note(live())));
    // Another kind (the lost-session word), another authority, another event type: no note.
    let mut lost = live();
    lost["kind"] = json!(lost_session::KIND);
    assert_eq!(CancelNote::parse(&waiting(1, at, lost)), None);
    assert_eq!(
        CancelNote::parse(&ev(1, at, EventType::RunWaiting, Authority::Agent, live())),
        None
    );
    assert_eq!(
        CancelNote::parse(&ev(
            1,
            at,
            EventType::TurnProgress,
            Authority::Harness,
            live()
        )),
        None
    );
    // And the lost-session reader does not take the cancel event.
    assert_eq!(SessionLost::parse(&waiting(1, at, live())), None);
    // Without the time of the cancel there is no note; another time that is no UTC time is dropped.
    let mut no_time = live();
    no_time["requestedAt"] = json!("yesterday");
    assert_eq!(CancelNote::parse(&waiting(1, at, no_time)), None);
    let mut odd = live();
    odd["lostAfter"] = json!("soon");
    odd["lastHeartbeatAt"] = json!("\u{1b}[31mnow");
    let n = note(odd);
    assert_eq!((n.lost_after, n.last_heartbeat_at), (None, None));
    // The run id is made safe for a terminal.
    let lines = note(live()).answer_lines("01M7\u{1b}[31mX");
    assert!(lines.iter().all(|l| !l.contains('\u{1b}')), "{lines:?}");
}

fn state_of(events: Vec<Envelope>) -> RunState {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    s
}

fn working() -> Vec<Envelope> {
    vec![
        ev(
            1,
            "2026-11-16T07:03:00Z",
            EventType::RunCreated,
            Authority::Harness,
            json!({"target": "s3-only"}),
        ),
        ev(
            2,
            "2026-11-16T07:03:01Z",
            EventType::GoalStatus,
            Authority::Harness,
            json!({"status": "active", "phase": "green"}),
        ),
        ev(
            3,
            "2026-11-16T07:03:10Z",
            EventType::TurnStarted,
            Authority::Agent,
            json!({"turn": 1, "engine": "cline", "model": "us.anthropic.claude-opus-5-5", "role": "direct", "session": 1, "beatS": 60}),
        ),
    ]
}

#[test]
fn b853_the_view_keeps_the_note_while_the_run_goes_on() {
    // TRIGGER PRESENT: a run without the event has no note and no gap.
    let s = state_of(working());
    assert_eq!(s.view().cancel_recorded(), None);
    assert_eq!(s.view().session_gap_at(None), None);

    let mut events = working();
    events.push(waiting(4, "2026-11-16T07:05:22Z", live()));
    let s = state_of(events.clone());
    let v = s.view();
    assert_eq!(v.cancel_recorded(), Some(&note(live())));
    assert_eq!(v.session_gap_at(None), Some(SessionGap::Cancelling));
    assert_eq!(SessionGap::Cancelling.words(), RECORDED_WORDS);
    assert_eq!(RECORDED_WORDS, "cancel recorded");
    // The words win over a session that the service lost and replaces, and over a silent one.
    let mut lost = events.clone();
    lost.push(waiting(
        5,
        "2026-11-16T07:15:33Z",
        json!({
            "kind": lost_session::KIND, "session": 1, "decidedAt": "2026-11-16T07:15:33Z", "cause": "no-heartbeat", "silenceS": 600,
            "lostInARow": 1, "limit": 3, "then": "restart", "nextSession": 2, "from": "start"
        }),
    ));
    assert_eq!(
        state_of(lost).view().session_gap_at(Some(1_794_813_400)),
        Some(SessionGap::Cancelling)
    );
    // A second event (a client that reads the events twice) changes nothing, and the first time stands.
    let mut twice = events.clone();
    let mut later = live();
    later["requestedAt"] = json!("2026-11-16T07:09:00Z");
    twice.push(waiting(6, "2026-11-16T07:09:00Z", later));
    assert_eq!(
        state_of(twice)
            .view()
            .cancel_recorded()
            .map(|n| n.requested_at.as_str()),
        Some("2026-11-16T07:05:22Z")
    );
    // Once the run has ended the note is history: nothing is said beside the status.
    events.push(ev(
        7,
        "2026-11-16T07:16:00Z",
        EventType::GoalStatus,
        Authority::Harness,
        json!({"status": "complete", "phase": "delivering"}),
    ));
    let s = state_of(events);
    assert_eq!(s.view().cancel_recorded(), None);
    assert_eq!(s.view().session_gap_at(None), None);
}

fn lost(cancelled_at: Option<&str>, delivers: &str, lost_in_a_row: u32) -> SessionLost {
    let mut v = json!({
        "kind": lost_session::KIND, "session": 1, "decidedAt": "2026-11-16T07:15:33Z", "lastHeartbeatAt": "2026-11-16T07:05:22Z",
        "cause": "no-heartbeat", "silenceS": 600, "lostInARow": lost_in_a_row, "limit": 3, "then": "end", "delivers": delivers
    });
    if let Some(at) = cancelled_at {
        v["cancelledAt"] = json!(at);
    }
    SessionLost::parse(&waiting(8, "2026-11-16T07:15:33Z", v)).expect("the lost-session word")
}

#[test]
fn b853_a_session_lost_after_the_cancel_says_why_the_run_ends() {
    let head = "Session 1 was lost: the service received no heartbeat from it for 600 seconds (the last one came at 2026-11-16 07:05 UTC).";
    let cancelled = Some("2026-11-16T07:05:22Z");
    assert_eq!(
        lost(cancelled, "copy", 1).sentence(),
        format!(
            "{head} You had stopped the run at 2026-11-16 07:05 UTC, so no new session starts: the run ends and the service delivers the \
             last saved copy."
        )
    );
    assert_eq!(
        lost(cancelled, "nothing", 1).sentence(),
        format!(
            "{head} You had stopped the run at 2026-11-16 07:05 UTC, so no new session starts: the run ends. The run saved no copy of its \
             work, so there is nothing to deliver."
        )
    );
    // The cancel is the reason, whatever the count of lost sessions says.
    assert!(
        lost(cancelled, "copy", 3)
            .sentence()
            .contains("You had stopped the run at 2026-11-16 07:05 UTC")
    );
    // The past tense of an event that starts no session is the same sentence.
    assert_eq!(
        lost(cancelled, "copy", 1).sentence_started(),
        lost(cancelled, "copy", 1).sentence()
    );
    assert_eq!(
        lost(cancelled, "copy", 1).card_row(),
        "Lost      session 1: last heartbeat 2026-11-16 07:05 UTC, called lost 07:15 UTC; you had stopped the run, so no new session \
         started and the run ended with the last saved copy"
    );
    assert_eq!(
        lost(cancelled, "nothing", 1).card_row(),
        "Lost      session 1: last heartbeat 2026-11-16 07:05 UTC, called lost 07:15 UTC; you had stopped the run, so no new session \
         started and the run ended"
    );
    // A time that is no UTC time is dropped, and the sentence is the one of a run that ends for another reason.
    assert_eq!(lost(Some("soon"), "copy", 1).cancelled_at, None);

    // Without the member: the sentences of release 5.22, byte for byte.
    assert_eq!(
        lost(None, "copy", 1).sentence(),
        format!(
            "{head} No new session starts, so the run ends and the service delivers the last saved copy."
        )
    );
    assert_eq!(
        lost(None, "nothing", 3).sentence(),
        format!(
            "{head} It was lost session 3 in a row, so the run ends. The run saved no copy of its work, so there is nothing to deliver."
        )
    );
    assert_eq!(
        lost(None, "copy", 1).card_row(),
        "Lost      session 1: last heartbeat 2026-11-16 07:05 UTC, called lost 07:15 UTC; no new session started, so the run ended"
    );
    assert_eq!(
        lost(None, "copy", 3).card_row(),
        "Lost      session 1: last heartbeat 2026-11-16 07:05 UTC, called lost 07:15 UTC; it was lost session 3 in a row, so the run \
         ended"
    );
}

/// The one line of `ham watch` and the notice of the terminal app after a cancel made with its keys end with the command that
/// downloads the newest saved copy of the work, in every form (root invariant 9).
#[test]
fn b853_cancel_notice_names_the_copy_command() {
    let copy = format!(
        "ham pull {RUN} --snapshot downloads the newest saved copy of the work at any time, if the run saved one."
    );
    assert_eq!(CancelNote::copy_line(RUN), copy);
    for payload in [live(), none_answers(), delivering()] {
        let n = note(payload);
        assert_eq!(n.notice_line(RUN), format!("{} {copy}", n.watch_line()));
    }
    // The run's name is cleaned as in the answer of `ham cancel`.
    assert!(
        !note(live())
            .notice_line("01M7\u{1b}[31mX")
            .contains('\u{1b}')
    );
}

/// The conversation of the run screen says the recorded cancel as a notice of the harness, whole and wrapped, and the notice ends with
/// the command that downloads the newest saved copy of the work (root invariant 9). The notice row under the message box is one row
/// and cuts a long line, so the conversation is where a person on the run screen reads the command.
#[test]
fn b853_the_conversation_says_the_recorded_cancel_with_the_copy_command() {
    use ham_core::chat::{BlockKind, ChatInputs, ChatModel};
    for payload in [live(), none_answers(), delivering()] {
        let mut events = working();
        events.push(waiting(4, "2026-11-16T07:05:22Z", payload.clone()));
        let chat = ChatModel::from_progress_feed(&state_of(events), &ChatInputs::default());
        let notice = chat
            .blocks
            .iter()
            .find(|b| b.body.starts_with("Cancel recorded at"))
            .expect("a notice for the recorded cancel");
        assert_eq!(notice.body, note(payload).notice_line(RUN));
        assert!(
            notice.body.ends_with(&format!(
                "ham pull {RUN} --snapshot downloads the newest saved copy of the work at any time, if the run saved one."
            )),
            "{}",
            notice.body
        );
        assert!(matches!(notice.kind, BlockKind::Notice(_)));
        assert_eq!(notice.origin, ham_core::chat::Origin::Harness);
    }
    // TRIGGER PRESENT: a run without the event has no such notice.
    let chat = ChatModel::from_progress_feed(&state_of(working()), &ChatInputs::default());
    assert!(
        !chat
            .blocks
            .iter()
            .any(|b| b.body.contains("Cancel recorded"))
    );
}
