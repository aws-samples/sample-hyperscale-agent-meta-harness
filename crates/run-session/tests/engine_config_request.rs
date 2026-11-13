//! The stored request and the `engineConfig` member.
//!
//! `run-session` reads the request control-api stored with the same strict type (`deny_unknown_fields`). A stored request that carries the
//! new member must pass `prepare` and leave its answer unchanged (the settings are agentd's business, read from the stored request);
//! a stored request that carries a member this build does not know must fail `prepare` with words that name the member, so the deploy
//! order of the runbook (control, runtimes, orchestration) shows up as a loud, named failure and never as a silent drop.
#![allow(clippy::unwrap_used)]

mod support;

use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

async fn prepare(mem: &Mem) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(json!({"op": "prepare", "runId": RUN})).unwrap();
    handle(&mem.deps(), &op).await
}

/// Guard: green before and after the member exists.
#[tokio::test]
async fn an_unknown_member_in_a_stored_request_fails_prepare_naming_the_member() {
    for name in ["engineConfigs", "semanticSearchOn", "someLaterMember"] {
        let mem = Mem::default();
        mem.seed_with_members(
            repo_target(),
            "p",
            MODEL,
            json!({ name: {"enabled": false} }),
        );
        let e = prepare(&mem).await.unwrap_err();
        assert_eq!(e.error_type(), "RunSessionInvalid", "{name}: {e}");
        assert!(e.to_string().contains(name), "{name}: {e}");
    }
}

fn member() -> Value {
    json!({
        "engine": "cline", "schema": 1,
        "settings": {"reasoning": {"effort": "high"}, "tools": {"disable": ["spawn_agent"]}}
    })
}

#[tokio::test]
async fn a_stored_request_with_the_member_passes_prepare_unchanged() {
    let plain = Mem::default();
    plain.seed(repo_target(), "p", MODEL);
    let want = prepare(&plain).await.unwrap();

    let mem = Mem::default();
    mem.seed_with_members(
        repo_target(),
        "p",
        MODEL,
        json!({ "engineConfig": member() }),
    );
    let got = prepare(&mem).await.unwrap();
    assert_eq!(got, want, "the member changes nothing prepare answers");
    assert!(!got.to_string().contains("effort"), "{got}");

    // The member reached the request run-session read, as typed settings, and the stored bytes are the ones control-api wrote.
    let (request, _sha) = run_session::prepare::read_request(&mem, &mem, RUN)
        .await
        .unwrap();
    let cfg = request.engine_config.unwrap();
    assert_eq!((cfg.engine.as_str(), cfg.schema), ("cline", 1));
    assert_eq!(cfg.settings["reasoning"]["effort"], "high");
}
