//! `POST /v1/direct/runs` with `compute.containerSignals`. The list is checked at the start with the one check
//! `ham` also calls: an entry is a signal or the word `default`. A bad entry is a 400 that names it and writes nothing; a good list is stored as
//! sent; a request without the list is stored byte for byte as an older client sent it.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, SONNET, World, deps, request};
use harness_protocol::store::s3;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

fn body(compute: Value) -> Value {
    json!({
        "prompt": "Add percent_change(a, b) with tests",
        "target": {"mode": "s3-only"},
        "model": SONNET,
        "rules": [],
        "compute": compute
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

fn stored_text(w: &World, run_id: &str) -> String {
    let g = w.objects.lock().unwrap();
    String::from_utf8(g[&s3::request_key(run_id)].clone()).unwrap()
}

#[tokio::test]
async fn b632_a_bad_entry_is_a_400_that_names_it_and_writes_nothing() {
    for (i, bad) in [
        "pom.xml contains",
        "my file.txt",
        "none",
        "",
        "a,,b",
        "/etc/passwd",
    ]
    .into_iter()
    .enumerate()
    {
        let w = World::new();
        w.instances.store(true, Ordering::SeqCst);
        let b = body(json!({"mode": "auto", "containerSignals": ["default", bad]}));
        let (status, v) = start(&w, &format!("bad-{i}"), &b).await;
        assert_eq!(status, 400, "{bad:?}: {v}");
        assert_eq!(v["code"], "invalid-container-signals", "{bad:?}: {v}");
        assert!(
            v["message"].as_str().unwrap().contains("signal"),
            "{bad:?}: {v}"
        );
        assert!(w.runs.lock().unwrap().is_empty(), "no run was created");
        assert!(w.idem.lock().unwrap().is_empty(), "no claim was written");
        assert_eq!(w.count("blob_put"), 0, "no blob was written");
    }
}

#[tokio::test]
async fn b632_a_good_list_and_an_empty_list_are_stored_as_sent() {
    let w = World::new();
    w.instances.store(true, Ordering::SeqCst);
    let list = json!([
        "default",
        "Gemfile contains testcontainers",
        "compose.yml, compose.yaml"
    ]);
    let (status, v) = start(
        &w,
        "k-1",
        &body(json!({"mode": "auto", "containerSignals": list})),
    )
    .await;
    assert_eq!(status, 201, "{v}");
    let text = stored_text(&w, v["runId"].as_str().unwrap());
    assert!(
        text.contains(r#""compute":{"mode":"auto","containerSignals":["default","Gemfile contains testcontainers","compose.yml, compose.yaml"]}"#),
        "{text}"
    );
    let (status, v) = start(
        &w,
        "k-2",
        &body(json!({"mode": "instances", "containerSignals": []})),
    )
    .await;
    assert_eq!(status, 201, "{v}");
    assert!(
        stored_text(&w, v["runId"].as_str().unwrap())
            .contains(r#""compute":{"mode":"instances","containerSignals":[]}"#)
    );
}

#[tokio::test]
async fn b632_a_request_without_a_list_is_stored_without_one() {
    let w = World::new();
    w.instances.store(true, Ordering::SeqCst);
    let (status, v) = start(&w, "k-3", &body(json!({"mode": "microvm"}))).await;
    assert_eq!(status, 201, "{v}");
    let text = stored_text(&w, v["runId"].as_str().unwrap());
    assert!(text.contains(r#""compute":{"mode":"microvm"}"#), "{text}");
    assert!(!text.contains("containerSignals"), "{text}");
}

#[tokio::test]
async fn b632_the_list_is_not_the_stages_instances_check() {
    // A list with mode auto does not ask the stage for its Instances runtime: only the word `instances` does.
    let w = World::new();
    w.instances_down.store(true, Ordering::SeqCst);
    let (status, v) = start(
        &w,
        "k-4",
        &body(json!({"mode": "auto", "containerSignals": ["Makefile contains docker"]})),
    )
    .await;
    assert_eq!(status, 201, "{v}");
    assert_eq!(w.count("instances_available"), 0);
}
