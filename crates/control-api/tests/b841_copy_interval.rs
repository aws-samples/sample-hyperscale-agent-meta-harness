//! `POST /v1/direct/runs` with the copy interval: `snapshotEveryS`, the seconds between two looks of the worker at the work
//! tree for a copy of the run's work, from the user's `run.copy_every_minutes`. A value outside 60 to 3,600 is a 400 that names the member and the
//! range before anything is written. 0 is refused too: the worker's invocation knows it as "no copies while the engine works", and a start request
//! cannot ask for that. A good value is stored as sent, so run-session reads exactly what the user set; a request without the member stores none.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, SONNET, World, deps, request};
use harness_protocol::store::s3;
use serde_json::{Value, json};

fn body(extra: Value) -> Value {
    let mut b = json!({
        "prompt": "Add percent_change(a, b) with tests",
        "target": {"mode": "s3-only"},
        "model": SONNET,
        "rules": []
    });
    for (k, v) in extra.as_object().unwrap() {
        b[k] = v.clone();
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

/// The stored start request of the run, as JSON.
fn stored(w: &World, run_id: &str) -> Value {
    let g = w.objects.lock().unwrap();
    serde_json::from_slice(&g[&s3::request_key(run_id)]).unwrap()
}

#[tokio::test]
async fn b841_copy_an_interval_inside_the_range_is_stored_as_sent() {
    for s in [60u32, 600, 3_600] {
        let w = World::new();
        let (status, v) = start(&w, "k-1", &body(json!({"snapshotEveryS": s}))).await;
        assert_eq!(status, 201, "{s}: {v}");
        let kept = stored(&w, v["runId"].as_str().unwrap());
        assert_eq!(kept["snapshotEveryS"], json!(s), "{kept}");
    }
}

#[tokio::test]
async fn b841_copy_a_request_without_the_member_stores_none() {
    let w = World::new();
    let (status, v) = start(&w, "k-2", &body(json!({}))).await;
    assert_eq!(status, 201, "{v}");
    let kept = stored(&w, v["runId"].as_str().unwrap());
    assert!(kept.get("snapshotEveryS").is_none(), "{kept}");
}

#[tokio::test]
async fn b841_copy_an_interval_outside_the_range_is_refused_with_the_member_and_the_range_and_nothing_is_written()
 {
    for s in [0u32, 1, 59, 3_601, 86_400] {
        let w = World::new();
        let (status, v) = start(&w, "k-3", &body(json!({"snapshotEveryS": s}))).await;
        assert_eq!(status, 400, "{s}: {v}");
        assert_eq!(v["code"], "bad-request", "{s}: {v}");
        let message = v["message"].as_str().unwrap();
        assert!(
            message.contains("snapshotEveryS") && message.contains("60 to 3600"),
            "{s}: {message}"
        );
        untouched(&w);
    }
}

#[tokio::test]
async fn b841_copy_the_refusal_of_zero_says_that_the_copies_cannot_be_switched_off() {
    let w = World::new();
    let (status, v) = start(&w, "k-4", &body(json!({"snapshotEveryS": 0}))).await;
    assert_eq!(status, 400, "{v}");
    let message = v["message"].as_str().unwrap();
    assert!(
        message.contains("cannot be switched off"),
        "the message says why 0 is refused: {message}"
    );
    untouched(&w);
}
