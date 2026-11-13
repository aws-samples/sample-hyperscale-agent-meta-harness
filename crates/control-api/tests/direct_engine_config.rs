//! `POST /v1/direct/runs` and the `engineConfig` member.
//!
//! The guard test below is green before and after the member exists: a request that leaves the member out stores the very bytes
//! the previous release (protocol 1.38.0) stored, so the idempotency hash of every existing client is unchanged.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, SONNET, World, deps, request};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{DENIED, EngineConfig, check_engine_config};
use harness_protocol::store::s3;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn body() -> Value {
    json!({
        "prompt": "Add percent_change(a, b) with tests",
        "target": {"mode": "s3-only"},
        "model": SONNET,
        "rules": []
    })
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

fn stored_bytes(w: &World, run_id: &str) -> Vec<u8> {
    w.objects.lock().unwrap()[&s3::request_key(run_id)].clone()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// SHA-256 of the stored `request.json` of `body()`, captured by running this test on the previous release (protocol 1.38.0).
const STORED_SHA256_AT_1_38_0: &str =
    "346fa8c34e4a2fb9ac5dfff04a05d1debe3478b8028f24655a3080c6f4c3c67d";

#[tokio::test]
async fn a_request_without_the_member_stores_the_same_bytes_as_before() {
    let w = World::new();
    let (status, v) = start(&w, "k-1", &body()).await;
    assert_eq!(status, 201, "{v}");
    let bytes = stored_bytes(&w, v["runId"].as_str().unwrap());
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert!(!text.contains("engineConfig"), "{text}");
    assert_eq!(sha256_hex(&bytes), STORED_SHA256_AT_1_38_0, "{text}");
}

// ---- the member --------------------------------------------------------------------------------------------------------------

fn member() -> Value {
    json!({
        "engine": "cline", "schema": 1,
        "settings": {"reasoning": {"effort": "high"}, "commands": {"timeout_seconds": 900}}
    })
}

fn with(member: Value) -> Value {
    let mut b = body();
    b["engineConfig"] = member;
    b
}

fn untouched(w: &World) {
    assert!(w.runs.lock().unwrap().is_empty(), "no run was created");
    assert!(w.goals.lock().unwrap().is_empty(), "no goal was created");
    assert!(w.idem.lock().unwrap().is_empty(), "no claim was written");
    assert!(w.started.lock().unwrap().is_empty(), "no execution started");
    assert!(w.objects.lock().unwrap().is_empty(), "no object was stored");
    assert_eq!(w.count("blob_put"), 0, "no blob was written");
    assert!(
        w.calls.lock().unwrap().is_empty(),
        "the store, the blobs and the execution saw no call: {:?}",
        w.calls.lock().unwrap()
    );
}

fn stored(w: &World, run_id: &str) -> StartDirectRunRequest {
    serde_json::from_slice(&stored_bytes(w, run_id)).unwrap()
}

#[tokio::test]
async fn a_bad_member_is_400_invalid_engine_config_and_writes_nothing() {
    const SECRET: &str = "VALUE-SENTINEL-9d41";
    let cases: Vec<(&str, Value, &str, Option<&str>)> = vec![
        (
            "a denied key",
            json!({"engine": "cline", "schema": 1, "settings": {"aws": {"profile": SECRET}}}),
            "denied_key",
            Some("aws.profile"),
        ),
        (
            "a level that does not exist",
            json!({"engine": "cline", "schema": 1, "settings": {"reasoning": {"effort": "max"}}}),
            "out_of_range",
            Some("reasoning.effort"),
        ),
        (
            "an unknown key",
            json!({"engine": "cline", "schema": 1, "settings": {"nope": SECRET}}),
            "unknown_key",
            Some("nope"),
        ),
        (
            "an unusual key",
            json!({"engine": "cline", "schema": 1, "settings": {"\u{1b}[31m": SECRET}}),
            "unknown_key",
            None,
        ),
        (
            "another engine",
            json!({"engine": "pi", "schema": 1, "settings": {}}),
            "wrong_engine",
            None,
        ),
        (
            "a newer schema",
            json!({"engine": "cline", "schema": 2, "settings": {}}),
            "bad_schema",
            None,
        ),
        (
            "settings larger than 4 KiB",
            json!({"engine": "cline", "schema": 1, "settings": {"zz": "x".repeat(5000)}}),
            "too_large",
            None,
        ),
        (
            "tables nested too deep",
            json!({"engine": "cline", "schema": 1, "settings": {"a": {"b": {"c": {"d": 1}}}}}),
            "too_deep",
            None,
        ),
    ];
    for (name, m, problem, key) in cases {
        let w = World::new();
        let (status, v) = start(&w, "k-1", &with(m.clone())).await;
        assert_eq!(status, 400, "{name}: {v}");
        assert_eq!(v["code"], "invalid-engine-config", "{name}: {v}");
        assert_eq!(v["problem"], problem, "{name}: {v}");
        match key {
            Some(k) => assert_eq!(v["key"], k, "{name}: {v}"),
            None => assert!(v.get("key").is_none(), "{name}: {v}"),
        }
        // The words are the words of the one check that `ham` and agentd also call.
        let cfg: EngineConfig = serde_json::from_value(m).unwrap();
        let expected = check_engine_config(&cfg).unwrap_err();
        assert_eq!(v["message"], expected.to_string(), "{name}");
        assert_eq!(v["rule"], expected.rule(), "{name}");
        // The reply names the key and the rule and never repeats a value.
        assert!(!v.to_string().contains(SECRET), "{name}: {v}");
        assert!(!v.to_string().contains('\u{1b}'), "{name}: {v}");
        untouched(&w);
    }
    // the denied key carries the short reason of its class
    let w = World::new();
    let (_, v) = start(
        &w,
        "k-1",
        &with(json!({"engine": "cline", "schema": 1, "settings": {"aws": {"profile": "p"}}})),
    )
    .await;
    assert_eq!(v["rule"], DENIED[0].reason);
}

#[tokio::test]
async fn a_member_of_the_wrong_shape_is_a_400_and_nothing_is_written() {
    for bad in [
        json!("high"),
        json!([]),
        json!({"engine": "cline", "schema": 1}),
        json!({"engine": "cline", "schema": 1, "settings": "high"}),
        json!({"engine": "cline", "schema": 1, "settings": {}, "extra": true}),
    ] {
        let w = World::new();
        let (status, v) = start(&w, "k-1", &with(bad.clone())).await;
        assert_eq!(status, 400, "{bad}: {v}");
        assert_eq!(v["code"], "bad-request", "{bad}: {v}");
        untouched(&w);
    }
}

#[tokio::test]
async fn a_good_member_is_stored_as_sent_and_an_edit_is_another_request() {
    let w = World::new();
    let (status, v) = start(&w, "k-1", &with(member())).await;
    assert_eq!(status, 201, "{v}");
    let id = v["runId"].as_str().unwrap().to_string();
    let s = stored(&w, &id);
    let want: EngineConfig = serde_json::from_value(member()).unwrap();
    assert_eq!(s.engine_config, Some(want));
    let text = String::from_utf8(stored_bytes(&w, &id)).unwrap();
    assert!(
        text.contains(r#""engineConfig":{"engine":"cline","schema":1,"settings":{"#),
        "{text}"
    );

    // The same file again is the same run.
    let (status, again) = start(&w, "k-1", &with(member())).await;
    assert_eq!(status, 201, "{again}");
    assert_eq!(again["runId"], v["runId"]);

    // A file that differs only in the order of its keys stores the same bytes: the order is not a setting.
    let reordered = with(json!({
        "settings": {"commands": {"timeout_seconds": 900}, "reasoning": {"effort": "high"}},
        "schema": 1, "engine": "cline"
    }));
    let (status, same) = start(&w, "k-1", &reordered).await;
    assert_eq!(status, 201, "{same}");
    assert_eq!(same["runId"], v["runId"]);

    // An edited file under the same key is another request (the member is part of the idempotency hash) ...
    let mut edited = member();
    edited["settings"]["reasoning"]["effort"] = json!("low");
    let (status, v2) = start(&w, "k-1", &with(edited.clone())).await;
    assert_eq!(
        (status, v2["code"].as_str()),
        (422, Some("idempotency-key-reused")),
        "{v2}"
    );
    // ... and under a new key it is a new run that carries the new settings.
    let (status, v3) = start(&w, "k-2", &with(edited)).await;
    assert_eq!(status, 201, "{v3}");
    assert_ne!(v3["runId"], v["runId"]);
    let s3 = stored(&w, v3["runId"].as_str().unwrap());
    assert_eq!(
        s3.engine_config.unwrap().settings["reasoning"]["effort"],
        "low"
    );
}

#[tokio::test]
async fn a_settings_object_with_no_key_is_accepted_and_stored() {
    let w = World::new();
    let m = json!({"engine": "cline", "schema": 1, "settings": {}});
    let (status, v) = start(&w, "k-1", &with(m)).await;
    assert_eq!(status, 201, "{v}");
    let s = stored(&w, v["runId"].as_str().unwrap());
    assert_eq!(s.engine_config.unwrap().settings.len(), 0);
}

/// The hash of a member counts a number as serde_json prints it. serde_json's `arbitrary_precision` feature, which no crate of the workspace
/// enables, would keep the spelling of `0.90`; the unified features of THIS binary's graph are what the Lambda runs with.
#[test]
fn this_build_reads_numbers_as_values_and_not_as_their_spelling() {
    let a: Value = serde_json::from_str("0.90").unwrap();
    let b: Value = serde_json::from_str("0.9").unwrap();
    assert_eq!(a.to_string(), b.to_string());
    assert_eq!(a.to_string(), "0.9");
}
