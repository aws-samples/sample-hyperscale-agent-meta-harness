//! The ids of the events a run-session flow writes ascend in the order it writes them.
//!
//! A reader of a run's log keeps, per shard, the largest event id it has read and asks for the ids above it. An event that is written
//! after a larger id was readable, and has a smaller id, is never returned to that reader again. The live A1 proof lost
//! `session.started` that way: `goal.status` and `session.started` were made with the same millisecond, their order was the order of
//! two hashes, and `goal.status` is written first. In 34 of the 68 runs on the testacct stage `session.started` sorted below
//! `goal.status`, so a poll between the two writes moved the reader past it. Every flow here is checked against the order of its writes.
#![allow(clippy::unwrap_used)]

mod support;

use dispatcher::ids::ulid_from;
use harness_protocol::common::Engine;
use harness_protocol::direct::DirectOutcome;
use harness_protocol::target::RunTarget;
use run_session::events;
use run_session::ops::{Op, handle};
use run_session::run_ended::{parse_run_end, reap};
use serde_json::{Value, json};
use support::fakes::*;

/// The run of the live A1 leg that lost its model name and the millisecond of its two start events.
const FAILED_A1_RUN: &str = "01M54NV6WAC7ADQMC8WXQJ5EQ2";
const FAILED_A1_MS: u64 = 1_792_232_234_911;

async fn call(mem: &Mem, v: Value) -> Result<Value, run_session::error::RunSessionError> {
    let op: Op = serde_json::from_value(v).unwrap();
    handle(&mem.deps(), &op).await
}

async fn start(mem: &Mem) {
    call(
        mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
}

/// A run that has a summary and a bundle, so it can be planned and delivered.
fn seeded(target: RunTarget) -> Mem {
    let mem = Mem::default();
    mem.seed(target, "Add percent_change(a, b) with tests", MODEL);
    mem.put_bundle();
    mem.put_summary(&summary(
        DirectOutcome::ReportedDone,
        Some("Added pct."),
        true,
    ));
    mem
}

fn done_session() -> Value {
    json!({"turn": turn_output(true), "failure": {}})
}

fn ids(run: &str, ms: u64) -> (String, String) {
    (
        events::goal_active(run, ms).event_id,
        events::session_started(run, ms, Engine::CLINE, MODEL, 1_000_000).event_id,
    )
}

// ---- the ids themselves -----------------------------------------------------------------------------------------------------

#[test]
fn session_started_sorts_after_goal_status_for_the_run_of_the_failed_a1_leg() {
    let (goal, started) = ids(FAILED_A1_RUN, FAILED_A1_MS);
    assert!(
        goal < started,
        "goal.status {goal} is written first and must sort first; session.started is {started}"
    );
}

#[test]
fn session_started_sorts_after_goal_status_for_any_run_and_any_start_time() {
    // The two ids used to share a millisecond and differ in 80 bits of a hash of the run id: about half the runs sorted the wrong way.
    // 4,000 run ids at 5 times make the old order fail about 10,000 times, so luck cannot pass this.
    let mut wrong = Vec::new();
    for i in 0..4_000u32 {
        let run = ulid_from(1_790_000_000_000, &i.to_be_bytes());
        for ms in [
            0,
            1,
            T0,
            FAILED_A1_MS,
            (1u64 << 48) - 1, // the largest time a ULID holds
        ] {
            let (goal, started) = ids(&run, ms);
            assert!(
                harness_protocol::common::is_ulid(&goal)
                    && harness_protocol::common::is_ulid(&started),
                "both ids stay ULIDs: {goal} {started}"
            );
            if goal >= started {
                wrong.push((run.clone(), ms));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of 20000 start pairs sort session.started below goal.status, first {:?}",
        wrong.len(),
        wrong.first()
    );
}

#[test]
fn the_start_events_are_still_deterministic_so_a_retried_lambda_writes_the_same_items() {
    assert_eq!(
        ids(FAILED_A1_RUN, FAILED_A1_MS),
        ids(FAILED_A1_RUN, FAILED_A1_MS)
    );
    assert_ne!(
        ids(FAILED_A1_RUN, FAILED_A1_MS),
        ids(RUN, FAILED_A1_MS),
        "another run has other ids"
    );
}

// ---- the flows, against the order of their writes ---------------------------------------------------------------------------

#[tokio::test]
async fn the_start_events_are_written_in_id_order() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    start(&mem).await;
    assert_eq!(
        mem.event_writes()
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>(),
        ["goal.status", "session.started"]
    );
    mem.assert_event_writes_ascend(0, "start-session");
}

#[tokio::test]
async fn a_start_that_is_run_again_after_a_lost_answer_writes_in_id_order_too() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    start(&mem).await;
    let first = mem.event_writes().len();
    // The Turn is invoked already: the retry only announces again, with the same ids.
    start(&mem).await;
    mem.assert_event_writes_ascend(first, "start-session retried");
    let ids: std::collections::BTreeSet<_> =
        mem.event_writes().into_iter().map(|(id, _)| id).collect();
    assert_eq!(ids.len(), 2, "the retry wrote the same two items");
}

#[tokio::test]
async fn the_delivery_events_are_written_in_id_order() {
    let mem = seeded(repo_target());
    start(&mem).await;
    let skip = mem.event_writes().len();
    let plan = call(
        &mem,
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged(), "session": done_session()}),
    )
    .await
    .unwrap();
    call(
        &mem,
        json!({"op": "finalize", "runId": RUN, "plan": plan, "validated": {"pass": true},
            "packaged": {"artifactId": "zip-0123456789abcdef0123", "sha256": "ab".repeat(32), "sizeBytes": 4096},
            "prLink": {"prUrl": "https://example.invalid/pr/1"}, "prFailure": {}}),
    )
    .await
    .unwrap();
    let types: Vec<_> = mem
        .event_writes()
        .into_iter()
        .skip(skip)
        .map(|(_, t)| t)
        .collect();
    assert_eq!(
        types,
        [
            "goal.status",
            "delivery.pr",
            "delivery.ready",
            "goal.status",
            "run.completed"
        ]
    );
    mem.assert_event_writes_ascend(skip, "plan-delivery and finalize");
}

#[tokio::test]
async fn the_blocked_events_are_written_in_id_order() {
    let mem = seeded(repo_target());
    start(&mem).await;
    let skip = mem.event_writes().len();
    call(
        &mem,
        json!({"op": "mark-blocked", "runId": RUN,
            "state": {"failure": {"Error": "States.TaskFailed", "Cause": "boom"}}}),
    )
    .await
    .unwrap();
    mem.assert_event_writes_ascend(skip, "mark-blocked");
}

#[tokio::test]
async fn the_events_of_an_aborted_execution_are_written_in_id_order() {
    let mem = seeded(repo_target());
    start(&mem).await;
    let skip = mem.event_writes().len();
    let end = parse_run_end(
        &json!({"source": "aws.states", "detail-type": "Step Functions Execution Status Change",
            "detail": {"status": "ABORTED", "name": RUN, "stateMachineArn": MACHINE, "executionArn": "x"}}),
        MACHINE,
    )
    .unwrap();
    reap(&mem.deps(), &end).await.unwrap();
    mem.assert_event_writes_ascend(skip, "run-ended");
}
