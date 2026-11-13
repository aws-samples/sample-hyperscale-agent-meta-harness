//! `POST /v1/direct/runs` for Claude Fable 5.2 on OpenCode. The agreement to provider data sharing is checked for the pair the request names:
//! without `providerSharing: true` the answer is 400 `provider-sharing-required` and nothing is written; with it the run is accepted and the stored request keeps
//! both the engine and the agreement.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, World, deps, request};
use serde_json::{Value, json};

const FABLE: &str = "global.anthropic.claude-fable-5-2";

fn body(sharing: Option<bool>) -> Value {
    let mut b = json!({
        "prompt": "Add percent_change(a, b) with tests",
        "target": {"mode": "s3-only"},
        "model": FABLE,
        "engine": "opencode",
        "rules": []
    });
    if let Some(s) = sharing {
        b["providerSharing"] = json!(s);
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

fn stored_request(w: &World) -> Value {
    let objects = w.objects.lock().unwrap();
    let (_, bytes) = objects
        .iter()
        .find(|(k, _)| k.ends_with("input/request.json"))
        .expect("the stored request");
    serde_json::from_slice(bytes).unwrap()
}

#[tokio::test]
async fn b805_a_start_on_opencode_with_fable_and_no_agreement_is_400_provider_sharing_required_and_writes_nothing()
 {
    for (i, sharing) in [None, Some(false)].into_iter().enumerate() {
        let w = World::new();
        let (status, v) = start(&w, &format!("k-{i}"), &body(sharing)).await;
        assert_eq!(status, 400, "{sharing:?}: {v}");
        assert_eq!(v["code"], "provider-sharing-required", "{v}");
        let message = v["message"].as_str().unwrap();
        for want in ["Claude Fable 5.2", FABLE, "providerSharing"] {
            assert!(message.contains(want), "{want:?} missing: {message}");
        }
        assert!(w.runs.lock().unwrap().is_empty(), "{sharing:?}");
        assert!(w.objects.lock().unwrap().is_empty(), "{sharing:?}");
    }
}

#[tokio::test]
async fn b805_a_start_on_opencode_with_fable_and_the_agreement_is_accepted_and_stored_with_both_members()
 {
    let w = World::new();
    let (status, v) = start(&w, "k-1", &body(Some(true))).await;
    assert_eq!(status, 201, "{v}");
    let stored = stored_request(&w);
    assert_eq!(stored["engine"], "opencode", "{stored}");
    assert_eq!(stored["providerSharing"], true, "{stored}");
    assert_eq!(stored["model"], FABLE, "{stored}");
    let run = w.runs.lock().unwrap().values().next().cloned().unwrap();
    assert_eq!(run.engine, Some(harness_protocol::common::Engine::OPENCODE));
}
