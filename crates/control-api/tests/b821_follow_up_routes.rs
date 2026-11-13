//! A run with the zip target delivers a workflow file or a build specification in its zip. These tests pin the two routes of
//! the control plane by which such a zip run could be followed by a repository run, so the file cannot travel from a zip to a
//! repository the service writes to:
//!
//! - a follow-up on a repository continues a run of that repository only, so a zip run is never the source of a repository run, at
//!   any length of chain (the source is always the run named in the request, and it must itself be a run of that repository);
//! - a retry starts the stored request again: the same target, the same base, and none of the work the blocked run left.
//!
//! A third test pins the route the other way, which the blocked reason of a repository run names as the way to get the work back: a
//! follow-up with the zip target starts on the head the validator refused.
//!
//! All pass before the change; the item's delivery check relies on them, and run-session judges the commits of a zip
//! source again if the first is ever relaxed (`crates/run-session/tests/b821_zip_delivery.rs`).
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, SONNET, World, deps, request, run_item};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::store::s3;
use harness_protocol::target::RunTarget;
use serde_json::{Value, json};

const ZIP_RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const REPO_RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X4Z";
const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const BASE: &str = "1111111111111111111111111111111111111111";
/// The head the repository run delivered.
const REPO_HEAD: &str = "2222222222222222222222222222222222222222";
const REPO: &str = "cc:hamh-e2e-python-calc";

/// The first bytes of a verified bundle that names `head` for `run`: what the validator leaves for a run it checked, delivered or not.
fn bundle(head: &str, run: &str) -> Vec<u8> {
    let mut b =
        format!("# v2 git bundle\n-{BASE} base subject\n{head} refs/heads/harness/{run}\n\nPACK")
            .into_bytes();
    b.extend(std::iter::repeat_n(7u8, 3000));
    b
}

async fn post(w: &World, path: &str, key: &str, body: &Value) -> (u16, Value) {
    let mut r = request(ME, "POST", path, &[], &body.to_string());
    r.headers.insert("idempotency-key".into(), key.into());
    let out = control_api::handler::handle(r, &deps(w)).await;
    (
        out.status,
        serde_json::from_str(&out.body).unwrap_or(Value::Null),
    )
}

fn follow_up_on_the_repository(source: &str) -> Value {
    json!({
        "prompt": "Open the pull request for this work",
        "target": {"mode": "repo", "repo": REPO},
        "model": SONNET,
        "rules": [],
        "continueFrom": {"runId": source}
    })
}

#[tokio::test]
async fn b821_a_repository_follow_up_of_a_zip_run_is_refused_before_anything_is_written() {
    // The zip run delivered (its zip may hold a workflow file); its verified bundle names its head.
    for status in ["complete", "paused", "stalled", "blocked"] {
        let w = World::new();
        w.add_run(run_item(
            ZIP_RUN,
            ME,
            status,
            "2026-11-11T13:00:00Z",
            json!({"mode": "s3-only"}),
        ));
        w.put_object(&s3::verified_bundle_key(ZIP_RUN), bundle(HEAD, ZIP_RUN));
        let (code, v) = post(
            &w,
            "/v1/direct/runs",
            "k-1",
            &follow_up_on_the_repository(ZIP_RUN),
        )
        .await;
        assert_eq!(
            (code, v["code"].as_str()),
            (400, Some("invalid-continue-from")),
            "{status}: {v}"
        );
        let m = v["message"].as_str().unwrap();
        assert!(
            m.contains("s3-only") && m.contains("same repository"),
            "{status}: the refusal says why: {m}"
        );
        assert_eq!(
            w.runs.lock().unwrap().len(),
            1,
            "{status}: no run was created"
        );
        assert!(w.started.lock().unwrap().is_empty(), "{status}");
        assert_eq!(w.count("blob_put"), 0, "{status}");
    }
}

#[tokio::test]
async fn b821_a_zip_run_in_the_middle_of_a_chain_does_not_lead_back_to_the_repository() {
    // REPO_RUN (repository) <- ZIP_RUN (zip follow-up of it, delivered). A repository follow-up of ZIP_RUN is refused: the source of a
    // follow-up is the run the request names, and that run is a zip run. A repository follow-up of REPO_RUN starts from REPO_RUN's own
    // head, which holds nothing of the zip run.
    let w = World::new();
    w.add_run(run_item(
        REPO_RUN,
        ME,
        "complete",
        "2026-11-11T12:00:00Z",
        json!({"mode": "repo", "repo": REPO}),
    ));
    w.put_object(
        &s3::verified_bundle_key(REPO_RUN),
        bundle(REPO_HEAD, REPO_RUN),
    );
    w.add_run(run_item(
        ZIP_RUN,
        ME,
        "complete",
        "2026-11-11T13:00:00Z",
        json!({"mode": "s3-only"}),
    ));
    w.put_object(&s3::verified_bundle_key(ZIP_RUN), bundle(HEAD, ZIP_RUN));

    let (code, v) = post(
        &w,
        "/v1/direct/runs",
        "k-1",
        &follow_up_on_the_repository(ZIP_RUN),
    )
    .await;
    assert_eq!(
        (code, v["code"].as_str()),
        (400, Some("invalid-continue-from")),
        "{v}"
    );

    let (code, v) = post(
        &w,
        "/v1/direct/runs",
        "k-2",
        &follow_up_on_the_repository(REPO_RUN),
    )
    .await;
    assert_eq!(code, 201, "{v}");
    let new = v["runId"].as_str().unwrap();
    let stored: StartDirectRunRequest =
        serde_json::from_slice(&w.objects.lock().unwrap()[&s3::request_key(new)]).unwrap();
    let from = stored.continue_from.unwrap();
    assert_eq!(from.run_id, REPO_RUN);
    assert_eq!(
        from.head_sha.as_deref(),
        Some(REPO_HEAD),
        "the head of the repository run, not the zip run's"
    );
}

#[tokio::test]
async fn b821_a_retry_of_a_blocked_zip_run_starts_the_same_request_and_carries_none_of_its_work() {
    let w = World::new();
    let body = json!({
        "prompt": "Add a calculator with tests and continuous integration",
        "target": {"mode": "s3-only"},
        "model": SONNET,
        "rules": []
    });
    let (code, v) = post(&w, "/v1/direct/runs", "k-1", &body).await;
    assert_eq!(code, 201, "{v}");
    let blocked = v["runId"].as_str().unwrap().to_string();
    // The run ended blocked at the delivery check: the validator wrote its verified bundle before it refused the head.
    w.runs.lock().unwrap().get_mut(&blocked).unwrap().status =
        serde_json::from_value(json!("blocked")).unwrap();
    w.put_object(&s3::verified_bundle_key(&blocked), bundle(HEAD, &blocked));

    let (code, v) = post(
        &w,
        &format!("/v1/runs/{blocked}/retry"),
        &format!("retry-{blocked}"),
        &json!({}),
    )
    .await;
    assert_eq!(code, 201, "{v}");
    let new = v["runId"].as_str().unwrap();
    assert_ne!(new, blocked);
    let objects = w.objects.lock().unwrap();
    assert_eq!(
        objects[&s3::request_key(new)],
        objects[&s3::request_key(&blocked)],
        "the request of the blocked run, byte for byte"
    );
    let stored: StartDirectRunRequest =
        serde_json::from_slice(&objects[&s3::request_key(new)]).unwrap();
    assert_eq!(stored.target, RunTarget::S3Only, "a retry keeps the target");
    assert!(
        stored.continue_from.is_none(),
        "the new run starts from the base the blocked run started from, not from the head it left"
    );
    assert!(
        !objects.contains_key(&s3::verified_bundle_key(new)),
        "nothing of the blocked run's bundle is copied"
    );
}

/// The way back that the blocked reason of a repository run names (`crates/run-session/src/finalize.rs`): a follow-up with the zip
/// target. The validator writes the verified bundle before it refuses the head, so the service starts the zip run on exactly the head
/// that was refused. run-session has it judged from the blocked run's base under the zip rule
/// (`crates/run-session/tests/b821_zip_delivery.rs`), and the validator passes that range for a zip and reports the file
/// (`crates/delivery/tests/b821_zip_target.rs`).
#[tokio::test]
async fn b821_a_zip_follow_up_of_a_blocked_repository_run_starts_on_the_refused_head() {
    let w = World::new();
    w.add_run(run_item(
        REPO_RUN,
        ME,
        "blocked",
        "2026-11-11T12:00:00Z",
        json!({"mode": "repo", "repo": REPO}),
    ));
    w.put_object(
        &s3::verified_bundle_key(REPO_RUN),
        bundle(REPO_HEAD, REPO_RUN),
    );
    let body = json!({
        "prompt": "Deliver this work as it is",
        "target": {"mode": "s3-only"},
        "model": SONNET,
        "rules": [],
        "continueFrom": {"runId": REPO_RUN}
    });
    let (code, v) = post(&w, "/v1/direct/runs", "k-1", &body).await;
    assert_eq!(code, 201, "{v}");
    let new = v["runId"].as_str().unwrap();
    let stored: StartDirectRunRequest =
        serde_json::from_slice(&w.objects.lock().unwrap()[&s3::request_key(new)]).unwrap();
    assert_eq!(stored.target, RunTarget::S3Only);
    let from = stored.continue_from.unwrap();
    assert_eq!(from.run_id, REPO_RUN);
    assert_eq!(from.head_sha.as_deref(), Some(REPO_HEAD));
}
