//! Starting a run: one `POST /v1/direct/runs` with a fresh `Idempotency-Key` built by
//! `ham_core::request`; a retry after a 503 or a lost answer reuses the same key and gets the same
//! `runId` with one execution; a 4xx is not retried.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, ApiError, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::start::{new_nonce, start_direct};
use ham_core::command::{Command, parse};
use ham_core::models::ModelPrefs;
use ham_core::request::{DirectInputs, direct_idempotency_key, direct_run_request};
use harness_protocol::api::v1::StartDirectRunRequest;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use support::{Fake, REGION, creds};

const SONNET: &str = "global.anthropic.claude-sonnet-5-5";
const POSTS: &str = "/v1/direct/runs";

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 4,
        base: Duration::from_millis(10),
    })
}

/// The local `/v1` API as the `direct` stage: without a stage the route answers 404.
async fn fake() -> Arc<Fake> {
    Fake::start().await
}

fn request(argv: &[&str]) -> StartDirectRunRequest {
    let mut full = argv.to_vec();
    full.push("--direct");
    let Command::StartDirect(args) = parse(&full).unwrap().command else {
        panic!("not a direct start: {argv:?}");
    };
    direct_run_request(&args, DirectInputs::default(), &ModelPrefs::default()).unwrap()
}

#[tokio::test]
async fn one_post_with_an_idempotency_key_prints_the_run_id() {
    let fake = fake().await;
    let req = request(&[
        "Add a percent_change(a, b) function with tests",
        "--repo",
        "cc:hamh-e2e-python-calc",
    ]);
    let nonce = new_nonce();
    let started = start_direct(&api(&fake), &req, &nonce).await.unwrap();
    assert!(harness_protocol::common::is_ulid(&started.run_id));
    let posts = fake.mem.posts(POSTS);
    assert_eq!(posts.len(), 1);
    let key = &posts[0].headers["idempotency-key"];
    assert_eq!(key, &started.idempotency_key);
    let sent: StartDirectRunRequest = serde_json::from_slice(&posts[0].body).unwrap();
    assert_eq!(
        key,
        &direct_idempotency_key(&sent, &nonce),
        "the key is ham_core's"
    );
    assert_eq!(
        sent.prompt,
        "Add a percent_change(a, b) function with tests"
    );
    assert_eq!(
        serde_json::to_value(&sent.target).unwrap(),
        serde_json::json!({"mode": "repo", "repo": "cc:hamh-e2e-python-calc"})
    );
    assert_eq!(sent.model, SONNET);
    assert!(
        fake.mem
            .inner
            .lock()
            .unwrap()
            .runs
            .contains_key(&started.run_id)
    );
}

#[tokio::test]
async fn a_new_launch_of_the_same_prompt_gets_a_new_key_and_run() {
    let fake = fake().await;
    let req = request(&["same prompt", "--s3-only"]);
    let x = start_direct(&api(&fake), &req, &new_nonce()).await.unwrap();
    let y = start_direct(&api(&fake), &req, &new_nonce()).await.unwrap();
    assert_ne!(x.idempotency_key, y.idempotency_key);
    assert_ne!(x.run_id, y.run_id);
}

#[tokio::test]
async fn a_503_is_retried_with_the_same_key() {
    let fake = fake().await;
    fake.faults.post_503.store(2, Ordering::SeqCst);
    let req = request(&["retry me", "--new-repo", "cc:hamh-e2e-new-20260925"]);
    let started = start_direct(&api(&fake), &req, &new_nonce()).await.unwrap();
    let posts = fake.mem.posts(POSTS);
    assert_eq!(posts.len(), 3);
    assert!(
        posts
            .iter()
            .all(|p| p.headers["idempotency-key"] == started.idempotency_key)
    );
    assert_eq!(fake.mem.inner.lock().unwrap().runs.len(), 1);
}

#[tokio::test]
async fn a_lost_answer_after_the_run_was_recorded_returns_the_same_run_with_one_execution() {
    let fake = fake().await;
    fake.faults
        .post_drop_after_commit
        .store(1, Ordering::SeqCst);
    let req = request(&["lost answer", "--s3-only"]);
    let started = start_direct(&api(&fake), &req, &new_nonce()).await.unwrap();
    let posts = fake.mem.posts(POSTS);
    assert_eq!(posts.len(), 2, "the first answer was lost");
    assert_eq!(
        posts[0].headers["idempotency-key"],
        posts[1].headers["idempotency-key"]
    );
    let g = fake.mem.inner.lock().unwrap();
    assert_eq!(g.runs.len(), 1);
    assert!(g.runs.contains_key(&started.run_id));
    assert_eq!(g.started.len(), 1, "one execution");
}

#[tokio::test]
async fn a_client_error_is_not_retried_and_keeps_the_apis_reason() {
    let fake = fake().await;
    let req = request(&["   ", "--s3-only"]);
    let e = start_direct(&api(&fake), &req, &new_nonce())
        .await
        .unwrap_err();
    match e {
        ApiError::Http { status, error } => {
            assert_eq!(status, 400);
            assert_eq!(error.message, "prompt is empty");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fake.mem.posts(POSTS).len(), 1);
}

#[tokio::test]
async fn retries_stop_after_the_attempt_budget() {
    let fake = fake().await;
    fake.faults.post_503.store(10, Ordering::SeqCst);
    let req = request(&["always unavailable", "--s3-only"]);
    let e = start_direct(&api(&fake), &req, &new_nonce())
        .await
        .unwrap_err();
    assert!(matches!(e, ApiError::Http { status: 503, .. }), "{e:?}");
    assert_eq!(fake.mem.posts(POSTS).len(), 4);
}
