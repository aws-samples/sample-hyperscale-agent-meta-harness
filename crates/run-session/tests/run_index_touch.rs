//! The run index at the end of a run: `control-api` writes a search item when the run starts; `finalize`, `mark-blocked`
//! and `run-ended` record how and when it ended on that item. The write is an upsert of the two end fields (and the key fields, when
//! the item is not there yet): it never touches the prompt head, so it holds in any order with the start's write and the backfill.
//! An index that cannot be updated never fails the end of a run.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::direct::DirectOutcome;
use harness_protocol::goal::GoalStatus;
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use run_session::ports::Store;
use run_session::run_ended::{RunEnd, reap};
use serde_json::{Value, json};
use support::fakes::*;

async fn call(mem: &Mem, v: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(v).unwrap();
    handle(&mem.deps(), &op).await
}

async fn plan(mem: &Mem) -> Value {
    call(
        mem,
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged(),
               "session": {"turn": turn_output(true), "failure": {}}}),
    )
    .await
    .unwrap()
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

/// A run whose session started and left its bundle and a summary that says Cline reported done.
async fn started() -> Mem {
    let mem = Mem::default();
    mem.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
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

#[tokio::test]
async fn finalize_records_the_outcome_label_and_the_time_on_the_search_item() {
    let mem = started().await;
    let p = plan(&mem).await;
    finalize(&mem, &p).await.unwrap();
    let touches = mem.index_touches();
    assert_eq!(touches.len(), 1, "{touches:?}");
    let (owner, created, run, outcome, at) = &touches[0];
    assert_eq!(owner, OWNER);
    assert_eq!(created, CREATED);
    assert_eq!(run, RUN);
    assert_eq!(
        outcome.as_deref(),
        Some(DirectOutcome::ReportedDone.label())
    );
    // The time is the delivery plan's, in whole seconds, the same instant the run.completed event carries.
    let planned_ms = p["plannedAtMs"].as_u64().unwrap();
    assert_eq!(at, &dispatcher::items::rfc3339(planned_ms / 1000));
}

#[tokio::test]
async fn an_index_that_cannot_be_updated_never_fails_the_end_of_a_run() {
    let mem = started().await;
    mem.fail(
        "touch_run_index",
        RunSessionError::Unavailable("table throttled".into()),
    );
    let p = plan(&mem).await;
    let out = finalize(&mem, &p).await.unwrap();
    assert_eq!(out["status"], "complete");
    assert_eq!(mem.run().status, GoalStatus::Complete);
    assert!(mem.stops().len() == 1, "the session still ends");
}

#[tokio::test]
async fn a_retried_finalize_records_the_same_values_again() {
    let mem = started().await;
    let p = plan(&mem).await;
    finalize(&mem, &p).await.unwrap();
    finalize(&mem, &p).await.unwrap();
    let touches = mem.index_touches();
    assert_eq!(touches.len(), 2);
    assert_eq!(touches[0], touches[1], "an update that repeats is harmless");
}

#[tokio::test]
async fn a_discarded_run_leaves_its_search_item_alone() {
    let mem = started().await;
    let p = plan(&mem).await;
    mem.set_run_status_raw(GoalStatus::Cleared);
    finalize(&mem, &p).await.unwrap();
    assert!(mem.index_touches().is_empty());
}

#[tokio::test]
async fn mark_blocked_records_the_time_and_the_outcome_when_the_plan_has_one() {
    let mem = started().await;
    call(
        &mem,
        json!({"op": "mark-blocked", "runId": RUN,
               "state": {"failure": {"Error": "RunSessionInvalid", "Cause": "bundle did not validate"}}}),
    )
    .await
    .unwrap();
    let touches = mem.index_touches();
    assert_eq!(touches.len(), 1, "{touches:?}");
    assert_eq!(touches[0].3, None, "no plan, so no outcome word");
    assert_eq!(touches[0].2, RUN);
    assert_eq!(mem.run().status, GoalStatus::Blocked);
}

#[tokio::test]
async fn mark_blocked_survives_an_index_that_is_down() {
    let mem = started().await;
    mem.fail(
        "touch_run_index",
        RunSessionError::Unavailable("table throttled".into()),
    );
    let out = call(
        &mem,
        json!({"op": "mark-blocked", "runId": RUN, "state": {}}),
    )
    .await
    .unwrap();
    assert_eq!(out["status"], "blocked");
}

#[tokio::test]
async fn the_reaper_records_the_time_a_run_was_cleared() {
    let mem = started().await;
    let end = RunEnd {
        run_id: RUN.into(),
        execution_arn: "arn:aws:states:us-east-1:700000000011:execution:hamh-direct-run:x".into(),
        status: harness_protocol::orchestration::ExecutionEnd::Failed,
        error: Some("States.TaskFailed".into()),
        cause: None,
        stopped_at_ms: Some(1_790_000_600_000),
    };
    let out = reap(&mem.deps(), &end).await.unwrap();
    assert!(out.cleared);
    let touches = mem.index_touches();
    assert_eq!(touches.len(), 1, "{touches:?}");
    assert_eq!(touches[0].3, None);
    assert_eq!(touches[0].4, dispatcher::items::rfc3339(1_790_000_600));
    // A run that was already finished is not touched again by a repeat of the reap.
    reap(&mem.deps(), &end).await.unwrap();
    assert_eq!(mem.index_touches().len(), 1);
}

// ---- the run name shares the search item with the end of the run --------------------------------------------------------

const END: &str = "2026-10-13T19:05:00Z";
const HAIKU: &str = "us.anthropic.claude-haiku-5-5";
const NAME: &str = "Add percent change function with tests";

/// A run with naming on, no session: the search item is empty until a writer comes.
fn named_run() -> Mem {
    let mem = Mem::default();
    mem.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
    mem.enable_naming(vec![Scripted::title(NAME)]);
    mem
}

fn expected_both() -> std::collections::BTreeMap<String, String> {
    [
        ("runId", RUN),
        ("createdAt", CREATED),
        ("lastActivity", END),
        ("outcome", "Cline reported done"),
        ("runName", NAME),
        ("runNameSource", "ai"),
        ("runNameModel", HAIKU),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

async fn name_the_run(mem: &Mem) -> Value {
    call(mem, json!({"op": "name-run", "runId": RUN}))
        .await
        .unwrap()
}

async fn end_the_run(mem: &Mem) {
    mem.touch_run_index(&mem.run(), Some("Cline reported done"), END)
        .await
        .unwrap();
}

/// The two writers set disjoint attributes (and the same key fields only where the item is new), so the item ends the same whichever
/// is first: the name before the end of the run, or the end of the run before the name.
#[tokio::test]
async fn name_and_touch_keep_each_others_attributes() {
    for name_first in [true, false] {
        let mem = named_run();
        if name_first {
            assert_eq!(name_the_run(&mem).await["named"], "ai");
            end_the_run(&mem).await;
        } else {
            end_the_run(&mem).await;
            assert_eq!(name_the_run(&mem).await["named"], "ai");
        }
        assert_eq!(
            mem.search_item().unwrap(),
            expected_both(),
            "name first: {name_first}"
        );
    }
}

/// Each writer alone leaves only its own fields: the name never sets the time, the outcome or the prompt head, and the end of the run
/// never sets a name attribute.
#[tokio::test]
async fn each_writer_sets_only_its_own_attributes() {
    let mem = named_run();
    call(&mem, json!({"op": "name-run", "runId": RUN}))
        .await
        .unwrap();
    let mut keys: Vec<String> = mem.search_item().unwrap().into_keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "createdAt",
            "runId",
            "runName",
            "runNameModel",
            "runNameSource"
        ],
        "the name op sets no lastActivity, outcome or promptHead"
    );

    let mem = named_run();
    mem.touch_run_index(&mem.run(), Some("Cline reported done"), END)
        .await
        .unwrap();
    let mut keys: Vec<String> = mem.search_item().unwrap().into_keys().collect();
    keys.sort();
    assert_eq!(keys, ["createdAt", "lastActivity", "outcome", "runId"]);
}

/// A later end of the run does not remove a name, and a name written after a later time does not lower it: the name and the time
/// are separate fields.
#[tokio::test]
async fn a_second_end_of_the_run_leaves_the_name_alone() {
    let mem = named_run();
    let run = mem.run();
    call(&mem, json!({"op": "name-run", "runId": RUN}))
        .await
        .unwrap();
    mem.touch_run_index(&run, Some("Cline reported done"), END)
        .await
        .unwrap();
    mem.touch_run_index(&run, Some("Cline reported done"), "2026-10-13T19:06:00Z")
        .await
        .unwrap();
    let item = mem.search_item().unwrap();
    assert_eq!(item["runName"], NAME);
    assert_eq!(item["lastActivity"], "2026-10-13T19:06:00Z");
}
