//! Provider data sharing on the wire: the start request gains the optional member `providerSharing`, the model list gains the
//! optional row member `requiresProviderSharing`. Both are additive and left off the wire at their default, so the request of a client that never turns
//! the setting on, and the list row of a model that needs nothing, are byte for byte what protocol 1.56.0 carried.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::api::v1::{ModelRow, StartDirectRunRequest};
use serde_json::{Value, json};

fn base() -> Value {
    json!({
        "prompt": "add a function", "target": {"mode": "s3-only"}, "model": "us.openai.gpt-6.1-sol", "rules": []
    })
}

fn parse(extra: Value) -> Result<StartDirectRunRequest, serde_json::Error> {
    let mut v = base();
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    serde_json::from_value(v)
}

#[test]
fn b803_the_protocol_is_1_59_0() {
    assert_eq!(PROTOCOL_VERSION, "1.59.0");
}

#[test]
fn b803_the_member_is_optional_and_left_off_the_wire_when_absent() {
    let plain = parse(json!({})).unwrap();
    assert_eq!(plain.provider_sharing, None);
    let wire = serde_json::to_value(&plain).unwrap();
    assert!(wire.get("providerSharing").is_none(), "{wire}");
}

#[test]
fn b803_provider_sharing_true_round_trips_under_the_camel_case_name() {
    let on = parse(json!({"providerSharing": true})).unwrap();
    assert_eq!(on.provider_sharing, Some(true));
    let wire = serde_json::to_value(&on).unwrap();
    assert_eq!(wire["providerSharing"], json!(true));
    let back: StartDirectRunRequest = serde_json::from_value(wire).unwrap();
    assert_eq!(back, on);
}

#[test]
fn b803_a_non_boolean_provider_sharing_is_refused() {
    for bad in [
        json!("on"),
        json!(1),
        json!({"enabled": true}),
        json!([true]),
    ] {
        assert!(
            parse(json!({"providerSharing": bad.clone()})).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn b803_the_old_snake_case_spelling_is_still_an_unknown_member() {
    assert!(parse(json!({"provider_sharing": true})).is_err());
}

fn row(extra: Value) -> Value {
    let mut v = json!({
        "id": "x.y", "name": "X", "vendor": "v", "protocol": "converse", "region": "us-east-1",
        "contextTokens": 1000, "maxOutputTokens": 100, "selectable": true, "default": false
    });
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    v
}

#[test]
fn b803_a_model_row_that_needs_nothing_has_no_sharing_member() {
    let r: ModelRow = serde_json::from_value(row(json!({}))).unwrap();
    assert!(!r.requires_provider_sharing);
    let wire = serde_json::to_value(&r).unwrap();
    assert!(wire.get("requiresProviderSharing").is_none(), "{wire}");
}

#[test]
fn b803_a_model_row_that_needs_sharing_says_so() {
    let r: ModelRow =
        serde_json::from_value(row(json!({"requiresProviderSharing": true}))).unwrap();
    assert!(r.requires_provider_sharing);
    let wire = serde_json::to_value(&r).unwrap();
    assert_eq!(wire["requiresProviderSharing"], json!(true));
    let back: ModelRow = serde_json::from_value(wire).unwrap();
    assert_eq!(back, r);
}

#[test]
fn b803_the_contract_files_describe_both_members() {
    let docs: std::collections::BTreeMap<&str, Value> = harness_protocol::export::all_schemas()
        .into_iter()
        .collect();
    let v1 = &docs["openapi-v1.json"];
    let req = &v1["components"]["schemas"]["StartDirectRunRequest"]["properties"];
    assert_eq!(req["providerSharing"]["type"], json!(["boolean", "null"]));
    let model_row = &v1["components"]["schemas"]["ModelRow"]["properties"];
    assert_eq!(model_row["requiresProviderSharing"]["type"], "boolean");
}
