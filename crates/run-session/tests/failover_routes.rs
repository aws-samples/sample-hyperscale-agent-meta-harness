//! `start-session` with provider failover: the invocation carries the routes agentd may switch
//! to, and the credentials it carries are minted with a policy that covers every one of them (before this, the policy named the one
//! model of the run, so a switch would have been refused by IAM).
#![allow(clippy::unwrap_used)]

mod support;

use dispatcher::stage::{ACCOUNT, REGION};
use ham_core::models::builtin;
use harness_protocol::direct::{DirectInvocationRequest, validate_direct_invocation};
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use support::fakes::*;

async fn start(mem: &Mem) -> Value {
    let op: Op = serde_json::from_value(
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .unwrap();
    handle(&mem.deps(), &op).await.unwrap()
}

/// The five models the maintainers supports, as a request names them.
fn the_five() -> Vec<String> {
    builtin()
        .models()
        .iter()
        .filter(|m| m.selectable())
        .map(|m| m.id.clone())
        .collect()
}

fn leased(policy: &str) -> BTreeSet<String> {
    let v: Value = serde_json::from_str(policy).unwrap();
    let st = v["Statement"].as_array().unwrap();
    let m = st.iter().find(|s| s["Sid"] == "LeasedModel").unwrap();
    m["Resource"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap().to_string())
        .collect()
}

fn base(id: &str) -> &str {
    id.split_once('.').map_or(id, |(_, rest)| rest)
}

#[tokio::test]
async fn the_invocation_lists_the_failover_routes_of_the_chosen_model() {
    for id in the_five() {
        let mem = Mem::default();
        mem.seed(repo_target(), "p", &id);
        start(&mem).await;
        let sent = mem.invocations();
        assert_eq!(sent.len(), 1, "{id}");
        let req: DirectInvocationRequest = serde_json::from_value(sent[0].clone()).unwrap();
        validate_direct_invocation(&req).unwrap();
        assert_eq!(req.model.id, id);
        assert_eq!(req.failover, builtin().failover_routes(&id), "{id}");
        assert!(
            req.failover.len() >= 7,
            "{id}: {} routes",
            req.failover.len()
        );
        assert!(req.failover.iter().all(|r| r.id != id));
        assert!(sent[0]["failover"].is_array(), "{id}");
        assert!(
            serde_json::to_vec(&sent[0]).unwrap().len() < 16 * 1024,
            "{id}: the body stays far under the 64 KiB limit"
        );
    }
}

#[tokio::test]
async fn the_credentials_are_minted_with_a_policy_that_covers_every_route_and_fits_sts() {
    for id in the_five() {
        let mem = Mem::default();
        mem.seed(repo_target(), "p", &id);
        start(&mem).await;
        let policy = mem.0.minted.lock().unwrap()[0].session_policy.clone();
        assert!(
            policy.chars().count() <= 2048,
            "{id}: {} characters",
            policy.chars().count()
        );
        let mut want = BTreeSet::new();
        let mut all = vec![id.clone()];
        all.extend(builtin().failover_routes(&id).into_iter().map(|r| r.id));
        for r in &all {
            want.insert(format!(
                "arn:aws:bedrock:{REGION}:{ACCOUNT}:inference-profile/{r}"
            ));
            want.insert(format!(
                "arn:aws:bedrock:us-*::foundation-model/{}",
                base(r)
            ));
            if r.starts_with("global.") {
                want.insert(format!("arn:aws:bedrock:::foundation-model/{}", base(r)));
            }
        }
        assert_eq!(leased(&policy), want, "{id}");
        // The refresh endpoint mints again from the Agent item's stored policy: it is the same text.
        assert_eq!(
            mem.agent().unwrap().creds.unwrap().session_policy,
            policy,
            "{id}"
        );
    }
}

#[tokio::test]
async fn no_route_outside_the_catalog_is_ever_granted() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", "us.zai.glm-5.3");
    start(&mem).await;
    let policy = mem.0.minted.lock().unwrap()[0].session_policy.clone();
    for r in leased(&policy) {
        assert!(
            builtin().models().iter().any(|m| r.ends_with(base(&m.id))),
            "{r} is not one of the five models"
        );
    }
}
