//! `POST /v1/direct/runs` with the switch of the delivery note: `deliveryNote: false`, from the user's
//! `run.delivery_note`, is stored as sent, so the worker reads it and writes no block on how the run delivers. `true` is the default said
//! out loud and is stored as no member: the stored request of a default run is byte for byte the one of protocol 1.61.0, which a worker
//! image that predates the member still reads. A value that is no boolean is refused before anything is written.
//!
//! The worker reads how the run delivers from the target of the stored request, and the delivery plan reads the target of the Run item:
//! one start writes both from one request.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, SONNET, World, deps, request};
use harness_protocol::direct::DirectDelivery;
use harness_protocol::goal::ResumeMode;
use harness_protocol::store::s3;
use harness_protocol::target::RunTarget;
use serde_json::{Value, json};

fn body(extra: Value) -> Value {
    let mut b = json!({
        "prompt": "Add percent_change(a, b) with tests and a workflow file",
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

/// The bytes of the stored start request of the run.
fn stored_bytes(w: &World, run_id: &str) -> Vec<u8> {
    w.objects.lock().unwrap()[&s3::request_key(run_id)].clone()
}

fn stored(w: &World, run_id: &str) -> Value {
    serde_json::from_slice(&stored_bytes(w, run_id)).unwrap()
}

#[tokio::test]
async fn b850_the_switch_is_stored_as_sent() {
    let w = World::new();
    let (status, v) = start(&w, "k-1", &body(json!({"deliveryNote": false}))).await;
    assert_eq!(status, 201, "{v}");
    let kept = stored(&w, v["runId"].as_str().unwrap());
    assert_eq!(kept["deliveryNote"], json!(false), "{kept}");
}

#[tokio::test]
async fn b850_a_request_that_keeps_the_default_stores_no_member_whether_it_says_so_or_not() {
    let w = World::new();
    let (status, plain) = start(&w, "k-2", &body(json!({}))).await;
    assert_eq!(status, 201, "{plain}");
    let (status, said) = start(&w, "k-3", &body(json!({"deliveryNote": true}))).await;
    assert_eq!(status, 201, "{said}");
    let a = stored_bytes(&w, plain["runId"].as_str().unwrap());
    let b = stored_bytes(&w, said["runId"].as_str().unwrap());
    println!(
        "B850 TRIGGER PRESENT: two runs were stored: {}",
        plain["runId"] != said["runId"]
    );
    assert!(
        serde_json::from_slice::<Value>(&b)
            .unwrap()
            .get("deliveryNote")
            .is_none(),
        "`true` is the default and is stored as no member"
    );
    assert_eq!(
        a, b,
        "the stored request of a default run does not depend on whether the client said the default"
    );
}

#[tokio::test]
async fn b850_a_value_that_is_no_boolean_is_refused_and_nothing_is_written() {
    for bad in [json!("off"), json!(0), json!({"enabled": false})] {
        let w = World::new();
        let (status, v) = start(&w, "k-4", &body(json!({"deliveryNote": bad}))).await;
        assert_eq!(status, 400, "{bad}: {v}");
        assert_eq!(v["code"], "bad-request", "{v}");
        assert!(w.runs.lock().unwrap().is_empty(), "no run was created");
        assert_eq!(w.count("blob_put"), 0, "no blob was written");
    }
}

/// The worker reads how the run delivers from the stored request's target; the delivery plan reads the Run item's. One start writes both,
/// so the words the engine reads at the start of a session and the check its work meets at the end cannot differ.
#[tokio::test]
async fn b850_the_stored_request_and_the_run_item_name_one_target() {
    for (target, word) in [
        (json!({"mode": "s3-only"}), DirectDelivery::zip()),
        (
            json!({"mode": "repo", "repo": "cc:hamh-e2e-python-calc"}),
            DirectDelivery::pull_request("cc:hamh-e2e-python-calc"),
        ),
    ] {
        let w = World::new();
        let (status, v) = start(&w, "k-5", &body(json!({"target": target}))).await;
        assert_eq!(status, 201, "{target}: {v}");
        let run_id = v["runId"].as_str().unwrap().to_string();
        let in_request: RunTarget =
            serde_json::from_value(stored(&w, &run_id)["target"].clone()).unwrap();
        let on_item = w.runs.lock().unwrap()[&run_id].target.clone();
        assert_eq!(in_request, on_item, "{target}");
        assert_eq!(DirectDelivery::of_target(&in_request), word, "{target}");
        assert_eq!(DirectDelivery::of_target(&on_item), word, "{target}");
    }
}

// ---- a run that starts from another run's stored request ------------------------------------------------------------------------------

const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const BASE: &str = "1111111111111111111111111111111111111111";

/// A delivered bundle of `run`: `ham resume` continues from the head it holds.
fn bundle(run: &str) -> Vec<u8> {
    let mut b =
        format!("# v2 git bundle\n-{BASE} base subject\n{HEAD} refs/heads/harness/{run}\n\nPACK")
            .into_bytes();
    b.extend(std::iter::repeat_n(7u8, 3000));
    b
}

async fn post(w: &World, path: &str, key: &str) -> (u16, Value) {
    let mut r = request(ME, "POST", path, &[], "{}");
    r.headers.insert("idempotency-key".into(), key.into());
    let out = control_api::handler::handle(r, &deps(w)).await;
    (
        out.status,
        serde_json::from_str(&out.body).unwrap_or(Value::Null),
    )
}

/// `ham resume` and `/retry` start the request the service stored for the run they come from, so the new run keeps that run's
/// `deliveryNote`, whatever the user's config says by then: a run started with the note off stays off, and a run that kept the default
/// stores no member. The pages say so (the README row, the user guide and the row of the settings registry).
#[tokio::test]
async fn b850_a_resumed_run_and_a_run_started_again_keep_the_setting_of_the_run_they_come_from() {
    for (sent, kept) in [
        (json!({"deliveryNote": false}), Some(json!(false))),
        (json!({}), None),
    ] {
        // `ham resume`: a paused run that delivered a head.
        let w = World::new();
        let (status, v) = start(&w, "k-6", &body(sent.clone())).await;
        assert_eq!(status, 201, "{v}");
        let id = v["runId"].as_str().unwrap().to_string();
        {
            let mut runs = w.runs.lock().unwrap();
            let run = runs.get_mut(&id).unwrap();
            run.status = serde_json::from_value(json!("paused")).unwrap();
            run.status_reason =
                Some("total_time: the run used the total time it was given".to_string());
            run.resume_mode = Some(ResumeMode::FollowUp);
            run.slice = Some(3);
        }
        w.put_object(&s3::verified_bundle_key(&id), bundle(&id));
        let (status, v) = post(&w, &format!("/v1/runs/{id}/resume"), "resume-1").await;
        assert_eq!(status, 201, "{sent}: {v}");
        let resumed = v["runId"].as_str().unwrap().to_string();
        assert_ne!(resumed, id);
        println!(
            "B850 TRIGGER PRESENT: the resume started a new run from the stored request: {}",
            stored(&w, &resumed).get("continueFrom").is_some()
        );
        assert_eq!(
            stored(&w, &resumed).get("deliveryNote").cloned(),
            kept,
            "{sent}: the resumed run keeps the setting of the run it comes from"
        );

        // `/retry`: a blocked run.
        let w = World::new();
        let (status, v) = start(&w, "k-7", &body(sent.clone())).await;
        assert_eq!(status, 201, "{v}");
        let id = v["runId"].as_str().unwrap().to_string();
        w.runs.lock().unwrap().get_mut(&id).unwrap().status =
            serde_json::from_value(json!("blocked")).unwrap();
        let (status, v) = post(&w, &format!("/v1/runs/{id}/retry"), "retry-1").await;
        assert_eq!(status, 201, "{sent}: {v}");
        let again = v["runId"].as_str().unwrap().to_string();
        assert_ne!(again, id);
        assert_eq!(
            stored(&w, &again).get("deliveryNote").cloned(),
            kept,
            "{sent}: the run started again keeps the setting of the run it comes from"
        );
    }
}
