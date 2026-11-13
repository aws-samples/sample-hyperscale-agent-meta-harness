//! The engine of a run is on the wire. The start request has an optional `engine`, the Run item stores it, and a row of the model
//! list may say which engine it is for. A request that names no engine is the request of protocol 1.57.0, byte for byte, and runs Cline.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::api::v1::{ModelRow, StartDirectRunRequest};
use harness_protocol::common::{Engine, Protocol};
use harness_protocol::export::all_schemas;
use harness_protocol::store::RunItem;
use serde_json::{Value, json};

fn request_json() -> Value {
    json!({"prompt": "Add percent_change(a, b) with tests", "target": {"mode": "s3-only"}, "model": "us.openai.gpt-6.1-sol"})
}

fn v1() -> Value {
    all_schemas()
        .into_iter()
        .find(|(n, _)| *n == "openapi-v1.json")
        .unwrap()
        .1
}

fn items() -> Value {
    all_schemas()
        .into_iter()
        .find(|(n, _)| *n == "items.schema.json")
        .unwrap()
        .1
}

#[test]
fn b787_the_protocol_is_1_59_0_in_both_openapi_documents() {
    assert_eq!(PROTOCOL_VERSION, "1.59.0");
    let docs: std::collections::BTreeMap<&str, Value> = all_schemas().into_iter().collect();
    assert_eq!(docs["openapi-v1.json"]["info"]["version"], "1.59.0");
    assert_eq!(docs["openapi-internal.json"]["info"]["version"], "1.59.0");
}

#[test]
fn b787_a_request_without_an_engine_keeps_the_bytes_of_protocol_1_57_0() {
    let r: StartDirectRunRequest = serde_json::from_value(request_json()).unwrap();
    assert_eq!(r.engine, None);
    let bytes = serde_json::to_string(&r).unwrap();
    assert_eq!(
        bytes,
        r#"{"prompt":"Add percent_change(a, b) with tests","target":{"mode":"s3-only"},"model":"us.openai.gpt-6.1-sol","rules":[]}"#
    );
    assert!(!bytes.contains("engine"), "{bytes}");
}

#[test]
fn b787_a_request_with_an_engine_round_trips_as_sent() {
    for name in ["cline", "opencode", "pi"] {
        let mut v = request_json();
        v["engine"] = json!(name);
        let r: StartDirectRunRequest = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(r.engine, Some(Engine::new(name).unwrap()), "{name}");
        let back = serde_json::to_value(&r).unwrap();
        assert_eq!(back["engine"], json!(name), "{name}");
        let again: StartDirectRunRequest = serde_json::from_value(back).unwrap();
        assert_eq!(again, r, "{name}");
    }
}

#[test]
fn b787_an_engine_that_is_not_a_profile_name_is_refused_by_the_type() {
    for bad in [
        "",
        "Cline",
        "open code",
        "-x",
        "1cline",
        "a_b",
        "x".repeat(33).as_str(),
    ] {
        let mut v = request_json();
        v["engine"] = json!(bad);
        assert!(
            serde_json::from_value::<StartDirectRunRequest>(v).is_err(),
            "{bad:?} must not parse as an engine name"
        );
    }
    // The member is a string: a number or an object is refused too.
    for bad in [json!(1), json!({"name": "cline"}), json!(["cline"])] {
        let mut v = request_json();
        v["engine"] = bad;
        assert!(serde_json::from_value::<StartDirectRunRequest>(v).is_err());
    }
}

#[test]
fn b787_the_openapi_document_has_the_engine_member_and_does_not_require_it() {
    let doc = v1();
    let schema = &doc["components"]["schemas"]["StartDirectRunRequest"];
    let engine = &schema["properties"]["engine"];
    assert!(engine.is_object(), "StartDirectRunRequest.engine: {engine}");
    let text = engine.to_string();
    assert!(
        text.contains("Engine") || text.contains("^[a-z][a-z0-9-]{0,31}$"),
        "{text}"
    );
    let required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(!required.contains(&"engine"), "{required:?}");
}

fn run_json() -> Value {
    json!({
        "runId": "01M3QA3YCXGFW428XNGR7R8VWD", "ownerPrincipalArn": "arn:aws:sts::123456789012:assumed-role/Admin/user",
        "promptSha256": "cd".repeat(32), "target": {"mode": "s3-only"}, "status": "active",
        "phase": "green", "enginePolicyVersion": 1, "createdAt": "2026-11-08T19:26:27Z",
        "version": 1
    })
}

#[test]
fn b787_the_run_item_stores_the_engine_and_a_run_without_one_writes_nothing() {
    let old: RunItem = serde_json::from_value(run_json()).unwrap();
    assert_eq!(old.engine, None);
    assert!(serde_json::to_value(&old).unwrap().get("engine").is_none());
    let mut v = run_json();
    v["engine"] = json!("opencode");
    let r: RunItem = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(r.engine, Some(Engine::OPENCODE));
    assert_eq!(serde_json::to_value(&r).unwrap(), v);
    let item = items();
    assert!(
        item["$defs"]["RunItem"]["properties"]["engine"].is_object()
            || item["definitions"]["RunItem"]["properties"]["engine"].is_object()
            || item.to_string().contains("\"engine\""),
        "the Run item schema names the engine"
    );
}

fn model_row(engine: Option<&str>) -> ModelRow {
    serde_json::from_value(json!({
        "id": "us.openai.gpt-6.1-sol", "name": "GPT-6.1 Sol", "vendor": "openai", "protocol": "converse", "region": "us-east-1",
        "contextTokens": 1000000, "maxOutputTokens": 128000, "selectable": true, "default": false,
        "engine": engine
    }))
    .unwrap()
}

#[test]
fn b787_a_model_row_names_its_engine_only_when_it_is_not_cline() {
    let cline = model_row(None);
    assert_eq!(cline.engine, None);
    assert!(
        serde_json::to_value(&cline)
            .unwrap()
            .get("engine")
            .is_none()
    );
    let open = model_row(Some("opencode"));
    assert_eq!(open.engine, Some(Engine::OPENCODE));
    assert_eq!(
        serde_json::to_value(&open).unwrap()["engine"],
        json!("opencode")
    );
    assert_eq!(open.protocol, Protocol::Converse);
}

#[test]
fn b787_the_display_name_is_one_function_of_the_engine() {
    assert_eq!(Engine::CLINE.display_name(), "Cline");
    assert_eq!(Engine::OPENCODE.display_name(), "OpenCode");
    // An engine that has no entry is shown by its own name, so a new engine reads right before the table is edited.
    assert_eq!(Engine::new("pi").unwrap().display_name(), "pi");
    assert_eq!(Engine::new("fake-acp").unwrap().display_name(), "fake-acp");
}
