//! `POST /v1/direct/runs` with the `compute` member. The member is checked by the request type itself (an unknown word or
//! member is a 400 `bad-request` that never repeats the value, before anything is written) and stored as sent. A request for `instances` on a stage
//! that has no Instances worker runtime is refused at the start (422 `compute-unavailable`, nothing written) with the words that say what to do;
//! `microvm`, `auto` and a request with no member never look at the stage's Instances runtime.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, SONNET, World, deps, request};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{ComputeMode, ComputeOptions};
use harness_protocol::store::s3;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

fn body(compute: Option<Value>) -> Value {
    let mut b = json!({
        "prompt": "Add percent_change(a, b) with tests",
        "target": {"mode": "s3-only"},
        "model": SONNET,
        "rules": []
    });
    if let Some(c) = compute {
        b["compute"] = c;
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

fn untouched(w: &World) {
    assert!(w.runs.lock().unwrap().is_empty(), "no run was created");
    assert!(w.idem.lock().unwrap().is_empty(), "no claim was written");
    assert!(w.started.lock().unwrap().is_empty(), "no execution started");
    assert_eq!(w.count("blob_put"), 0, "no blob was written");
}

fn stored_text(w: &World, run_id: &str) -> String {
    let g = w.objects.lock().unwrap();
    String::from_utf8(g[&s3::request_key(run_id)].clone()).unwrap()
}

#[tokio::test]
async fn an_unknown_word_or_shape_is_a_400_that_never_repeats_the_value() {
    for (i, bad) in [
        json!({"mode": "SENTINEL-gpu-4c2f"}),
        json!({"mode": "Instances"}),
        json!({"mode": 3}),
        json!({}),
        json!({"mode": "instances", "SENTINEL-extra-4c2f": 1}),
        json!("SENTINEL-instances-4c2f"),
    ]
    .into_iter()
    .enumerate()
    {
        let w = World::new();
        w.instances.store(true, Ordering::SeqCst);
        let (status, v) = start(&w, &format!("bad-{i}"), &body(Some(bad.clone()))).await;
        assert_eq!(status, 400, "{bad}: {v}");
        assert_eq!(v["code"], "bad-request", "{bad}: {v}");
        assert!(!v.to_string().contains("SENTINEL"), "{bad}: {v}");
        untouched(&w);
    }
}

#[tokio::test]
async fn microvm_auto_and_no_member_are_stored_as_sent_and_never_ask_the_stage() {
    let w = World::new();
    // The stage has no Instances runtime and its lookup is down: none of these requests may look.
    w.instances_down.store(true, Ordering::SeqCst);
    let (status, v) = start(&w, "k-1", &body(Some(json!({"mode": "microvm"})))).await;
    assert_eq!(status, 201, "{v}");
    let text = stored_text(&w, v["runId"].as_str().unwrap());
    assert!(text.contains(r#""compute":{"mode":"microvm"}"#), "{text}");
    let req: StartDirectRunRequest = serde_json::from_str(&text).unwrap();
    assert_eq!(
        req.compute,
        Some(ComputeOptions {
            mode: ComputeMode::Microvm
        })
    );

    let (status, v) = start(&w, "k-2", &body(Some(json!({"mode": "auto"})))).await;
    assert_eq!(status, 201, "{v}");
    assert!(stored_text(&w, v["runId"].as_str().unwrap()).contains(r#""compute":{"mode":"auto"}"#));

    let (status, v) = start(&w, "k-3", &body(None)).await;
    assert_eq!(status, 201, "{v}");
    assert!(
        !stored_text(&w, v["runId"].as_str().unwrap()).contains("compute"),
        "a request that keeps the defaults is the request it was"
    );
    assert_eq!(w.count("instances_available"), 0, "the stage was not asked");
}

#[tokio::test]
async fn instances_is_stored_as_sent_when_the_stage_has_the_runtime() {
    let w = World::new();
    w.instances.store(true, Ordering::SeqCst);
    let (status, v) = start(&w, "k-4", &body(Some(json!({"mode": "instances"})))).await;
    assert_eq!(status, 201, "{v}");
    let text = stored_text(&w, v["runId"].as_str().unwrap());
    assert!(text.contains(r#""compute":{"mode":"instances"}"#), "{text}");
    assert_eq!(w.count("instances_available"), 1);
}

#[tokio::test]
async fn instances_on_a_stage_without_the_runtime_is_refused_before_anything_is_written() {
    let w = World::new();
    let (status, v) = start(&w, "k-5", &body(Some(json!({"mode": "instances"})))).await;
    assert_eq!(status, 422, "{v}");
    assert_eq!(v["code"], "compute-unavailable", "{v}");
    let message = v["message"].as_str().unwrap();
    assert!(
        message.contains("Instances")
            && message.contains("--compute microvm")
            && message.contains("run.compute"),
        "the refusal says what to do: {message}"
    );
    untouched(&w);
}

#[tokio::test]
async fn a_failed_lookup_is_a_503_that_leaves_nothing_behind() {
    let w = World::new();
    w.instances_down.store(true, Ordering::SeqCst);
    let (status, v) = start(&w, "k-6", &body(Some(json!({"mode": "instances"})))).await;
    assert_eq!(status, 503, "{v}");
    assert_eq!(v["code"], "unavailable", "{v}");
    untouched(&w);
}
