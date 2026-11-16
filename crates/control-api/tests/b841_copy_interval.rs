//! `POST /v1/direct/runs` with the copy interval: `snapshotEveryS`, the seconds between two looks of the worker at the work
//! tree for a copy of the run's work, from the user's `run.copy_every_seconds` or `run.copy_every_minutes`. A value outside 15 to 3,600 is a 400 that names the member and the
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
    // 15 seconds, the default of a run, is the least a request may name (60 before).
    for s in [15u32, 30, 59, 60, 600, 3_600] {
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
    for s in [0u32, 1, 14, 3_601, 86_400] {
        let w = World::new();
        let (status, v) = start(&w, "k-3", &body(json!({"snapshotEveryS": s}))).await;
        assert_eq!(status, 400, "{s}: {v}");
        assert_eq!(v["code"], "bad-request", "{s}: {v}");
        let message = v["message"].as_str().unwrap();
        assert!(
            message.contains("snapshotEveryS")
                && message.contains("15 to 3600")
                && message.contains("leaving it out means 15"),
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

/// With: a request that names a copy every 15 seconds is taken for a zip-only run and for a run to a repository, and the
/// stored request keeps the interval and the target. run-session reads both: the interval goes into the invocation, and the target says
/// how the run delivers. The start request itself has no member for the delivery; a request that names one is refused like any member
/// this API does not know, and nothing is written.
#[tokio::test]
async fn b823_b847_a_copy_every_15_seconds_is_stored_with_the_target_of_a_zip_only_run_and_of_a_repository_run()
 {
    for target in [
        json!({"mode": "s3-only"}),
        json!({"mode": "repo", "repo": "cc:hamh-e2e-python-calc"}),
    ] {
        let w = World::new();
        let (status, v) = start(
            &w,
            "k-5",
            &body(json!({"snapshotEveryS": 15, "target": target.clone()})),
        )
        .await;
        assert_eq!(status, 201, "{target}: {v}");
        let kept = stored(&w, v["runId"].as_str().unwrap());
        assert_eq!(kept["snapshotEveryS"], json!(15), "{kept}");
        assert_eq!(kept["target"], target, "{kept}");
        assert!(kept.get("delivery").is_none(), "{kept}");
    }
    let w = World::new();
    let (status, v) = start(
        &w,
        "k-6",
        &body(json!({"snapshotEveryS": 15, "delivery": {"kind": "zip"}})),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["code"], "bad-request", "{v}");
    untouched(&w);
}
