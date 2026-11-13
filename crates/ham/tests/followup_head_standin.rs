//! The contract between the client and the head route: `Api::delivered_head` against the real control-api handler over an
//! in-memory store and bucket, signed and sent over HTTP like any call. What control-api serialises is what the client decodes and checks, for a
//! delivery with a change list, one with none and one the validator recorded as empty, and the route's refusals arrive with their status and code.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, ApiError};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham_core::head::{ChangeBase, ChangeStatus, Changes};
use harness_protocol::direct::s3 as direct_s3;
use harness_protocol::store::s3;
use serde_json::json;
use std::sync::Arc;
use support::{Fake, REGION, creds};

const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
}

async fn start(fake: &Fake) -> String {
    let req = serde_json::from_value::<harness_protocol::api::v1::StartDirectRunRequest>(
        json!({"prompt": "make hello.py", "target": {"mode": "s3-only"}, "model": support::MODEL}),
    )
    .unwrap();
    api(fake)
        .start_direct_run(&req, "k-head")
        .await
        .unwrap()
        .run_id
}

fn bundle(run: &str) -> Vec<u8> {
    let mut b = format!("# v2 git bundle\n{HEAD} refs/heads/harness/{run}\n\nPACK").into_bytes();
    b.extend(std::iter::repeat_n(7u8, 2000));
    b
}

/// A run that finished and left its verified bundle, as a delivery does.
async fn delivered(fake: &Fake) -> String {
    let run = start(fake).await;
    fake.mem.finish_run(&run);
    fake.mem
        .put_object(&s3::verified_bundle_key(&run), bundle(&run));
    run
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_client_reads_what_control_api_wrote_for_a_delivery_with_a_change_list() {
    let fake = Fake::start_with(true).await;
    let run = delivered(&fake).await;
    fake.mem.put_object(
        &direct_s3::evidence_key(&run),
        // The file as run-session's `plan-delivery` writes it: Cline's last message is `summary.clineMessage`, not a member of the envelope.
        json!({"schemaVersion": 1, "runId": run, "outcome": "reported_done", "label": "Cline reported done", "detail": null,
               "headSource": "final", "headSha": HEAD, "baseSha": null, "failure": null,
               "summary": {"schemaVersion": 1, "runId": run, "outcome": "reported_done", "label": "Cline reported done",
                           "clineMessage": "hello.py printed 55.", "rules": []}})
        .to_string()
        .into_bytes(),
    );
    fake.mem.put_object(
        &ham_core::head::changes_key(&run),
        json!({"schemaVersion": 1, "runId": run, "headSha": HEAD, "base": {"kind": "empty-tree"}, "total": 2, "omitted": 0,
               "files": [{"path": "README.md", "status": "A"}, {"path": "hello.py", "status": "A"}]})
        .to_string()
        .into_bytes(),
    );
    let h = api(&fake).delivered_head(&run).await.unwrap();
    assert_eq!(h.run_id, run);
    assert_eq!(h.head_sha, HEAD);
    assert_eq!(h.message().as_deref(), Some("hello.py printed 55."));
    let Changes::Recorded(list) = &h.changes else {
        panic!("{h:?}")
    };
    assert_eq!(list.base, ChangeBase::EmptyTree);
    assert_eq!(
        list.files
            .iter()
            .map(|f| (f.path.as_str(), f.status))
            .collect::<Vec<_>>(),
        vec![
            ("README.md", ChangeStatus::Added),
            ("hello.py", ChangeStatus::Added)
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delivery_with_no_record_is_unavailable_and_one_recorded_empty_is_none() {
    let fake = Fake::start_with(true).await;
    let run = delivered(&fake).await;
    let h = api(&fake).delivered_head(&run).await.unwrap();
    assert_eq!(h.changes, Changes::Unavailable);
    assert_eq!(h.message(), None);
    fake.mem.put_object(
        &ham_core::head::changes_key(&run),
        json!({"schemaVersion": 1, "runId": run, "headSha": HEAD, "base": {"kind": "empty-tree"}, "total": 0, "omitted": 0, "files": []})
            .to_string()
            .into_bytes(),
    );
    let h = api(&fake).delivered_head(&run).await.unwrap();
    let Changes::Recorded(list) = h.changes else {
        panic!("recorded")
    };
    assert_eq!((list.total, list.files.len()), (0, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_that_is_still_going_and_one_that_delivered_nothing_are_refused_with_their_codes() {
    let fake = Fake::start_with(true).await;
    let active = start(&fake).await;
    let e = api(&fake).delivered_head(&active).await.unwrap_err();
    assert!(
        matches!(&e, ApiError::Http { status: 409, error } if error.code == "run-active"),
        "{e:?}"
    );
    // A second run needs another idempotency key.
    let other = {
        let req = serde_json::from_value::<harness_protocol::api::v1::StartDirectRunRequest>(
            json!({"prompt": "another", "target": {"mode": "s3-only"}, "model": support::MODEL}),
        )
        .unwrap();
        api(&fake)
            .start_direct_run(&req, "k-head-2")
            .await
            .unwrap()
            .run_id
    };
    fake.mem.finish_run(&other);
    let e = api(&fake).delivered_head(&other).await.unwrap_err();
    assert!(
        matches!(&e, ApiError::Http { status: 409, error } if error.code == "no-delivered-head"),
        "{e:?}"
    );
}
