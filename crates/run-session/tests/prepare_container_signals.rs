//! `prepare` and the container check. The stage payload for the repo-gateway carries `containerSignals` only when the machine of
//! the run could end up on Instances: an explicit `instances` request, or `auto` (or no member) under a stage whose policy is `instances`. The
//! list is the user's own, else the one word `default`; the gateway parses exactly this payload.
#![allow(clippy::unwrap_used)]

mod support;

use delivery::gateway::{GatewayRequest, StageRequest};
use run_session::compute::ComputePolicy;
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

async fn prepare(mem: &Mem) -> Value {
    let op: Op = serde_json::from_value(json!({"op": "prepare", "runId": RUN})).unwrap();
    handle(&mem.deps(), &op).await.unwrap()
}

fn seeded(compute: Option<Value>, policy: ComputePolicy) -> Mem {
    let mem = Mem::default();
    match compute {
        Some(c) => mem.seed_with_members(repo_target(), "p", MODEL, json!({ "compute": c })),
        None => {
            mem.seed(repo_target(), "p", MODEL);
        }
    }
    mem.instances_stage(policy, None);
    mem
}

#[tokio::test]
async fn b632_an_explicit_instances_request_asks_for_the_check_with_the_default_list() {
    for policy in [ComputePolicy::Microvm, ComputePolicy::Instances] {
        let out = prepare(&seeded(Some(json!({"mode": "instances"})), policy)).await;
        assert_eq!(
            out["stage"]["containerSignals"],
            json!(["default"]),
            "{policy:?}"
        );
    }
}

#[tokio::test]
async fn b632_auto_asks_for_the_check_only_under_an_instances_policy() {
    for member in [None, Some(json!({"mode": "auto"}))] {
        let on = prepare(&seeded(member.clone(), ComputePolicy::Instances)).await;
        assert_eq!(
            on["stage"]["containerSignals"],
            json!(["default"]),
            "{member:?}"
        );
        let off = prepare(&seeded(member.clone(), ComputePolicy::Microvm)).await;
        assert!(off["stage"].get("containerSignals").is_none(), "{off}");
    }
}

#[tokio::test]
async fn b632_an_explicit_microvm_request_never_asks() {
    for policy in [ComputePolicy::Microvm, ComputePolicy::Instances] {
        let out = prepare(&seeded(Some(json!({"mode": "microvm"})), policy)).await;
        assert!(out["stage"].get("containerSignals").is_none(), "{out}");
    }
}

#[tokio::test]
async fn b632_the_users_list_goes_to_the_gateway_as_it_is_and_an_empty_list_stays_empty() {
    let list = json!(["default", "Gemfile contains testcontainers"]);
    let out = prepare(&seeded(
        Some(json!({"mode": "auto", "containerSignals": list})),
        ComputePolicy::Instances,
    ))
    .await;
    assert_eq!(out["stage"]["containerSignals"], list);
    let none = prepare(&seeded(
        Some(json!({"mode": "instances", "containerSignals": []})),
        ComputePolicy::Microvm,
    ))
    .await;
    assert_eq!(none["stage"]["containerSignals"], json!([]), "{none}");
}

#[tokio::test]
async fn b632_the_gateway_parses_the_payload_prepare_writes() {
    let out = prepare(&seeded(
        Some(json!({"mode": "instances", "containerSignals": ["compose.yml"]})),
        ComputePolicy::Microvm,
    ))
    .await;
    let GatewayRequest::Stage(StageRequest {
        container_signals, ..
    }) = serde_json::from_value::<GatewayRequest>(out["stage"].clone()).unwrap()
    else {
        panic!("a stage request")
    };
    assert_eq!(container_signals, Some(vec!["compose.yml".to_string()]));
}
