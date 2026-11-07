//! `ham` starts a run whose prompt is over the old 256 KiB cap, and refuses a request that is over what the platform takes (6 MiB per
//! synchronous Lambda call, less room for the proxy event) before it sends a byte, with the bound and the largest part named. The stage answered a
//! request over that bound with a gateway error that names nothing.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, ApiError, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::start::{new_nonce, start_direct};
use ham_core::command::{Command, parse};
use ham_core::models::ModelPrefs;
use ham_core::request::{DirectInputs, direct_run_request};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{Attachment, MAX_REQUEST_BYTES};
use std::sync::Arc;
use std::time::Duration;
use support::{Fake, REGION, creds};

const POSTS: &str = "/v1/direct/runs";

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 2,
        base: Duration::from_millis(10),
    })
}

fn request(prompt: &str, inputs: DirectInputs) -> StartDirectRunRequest {
    let Command::StartDirect(mut args) = parse(&["placeholder", "--s3-only", "--direct"])
        .unwrap()
        .command
    else {
        panic!("not a direct start");
    };
    args.prompt = prompt.to_string();
    direct_run_request(&args, inputs, &ModelPrefs::default()).unwrap()
}

#[tokio::test]
async fn b776_a_prompt_over_the_old_256_kib_cap_starts_a_run() {
    let fake = Fake::start().await;
    let prompt = format!("{}\n", "word ".repeat(100_000));
    assert!(prompt.len() > 262_144);
    let started = start_direct(
        &api(&fake),
        &request(&prompt, DirectInputs::default()),
        &new_nonce(),
    )
    .await
    .unwrap();
    assert_eq!(fake.mem.posts(POSTS).len(), 1);
    let sent: StartDirectRunRequest =
        serde_json::from_slice(&fake.mem.posts(POSTS)[0].body).unwrap();
    assert_eq!(sent.prompt, prompt, "the prompt travels whole");
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
async fn b776_a_request_over_the_bound_is_refused_before_it_is_sent_and_names_the_bound() {
    let fake = Fake::start().await;
    let req = request(&"x".repeat(MAX_REQUEST_BYTES + 1), DirectInputs::default());
    let e = start_direct(&api(&fake), &req, &new_nonce())
        .await
        .unwrap_err();
    let ApiError::Http { status, error } = &e else {
        panic!("not a refusal: {e}");
    };
    assert_eq!((*status, error.code.as_str()), (413, "payload-too-large"));
    for want in ["6,225,920 bytes", "6 MiB", "AWS Lambda", "the prompt"] {
        assert!(
            error.message.contains(want),
            "{want} missing from: {}",
            error.message
        );
    }
    assert!(!e.is_retryable(), "a refusal is not retried");
    assert!(fake.mem.posts(POSTS).is_empty(), "nothing was sent");
}

#[tokio::test]
async fn b776_the_refusal_names_the_largest_attachment() {
    let fake = Fake::start().await;
    let inputs = DirectInputs {
        attachments: vec![
            Attachment::new("small.txt", &"a".repeat(1024 * 1024)),
            Attachment::new("notes.txt", &"b".repeat(5 * 1024 * 1024)),
        ],
        ..DirectInputs::default()
    };
    let e = start_direct(&api(&fake), &request("do it", inputs), &new_nonce())
        .await
        .unwrap_err();
    let text = e.to_string();
    assert!(
        text.contains("the attachments") && text.contains("notes.txt"),
        "{text}"
    );
    assert!(fake.mem.posts(POSTS).is_empty());
}

#[tokio::test]
async fn b776_a_request_just_under_the_bound_is_sent() {
    let fake = Fake::start().await;
    let base = request("", DirectInputs::default());
    let base_bytes =
        harness_protocol::direct::event_body_bytes(&serde_json::to_vec(&base).unwrap());
    let req = request(
        &"x".repeat(MAX_REQUEST_BYTES - base_bytes),
        DirectInputs::default(),
    );
    start_direct(&api(&fake), &req, &new_nonce()).await.unwrap();
    assert_eq!(fake.mem.posts(POSTS).len(), 1);
}
