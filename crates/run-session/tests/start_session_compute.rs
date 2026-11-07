//! `start-session` and the compute choice. The request's `compute` member, the stage policy and the Agent item
//! decide which runtime the session is invoked on; the choice is recorded on the Agent item before the first invoke and a retry reads it back,
//! so no later policy moves a run; an `instances` run claims one of the stage's slots, and a stage without the runtime refuses with the reason.
#![allow(clippy::unwrap_used)]

mod support;

use dispatcher::ids::runtime_session_id;
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

fn seeded(compute: Option<Value>) -> Mem {
    let mem = Mem::default();
    match compute {
        Some(c) => mem.seed_with_members(repo_target(), "p", MODEL, json!({ "compute": c })),
        None => {
            mem.seed(repo_target(), "p", MODEL);
        }
    }
    mem
}

#[tokio::test]
async fn an_absent_member_uses_the_microvm_runtime_under_the_default_policy() {
    let mem = seeded(None);
    mem.instances_stage(ComputePolicy::Microvm, Some(4));
    start(&mem).await.unwrap();
    let targets = mem.invoke_targets();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].0, RUNTIME);
    assert_eq!(
        targets[0].1, "v7",
        "pinned to the version endpoint as before"
    );
    let agent = mem.agent().unwrap();
    assert_eq!(agent.compute.as_deref(), Some("microvm"));
    assert_eq!(agent.capacity_provider_id, None);
    assert_eq!(agent.runtime_arn, RUNTIME);
    assert!(
        mem.slot_calls().is_empty(),
        "a microVM run never counts as an Instances session"
    );
}

#[tokio::test]
async fn instances_in_the_request_uses_the_instances_runtime_and_records_the_choice() {
    let mem = seeded(Some(json!({"mode": "instances"})));
    mem.instances_stage(ComputePolicy::Microvm, None);
    start(&mem).await.unwrap();
    let targets = mem.invoke_targets();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].0, INSTANCES);
    assert_eq!(
        targets[0].1, "DEFAULT",
        "an Instances runtime with no endpoints is invoked at its default endpoint"
    );
    assert_eq!(targets[0].2, runtime_session_id(RUN, "direct", 1));
    let agent = mem.agent().unwrap();
    assert_eq!(agent.compute.as_deref(), Some("instances"));
    assert_eq!(agent.capacity_provider_id.as_deref(), Some(PROVIDER));
    assert_eq!(agent.runtime_arn, INSTANCES);
    assert_eq!(agent.runtime_session_id, targets[0].2);
    assert_eq!(mem.run().runtime_endpoint.as_deref(), Some("DEFAULT"));
}

#[tokio::test]
async fn the_agent_item_is_written_before_the_first_invoke_so_the_choice_is_durable() {
    let mem = seeded(Some(json!({"mode": "instances"})));
    mem.instances_stage(ComputePolicy::Microvm, None);
    mem.fail(
        "invoke",
        RunSessionError::Unavailable("AgentCore 503".into()),
    );
    start(&mem).await.unwrap_err();
    let agent = mem
        .agent()
        .expect("the Agent item exists although the invoke failed");
    assert_eq!(agent.compute.as_deref(), Some("instances"));
    assert_eq!(agent.capacity_provider_id.as_deref(), Some(PROVIDER));
    assert!(mem.invoke_targets().is_empty());
}

#[tokio::test]
async fn an_explicit_microvm_wins_over_an_instances_policy_and_auto_follows_it() {
    for (member, want_arn, want) in [
        (None, INSTANCES, "instances"),
        (Some(json!({"mode": "auto"})), INSTANCES, "instances"),
        (Some(json!({"mode": "microvm"})), RUNTIME, "microvm"),
        (Some(json!({"mode": "instances"})), INSTANCES, "instances"),
    ] {
        let mem = seeded(member.clone());
        mem.instances_stage(ComputePolicy::Instances, None);
        start(&mem).await.unwrap();
        assert_eq!(mem.invoke_targets()[0].0, want_arn, "{member:?}");
        assert_eq!(
            mem.agent().unwrap().compute.as_deref(),
            Some(want),
            "{member:?}"
        );
    }
}

#[tokio::test]
async fn a_stage_without_the_instances_runtime_refuses_with_the_reason_and_starts_nothing() {
    let mem = seeded(Some(json!({"mode": "instances"})));
    mem.capacity_provider(PROVIDER);
    mem.set_compute_stage(run_session::compute::ComputeStage {
        policy: ComputePolicy::Microvm,
        max_active: None,
    });
    let e = start(&mem).await.unwrap_err();
    assert!(matches!(e, RunSessionError::Invalid(_)), "{e:?}");
    assert!(
        e.to_string()
            .contains("the Instances worker runtime is not deployed"),
        "{e}"
    );
    assert!(mem.invoke_targets().is_empty());
    assert!(mem.agent().is_none());
    assert!(mem.slot_calls().is_empty());
}

#[tokio::test]
async fn a_stage_that_names_no_capacity_provider_refuses_by_variable() {
    let mem = seeded(Some(json!({"mode": "instances"})));
    mem.instances_runtime(INSTANCES);
    let e = start(&mem).await.unwrap_err();
    assert!(matches!(e, RunSessionError::Invalid(_)), "{e:?}");
    assert!(
        e.to_string().contains("CAPACITY_PROVIDER_ID_PARAMETER"),
        "{e}"
    );
    assert!(mem.invoke_targets().is_empty() && mem.agent().is_none());
}

#[tokio::test]
async fn a_retry_keeps_the_first_choice_whatever_the_policy_says_now() {
    let mem = seeded(None);
    mem.instances_stage(ComputePolicy::Instances, None);
    mem.fail(
        "invoke",
        RunSessionError::Unavailable("AgentCore 503".into()),
    );
    start(&mem).await.unwrap_err();
    assert_eq!(mem.agent().unwrap().compute.as_deref(), Some("instances"));
    // The policy changes between the attempts (a deploy); the request says nothing, so a fresh choice would now be the microVM.
    mem.instances_stage(ComputePolicy::Microvm, None);
    start(&mem).await.unwrap();
    let targets = mem.invoke_targets();
    assert_eq!(targets.len(), 1, "the failed first attempt sent nothing");
    assert_eq!(
        targets[0].0, INSTANCES,
        "the retry stays on the Instances runtime"
    );
    assert_eq!(mem.agent().unwrap().compute.as_deref(), Some("instances"));
    assert_eq!(
        targets[0].2,
        runtime_session_id(RUN, "direct", 1),
        "the same session id"
    );
}

#[tokio::test]
async fn an_instances_runtime_with_a_live_endpoint_is_pinned_like_the_microvm_one() {
    let mem = seeded(Some(json!({"mode": "instances"})));
    mem.instances_stage(ComputePolicy::Microvm, None);
    {
        let mut e = mem.0.instances_endpoints.lock().unwrap();
        for (name, v) in [("live", "3"), ("v3", "3")] {
            e.insert(
                name.into(),
                dispatcher::endpoints::EndpointState {
                    name: name.into(),
                    status: "READY".into(),
                    live_version: Some(v.into()),
                },
            );
        }
    }
    start(&mem).await.unwrap();
    assert_eq!(mem.invoke_targets()[0].1, "v3");
    assert_eq!(mem.run().runtime_endpoint.as_deref(), Some("v3"));
}

#[tokio::test]
async fn the_most_active_sessions_refuses_the_next_instances_run_and_a_repeat_does_not_count_twice()
{
    // Another run holds the only slot.
    let mem = seeded(Some(json!({"mode": "instances"})));
    mem.instances_stage(ComputePolicy::Microvm, Some(1));
    mem.hold_slot("01M4OTHER00000000000000000");
    let e = start(&mem).await.unwrap_err();
    assert!(matches!(e, RunSessionError::Invalid(_)), "{e:?}");
    let text = e.to_string();
    assert!(
        text.contains("most Instances sessions") && text.contains("--compute microvm"),
        "the refusal says what to do: {text}"
    );
    assert!(mem.invoke_targets().is_empty() && mem.agent().is_none());

    // With room, the run takes a slot; a retry of the same run holds one slot only.
    let mem = seeded(Some(json!({"mode": "instances"})));
    mem.instances_stage(ComputePolicy::Microvm, Some(2));
    mem.hold_slot("01M4OTHER00000000000000000");
    mem.fail(
        "invoke",
        RunSessionError::Unavailable("AgentCore 503".into()),
    );
    start(&mem).await.unwrap_err();
    start(&mem).await.unwrap();
    assert_eq!(mem.slot_holders().len(), 2, "{:?}", mem.slot_holders());
    assert!(mem.slot_holders().contains(&RUN.to_string()));
}

#[tokio::test]
async fn a_cap_is_not_asked_for_when_the_stage_sets_none() {
    let mem = seeded(Some(json!({"mode": "instances"})));
    mem.instances_stage(ComputePolicy::Microvm, None);
    start(&mem).await.unwrap();
    assert!(mem.slot_calls().is_empty(), "{:?}", mem.slot_calls());
}

// ---- the capacity provider's id is a parameter of the stage --------------------------------------

#[tokio::test]
async fn the_provider_id_is_read_from_its_parameter_when_an_instances_session_starts_and_recorded_for_the_end()
 {
    let mem = seeded(Some(json!({"mode": "instances"})));
    mem.instances_stage(ComputePolicy::Microvm, None);
    start(&mem).await.unwrap();
    assert_eq!(mem.provider_lookups(), 1);
    assert_eq!(
        mem.agent().unwrap().capacity_provider_id.as_deref(),
        Some(PROVIDER)
    );
    // The end deletes the session under the id that was recorded, whatever the parameter holds by then.
    mem.capacity_provider("ham_other_instances-ZyXwVuTsRq");
    reap(
        &mem.deps(),
        &RunEnd {
            run_id: RUN.into(),
            execution_arn: "arn:aws:states:us-east-1:123456789012:execution:hamh-direct-run:x"
                .into(),
            status: ExecutionEnd::Failed,
            error: Some("States.TaskFailed".into()),
            cause: None,
            stopped_at_ms: Some(1_790_000_600_000),
        },
    )
    .await
    .unwrap();
    assert_eq!(mem.provider_lookups(), 1, "the end reads no parameter");
    assert_eq!(
        mem.provider_deletes(),
        [(PROVIDER.to_string(), runtime_session_id(RUN, "direct", 1))]
    );
}

#[tokio::test]
async fn microvm_runs_never_ask_for_the_provider() {
    for member in [
        None,
        Some(json!({"mode": "microvm"})),
        Some(json!({"mode": "auto"})),
    ] {
        let mem = seeded(member.clone());
        mem.instances_stage(ComputePolicy::Microvm, Some(4));
        start(&mem).await.unwrap();
        assert_eq!(mem.provider_lookups(), 0, "{member:?}");
    }
}

#[tokio::test]
async fn a_provider_parameter_that_is_not_set_up_refuses_with_the_reason_and_starts_nothing() {
    let mem = seeded(Some(json!({"mode": "instances"})));
    mem.instances_runtime(INSTANCES);
    mem.capacity_provider_fails(RunSessionError::Invalid(
        "the Instances capacity provider is not set up: SSM parameter /hamh/direct/instances-capacity-provider-id does not exist".into(),
    ));
    let e = start(&mem).await.unwrap_err();
    assert!(matches!(e, RunSessionError::Invalid(_)), "{e:?}");
    assert!(
        e.to_string().contains("capacity provider is not set up"),
        "{e}"
    );
    assert!(mem.invoke_targets().is_empty() && mem.agent().is_none());
    assert!(mem.slot_calls().is_empty(), "no slot was claimed");
}

#[tokio::test]
async fn a_failed_provider_read_may_pass_so_the_op_is_retried_and_starts_nothing() {
    let mem = seeded(Some(json!({"mode": "instances"})));
    mem.instances_stage(ComputePolicy::Microvm, Some(2));
    mem.capacity_provider_fails(RunSessionError::Unavailable("SSM throttled".into()));
    let e = start(&mem).await.unwrap_err();
    assert!(matches!(e, RunSessionError::Unavailable(_)), "{e:?}");
    assert!(mem.invoke_targets().is_empty() && mem.agent().is_none());
    assert!(mem.slot_calls().is_empty());
    // The parameter is readable at the retry: the run starts.
    mem.capacity_provider(PROVIDER);
    start(&mem).await.unwrap();
    assert_eq!(mem.invoke_targets().len(), 1);
}
