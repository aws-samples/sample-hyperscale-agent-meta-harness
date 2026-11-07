//! `POST /v1/direct/runs` and the reasoning level of the model. The key `reasoning.effort` of the engine
//! settings now takes `default` (the model's catalog level), `off` and the four launch levels. The stage keeps a user from asking for a level the
//! chosen model's row of `engine/catalog/models.json` does not list as accepted: 422 `reasoning-level-not-accepted`, the accepted levels in the
//! words, nothing written. `default` and `off` are never checked against the row. A request without the member is not checked at all.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, SONNET, World, deps, request};
use control_api::direct::{REASONING_REFUSAL_STATUS, reasoning_refusal};
use ham_core::models::Models;
use harness_protocol::api::v1::StartDirectRunRequest;
use serde_json::{Value, json};

const MODELS_JSON: &str = include_str!("../../../engine/catalog/models.json");

fn body(model: &str, effort: Option<&str>) -> Value {
    let mut b = json!({
        "prompt": "Add percent_change(a, b) with tests",
        "target": {"mode": "s3-only"},
        "model": model,
        "rules": []
    });
    if let Some(e) = effort {
        b["engineConfig"] =
            json!({"engine": "cline", "schema": 1, "settings": {"reasoning": {"effort": e}}});
    }
    b
}

async fn start(w: &World, key: &str, b: &Value) -> (u16, Value) {
    let mut r = request(ME, "POST", "/v1/direct/runs", &[], &b.to_string());
    r.headers.insert("idempotency-key".into(), key.into());
    let out = control_api::handler::handle(r, &deps(w)).await;
    (
        out.status,
        serde_json::from_str(&out.body).unwrap_or(Value::Null),
    )
}

fn parsed(b: &Value) -> StartDirectRunRequest {
    serde_json::from_value(b.clone()).unwrap()
}

fn catalog_with_sonnet_accepting(levels: &[&str]) -> Models {
    let mut v: Value = serde_json::from_str(MODELS_JSON).unwrap();
    let row = v["models"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|m| m["id"] == SONNET)
        .unwrap();
    row["reasoning"]["accepted"] = json!(levels);
    row["reasoning"]["default"] = json!(levels[0]);
    Models::parse(&v.to_string()).unwrap()
}

#[tokio::test]
async fn every_value_of_the_key_starts_a_run_on_every_model() {
    let models = ham_core::models::builtin();
    for m in models.models() {
        for (i, effort) in ["default", "off", "low", "medium", "high", "xhigh"]
            .into_iter()
            .enumerate()
        {
            let w = World::new();
            let (status, v) = start(&w, &format!("k-{i}"), &body(&m.id, Some(effort))).await;
            assert_eq!(status, 201, "{} {effort}: {v}", m.id);
        }
    }
}

#[test]
fn an_explicit_level_the_row_does_not_accept_is_422_with_the_code_and_the_accepted_levels() {
    let models = catalog_with_sonnet_accepting(&["low", "medium", "high"]);
    let e = reasoning_refusal(&models, &parsed(&body(SONNET, Some("xhigh")))).expect("a refusal");
    assert_eq!(e.code(), "reasoning-level-not-accepted");
    let message = e.to_string();
    assert!(
        message.contains("xhigh") && message.contains("low, medium, high"),
        "{message}"
    );
    // The reply of the stage is a 422, like the model check next to it.
    assert_eq!(REASONING_REFUSAL_STATUS, 422);
    // The accepted levels pass.
    for level in ["low", "medium", "high"] {
        assert!(
            reasoning_refusal(&models, &parsed(&body(SONNET, Some(level)))).is_none(),
            "{level}"
        );
    }
}

#[test]
fn default_and_off_are_never_checked_against_the_row() {
    let models = catalog_with_sonnet_accepting(&["low"]);
    for effort in ["default", "off"] {
        assert!(
            reasoning_refusal(&models, &parsed(&body(SONNET, Some(effort)))).is_none(),
            "{effort}"
        );
    }
}

#[test]
fn a_request_without_the_member_or_without_the_key_is_not_checked() {
    let models = catalog_with_sonnet_accepting(&["low"]);
    assert!(reasoning_refusal(&models, &parsed(&body(SONNET, None))).is_none());
    let mut b = body(SONNET, None);
    b["engineConfig"] =
        json!({"engine": "cline", "schema": 1, "settings": {"commands": {"timeout_seconds": 900}}});
    assert!(reasoning_refusal(&models, &parsed(&b)).is_none());
}

#[tokio::test]
async fn a_refused_level_writes_nothing() {
    // The shipped catalog accepts every launch level, so the refusal is reached through a model the catalog does not know: that is the model
    // check's 422, and no run, goal, claim or object exists afterwards.
    let w = World::new();
    let (status, v) = start(&w, "k-x", &body("us.vendor.unknown-model", Some("xhigh"))).await;
    assert_eq!(status, 422, "{v}");
    assert_eq!(v["code"], "unknown-model", "{v}");
    assert!(w.runs.lock().unwrap().is_empty());
    assert!(w.objects.lock().unwrap().is_empty());
}
