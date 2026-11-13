//! The `engineConfig` member of the start request and the engine-settings reference in the run summary.
//! The member is additive: a request that leaves it out is byte for byte the request of protocol 1.38.0, and the
//! request type still refuses every member it does not know.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{DirectEngineConfigRef, EngineConfig, check_engine_config};
use harness_protocol::export::all_schemas;
use serde_json::{Value, json};

/// A request with no optional member: the serialization of protocol 1.38.0, the same bytes `tests/wire_direct.rs` pins.
const MINIMAL_1_38_0: &str = r#"{"prompt":"p","target":{"mode":"s3-only"},"model":"m","rules":[]}"#;

/// A request that sets every optional member of protocol 1.38.0, in the order the type writes them.
const FULL_1_38_0: &str = concat!(
    r#"{"prompt":"p","target":{"mode":"s3-only"},"baseRef":"main","model":"m","rules":[],"#,
    r#""nudge":{"text":"go on","sha256":"b","maxNudges":2},"inputWaitS":1800,"#,
    r#""research":{"enabled":false},"reportTests":false,"goal":{"enabled":false},"thirdParty":"allow"}"#
);

#[test]
fn the_member_is_left_out_when_absent_and_a_default_request_serializes_as_1_38_0() {
    for golden in [MINIMAL_1_38_0, FULL_1_38_0] {
        let r: StartDirectRunRequest = serde_json::from_str(golden).unwrap();
        assert!(r.engine_config.is_none());
        assert_eq!(serde_json::to_string(&r).unwrap(), golden);
        assert!(!golden.contains("engineConfig"));
    }
}

#[test]
fn an_unknown_member_is_still_refused_and_named() {
    for name in ["engineConfigs", "engine_config", "someLaterMember", "later"] {
        let mut v: serde_json::Value = serde_json::from_str(MINIMAL_1_38_0).unwrap();
        v[name] = json!({"engine": "cline", "schema": 1, "settings": {}});
        let e = serde_json::from_value::<StartDirectRunRequest>(v).unwrap_err();
        assert!(e.to_string().contains(name), "{name}: {e}");
    }
}

fn with_member(member: Value) -> Value {
    let mut v: Value = serde_json::from_str(MINIMAL_1_38_0).unwrap();
    v["engineConfig"] = member;
    v
}

fn good_member() -> Value {
    json!({
        "engine": "cline", "schema": 1,
        "settings": {"reasoning": {"effort": "high"}, "tools": {"disable": ["fetch_web_content"]}}
    })
}

#[test]
fn the_member_round_trips_and_an_unknown_member_is_still_refused() {
    let v = with_member(good_member());
    let r: StartDirectRunRequest = serde_json::from_value(v.clone()).unwrap();
    let cfg = r.engine_config.clone().unwrap();
    assert_eq!((cfg.engine.as_str(), cfg.schema), ("cline", 1));
    assert_eq!(cfg.settings["reasoning"]["effort"], "high");
    assert_eq!(check_engine_config(&cfg), Ok(()));
    // The member is camelCase on the wire.
    assert_eq!(serde_json::to_value(&r).unwrap(), v);
    let text = serde_json::to_string(&r).unwrap();
    assert!(
        text.contains(r#""engineConfig":{"engine":"cline","schema":1,"settings":{"#),
        "{text}"
    );
    // Serialization is stable: parse and write again gives the same bytes.
    let again: StartDirectRunRequest = serde_json::from_str(&text).unwrap();
    assert_eq!(serde_json::to_string(&again).unwrap(), text);

    // The member is strict: a member this build does not know, inside it or beside it, is refused and named.
    let mut inner = good_member();
    inner["extra"] = json!(1);
    let e = serde_json::from_value::<StartDirectRunRequest>(with_member(inner)).unwrap_err();
    assert!(e.to_string().contains("extra"), "{e}");
    let mut beside = with_member(good_member());
    beside["engineConfigs"] = json!({});
    let e = serde_json::from_value::<StartDirectRunRequest>(beside).unwrap_err();
    assert!(e.to_string().contains("engineConfigs"), "{e}");
}

#[test]
fn the_member_needs_all_three_parts_and_an_object_of_settings() {
    for bad in [
        json!({"schema": 1, "settings": {}}),
        json!({"engine": "cline", "settings": {}}),
        json!({"engine": "cline", "schema": 1}),
        json!({"engine": "cline", "schema": 1, "settings": "high"}),
        json!({"engine": "cline", "schema": 1, "settings": ["high"]}),
        json!({"engine": "cline", "schema": 1, "settings": null}),
        json!({"engine": 1, "schema": 1, "settings": {}}),
        json!({"engine": "cline", "schema": "1", "settings": {}}),
        json!({"engine": "cline", "schema": -1, "settings": {}}),
        json!({"engine": "cline", "schema": 1.5, "settings": {}}),
        json!("reasoning.effort=high"),
        json!([]),
    ] {
        assert!(
            serde_json::from_value::<StartDirectRunRequest>(with_member(bad.clone())).is_err(),
            "{bad}"
        );
    }
    // `null` is the same as leaving the member out, as for every other optional member, and it is not stored.
    let none: StartDirectRunRequest = serde_json::from_value(with_member(json!(null))).unwrap();
    assert!(none.engine_config.is_none());
    assert!(
        !serde_json::to_string(&none)
            .unwrap()
            .contains("engineConfig")
    );

    // A schema number beyond u32 still reads, so the check (not the type) says `bad_schema`.
    let big: StartDirectRunRequest = serde_json::from_value(with_member(
        json!({"engine": "cline", "schema": 4_294_967_296u64, "settings": {}}),
    ))
    .unwrap();
    assert_eq!(
        check_engine_config(&big.engine_config.unwrap())
            .unwrap_err()
            .code(),
        "bad_schema"
    );
}

#[test]
fn the_member_is_part_of_the_exported_contract_and_the_version_is_1_39_0() {
    assert_eq!(PROTOCOL_VERSION, "1.59.0");
    let docs: std::collections::BTreeMap<&str, Value> = all_schemas().into_iter().collect();
    let v1 = &docs["openapi-v1.json"]["components"]["schemas"];
    assert_eq!(docs["openapi-v1.json"]["info"]["version"], "1.59.0");
    assert_eq!(docs["openapi-internal.json"]["info"]["version"], "1.59.0");
    assert!(v1["StartDirectRunRequest"]["properties"]["engineConfig"].is_object());
    let member = &v1["EngineConfig"];
    assert_eq!(member["additionalProperties"], json!(false));
    let mut required: Vec<&str> = member["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect();
    required.sort_unstable();
    assert_eq!(required, ["engine", "schema", "settings"]);
    assert_eq!(member["properties"]["settings"]["type"], "object");
    let runtime = &docs["runtime.schema.json"]["$defs"];
    assert!(runtime["DirectSummary"]["properties"]["engineConfig"].is_object());
    assert!(runtime["DirectEngineConfigRef"]["properties"]["sha256"].is_object());
    // The member is optional: neither the request nor the summary requires it.
    for (doc, name) in [
        (&v1["StartDirectRunRequest"], "request"),
        (&runtime["DirectSummary"], "summary"),
    ] {
        let required = doc["required"].as_array().unwrap();
        assert!(!required.iter().any(|r| r == "engineConfig"), "{name}");
    }
}

#[test]
fn the_summary_reference_is_hash_and_count_only() {
    let cfg: EngineConfig = serde_json::from_value(good_member()).unwrap();
    let r: DirectEngineConfigRef = cfg.to_ref();
    assert_eq!(r.keys, 2);
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(
        v,
        json!({"engine": "cline", "schema": 1, "sha256": cfg.canonical_sha256(), "keys": 2})
    );
    // No value of the settings is in the reference.
    assert!(!v.to_string().contains("high") && !v.to_string().contains("fetch_web_content"));
    let back: DirectEngineConfigRef = serde_json::from_value(v).unwrap();
    assert_eq!(back, r);
}
