//! `POST /v1/runs/{id}/snapshot/download-url`: a short-lived download URL for the latest snapshot bundle of a run. The
//! route reads two small things from the bucket (the record agentd writes beside the bundle, and the first bytes of the bundle),
//! checks that they name the same commit, and presigns the bundle. Only the owner gets a URL; a presigned URL never reaches a log.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, OTHER, World, deps, etag_of, post, request, run_item, version_of};
use control_api::handler::DOWNLOAD_URL_TTL;
use ham_core::snapshot::{HEADER_BYTES, MAX_RECORD_BYTES, SNAPSHOT_REF, SnapshotDownload};
use harness_protocol::direct::s3;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const BASE: &str = "1111111111111111111111111111111111111111";
const NEW_HEAD: &str = "3333333333333333333333333333333333333333";

fn path() -> String {
    format!("/v1/runs/{RUN}/snapshot/download-url")
}

fn bundle(head: &str, filler: usize) -> Vec<u8> {
    let mut b = format!("# v2 git bundle\n-{BASE} base subject\n{head} {SNAPSHOT_REF}\n\nPACK")
        .into_bytes();
    b.extend(std::iter::repeat_n(7u8, filler));
    b
}

fn record(head: &str) -> Vec<u8> {
    json!({"schemaVersion": 1, "headSha": head, "baseSha": BASE, "at": "2026-10-15T13:10:00Z", "count": 3})
        .to_string()
        .into_bytes()
}

/// A run of `ME` with a saved snapshot of `filler` bytes of pack.
fn world(status: &str, filler: usize) -> World {
    let w = World::new();
    w.add_run(run_item(
        RUN,
        ME,
        status,
        "2026-10-15T13:00:00Z",
        json!({"mode": "s3-only"}),
    ));
    w.put_object(&s3::snapshot_json_key(RUN), record(HEAD));
    w.put_object(&s3::snapshot_bundle_key(RUN), bundle(HEAD, filler));
    w
}

#[tokio::test]
async fn the_owner_gets_a_url_for_the_last_snapshot_and_what_it_holds() {
    let w = world("active", 5_000_000);
    let (status, body, logs) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(status, 200, "{body}");
    let d: SnapshotDownload = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(d.head_sha, HEAD);
    assert_eq!(d.base_sha.as_deref(), Some(BASE));
    assert_eq!(d.taken_at, "2026-10-15T13:10:00Z");
    assert_eq!(d.count, Some(3));
    assert!(!d.record_stale);
    assert_eq!(d.size_bytes, bundle(HEAD, 5_000_000).len() as u64);
    // The ETag of the very object whose header was checked: the client sends it as If-Match, so a bundle replaced after the check is
    // refused by the store instead of downloaded.
    assert_eq!(d.etag, etag_of(&bundle(HEAD, 5_000_000)));
    assert_eq!(body["runStatus"], "active");
    assert!(d.url.contains(&s3::snapshot_bundle_key(RUN)));
    assert!(logs.is_empty(), "{logs:?}");
    // Exactly the bundle is signed, for the same fifteen minutes as every download.
    let signed = w.presigned.lock().unwrap().clone();
    assert_eq!(
        signed,
        vec![(s3::snapshot_bundle_key(RUN), DOWNLOAD_URL_TTL)]
    );
    // Pinned to the version whose header was checked: agentd overwrites the bundle every minute while the files change, and a slow download of the
    // checked one must not end in a replaced file.
    let versions = w.presigned_versions.lock().unwrap().clone();
    assert_eq!(versions, vec![Some(version_of(&bundle(HEAD, 5_000_000)))]);
}

#[tokio::test]
async fn the_route_reads_a_few_kilobytes_never_the_bundle() {
    let w = world("active", 50_000_000);
    let (status, _, _) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(status, 200);
    let calls = w.calls.lock().unwrap().clone();
    let reads: Vec<&String> = calls
        .iter()
        .filter(|c| c.starts_with("read_prefix"))
        .collect();
    assert_eq!(reads.len(), 2, "{calls:?}");
    assert!(
        reads[0].ends_with(&format!("snapshot.json {MAX_RECORD_BYTES}")),
        "{}",
        reads[0]
    );
    assert!(
        reads[1].ends_with(&format!("snapshot.bundle {HEADER_BYTES}")),
        "{}",
        reads[1]
    );
}

#[tokio::test]
async fn a_finished_run_still_gives_its_last_snapshot_and_says_the_run_is_over() {
    let w = world("complete", 100);
    let (status, body, _) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(status, 200);
    assert_eq!(body["runStatus"], "complete");
}

#[tokio::test]
async fn no_snapshot_yet_is_a_404_that_says_when_one_comes() {
    let w = World::new();
    w.add_run(run_item(
        RUN,
        ME,
        "active",
        "2026-10-15T13:00:00Z",
        json!({"mode": "s3-only"}),
    ));
    let (status, body, _) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["code"], "no-snapshot");
    // A run that works is told when to ask again: once it has changed a file.
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("ask again when the run has changed a file"),
        "{body}"
    );
    // The record without its bundle is no snapshot either. A bundle that was just written and has no record yet is a pair that is
    // still being saved.
    w.put_object(&s3::snapshot_json_key(RUN), record(HEAD));
    assert_eq!(post(&w, ME, &path(), &[], "").await.0, 404);
    w.objects.lock().unwrap().clear();
    w.put_object_at(
        &s3::snapshot_bundle_key(RUN),
        bundle(HEAD, 10),
        &control_api::ids::now_rfc3339(),
    );
    let (status, body, _) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], "snapshot-updating");
    assert!(w.presigned.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_bundle_that_has_just_been_written_and_disagrees_with_its_record_is_a_409_the_client_retries()
 {
    let w = world("active", 100);
    // agentd uploads the bundle, then the record: for a few seconds the two name different commits.
    w.put_object_at(
        &s3::snapshot_bundle_key(RUN),
        bundle(NEW_HEAD, 100),
        &control_api::ids::now_rfc3339(),
    );
    let (status, body, _) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], "snapshot-updating");
    assert!(
        w.presigned.lock().unwrap().is_empty(),
        "nothing is signed for a pair that disagrees"
    );
}

#[tokio::test]
async fn a_record_that_lags_an_old_bundle_is_served_from_the_bundle_and_says_so() {
    // The record upload failed, or agentd ended between the two uploads: nothing will fix the pair soon.
    let w = world("active", 100);
    w.put_object_at(
        &s3::snapshot_bundle_key(RUN),
        bundle(NEW_HEAD, 100),
        "2026-10-15T13:30:00Z",
    );
    let (status, body, _) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(status, 200, "{body}");
    let d: SnapshotDownload = serde_json::from_value(body).unwrap();
    // The commit is the bundle's own; what the stale record knew (count, base) is not claimed.
    assert_eq!(d.head_sha, NEW_HEAD);
    assert!(d.record_stale);
    assert_eq!(d.count, None);
    assert_eq!(d.base_sha, None);
    assert_eq!(
        d.taken_at, "2026-10-15T13:30:00Z",
        "the bundle's own write time"
    );
    assert_eq!(w.presigned.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn files_the_route_cannot_trust_are_a_422_and_never_signed() {
    let junk_record = world("active", 10);
    junk_record.put_object(&s3::snapshot_json_key(RUN), b"{\"headSha\": 7}".to_vec());
    let (status, body, _) = post(&junk_record, ME, &path(), &[], "").await;
    assert_eq!(
        (status, body["code"].as_str().unwrap()),
        (422, "snapshot-unreadable"),
        "{body}"
    );
    let junk_bundle = world("active", 10);
    junk_bundle.put_object(
        &s3::snapshot_bundle_key(RUN),
        b"PK\x03\x04 not a bundle at all".to_vec(),
    );
    let (status, body, _) = post(&junk_bundle, ME, &path(), &[], "").await;
    assert_eq!(
        (status, body["code"].as_str().unwrap()),
        (422, "snapshot-unreadable"),
        "{body}"
    );
    // A header that does not end inside the first 8 KiB is not read on trust, and it is not a snapshot being saved.
    let long_header = world("active", 10);
    let mut b = b"# v2 git bundle\n".to_vec();
    for i in 0..400 {
        b.extend(format!("-{BASE} prerequisite number {i} with a long subject line\n").as_bytes());
    }
    b.extend(format!("{HEAD} {SNAPSHOT_REF}\n\nPACK").as_bytes());
    long_header.put_object(&s3::snapshot_bundle_key(RUN), b);
    let (status, body, _) = post(&long_header, ME, &path(), &[], "").await;
    assert_eq!(
        (status, body["code"].as_str().unwrap()),
        (422, "snapshot-unreadable"),
        "{body}"
    );
    assert!(long_header.presigned.lock().unwrap().is_empty());
    assert!(junk_record.presigned.lock().unwrap().is_empty());
    assert!(junk_bundle.presigned.lock().unwrap().is_empty());
}

#[tokio::test]
async fn another_principal_gets_a_404_and_nothing_is_read_or_signed() {
    let w = world("active", 100);
    let (status, body, _) = post(&w, OTHER, &path(), &[], "").await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["code"], "not-found");
    assert_eq!(w.count("read_prefix"), 0);
    assert_eq!(w.count("presign"), 0);
    // A run that does not exist reads the same.
    let (status, _, _) = post(
        &w,
        ME,
        "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X4Z/snapshot/download-url",
        &[],
        "",
    )
    .await;
    assert_eq!(status, 404);
    let (status, _, _) = post(&w, ME, "/v1/runs/not-a-run/snapshot/download-url", &[], "").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn the_route_takes_post_and_needs_a_caller() {
    let w = world("active", 10);
    let out = control_api::handler::handle(request(ME, "GET", &path(), &[], ""), &deps(&w)).await;
    assert_eq!(out.status, 405);
    let mut anon = request(ME, "POST", &path(), &[], "");
    anon.caller_arn = None;
    assert_eq!(
        control_api::handler::handle(anon, &deps(&w)).await.status,
        403
    );
}

#[tokio::test]
async fn a_store_failure_is_a_503_and_the_url_is_in_no_log() {
    let w = world("active", 10);
    w.objects_down.store(true, Ordering::SeqCst);
    let (status, body, logs) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(status, 503, "{body}");
    assert!(!body.to_string().contains("throttled"));
    // The route asks whether the record exists by a listing before it reads it, so the listing is what fails.
    assert!(logs.iter().any(|l| l.contains("object list")), "{logs:?}");
    w.objects_down.store(false, Ordering::SeqCst);
    w.presign_down.store(true, Ordering::SeqCst);
    let (status, _, logs) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(status, 503);
    assert!(logs.iter().all(|l| !l.contains("X-Amz")), "{logs:?}");
}

#[tokio::test]
async fn a_presigned_url_reaches_no_log_line() {
    let w = world("active", 10);
    let req = request(ME, "POST", &path(), &[], "");
    let (out, logs) = control_api::handler::handle_logged(req.clone(), &deps(&w)).await;
    assert_eq!(out.status, 200);
    let body: Value = serde_json::from_str(&out.body).unwrap();
    let url = body["url"].as_str().unwrap();
    assert!(url.contains("X-Amz-Signature"));
    let lines = control_api::lambda::log_lines(&req, out.status, &logs);
    for l in &lines {
        assert!(!l.contains("X-Amz"), "{l}");
        assert!(!l.contains("SESSIONTOKEN"), "{l}");
        assert!(!l.contains("amazonaws.com"), "{l}");
    }
}
