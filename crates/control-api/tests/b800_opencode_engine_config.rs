//! `POST /v1/direct/runs` and the `engineConfig` member of a run on the second engine. The member may hold the shell command
//! time limit and nothing else. The check is the shared one (`ham`, control-api and agentd call it), so the answers here are its words.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, SONNET, World, deps, request};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{EngineConfig, check_engine_config};
use serde_json::{Value, json};

const SOL: &str = "us.openai.gpt-6.1-sol";

fn body(model: &str, engine: Option<&str>, member: Option<Value>) -> Value {
    let mut b = json!({
        "prompt": "Add percent_change(a, b) with tests",
        "target": {"mode": "s3-only"},
        "model": model,
        "rules": []
    });
    if let Some(e) = engine {
        b["engine"] = json!(e);
    }
    if let Some(m) = member {
        b["engineConfig"] = m;
    }
    b
}

fn opencode_member(settings: Value) -> Value {
    json!({"engine": "opencode", "schema": 1, "settings": settings})
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

fn stored_request(w: &World) -> Value {
    let objects = w.objects.lock().unwrap();
    let (_, bytes) = objects
        .iter()
        .find(|(k, _)| k.ends_with("input/request.json"))
        .expect("the stored request");
    serde_json::from_slice(bytes).unwrap()
}

fn untouched(w: &World) {
    assert!(w.runs.lock().unwrap().is_empty(), "no run was created");
    assert!(w.idem.lock().unwrap().is_empty(), "no claim was written");
    assert!(w.started.lock().unwrap().is_empty(), "no execution started");
    assert!(w.objects.lock().unwrap().is_empty(), "no object was stored");
}

#[tokio::test]
async fn b800_a_start_on_the_second_engine_with_the_limit_is_accepted_and_stored_as_sent() {
    for seconds in [1, 600, 900, 1800] {
        let w = World::new();
        let m = opencode_member(json!({"commands": {"timeout_seconds": seconds}}));
        let (status, v) = start(&w, "k-1", &body(SOL, Some("opencode"), Some(m.clone()))).await;
        assert_eq!(status, 201, "{seconds}: {v}");
        let stored = stored_request(&w);
        assert_eq!(stored["engine"], "opencode", "{stored}");
        assert_eq!(stored["engineConfig"], m, "{seconds}: {stored}");
        // The stored request reads back as the typed request the run's agent reads.
        let typed: StartDirectRunRequest = serde_json::from_value(stored).unwrap();
        let cfg = typed.engine_config.unwrap();
        check_engine_config(&cfg).unwrap();
        assert_eq!(cfg.get("commands.timeout_seconds"), Some(&json!(seconds)));
    }
}

#[tokio::test]
async fn b800_a_limit_outside_the_range_is_400_with_the_words_of_the_shared_check_and_writes_nothing()
 {
    for (bad, problem) in [
        (json!(0), "out_of_range"),
        (json!(1801), "out_of_range"),
        (json!("900"), "bad_type"),
        (json!(9.5), "bad_type"),
    ] {
        let w = World::new();
        let m = opencode_member(json!({"commands": {"timeout_seconds": bad}}));
        let (status, v) = start(&w, "k-1", &body(SOL, Some("opencode"), Some(m.clone()))).await;
        assert_eq!(status, 400, "{bad}: {v}");
        assert_eq!(v["code"], "invalid-engine-config", "{bad}: {v}");
        assert_eq!(v["key"], "commands.timeout_seconds", "{bad}: {v}");
        assert_eq!(v["problem"], problem, "{bad}: {v}");
        let cfg: EngineConfig = serde_json::from_value(m).unwrap();
        let expected = check_engine_config(&cfg).unwrap_err();
        assert_eq!(v["problem"], expected.code(), "{bad}: {v}");
        assert_eq!(v["message"], expected.to_string(), "{bad}: {v}");
        untouched(&w);
    }
}

#[tokio::test]
async fn b800_the_other_keys_of_the_first_engine_are_400_unknown_key_on_the_second() {
    for (settings, key) in [
        (json!({"reasoning": {"effort": "high"}}), "reasoning.effort"),
        (
            json!({"context": {"compaction": "off"}}),
            "context.compaction",
        ),
        (
            json!({"tools": {"disable": ["spawn_agent"]}}),
            "tools.disable",
        ),
    ] {
        let w = World::new();
        let m = opencode_member(settings);
        let (status, v) = start(&w, "k-1", &body(SOL, Some("opencode"), Some(m))).await;
        assert_eq!(status, 400, "{key}: {v}");
        assert_eq!(v["code"], "invalid-engine-config", "{key}: {v}");
        assert_eq!(v["problem"], "unknown_key", "{key}: {v}");
        assert_eq!(v["key"], key, "{key}: {v}");
        untouched(&w);
    }
}

#[tokio::test]
async fn b800_a_member_for_the_second_engine_on_a_run_of_the_first_is_refused_naming_both() {
    // The run names no engine, so it runs the default one; the limit would be set and never applied.
    let w = World::new();
    let m = opencode_member(json!({"commands": {"timeout_seconds": 900}}));
    let (status, v) = start(&w, "k-1", &body(SONNET, None, Some(m))).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["code"], "invalid-engine-config", "{v}");
    let words = v["message"].as_str().unwrap();
    assert!(
        words.contains("opencode") && words.contains("cline"),
        "{words}"
    );
    untouched(&w);
}

#[tokio::test]
async fn b800_a_member_for_the_first_engine_on_a_run_of_the_second_is_refused_naming_both() {
    let w = World::new();
    let m =
        json!({"engine": "cline", "schema": 1, "settings": {"commands": {"timeout_seconds": 900}}});
    let (status, v) = start(&w, "k-1", &body(SOL, Some("opencode"), Some(m))).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["code"], "invalid-engine-config", "{v}");
    let words = v["message"].as_str().unwrap();
    assert!(
        words.contains("opencode") && words.contains("cline"),
        "{words}"
    );
    untouched(&w);
}
