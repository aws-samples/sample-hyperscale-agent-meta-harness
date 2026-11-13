//! `start-session` and the container check of the staged repository. The repo-gateway's `containerCheck` decides, with the
//! request and the stage's policy, which runtime the session is invoked on. A refusal comes before any write, any slot claim and any invoke;
//! a machine that is already recorded on the Agent item is never decided again.
#![allow(clippy::unwrap_used)]

mod support;

use run_session::compute::ComputePolicy;
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

fn staged_with(check: Option<Value>) -> Value {
    let mut s = staged();
    match check {
        Some(c) => s["containerCheck"] = c,
        None => {
            s.as_object_mut().unwrap().remove("containerCheck");
        }
    }
    s
}

fn hit() -> Value {
    json!({"found": {"signal": "docker-compose.yml", "path": "deploy/docker-compose.yml"}})
}

async fn start(mem: &Mem, staged: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged}),
    )
    .unwrap();
    handle(&mem.deps(), &op).await
}

fn seeded(compute: Option<Value>, policy: ComputePolicy) -> Mem {
    let mem = Mem::default();
    match compute {
        Some(c) => mem.seed_with_members(repo_target(), "p", MODEL, json!({ "compute": c })),
        None => {
            mem.seed(repo_target(), "p", MODEL);
        }
    }
    mem.instances_stage(policy, Some(4));
    mem
}

fn invalid(r: Result<Value, RunSessionError>) -> String {
    match r {
        Err(RunSessionError::Invalid(m)) => m,
        other => panic!("expected an invalid run, got {other:?}"),
    }
}

#[tokio::test]
async fn b632_auto_with_a_signal_starts_on_the_microvm_runtime_under_an_instances_policy() {
    for member in [None, Some(json!({"mode": "auto"}))] {
        let mem = seeded(member.clone(), ComputePolicy::Instances);
        start(&mem, staged_with(Some(hit()))).await.unwrap();
        assert_eq!(mem.invoke_targets()[0].0, RUNTIME, "{member:?}");
        let agent = mem.agent().unwrap();
        assert_eq!(agent.compute.as_deref(), Some("microvm"));
        assert_eq!(agent.capacity_provider_id, None);
        assert!(
            mem.slot_calls().is_empty(),
            "a microVM run never counts as an Instances session"
        );
    }
}

#[tokio::test]
async fn b632_explicit_instances_with_a_signal_is_refused_before_anything_is_written_or_started() {
    let mem = seeded(Some(json!({"mode": "instances"})), ComputePolicy::Microvm);
    let message = invalid(start(&mem, staged_with(Some(hit()))).await);
    for part in [
        "deploy/docker-compose.yml",
        "\"docker-compose.yml\"",
        "--compute microvm",
        "run.container_signals",
    ] {
        assert!(message.contains(part), "{part}: {message}");
    }
    assert!(mem.agent().is_none(), "no Agent item was written");
    assert!(mem.invoke_targets().is_empty(), "no session was invoked");
    assert!(mem.slot_calls().is_empty(), "no Instances slot was claimed");
    assert_eq!(
        mem.provider_lookups(),
        0,
        "the stage's provider was not even read"
    );
}

#[tokio::test]
async fn b632_the_signal_is_reported_before_the_stage_runs_out_of_instances_slots() {
    let mem = seeded(Some(json!({"mode": "instances"})), ComputePolicy::Instances);
    mem.hold_slot("another-run-1");
    for i in 2..=4 {
        mem.hold_slot(&format!("another-run-{i}"));
    }
    let message = invalid(start(&mem, staged_with(Some(hit()))).await);
    assert!(message.contains("container"), "{message}");
    assert!(!message.contains("most Instances sessions"), "{message}");
}

#[tokio::test]
async fn b632_a_repository_with_nothing_found_keeps_the_machine_it_had() {
    // Instances policy, auto, nothing found: Instances, as before the check existed.
    let mem = seeded(None, ComputePolicy::Instances);
    start(&mem, staged_with(Some(json!({})))).await.unwrap();
    assert_eq!(mem.invoke_targets()[0].0, INSTANCES);
    assert_eq!(mem.agent().unwrap().compute.as_deref(), Some("instances"));
    // An explicit instances request with nothing found starts on Instances.
    let mem = seeded(Some(json!({"mode": "instances"})), ComputePolicy::Microvm);
    start(&mem, staged_with(Some(json!({})))).await.unwrap();
    assert_eq!(mem.invoke_targets()[0].0, INSTANCES);
    // An explicit microvm request never looks at the check.
    let mem = seeded(Some(json!({"mode": "microvm"})), ComputePolicy::Instances);
    start(&mem, staged_with(Some(hit()))).await.unwrap();
    assert_eq!(mem.invoke_targets()[0].0, RUNTIME);
}

#[tokio::test]
async fn b632_a_gateway_that_reported_no_check_cannot_put_a_run_on_instances() {
    let mem = seeded(None, ComputePolicy::Instances);
    start(&mem, staged_with(None)).await.unwrap();
    assert_eq!(
        mem.invoke_targets()[0].0,
        RUNTIME,
        "auto falls back to the microVM"
    );
    let mem = seeded(Some(json!({"mode": "instances"})), ComputePolicy::Microvm);
    let message = invalid(start(&mem, staged_with(None)).await);
    assert!(message.contains("did not check"), "{message}");
    assert!(mem.agent().is_none() && mem.invoke_targets().is_empty());
    // A stage that keeps the microVM as its policy never needed the check.
    let mem = seeded(None, ComputePolicy::Microvm);
    start(&mem, staged_with(None)).await.unwrap();
    assert_eq!(mem.invoke_targets()[0].0, RUNTIME);
}

#[tokio::test]
async fn b632_a_machine_that_is_already_recorded_is_not_decided_again() {
    let mem = seeded(Some(json!({"mode": "instances"})), ComputePolicy::Microvm);
    mem.fail(
        "invoke",
        RunSessionError::Unavailable("AgentCore 503".into()),
    );
    start(&mem, staged_with(Some(json!({})))).await.unwrap_err();
    assert_eq!(mem.agent().unwrap().compute.as_deref(), Some("instances"));
    // The retry sees a signal now (it cannot in practice, the tree is the same); the recorded machine wins.
    start(&mem, staged_with(Some(hit()))).await.unwrap();
    let last = mem.invoke_targets().pop().unwrap();
    assert_eq!(last.0, INSTANCES);
}

#[tokio::test]
async fn b632_start_session_reads_the_exact_shape_the_gateway_serializes() {
    use delivery::gateway::StageOutput;
    use ham_core::container_signals::{Check, Found};
    let out = StageOutput {
        bundle_key: "k".into(),
        bundle_uri: "s3://b/k".into(),
        base_sha: BASE.into(),
        base_ref: "refs/heads/main".into(),
        size_bytes: 10,
        sha256: "ef".repeat(32),
        container_check: Some(Check {
            found: Some(Found {
                signal: "go.mod contains testcontainers".into(),
                path: "svc/go.mod".into(),
            }),
        }),
    };
    let wire = serde_json::to_value(&out).unwrap();
    assert_eq!(
        wire["containerCheck"],
        json!({"found": {"signal": "go.mod contains testcontainers", "path": "svc/go.mod"}})
    );
    // `auto` under an Instances policy: the gateway's own bytes keep the run on the microVM.
    let mem = seeded(None, ComputePolicy::Instances);
    start(&mem, wire.clone()).await.unwrap();
    assert_eq!(mem.invoke_targets()[0].0, RUNTIME);
    // An explicit `instances` request: the message names the path the gateway reported.
    let mem = seeded(Some(json!({"mode": "instances"})), ComputePolicy::Microvm);
    let message = invalid(start(&mem, wire).await);
    assert!(message.contains("svc/go.mod"), "{message}");
}
