//! The Instances slot of a run is given back whenever the run ends, whatever the stage's cap says now and whichever machine the run ended on.
//! The claim is made under the cap of one attempt, and the end can come under another: a deploy may remove the
//! cap while the run works, and a retry may choose the microVM after an attempt on Instances claimed a slot and failed before the Agent item
//! was written. A release that depends on the cap, or on the Agent item recording Instances, leaves the run's id in the stage's set for good;
//! the set counts it against every later cap. The release is one idempotent DELETE of the run's own id, so `end_session` makes it for every run.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::orchestration::ExecutionEnd;
use run_session::compute::ComputePolicy;
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use run_session::run_ended::{RunEnd, reap};
use serde_json::{Value, json};
use support::fakes::*;

async fn start(mem: &Mem) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .unwrap();
    handle(&mem.deps(), &op).await
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

fn instances_run() -> Mem {
    let mem = Mem::default();
    mem.seed_with_members(
        repo_target(),
        "p",
        MODEL,
        json!({"compute": {"mode": "instances"}}),
    );
    mem.instances_stage(ComputePolicy::Microvm, Some(3));
    mem
}

#[tokio::test]
async fn a_cap_that_is_gone_when_the_run_ends_still_frees_the_slot() {
    let mem = instances_run();
    start(&mem).await.unwrap();
    assert_eq!(mem.slot_holders(), [RUN.to_string()], "the claim was made");
    // A deploy removes COMPUTE_MAX_ACTIVE while the run works.
    mem.instances_stage(ComputePolicy::Microvm, None);
    reap(&mem.deps(), &failed_end()).await.unwrap();
    assert!(
        mem.slot_holders().is_empty(),
        "the run's id stays in the set: {:?}",
        mem.slot_holders()
    );
}

#[tokio::test]
async fn a_claim_whose_attempt_failed_is_freed_when_the_next_attempt_chooses_the_microvm() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    mem.instances_stage(ComputePolicy::Instances, Some(3));
    // The first attempt claims a slot, then dies before it writes the Agent item.
    mem.fail(
        "create_turn",
        RunSessionError::Unavailable("DynamoDB 500".into()),
    );
    start(&mem).await.unwrap_err();
    assert_eq!(mem.slot_holders(), [RUN.to_string()], "the claim is held");
    assert!(mem.agent().is_none(), "and nothing records Instances");
    // A deploy changes the policy; the request says nothing, so the next attempt chooses the microVM.
    mem.instances_stage(ComputePolicy::Microvm, Some(3));
    start(&mem).await.unwrap();
    assert_eq!(mem.agent().unwrap().compute.as_deref(), Some("microvm"));
    reap(&mem.deps(), &failed_end()).await.unwrap();
    assert!(
        mem.slot_holders().is_empty(),
        "the microVM run's end must free the claim of the attempt before it: {:?}",
        mem.slot_holders()
    );
}

#[tokio::test]
async fn a_run_that_claimed_and_never_wrote_its_agent_item_frees_the_slot_even_with_no_cap() {
    let mem = instances_run();
    mem.fail(
        "create_turn",
        RunSessionError::Unavailable("DynamoDB 500".into()),
    );
    start(&mem).await.unwrap_err();
    assert_eq!(mem.slot_holders(), [RUN.to_string()]);
    mem.instances_stage(ComputePolicy::Microvm, None);
    reap(&mem.deps(), &failed_end()).await.unwrap();
    assert!(mem.slot_holders().is_empty(), "{:?}", mem.slot_holders());
}

#[tokio::test]
async fn the_release_of_a_run_that_holds_nothing_is_harmless_and_repeatable() {
    // A microVM run on a stage with no Instances at all: the end asks the store to drop its id, which is not there, and nothing fails.
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    start(&mem).await.unwrap();
    reap(&mem.deps(), &failed_end()).await.unwrap();
    reap(&mem.deps(), &failed_end()).await.unwrap();
    assert!(mem.slot_holders().is_empty());
    assert!(mem.provider_deletes().is_empty(), "no provider session");
    assert_eq!(
        mem.slot_calls(),
        [format!("release {RUN}"), format!("release {RUN}")],
        "only its own id is ever named, and never a claim"
    );
    assert_eq!(
        mem.ending(),
        [
            "stop",
            "release_slot",
            "end_agent",
            "stop",
            "release_slot",
            "end_agent"
        ]
    );
}

#[tokio::test]
async fn another_runs_slot_is_never_touched() {
    let mem = instances_run();
    mem.hold_slot("01M4OTHER00000000000000000");
    start(&mem).await.unwrap();
    mem.instances_stage(ComputePolicy::Microvm, None);
    reap(&mem.deps(), &failed_end()).await.unwrap();
    assert_eq!(
        mem.slot_holders(),
        ["01M4OTHER00000000000000000".to_string()]
    );
}
