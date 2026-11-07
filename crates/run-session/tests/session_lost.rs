//! A session the cloud lost: the files stay on managed storage, Cline's conversation is gone, and the
//! `StartSession` task misses its heartbeat after 600 s. Step Functions reports that as `States.Timeout`, with no cause, the same
//! error name as the task's own limit of 28,500 s. `plan-delivery` used to read the name as the time limit, so the user saw "the run
//! reached its time limit" after eleven minutes. These tests tell the two apart by the age of the session, say what was delivered
//! when only a snapshot exists, and keep the real time limit as it was.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::direct::{DirectOutcome, HeadSource};
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use run_session::plan_delivery::{SESSION_HEARTBEAT_S, SESSION_TASK_TIMEOUT_S};
use serde_json::{Value, json};
use support::fakes::*;

async fn call(mem: &Mem, v: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(v).unwrap();
    handle(&mem.deps(), &op).await
}

async fn plan(mem: &Mem, session: Value) -> Value {
    call(
        mem,
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged(), "session": session}),
    )
    .await
    .unwrap()
}

/// A run whose session started `age_s` seconds before the plan is made (the fake clock stands at `T0`), as `start-session` records it.
async fn started_aged(age_s: u64, target: harness_protocol::target::RunTarget) -> Mem {
    let mem = Mem::default();
    mem.seed(target, "Add percent_change(a, b) with tests", MODEL);
    call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
    mem.0
        .turns
        .lock()
        .unwrap()
        .get_mut(&("direct".to_string(), 1))
        .unwrap()
        .started_at_ms = Some(T0 - age_s * 1000);
    mem.clear_events();
    mem
}

/// What the `StartSession` Catch writes for a missed heartbeat on the live stage: the error name and no cause.
fn missed_heartbeat() -> Value {
    json!({"turn": {}, "failure": {"Error": "States.Timeout"}})
}

const LOST_LABEL: &str = "Cline stopped: the session was lost";
const TIME_LIMIT_LABEL: &str = "Cline stopped: the run reached its time limit";

#[test]
fn the_constants_are_the_ones_of_the_state_machine() {
    // statemachines/direct.asl.json: the `StartSession` task. A change there moves the line between a lost session and the time limit.
    let asl: Value =
        serde_json::from_str(include_str!("../../../statemachines/direct.asl.json")).unwrap();
    let task = &asl["States"]["StartSession"];
    assert_eq!(task["TimeoutSeconds"], json!(SESSION_TASK_TIMEOUT_S));
    assert_eq!(task["HeartbeatSeconds"], json!(SESSION_HEARTBEAT_S));
    assert_eq!(task["Catch"][0]["ResultPath"], "$.session.failure");
    assert_eq!(task["Catch"][0]["Next"], "NextSlice");
    assert_eq!(SESSION_TASK_TIMEOUT_S, 28_500);
    assert_eq!(SESSION_HEARTBEAT_S, 600);
}

#[tokio::test]
async fn a_missed_heartbeat_that_step_functions_names_states_timeout_is_a_lost_session() {
    // The live row: 662 s after the task started.
    let mem = started_aged(662, repo_target()).await;
    let p = plan(&mem, missed_heartbeat()).await;
    assert_eq!(p["deliver"], false);
    assert_eq!(p["outcome"], "stopped_session_lost", "{p}");
    assert_eq!(p["label"], LOST_LABEL);
    assert_eq!(p["detail"], "no heartbeat for 600 s");
    assert_eq!(p["reason"], "the session ended before any work was saved");
}

#[tokio::test]
async fn the_blocked_run_says_the_session_was_lost_in_its_reason_and_its_completed_event() {
    let mem = started_aged(662, repo_target()).await;
    let p = plan(&mem, missed_heartbeat()).await;
    let out = call(
        &mem,
        json!({"op": "mark-blocked", "runId": RUN,
            "state": {"plan": {"result": p}, "session": missed_heartbeat()}}),
    )
    .await
    .unwrap();
    assert_eq!(out["status"], "blocked");
    assert_eq!(
        out["reason"],
        "the session ended before any work was saved (no heartbeat for 600 s)"
    );
    let completed = mem
        .events()
        .into_iter()
        .find(|(t, _)| t == "run.completed")
        .unwrap()
        .1;
    assert_eq!(completed["outcome"], "stopped_session_lost", "{completed}");
    assert_eq!(completed["label"], LOST_LABEL);
    assert_eq!(completed["detail"], "no heartbeat for 600 s");
}

#[tokio::test]
async fn a_state_timeout_at_the_limit_of_the_task_is_still_the_time_limit() {
    // The task limit, less the slack for the clock between the state's entry and the Turn item (60 s).
    for (age_s, want, label) in [
        (SESSION_TASK_TIMEOUT_S, "stopped_timeout", TIME_LIMIT_LABEL),
        (
            SESSION_TASK_TIMEOUT_S - 60,
            "stopped_timeout",
            TIME_LIMIT_LABEL,
        ),
        (
            SESSION_TASK_TIMEOUT_S - 61,
            "stopped_session_lost",
            LOST_LABEL,
        ),
    ] {
        let mem = started_aged(age_s, repo_target()).await;
        let p = plan(&mem, missed_heartbeat()).await;
        assert_eq!(p["outcome"], want, "age {age_s} s: {p}");
        assert_eq!(p["label"], label, "age {age_s} s");
    }
}

#[tokio::test]
async fn a_state_timeout_with_no_start_time_keeps_the_time_limit_outcome() {
    // No Turn item (the task never recorded a start) or one without `started_at_ms`: the age is unknown and nothing says the
    // heartbeat was the cause, so the name keeps its old meaning.
    let mem = Mem::default();
    mem.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
    let p = plan(&mem, missed_heartbeat()).await;
    assert_eq!(p["outcome"], "stopped_timeout", "{p}");
    let mem = started_aged(100, repo_target()).await;
    mem.0
        .turns
        .lock()
        .unwrap()
        .get_mut(&("direct".to_string(), 1))
        .unwrap()
        .started_at_ms = None;
    let p = plan(&mem, missed_heartbeat()).await;
    assert_eq!(p["outcome"], "stopped_timeout", "{p}");
}

#[tokio::test]
async fn the_other_names_keep_their_outcomes_whatever_the_age() {
    for (error, want) in [
        ("States.HeartbeatTimeout", "stopped_session_lost"),
        ("States.TaskFailed", "stopped_session_lost"),
        ("RunSessionInvalid", "stopped_session_lost"),
    ] {
        for age_s in [10, SESSION_TASK_TIMEOUT_S + 5] {
            let mem = started_aged(age_s, repo_target()).await;
            let p = plan(
                &mem,
                json!({"turn": {}, "failure": {"Error": error, "Cause": "c"}}),
            )
            .await;
            assert_eq!(p["outcome"], want, "{error} at {age_s} s");
        }
    }
}

// ---- the work that is delivered when the session is gone -----------------------------------------------------------------

const SNAPSHOT_SHA: &str = "3333333333333333333333333333333333333333";
const SNAPSHOT_SENTENCE: &str =
    "The work is the last saved snapshot, taken at 2026-10-13 18:50 UTC.";

#[tokio::test]
async fn a_lost_session_with_a_snapshot_delivers_it_and_says_what_it_is() {
    let mem = started_aged(662, repo_target()).await;
    mem.put_snapshot(SNAPSHOT_SHA);
    let p = plan(&mem, missed_heartbeat()).await;
    assert_eq!(p["deliver"], true, "{p}");
    assert_eq!(p["headSha"], SNAPSHOT_SHA);
    assert_eq!(p["headSource"], "snapshot");
    assert_eq!(p["outcome"], "stopped_session_lost");
    assert_eq!(
        p["detail"],
        format!("no heartbeat for 600 s. {SNAPSHOT_SENTENCE}")
    );
    // The zip's EVIDENCE.md prints the same detail, from the package request.
    assert_eq!(p["package"]["direct"]["detail"], p["detail"]);
    assert_eq!(p["package"]["direct"]["headSource"], "snapshot");
    // The pull request says it is a snapshot, in words, and keeps the sentence that the harness checked nothing.
    let description = p["pushPr"]["description"].as_str().unwrap();
    assert!(
        description.contains("This pull request holds the last saved snapshot of the work, taken at 2026-10-13 18:50 UTC, not Cline's final result."),
        "{description}"
    );
    assert!(description.ends_with("The zip carries what Cline reported (EVIDENCE.md)."));
    // And the decision is stored once with the time, so a retry repeats it.
    let stored: Value = serde_json::from_slice(
        &mem.blob(&harness_protocol::direct::s3::evidence_key(RUN))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(stored["snapshotAt"], "2026-10-13T18:50:00Z");
    let again = plan(&mem, missed_heartbeat()).await;
    assert_eq!(again["detail"], p["detail"]);
    assert_eq!(again["pushPr"]["description"], p["pushPr"]["description"]);
}

#[tokio::test]
async fn an_s3_only_run_gets_the_sentence_in_its_detail_and_no_pull_request() {
    let mem = started_aged(662, harness_protocol::target::RunTarget::S3Only).await;
    mem.put_snapshot(SNAPSHOT_SHA);
    let p = plan(&mem, missed_heartbeat()).await;
    assert_eq!(p["deliver"], true, "{p}");
    assert!(p.get("pushPr").is_none());
    assert!(
        p["detail"].as_str().unwrap().ends_with(SNAPSHOT_SENTENCE),
        "{p}"
    );
}

#[tokio::test]
async fn a_final_bundle_needs_no_snapshot_sentence() {
    let mem = started_aged(100, repo_target()).await;
    mem.put_bundle();
    mem.put_snapshot(SNAPSHOT_SHA);
    mem.put_summary(&summary(
        DirectOutcome::ReportedDone,
        Some("Added pct."),
        true,
    ));
    let p = plan(
        &mem,
        json!({"turn": support::fakes::turn_output(true), "failure": {}}),
    )
    .await;
    assert_eq!(p["headSource"], "final");
    assert!(p.get("detail").is_none() || p["detail"].is_null(), "{p}");
    let description = p["pushPr"]["description"].as_str().unwrap();
    assert!(!description.contains("snapshot"), "{description}");
}

#[tokio::test]
async fn agentds_own_detail_comes_first_and_the_sentence_survives_a_long_one() {
    // A summary exists (agentd stopped by its own deadline) but its final bundle never arrived: the snapshot is delivered.
    let mem = started_aged(100, repo_target()).await;
    mem.put_snapshot(SNAPSHOT_SHA);
    let mut s = summary(DirectOutcome::StoppedTimeout, Some("m"), false);
    s.detail = Some("x".repeat(harness_protocol::direct::MAX_DIRECT_DETAIL_CHARS));
    mem.put_summary(&s);
    let p = plan(&mem, json!({"turn": {}, "failure": {}})).await;
    assert_eq!(p["headSource"], "snapshot");
    assert_eq!(p["outcome"], "stopped_timeout");
    let detail = p["detail"].as_str().unwrap();
    assert!(
        detail.chars().count() <= harness_protocol::direct::MAX_DIRECT_DETAIL_CHARS,
        "{} chars",
        detail.chars().count()
    );
    assert!(detail.ends_with(SNAPSHOT_SENTENCE), "{detail}");
    assert!(detail.starts_with("xxxx"), "{detail}");
    assert_eq!(
        serde_json::from_value::<HeadSource>(p["headSource"].clone()).unwrap(),
        HeadSource::Snapshot
    );
}

#[tokio::test]
async fn a_snapshot_with_no_readable_time_is_still_delivered_with_a_shorter_sentence() {
    let mem = started_aged(662, repo_target()).await;
    mem.put_blob(
        &harness_protocol::direct::s3::snapshot_bundle_key(RUN),
        b"SNAP".to_vec(),
    );
    mem.put_blob(
        &harness_protocol::direct::s3::snapshot_json_key(RUN),
        serde_json::to_vec(&json!({"schemaVersion": 1, "headSha": SNAPSHOT_SHA, "baseSha": BASE, "at": "not a time", "count": 5}))
            .unwrap(),
    );
    let p = plan(&mem, missed_heartbeat()).await;
    assert_eq!(p["deliver"], true, "{p}");
    assert_eq!(
        p["detail"],
        "no heartbeat for 600 s. The work is the last saved snapshot."
    );
}
