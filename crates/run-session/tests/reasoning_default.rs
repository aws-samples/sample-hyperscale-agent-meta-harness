//! `start-session` and the reasoning level of the model: the invocation's model spec carries the level the
//! model's row of `engine/catalog/models.json` gives, and so does each failover route, so agentd can launch the engine with reasoning on when the user
//! keeps `default`. The catalog is the one source: no level is written in this crate.
#![allow(clippy::unwrap_used)]

mod support;

use ham_core::models::builtin;
use harness_protocol::direct::{DirectInvocationRequest, validate_direct_invocation};
use run_session::ops::{Op, handle};
use serde_json::json;
use support::fakes::*;

async fn start(mem: &Mem) {
    let op: Op = serde_json::from_value(
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .unwrap();
    handle(&mem.deps(), &op).await.unwrap();
}

#[tokio::test]
async fn the_invocation_carries_the_catalog_level_of_the_chosen_model_and_of_each_route() {
    for m in builtin().models() {
        let mem = Mem::default();
        mem.seed(repo_target(), "p", &m.id);
        start(&mem).await;
        let sent = mem.invocations();
        assert_eq!(sent.len(), 1, "{}", m.id);
        let level = m.reasoning_default().unwrap();
        assert_eq!(
            sent[0]["model"]["defaultReasoningEffort"],
            json!(level),
            "{}",
            m.id
        );
        let req: DirectInvocationRequest = serde_json::from_value(sent[0].clone()).unwrap();
        validate_direct_invocation(&req).unwrap();
        assert_eq!(req.model.default_reasoning_effort.as_deref(), Some(level));
        assert!(!req.failover.is_empty());
        for route in &req.failover {
            let home = builtin().find_route(&route.id).unwrap();
            assert_eq!(
                route.default_reasoning_effort.as_deref(),
                home.reasoning_default(),
                "{} -> {}",
                m.id,
                route.id
            );
            assert!(route.default_reasoning_effort.is_some(), "{}", route.id);
        }
    }
}

#[tokio::test]
async fn the_model_spec_helper_reads_the_same_row() {
    for m in builtin().models() {
        let spec = run_session::prepare::model_spec(&m.id).unwrap();
        assert_eq!(
            spec.default_reasoning_effort.as_deref(),
            m.reasoning_default(),
            "{}",
            m.id
        );
    }
}
