//! The endpoint a run is pinned to. AgentCore's qualifier is an
//! endpoint name, so the Run, Agent and Turn items carry `runtimeEndpoint` (the qualifier every
//! session call uses, `v<N>` or `live`) beside `runtimeVersion` (the version number, evidence
//! only). The field is additive: an item written before it still parses, and writes back
//! unchanged.
#![allow(clippy::unwrap_used)]

use harness_protocol::store::{AgentItem, RunItem, TurnItem};
use serde_json::json;

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";

fn run_json() -> serde_json::Value {
    json!({
        "runId": RUN, "ownerPrincipalArn": "arn:aws:sts::800000000022:assumed-role/Admin/x",
        "promptSha256": "p", "target": {"mode": "s3-only"}, "status": "active", "phase": "planning",
        "enginePolicyVersion": 0, "createdAt": "2026-09-27T10:00:00Z", "runtimeVersion": "50", "version": 1
    })
}

fn agent_json() -> serde_json::Value {
    json!({
        "runId": RUN, "agentId": "planner", "taskId": "root", "attempt": 1,
        "runtimeArn": "arn:aws:bedrock-agentcore:us-west-2:800000000022:runtime/hamh_live_harness_worker_python-ZfYv83FaP0",
        "runtimeSessionId": format!("{RUN}-planner-g1"), "generation": 1, "engine": "cline",
        "engineVersion": "3.0.65", "transport": "acp", "modelId": "m", "pool": "p", "status": "running",
        "runtimeVersion": "50", "version": 1
    })
}

fn turn_json() -> serde_json::Value {
    json!({
        "runId": RUN, "agentId": "planner", "n": 1, "taskTokenCiphertext": "ct",
        "startedAt": "2026-09-27T10:00:00Z", "rulesDigest": "", "runtimeVersion": "50", "version": 1
    })
}

fn with_endpoint(mut v: serde_json::Value, endpoint: &str) -> serde_json::Value {
    v.as_object_mut()
        .unwrap()
        .insert("runtimeEndpoint".into(), json!(endpoint));
    v
}

#[test]
fn a_run_pinned_before_the_endpoint_field_parses_without_one() {
    // The Run item the broken 4f77949 build wrote: a bare version and no endpoint.
    let r: RunItem = serde_json::from_value(run_json()).unwrap();
    assert_eq!(r.runtime_version.as_deref(), Some("50"));
    assert_eq!(r.runtime_endpoint, None);
    assert_eq!(serde_json::to_value(&r).unwrap(), run_json());
}

#[test]
fn the_run_agent_and_turn_items_carry_the_endpoint_their_calls_use() {
    let v = with_endpoint(run_json(), "v50");
    let r: RunItem = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(r.runtime_endpoint.as_deref(), Some("v50"));
    assert_eq!(serde_json::to_value(&r).unwrap(), v);

    let v = with_endpoint(agent_json(), "v50");
    let a: AgentItem = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(a.runtime_endpoint.as_deref(), Some("v50"));
    assert_eq!(serde_json::to_value(&a).unwrap(), v);
    let a: AgentItem = serde_json::from_value(agent_json()).unwrap();
    assert_eq!(a.runtime_endpoint, None);
    assert_eq!(serde_json::to_value(&a).unwrap(), agent_json());

    let v = with_endpoint(turn_json(), "live");
    let t: TurnItem = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(t.runtime_endpoint.as_deref(), Some("live"));
    assert_eq!(serde_json::to_value(&t).unwrap(), v);
    let t: TurnItem = serde_json::from_value(turn_json()).unwrap();
    assert_eq!(t.runtime_endpoint, None);
}
