//! When `next-slice` decides that a session was lost, the service says so in the run's events: one `run.waiting` event (harness
//! authority, kind `session-lost`) that names the session, why it was lost, the lost sessions in a row and what follows: the session that
//! starts and what it starts from, or the end of the run. The event is written from the records of the decision, so asking again writes
//! the same event and no second one. The client reads the payload with `ham_core::lost_session::SessionLost`.
#![allow(clippy::unwrap_used)]

mod support;

use ham_core::lost_session::{
    CAUSE_NO_HEARTBEAT, CAUSE_NO_RESULT, FROM_COPY, FROM_START, KIND, SessionLost, THEN_END,
    THEN_RESTART,
};
use harness_protocol::direct::{MAX_LOST_SLICES, SESSION_LOST_AFTER_S};
use harness_protocol::envelope::{Authority, EventType};
use harness_protocol::goal::GoalStatus;
use harness_protocol::target::RunTarget;
use run_session::ops::{Op, SessionState, StatesFailure, handle};
use run_session::slices::LoopState;
use serde_json::{Value, json};
use support::fakes::*;

const HEAD: &str = "3333333333333333333333333333333333333333";

async fn call(mem: &Mem, v: Value) -> Value {
    let op: Op = serde_json::from_value(v).unwrap();
    handle(&mem.deps(), &op).await.unwrap()
}

/// A run whose first session started at `T0`.
async fn started() -> Mem {
    let mem = Mem::default();
    mem.set_now_ms(T0);
    mem.seed_with_members(
        RunTarget::S3Only,
        "Add percent_change(a, b) with tests",
        MODEL,
        json!({"maxRunS": 8_000}),
    );
    call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await;
    mem.clear_events();
    mem
}

fn missed_heartbeat() -> SessionState {
    SessionState {
        turn: None,
        failure: Some(StatesFailure {
            error: Some("States.Timeout".into()),
            cause: None,
        }),
    }
}

async fn next_slice(mem: &Mem, session: SessionState, state: LoopState) -> Value {
    handle(
        &mem.deps(),
        &Op::NextSlice {
            run_id: RUN.to_string(),
            staged: staged(),
            session,
            loop_state: state,
        },
    )
    .await
    .unwrap()
}

fn lost_events(mem: &Mem) -> Vec<SessionLost> {
    mem.envelopes()
        .into_iter()
        .filter(|e| e.event_type == EventType::RunWaiting)
        .map(|e| {
            assert_eq!(e.authority, Authority::Harness);
            assert!(e.task_id.is_none() && e.agent_id.is_none(), "{e:?}");
            SessionLost::parse(&e).expect("the client reads the payload")
        })
        .collect()
}

#[tokio::test]
async fn b842_a_session_lost_with_a_copy_is_announced_with_the_session_that_starts_and_the_copy_it_starts_from()
 {
    let mem = started().await;
    // The worker's last heartbeat, as worker-api recorded it on the Agent item.
    mem.0
        .agents
        .lock()
        .unwrap()
        .get_mut("direct")
        .unwrap()
        .heartbeat_at = Some("2027-09-21T15:34:10Z".into());
    mem.put_snapshot_t(1, HEAD);
    // The state machine reports the missed heartbeat 700 s after the session started.
    mem.set_now_ms(T0 + 700_000);
    let out = next_slice(&mem, missed_heartbeat(), LoopState::default()).await;
    assert_eq!(out["continue"], json!(true));
    let events = lost_events(&mem);
    assert_eq!(events.len(), 1, "{events:?}");
    let e = &events[0];
    assert_eq!(e.kind, KIND);
    assert_eq!(e.session, 1);
    assert_eq!(e.cause, CAUSE_NO_HEARTBEAT);
    assert_eq!(e.silence_s, SESSION_LOST_AFTER_S);
    assert_eq!((e.lost_in_a_row, e.limit), (1, MAX_LOST_SLICES));
    assert_eq!(e.then, THEN_RESTART);
    assert_eq!(e.next_session, Some(2));
    assert_eq!(e.from.as_deref(), Some(FROM_COPY));
    assert_eq!(
        e.saved_at.as_deref(),
        Some("2026-10-13T18:50:00Z"),
        "the time the copy's record names"
    );
    assert_eq!(e.last_heartbeat_at.as_deref(), Some("2027-09-21T15:34:10Z"));
    assert_eq!(
        e.decided_at,
        dispatcher::items::rfc3339((T0 + 700_000) / 1000)
    );
    // The run goes on: the event changes no status.
    assert_eq!(mem.run().status, GoalStatus::Active);

    // Asked again (a retry of the state), later and with nothing in the session state: the same event, and no second one.
    let first = mem.envelopes();
    mem.set_now_ms(T0 + 900_000);
    let again = next_slice(&mem, SessionState::default(), LoopState::default()).await;
    assert_eq!(again, out);
    assert_eq!(mem.envelopes(), first);
}

#[tokio::test]
async fn b842_a_session_lost_before_any_copy_is_announced_as_a_start_from_the_runs_first_files() {
    let mem = started().await;
    mem.set_now_ms(T0 + 700_000);
    next_slice(&mem, missed_heartbeat(), LoopState::default()).await;
    let events = lost_events(&mem);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].then, THEN_RESTART);
    assert_eq!(events[0].from.as_deref(), Some(FROM_START));
    assert_eq!(events[0].saved_at, None);
    assert_eq!(events[0].last_heartbeat_at, None);
}

#[tokio::test]
async fn b842_a_session_that_ended_without_a_result_for_another_reason_is_not_called_silent() {
    let mem = started().await;
    mem.put_snapshot_t(1, HEAD);
    mem.set_now_ms(T0 + 30_000);
    let failed = SessionState {
        turn: None,
        failure: Some(StatesFailure {
            error: Some("RunSessionUnavailable".into()),
            cause: Some("the session did not accept turn 1".into()),
        }),
    };
    next_slice(&mem, failed, LoopState::default()).await;
    let events = lost_events(&mem);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].cause, CAUSE_NO_RESULT);
}

#[tokio::test]
async fn b842_the_lost_session_that_reaches_the_limit_is_announced_as_the_end_of_the_run() {
    let mem = started().await;
    mem.put_snapshot_t(1, HEAD);
    mem.set_now_ms(T0 + 700_000);
    // Two sessions in a row were lost before this one.
    let prev = LoopState {
        slice: 1,
        lost: MAX_LOST_SLICES - 1,
        ..LoopState::default()
    };
    let out = next_slice(&mem, missed_heartbeat(), prev).await;
    assert_eq!(out["continue"], json!(false));
    let events = lost_events(&mem);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].then, THEN_END);
    assert_eq!(events[0].next_session, None);
    assert_eq!(events[0].lost_in_a_row, MAX_LOST_SLICES);
    assert!(
        events[0].sentence().ends_with(
            "It was lost session 3 in a row, so the run ends and the service delivers the last saved copy."
        ),
        "{}",
        events[0].sentence()
    );
}

#[tokio::test]
async fn b842_a_session_that_ended_with_a_result_and_a_run_someone_ended_are_not_announced() {
    // A session that handed over at its deadline is no lost session.
    let mem = started().await;
    mem.put_bundle_t(1);
    mem.set_now_ms(T0 + 700_000);
    let turn = json!({
        "summary": "Worked.", "runId": RUN, "agentId": "direct", "turn": 1,
        "finishReason": "stopped_timeout", "outcome": "completed", "handoff": true,
        "bundleUri": format!("s3://{BUCKET}/{}", harness_protocol::direct::s3::bundle_key_t(RUN, 1)),
        "lastCommitSha": HEAD,
    });
    next_slice(
        &mem,
        SessionState {
            turn: Some(turn),
            failure: None,
        },
        LoopState::default(),
    )
    .await;
    assert!(lost_events(&mem).is_empty());

    // A run that was discarded while its session ran: the loop ends, and there is nothing to tell.
    let mem = started().await;
    mem.set_run_status_raw(GoalStatus::Cleared);
    mem.set_now_ms(T0 + 700_000);
    next_slice(&mem, missed_heartbeat(), LoopState::default()).await;
    assert!(lost_events(&mem).is_empty());
}

#[tokio::test]
async fn b842_the_end_after_lost_sessions_says_whether_there_is_a_copy_to_deliver() {
    let prev = || LoopState {
        slice: 1,
        lost: MAX_LOST_SLICES - 1,
        ..LoopState::default()
    };
    let word = |mem: &Mem| {
        mem.events()
            .into_iter()
            .find(|(t, _)| t == "run.waiting")
            .map(|(_, v)| v)
            .expect("the service's word")
    };
    // The session that reached the limit had saved a copy: that is what the run delivers.
    let mem = started().await;
    mem.put_snapshot_t(1, HEAD);
    mem.set_now_ms(T0 + 700_000);
    next_slice(&mem, missed_heartbeat(), prev()).await;
    assert_eq!(word(&mem)["delivers"], json!("copy"), "{}", word(&mem));
    // No session of the run saved a copy: nothing is delivered, and the word does not promise a delivery.
    let mem = started().await;
    mem.set_now_ms(T0 + 700_000);
    next_slice(&mem, missed_heartbeat(), prev()).await;
    assert_eq!(word(&mem)["delivers"], json!("nothing"), "{}", word(&mem));
    let events = lost_events(&mem);
    assert!(
        events[0].sentence().ends_with(
            "It was lost session 3 in a row, so the run ends. The run saved no copy of its work, so there is nothing to deliver."
        ),
        "{}",
        events[0].sentence()
    );
    // A restart delivers nothing yet: the member is absent.
    let mem = started().await;
    mem.put_snapshot_t(1, HEAD);
    mem.set_now_ms(T0 + 700_000);
    next_slice(&mem, missed_heartbeat(), LoopState::default()).await;
    assert!(word(&mem).get("delivers").is_none(), "{}", word(&mem));
}

/// The word about a lost session never changes what the run does. The decision and the plan of the next session are stored before the
/// event is written, so an event that cannot be written leaves the decision as it is, and asking again writes the event.
#[tokio::test]
async fn b842_a_lost_session_whose_word_cannot_be_written_still_gets_the_decision_that_was_stored()
{
    use run_session::error::RunSessionError;
    let mem = started().await;
    mem.put_snapshot_t(1, HEAD);
    mem.set_now_ms(T0 + 700_000);
    // An error the state machine does not retry: its Catch would end the run where the service had decided to start session 2.
    mem.fail(
        "put_event",
        RunSessionError::Invalid("the event was refused".into()),
    );
    let out = handle(
        &mem.deps(),
        &Op::NextSlice {
            run_id: RUN.to_string(),
            staged: staged(),
            session: missed_heartbeat(),
            loop_state: LoopState::default(),
        },
    )
    .await
    .expect("the stored decision is returned although its word could not be written");
    assert_eq!(out["continue"], json!(true), "session 2 starts: {out}");
    assert!(
        lost_events(&mem).is_empty(),
        "TRIGGER PRESENT: the event write failed"
    );
    // Asked again: the same decision, and its word is written now.
    let again = next_slice(&mem, SessionState::default(), LoopState::default()).await;
    assert_eq!(again, out);
    assert_eq!(lost_events(&mem).len(), 1);
}
