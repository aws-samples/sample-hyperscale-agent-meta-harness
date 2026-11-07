//! The end of an Instances run: the session is stopped, its provider session is deleted (the call that ends the machine's billing and
//! removes its disk), the slot is released and the Agent item is ended, in that order, safely repeatable. A microVM run never deletes a provider
//! session. `run-ended` reaches the same function for a run whose execution failed, so the failure path cleans up too.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::orchestration::ExecutionEnd;
use run_session::compute::ComputePolicy;
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use run_session::run_ended::{RunEnd, reap};
use serde_json::{Value, json};
use support::fakes::*;

async fn started(instances: bool) -> Mem {
    let mem = Mem::default();
    if instances {
        mem.seed_with_members(
            repo_target(),
            "p",
            MODEL,
            json!({"compute": {"mode": "instances"}}),
        );
        mem.instances_stage(ComputePolicy::Microvm, Some(3));
    } else {
        mem.seed(repo_target(), "p", MODEL);
    }
    let op: Op = serde_json::from_value(
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .unwrap();
    handle(&mem.deps(), &op).await.unwrap();
    mem
}

fn failed_end() -> RunEnd {
    RunEnd {
        run_id: RUN.into(),
        execution_arn: "arn:aws:states:us-east-1:123456789012:execution:hamh-direct-run:x".into(),
        status: ExecutionEnd::Failed,
        error: Some("States.TaskFailed".into()),
        cause: None,
        stopped_at_ms: Some(1_790_000_600_000),
    }
}

#[tokio::test]
async fn an_instances_session_is_stopped_then_deleted_then_released_then_ended() {
    let mem = started(true).await;
    assert_eq!(mem.slot_holders(), [RUN.to_string()]);
    reap(&mem.deps(), &failed_end()).await.unwrap();
    assert_eq!(
        mem.ending(),
        [
            "stop",
            "delete_provider_session",
            "release_slot",
            "end_agent"
        ],
        "the order of the end"
    );
    assert_eq!(
        mem.provider_deletes(),
        [(PROVIDER.to_string(), format!("{RUN}-direct-g1"))]
    );
    assert!(mem.slot_holders().is_empty(), "the slot is free again");
}

#[tokio::test]
async fn a_microvm_session_never_deletes_a_provider_session_or_claims_a_slot() {
    let mem = started(false).await;
    reap(&mem.deps(), &failed_end()).await.unwrap();
    // It drops its own id from the stage's set (one idempotent DELETE; tests/finalize_slots.rs says why every end does), and nothing else.
    assert_eq!(mem.ending(), ["stop", "release_slot", "end_agent"]);
    assert!(mem.provider_deletes().is_empty());
    assert_eq!(mem.slot_calls(), [format!("release {RUN}")]);
}

#[tokio::test]
async fn a_provider_session_that_is_already_gone_is_fine_and_the_slot_is_still_released() {
    let mem = started(true).await;
    mem.set_provider_gone(true);
    reap(&mem.deps(), &failed_end()).await.unwrap();
    assert!(mem.slot_holders().is_empty());
    assert_eq!(mem.ending().last().map(String::as_str), Some("end_agent"));
}

#[tokio::test]
async fn a_real_failure_of_the_delete_fails_the_op_and_a_repeat_finishes_the_job() {
    let mem = started(true).await;
    mem.fail(
        "delete_provider_session",
        RunSessionError::Unavailable("AgentCore 503".into()),
    );
    let e = reap(&mem.deps(), &failed_end()).await.unwrap_err();
    assert!(matches!(e, RunSessionError::Unavailable(_)), "{e:?}");
    assert_eq!(
        mem.agent().unwrap().status,
        harness_protocol::store::AgentStatus::Running,
        "the Agent item stays open while a machine may still bill"
    );
    assert_eq!(
        mem.slot_holders(),
        [RUN.to_string()],
        "and the slot stays held"
    );
    // The repeat of the reap (the machine retries every five minutes) stops again, deletes and ends.
    reap(&mem.deps(), &failed_end()).await.unwrap();
    assert!(mem.slot_holders().is_empty());
    assert_eq!(mem.provider_deletes().len(), 1);
    assert_eq!(
        mem.agent().unwrap().status,
        harness_protocol::store::AgentStatus::Stopped
    );
    let _: Value = json!(null);
}
