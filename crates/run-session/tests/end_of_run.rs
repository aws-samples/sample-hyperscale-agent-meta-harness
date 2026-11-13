//! How a run ends (review of the run machine, S2, S4 and the survivors of S5): a session that is still running is
//! stopped before it is planned, a stop that fails fails the op, the blocked events of a run with no plan are dated by the
//! clock of the first attempt, a retried plan repeats its first decision, and the lost races of the two status writes are
//! walked.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::direct::{DirectOutcome, s3};
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::AgentStatus;
use harness_protocol::target::RunTarget;
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use run_session::run_ended::{parse_rfc3339_ms, parse_run_end, reap};
use serde_json::{Value, json};
use support::fakes::*;

async fn call(mem: &Mem, v: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(v).unwrap();
    handle(&mem.deps(), &op).await
}

async fn plan_with(mem: &Mem, session: Value) -> Result<Value, RunSessionError> {
    call(
        mem,
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged(), "session": session}),
    )
    .await
}

async fn blocked(mem: &Mem, state: Value) -> Result<Value, RunSessionError> {
    call(
        mem,
        json!({"op": "mark-blocked", "runId": RUN, "state": state}),
    )
    .await
}

async fn finalize(mem: &Mem, plan: &Value) -> Result<Value, RunSessionError> {
    call(
        mem,
        json!({"op": "finalize", "runId": RUN, "plan": plan, "validated": {"pass": true},
            "packaged": {"artifactId": "zip-0123456789abcdef0123", "sha256": "ab".repeat(32), "sizeBytes": 4096},
            "prLink": {}, "prFailure": {}}),
    )
    .await
}

fn done_session() -> Value {
    json!({"turn": turn_output(true), "failure": {}})
}

fn lost_session() -> Value {
    json!({"turn": {}, "failure": {"Error": "States.HeartbeatTimeout", "Cause": "heartbeat"}})
}

/// A run whose session started (so it has an Agent item) and wrote its final bundle and summary.
async fn started(target: RunTarget) -> Mem {
    let mem = Mem::default();
    mem.seed(target, "Add percent_change(a, b) with tests", MODEL);
    mem.put_bundle();
    mem.put_summary(&summary(
        DirectOutcome::ReportedDone,
        Some("Added pct."),
        true,
    ));
    call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
    mem.clear_events();
    mem
}

const CREATED: &str = "2026-10-13T18:00:00Z";

fn created_ms() -> u64 {
    parse_rfc3339_ms(CREATED).unwrap()
}

// ---- S2: stopping the session -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_session_that_lost_its_heartbeat_is_stopped_before_anything_is_read_or_planned() {
    let mem = started(repo_target()).await;
    mem.put_snapshot(HEAD);
    let p = plan_with(&mem, lost_session()).await.unwrap();
    assert_eq!(p["deliver"], true);
    assert_eq!(mem.stops().len(), 1, "stopped by plan-delivery itself");
    let agent = mem.agent().unwrap();
    assert_eq!(agent.status, AgentStatus::Stopped);
    assert!(
        agent.creds.unwrap().revoked,
        "no credential refresh after the stop"
    );
}

#[tokio::test]
async fn plan_delivery_does_not_stop_a_session_that_finished_by_itself() {
    let mem = started(repo_target()).await;
    plan_with(&mem, done_session()).await.unwrap();
    assert!(mem.stops().is_empty());
    assert_eq!(mem.agent().unwrap().status, AgentStatus::Running);
}

#[tokio::test]
async fn a_stop_that_fails_before_the_plan_fails_the_op_and_plans_nothing() {
    let mem = started(repo_target()).await;
    mem.put_snapshot(HEAD);
    mem.fail(
        "stop",
        RunSessionError::Unavailable("AgentCore down".into()),
    );
    let e = plan_with(&mem, lost_session()).await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionUnavailable");
    assert!(
        mem.blob(&s3::evidence_key(RUN)).is_none(),
        "nothing was decided"
    );
    assert!(mem.events().is_empty());
    // the retry stops and plans
    let p = plan_with(&mem, lost_session()).await.unwrap();
    assert_eq!(p["deliver"], true);
    assert_eq!(mem.stops().len(), 1);
}

#[tokio::test]
async fn a_session_that_is_already_gone_is_fine() {
    let mem = started(repo_target()).await;
    mem.put_snapshot(HEAD);
    *mem.0.stop_gone.lock().unwrap() = true;
    assert_eq!(
        plan_with(&mem, lost_session()).await.unwrap()["deliver"],
        true
    );
}

#[tokio::test]
async fn a_run_with_no_agent_has_no_session_to_stop() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    let out = blocked(
        &mem,
        json!({"failure": {"Error": "RunSessionInvalid", "Cause": "x"}}),
    )
    .await
    .unwrap();
    assert_eq!(out["status"], "blocked");
    assert!(mem.stops().is_empty());
}

#[tokio::test]
async fn finalize_fails_when_the_stop_fails_and_the_retry_stops_the_run_it_already_completed() {
    let mem = started(repo_target()).await;
    let p = plan_with(&mem, done_session()).await.unwrap();
    mem.fail(
        "stop",
        RunSessionError::Unavailable("AgentCore down".into()),
    );
    let e = finalize(&mem, &p).await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionUnavailable");
    assert_eq!(
        mem.run().status,
        GoalStatus::Complete,
        "the delivery stands"
    );
    assert!(mem.stops().is_empty());
    finalize(&mem, &p).await.unwrap();
    assert_eq!(mem.stops().len(), 1);
    assert_eq!(mem.agent().unwrap().status, AgentStatus::Stopped);
    assert!(mem.agent().unwrap().creds.unwrap().revoked);
}

// ---- messages: the run takes no more once it is ending --------------------------------------------------

#[tokio::test]
async fn finalize_closes_the_run_to_messages_before_it_stops_the_session() {
    let mem = started(repo_target()).await;
    let p = plan_with(&mem, done_session()).await.unwrap();
    finalize(&mem, &p).await.unwrap();
    assert_eq!(
        mem.message_closes(),
        vec![(RUN.to_string(), 0)],
        "closed once, with no session stopped yet"
    );
    assert_eq!(mem.stops().len(), 1);
}

#[tokio::test]
async fn a_retried_finalize_closes_again_and_a_failed_close_fails_the_op_before_anything_is_stopped()
 {
    let mem = started(repo_target()).await;
    let p = plan_with(&mem, done_session()).await.unwrap();
    mem.fail(
        "close_messages",
        RunSessionError::Unavailable("table throttled".into()),
    );
    let e = finalize(&mem, &p).await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionUnavailable");
    assert!(
        mem.stops().is_empty(),
        "the session keeps running until the close succeeds"
    );
    finalize(&mem, &p).await.unwrap();
    assert_eq!(mem.message_closes().len(), 1);
    assert_eq!(mem.stops().len(), 1);
}

#[tokio::test]
async fn a_blocked_run_and_an_aborted_execution_close_the_run_to_messages_too() {
    let mem = started(repo_target()).await;
    blocked(&mem, json!({"error": "States.TaskFailed", "cause": "x"}))
        .await
        .unwrap();
    assert_eq!(mem.message_closes().len(), 1);
    let mem = started(repo_target()).await;
    let end = parse_run_end(
        &json!({"source": "aws.states", "detail-type": "Step Functions Execution Status Change",
            "detail": {"status": "ABORTED", "name": RUN, "stateMachineArn": MACHINE, "executionArn": "x"}}),
        MACHINE,
    )
    .unwrap();
    reap(&mem.deps(), &end).await.unwrap();
    assert_eq!(mem.message_closes().len(), 1);
}

#[tokio::test]
async fn a_discarded_run_whose_stop_fails_fails_the_op_too() {
    let mem = started(repo_target()).await;
    let p = plan_with(&mem, done_session()).await.unwrap();
    mem.set_run_status_raw(GoalStatus::Cleared);
    mem.fail(
        "stop",
        RunSessionError::Unavailable("AgentCore down".into()),
    );
    let e = finalize(&mem, &p).await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionUnavailable");
    assert_eq!(mem.run().status, GoalStatus::Cleared);
    let mem = started(repo_target()).await;
    mem.set_run_status_raw(GoalStatus::Cleared);
    mem.fail(
        "stop",
        RunSessionError::Unavailable("AgentCore down".into()),
    );
    let e = blocked(&mem, json!({})).await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionUnavailable");
}

#[tokio::test]
async fn mark_blocked_stops_the_session_of_a_run_that_is_already_complete() {
    let mem = started(repo_target()).await;
    let p = plan_with(&mem, done_session()).await.unwrap();
    // finalize completed the run but its stop failed; the machine then catches into Blocked
    mem.fail(
        "stop",
        RunSessionError::Unavailable("AgentCore down".into()),
    );
    finalize(&mem, &p).await.unwrap_err();
    assert!(mem.stops().is_empty());
    let events = mem.events();
    let out = blocked(&mem, json!({"failure": {"Error": "X", "Cause": "late"}}))
        .await
        .unwrap();
    assert_eq!(out["status"], "complete");
    assert_eq!(mem.events(), events, "the finished run is not touched");
    assert_eq!(mem.stops().len(), 1, "but its session is stopped");
}

#[tokio::test]
async fn mark_blocked_fails_when_the_stop_fails_and_the_retry_changes_nothing_else() {
    let mem = started(repo_target()).await;
    mem.fail(
        "stop",
        RunSessionError::Unavailable("AgentCore down".into()),
    );
    let state = json!({"failure": {"Error": "Invalid", "Cause": "package refused"}});
    let e = blocked(&mem, state.clone()).await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionUnavailable");
    assert_eq!(mem.run().status, GoalStatus::Blocked);
    let events = mem.events();
    blocked(&mem, state).await.unwrap();
    assert_eq!(mem.events(), events);
    assert_eq!(mem.stops().len(), 1);
}

#[tokio::test]
async fn an_aborted_execution_marks_the_agent_stopped_and_revoked() {
    let mem = started(repo_target()).await;
    let end = parse_run_end(
        &json!({"source": "aws.states", "detail-type": "Step Functions Execution Status Change",
            "detail": {"status": "ABORTED", "name": RUN, "stateMachineArn": MACHINE, "executionArn": "x"}}),
        MACHINE,
    )
    .unwrap();
    reap(&mem.deps(), &end).await.unwrap();
    let agent = mem.agent().unwrap();
    assert_eq!(agent.status, AgentStatus::Stopped);
    assert!(agent.creds.unwrap().revoked);
}

// ---- S2: a retried plan repeats its first decision ----------------------------------------------------------------------

#[tokio::test]
async fn a_retried_plan_returns_the_decision_it_stored_even_when_the_bucket_changed() {
    let mem = started(repo_target()).await;
    let first = plan_with(&mem, done_session()).await.unwrap();
    assert_eq!(first["headSource"], "final");
    // between the attempts the session (not yet stopped) wrote a newer snapshot and the final bundle went missing
    let other = "3333333333333333333333333333333333333333";
    mem.0.blobs.lock().unwrap().remove(&s3::bundle_key(RUN));
    mem.put_snapshot(other);
    mem.0.blobs.lock().unwrap().remove(&s3::summary_key(RUN));
    support::fakes::set_clock(T0 + 600_000);
    let second = plan_with(&mem, done_session()).await.unwrap();
    assert_eq!(
        second, first,
        "head, outcome, label, payloads and time are the first plan's"
    );
}

#[tokio::test]
async fn a_retried_plan_that_found_nothing_to_deliver_still_delivers_nothing() {
    let mem = started(repo_target()).await;
    mem.0.blobs.lock().unwrap().remove(&s3::bundle_key(RUN));
    mem.0.blobs.lock().unwrap().remove(&s3::summary_key(RUN));
    let first = plan_with(&mem, lost_session()).await.unwrap();
    assert_eq!(first["deliver"], false);
    mem.put_snapshot(HEAD);
    let second = plan_with(&mem, lost_session()).await.unwrap();
    assert_eq!(second, first);
}

#[tokio::test]
async fn a_plan_that_loses_the_create_only_write_returns_the_winners_decision() {
    // another attempt of the op decided first: its file appears between our read of the key and our write
    let a = started(repo_target()).await;
    let first = plan_with(&a, done_session()).await.unwrap();
    let winner = a.blob(&s3::evidence_key(RUN)).unwrap();
    let b = Mem::default();
    b.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
    b.put_snapshot("3333333333333333333333333333333333333333");
    *b.0.blob_race.lock().unwrap() = Some((s3::evidence_key(RUN), winner));
    let second = plan_with(&b, lost_session()).await.unwrap();
    assert_eq!(
        second["headSha"], HEAD,
        "the winner's head, not the snapshot this attempt saw"
    );
    assert_eq!(second["headSource"], "final");
    assert_eq!(second["outcome"], "reported_done");
    assert_eq!(second["plannedAtMs"], first["plannedAtMs"]);
    assert_eq!(second, first);
}

// ---- S4: the clock of a run that was never planned ----------------------------------------------------------------------

fn event_ms(e: &harness_protocol::envelope::Envelope) -> u64 {
    parse_rfc3339_ms(&e.ts).unwrap()
}

#[tokio::test]
async fn a_blocked_run_with_no_plan_is_dated_when_it_was_blocked_not_when_it_was_created() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    let now = created_ms() + 3 * 3_600_000;
    support::fakes::set_clock(now);
    let state = json!({"failure": {"Error": "States.TaskFailed", "Cause": "planner crashed"}});
    blocked(&mem, state.clone()).await.unwrap();
    let envelopes = mem.envelopes();
    assert_eq!(envelopes.len(), 3);
    for e in &envelopes {
        assert!(
            event_ms(e) >= now / 1000 * 1000,
            "{} {} is dated before the failure",
            e.event_type.as_str(),
            e.ts
        );
    }
    // a client following events by "id greater than the last one seen" sees them: the ids sort after the creation time
    let mut ids: Vec<_> = envelopes.iter().map(|e| e.event_id.clone()).collect();
    ids.sort();
    assert_eq!(
        ids,
        envelopes
            .iter()
            .map(|e| e.event_id.clone())
            .collect::<Vec<_>>()
    );
    // the first attempt's time is kept: a retry an hour later writes the same three events
    let before = mem.events();
    let ids_before: Vec<_> = mem.envelopes().iter().map(|e| e.event_id.clone()).collect();
    support::fakes::set_clock(now + 3_600_000);
    blocked(&mem, state).await.unwrap();
    assert_eq!(mem.events(), before);
    assert_eq!(
        mem.envelopes()
            .iter()
            .map(|e| e.event_id.clone())
            .collect::<Vec<_>>(),
        ids_before
    );
    let marker: Value = serde_json::from_slice(&mem.blob(&s3::evidence_key(RUN)).unwrap()).unwrap();
    assert_eq!(marker["plannedAtMs"], now);
    assert_eq!(marker["blocked"], true);
}

#[tokio::test]
async fn a_plan_that_failed_after_writing_its_evidence_dates_the_blocked_events_by_it() {
    let mem = started(repo_target()).await;
    support::fakes::set_clock(created_ms() + 7_200_000);
    let p = plan_with(&mem, done_session()).await.unwrap();
    let planned = p["plannedAtMs"].as_u64().unwrap();
    mem.clear_events();
    support::fakes::set_clock(planned + 9_000_000);
    // the state holds no plan (plan-delivery failed after it wrote the file)
    blocked(&mem, json!({"failure": {"Error": "X", "Cause": "boom"}}))
        .await
        .unwrap();
    assert_eq!(mem.envelopes().len(), 3);
    for e in mem.envelopes() {
        assert!(
            (event_ms(&e) / 1000).abs_diff(planned / 1000) <= 1,
            "{} is not dated by the plan at {planned}",
            e.ts
        );
    }
}

#[tokio::test]
async fn the_plans_own_time_still_dates_a_run_that_has_a_plan() {
    let mem = started(repo_target()).await;
    let p = plan_with(&mem, done_session()).await.unwrap();
    mem.clear_events();
    support::fakes::set_clock(created_ms() + 86_400_000);
    blocked(
        &mem,
        json!({"plan": {"result": p}, "failure": {"Error": "X", "Cause": "boom"}}),
    )
    .await
    .unwrap();
    let planned = p["plannedAtMs"].as_u64().unwrap();
    assert_eq!(mem.envelopes().len(), 3);
    for e in mem.envelopes() {
        assert!(
            (event_ms(&e) / 1000).abs_diff(planned / 1000) <= 1,
            "{}",
            e.ts
        );
    }
}

#[tokio::test]
async fn the_time_of_a_plan_in_the_state_dates_the_events_even_when_the_clock_and_the_bucket_say_otherwise()
 {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    let planned = created_ms() + 60_000;
    support::fakes::set_clock(created_ms() + 86_400_000);
    blocked(
        &mem,
        json!({"plan": {"result": {"plannedAtMs": planned}}, "failure": {"Error": "X", "Cause": "boom"}}),
    )
    .await
    .unwrap();
    assert_eq!(mem.envelopes().len(), 3);
    for e in mem.envelopes() {
        assert!(
            (event_ms(&e) / 1000).abs_diff(planned / 1000) <= 1,
            "{}",
            e.ts
        );
    }
    assert!(
        mem.blob(&s3::evidence_key(RUN)).is_none(),
        "a run with a plan writes no marker"
    );
}

#[tokio::test]
async fn a_blocked_run_that_loses_the_marker_write_is_dated_by_the_winners_marker() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    let winner = created_ms() + 120_000;
    support::fakes::set_clock(created_ms() + 86_400_000);
    *mem.0.blob_race.lock().unwrap() = Some((
        s3::evidence_key(RUN),
        serde_json::to_vec(
            &json!({"schemaVersion": 1, "runId": RUN, "blocked": true, "plannedAtMs": winner}),
        )
        .unwrap(),
    ));
    blocked(&mem, json!({"failure": {"Error": "X", "Cause": "boom"}}))
        .await
        .unwrap();
    for e in mem.envelopes() {
        assert!(
            (event_ms(&e) / 1000).abs_diff(winner / 1000) <= 1,
            "{}",
            e.ts
        );
    }
}

// ---- S5: the two status writes lose a race -----------------------------------------------------------------------------

#[tokio::test]
async fn finalize_that_loses_the_status_race_to_a_complete_run_carries_on() {
    let mem = started(repo_target()).await;
    let p = plan_with(&mem, done_session()).await.unwrap();
    *mem.0.race_status.lock().unwrap() = Some(GoalStatus::Complete);
    let out = finalize(&mem, &p).await.unwrap();
    assert_eq!(out["status"], "complete");
    assert!(mem.event_types().contains(&"run.completed".to_string()));
    assert_eq!(mem.stops().len(), 1);
}

#[tokio::test]
async fn finalize_that_loses_the_status_race_to_a_paused_run_is_a_conflict_the_machine_retries() {
    let mem = started(repo_target()).await;
    let p = plan_with(&mem, done_session()).await.unwrap();
    *mem.0.race_status.lock().unwrap() = Some(GoalStatus::Paused);
    let e = finalize(&mem, &p).await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionConflict");
    assert!(e.to_string().contains("could not be completed"), "{e}");
    assert!(!mem.event_types().contains(&"run.completed".to_string()));
}

#[tokio::test]
async fn mark_blocked_that_loses_the_status_race_to_a_finished_run_carries_on() {
    for other in [
        GoalStatus::Blocked,
        GoalStatus::Complete,
        GoalStatus::Cleared,
    ] {
        let mem = started(repo_target()).await;
        *mem.0.race_status.lock().unwrap() = Some(other);
        let out = blocked(&mem, json!({"failure": {"Error": "X", "Cause": "boom"}}))
            .await
            .unwrap();
        assert_eq!(out["status"], "blocked", "{other:?}");
        assert_eq!(mem.stops().len(), 1, "{other:?}");
    }
}

#[tokio::test]
async fn mark_blocked_that_loses_the_status_race_to_a_paused_run_is_a_conflict() {
    let mem = started(repo_target()).await;
    *mem.0.race_status.lock().unwrap() = Some(GoalStatus::Paused);
    let e = blocked(&mem, json!({"failure": {"Error": "X", "Cause": "boom"}}))
        .await
        .unwrap_err();
    assert_eq!(e.error_type(), "RunSessionConflict");
    assert!(e.to_string().contains("could not be blocked"), "{e}");
}

#[tokio::test]
async fn the_detail_of_run_completed_says_why_no_pull_request_was_opened() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    mem.put_bundle();
    let mut s = summary(DirectOutcome::ReportedDone, Some("m"), true);
    s.git.head_sha = Some(BASE.into());
    mem.put_summary(&s);
    let mut turn = turn_output(true);
    turn["lastCommitSha"] = json!(BASE);
    call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
    let p = plan_with(&mem, json!({"turn": turn, "failure": {}}))
        .await
        .unwrap();
    assert_eq!(p["noPrReason"], "Cline left the repository unchanged");
    mem.clear_events();
    finalize(&mem, &p).await.unwrap();
    let completed = mem
        .events()
        .into_iter()
        .find(|(t, _)| t == "run.completed")
        .unwrap()
        .1;
    assert_eq!(
        completed["detail"],
        "no pull request: Cline left the repository unchanged"
    );
}

#[tokio::test]
async fn the_observations_of_the_summary_reach_the_plan_and_run_completed() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    mem.put_bundle();
    let mut s = summary(DirectOutcome::ReportedDone, Some("m"), true);
    s.observations = vec![
        harness_protocol::direct::DirectObservation::last_log_entry_red(
            "pytest -q",
            "1",
            Some("2"),
        ),
    ];
    mem.put_summary(&s);
    call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
    let p = plan_with(&mem, json!({"turn": turn_output(true), "failure": {}}))
        .await
        .unwrap();
    assert_eq!(p["observations"][0]["kind"], "last_log_entry_red");
    mem.clear_events();
    finalize(&mem, &p).await.unwrap();
    let completed = mem
        .events()
        .into_iter()
        .find(|(t, _)| t == "run.completed")
        .unwrap()
        .1;
    assert_eq!(completed["label"], "Cline reported done");
    assert_eq!(completed["observations"][0]["command"], "pytest -q");
    assert_eq!(completed["observations"][0]["exit"], "1");
    assert_eq!(completed["observations"][0]["failed"], "2");
}

#[tokio::test]
async fn a_stop_by_the_harness_reads_as_one_in_run_completed() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    mem.put_bundle();
    let mut s = summary(DirectOutcome::StoppedOther, Some("m"), true);
    s.detail = Some(
        "The harness ended the run: 3 rounds in a row changed no file. Its words: \"waiting\""
            .into(),
    );
    s.label = DirectOutcome::StoppedOther.label_for(s.detail.as_deref());
    mem.put_summary(&s);
    call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
    let p = plan_with(&mem, json!({"turn": turn_output(true), "failure": {}}))
        .await
        .unwrap();
    assert_eq!(
        p["label"],
        "Cline stopped: the harness ended the run (3 rounds in a row changed no file)"
    );
    mem.clear_events();
    finalize(&mem, &p).await.unwrap();
    let completed = mem
        .events()
        .into_iter()
        .find(|(t, _)| t == "run.completed")
        .unwrap()
        .1;
    assert_eq!(completed["label"], p["label"]);
}
