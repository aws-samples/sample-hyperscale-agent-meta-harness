//! The session of a Claude Fable 5.2 run on OpenCode. run-session builds the model spec from the OpenCode row (no reasoning level: OpenCode has none) and
//! the failover list from the request: the credentials of the session cover exactly the routes in it, so a run that did not agree to provider data sharing can
//! never reach the sharing model by a switch, and a run on it that did stays on OpenCode's own list.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::common::Engine;
use harness_protocol::direct::DirectInvocationRequest;
use harness_protocol::target::RunTarget;
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use run_session::prepare::{failover_specs_for, model_spec_for};
use serde_json::{Value, json};
use support::fakes::*;

const FABLE: &str = "global.anthropic.claude-fable-5-2";
const OPUS: &str = "us.anthropic.claude-opus-5-5";

async fn start(mem: &Mem) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .unwrap();
    handle(&mem.deps(), &op).await
}

fn invocation(mem: &Mem) -> DirectInvocationRequest {
    serde_json::from_slice(&mem.invocation_bytes(0)).unwrap()
}

#[test]
fn b805_the_spec_of_fable_on_opencode_is_its_own_row_with_no_reasoning_level() {
    let open = model_spec_for(Engine::OPENCODE, FABLE).unwrap();
    assert_eq!(open.id, FABLE);
    assert_eq!(open.region, "us-east-1");
    assert_eq!(
        (open.context_tokens, open.max_output_tokens),
        (1_000_000, 128_000)
    );
    assert_eq!(open.default_reasoning_effort, None);
    // The Cline row of the same model has its reasoning level; the two specs differ only there.
    let cline = model_spec_for(Engine::CLINE, FABLE).unwrap();
    assert_eq!(cline.default_reasoning_effort.as_deref(), Some("high"));
    assert_eq!(
        (
            cline.id,
            cline.region,
            cline.context_tokens,
            cline.max_output_tokens
        ),
        (
            open.id,
            open.region,
            open.context_tokens,
            open.max_output_tokens
        )
    );
}

#[test]
fn b805_the_routes_of_an_opencode_run_follow_the_agreement() {
    let ids = |engine, model, sharing| -> Vec<String> {
        failover_specs_for(engine, model, sharing)
            .into_iter()
            .map(|s| s.id)
            .collect()
    };
    for sharing in [false, true] {
        let routes = ids(Engine::OPENCODE, OPUS, sharing);
        assert!(
            !routes.is_empty() && !routes.iter().any(|i| i == FABLE),
            "{sharing}: {routes:?}"
        );
    }
    let routes = ids(Engine::OPENCODE, FABLE, true);
    assert!(
        routes.iter().any(|i| i == OPUS) && routes.iter().all(|i| i != FABLE),
        "{routes:?}"
    );
}

#[tokio::test]
async fn b805_a_session_on_fable_with_opencode_starts_with_the_engine_the_model_and_routes_that_exclude_it()
 {
    let mem = Mem::default();
    mem.seed_with_members(
        RunTarget::S3Only,
        "Add percent_change(a, b) with tests",
        FABLE,
        json!({"engine": "opencode", "providerSharing": true}),
    );
    start(&mem).await.unwrap();
    let body = invocation(&mem);
    assert_eq!(body.engine, "opencode");
    assert_eq!(body.model.id, FABLE);
    assert_eq!(body.model.default_reasoning_effort, None);
    assert!(!body.failover.is_empty() && body.failover.iter().all(|r| r.id != FABLE));
}

#[tokio::test]
async fn b805_a_session_on_another_opencode_model_never_lists_fable_as_a_route_even_with_the_agreement()
 {
    for sharing in [None, Some(true)] {
        let mem = Mem::default();
        let mut members = json!({"engine": "opencode"});
        if let Some(s) = sharing {
            members["providerSharing"] = json!(s);
        }
        mem.seed_with_members(
            RunTarget::S3Only,
            "Add percent_change(a, b) with tests",
            OPUS,
            members,
        );
        start(&mem).await.unwrap();
        let body = invocation(&mem);
        assert_eq!(body.engine, "opencode");
        assert!(
            body.failover.iter().all(|r| r.id != FABLE),
            "{sharing:?}: {:?}",
            body.failover
        );
    }
}
