//! In control-api: `POST /v1/runs/{id}/snapshot/download-url` for a copy of the work that needs the run's base.
//!
//! A copy may leave out what the bundle the run started from holds; the header of the copy then names the commits it needs. The route
//! reads that header already. What this file pins:
//!
//! * a copy whose header names no such commit is answered as it always was, whatever the request says, with the reads it always made;
//! * a client that sends `parts=1` gets the copy as before and, for a copy that needs the base, a member `base` with the bundle the
//!   run started from, both signed for the version that was read;
//! * a client that does not send it (one that predates the member) is told to update for such a copy, in one sentence that says the
//!   work is saved, and nothing is signed;
//! * the files read back from a stopped session are served as before either way: they are never served in parts and never refused.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, World, etag_of, post, run_item, version_of};
use control_api::handler::DOWNLOAD_URL_TTL;
use ham_core::snapshot::{HEADER_BYTES, MAX_RECORD_BYTES, SNAPSHOT_REF, SnapshotDownload};
use harness_protocol::direct::s3;
use harness_protocol::store::RecoveredCopyRef;
use serde_json::json;
use std::sync::atomic::Ordering;

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const BASE: &str = "1111111111111111111111111111111111111111";
const READ_BACK: &str = "3333333333333333333333333333333333333333";

/// The sentence a client that sent no `parts` reads for a copy that needs the base, word for word.
const NEEDS_UPDATE: &str = "this copy of the work needs a newer ham to download; the work is saved; run ham update and ask again";

fn path() -> String {
    format!("/v1/runs/{RUN}/snapshot/download-url")
}

/// A copy that holds the whole history: its header names no commit it needs.
fn whole(head: &str) -> Vec<u8> {
    let mut b = format!("# v2 git bundle\n{head} {SNAPSHOT_REF}\n\nPACK").into_bytes();
    b.extend(std::iter::repeat_n(7u8, 4_000));
    b
}

/// A copy that holds what the run changed: its header names the commit of the base it needs.
fn thin(head: &str) -> Vec<u8> {
    let mut b = format!("# v2 git bundle\n-{BASE} harness: base\n{head} {SNAPSHOT_REF}\n\nPACK")
        .into_bytes();
    b.extend(std::iter::repeat_n(9u8, 700));
    b
}

/// The bundle the run started from.
fn base_bundle() -> Vec<u8> {
    let mut b =
        format!("# v2 git bundle\n{BASE} HEAD\n{BASE} refs/heads/main\n\nPACK").into_bytes();
    b.extend(std::iter::repeat_n(3u8, 90_000));
    b
}

fn record(head: &str) -> Vec<u8> {
    json!({"schemaVersion": 1, "headSha": head, "baseSha": BASE, "at": "2026-11-17T13:10:00Z", "count": 3})
        .to_string()
        .into_bytes()
}

/// A run of `ME` whose newest saved copy is `copy`, with the bundle it started from in the bucket.
fn world(copy: Vec<u8>) -> World {
    let w = World::new();
    w.add_run(run_item(
        RUN,
        ME,
        "active",
        "2026-11-17T13:00:00Z",
        json!({"mode": "s3-only"}),
    ));
    w.put_object(&s3::repo_bundle_key(RUN), base_bundle());
    w.put_object(&s3::snapshot_json_key(RUN), record(HEAD));
    w.put_object(&s3::snapshot_bundle_key(RUN), copy);
    w
}

#[tokio::test]
async fn b860_route_whole_copy_answers_as_before() {
    let plain = world(whole(HEAD));
    let (status, body, logs) = post(&plain, ME, &path(), &[], "").await;
    assert_eq!(status, 200, "{body}");
    assert!(logs.is_empty(), "{logs:?}");
    let with_parts = world(whole(HEAD));
    let (status, body_parts, _) = post(&with_parts, ME, &path(), &[("parts", "1")], "").await;
    assert_eq!(status, 200, "{body_parts}");
    println!(
        "B860 TRIGGER PRESENT: the copy's header names no commit it needs: {}",
        !String::from_utf8_lossy(&whole(HEAD)[..200]).contains("\n-")
    );
    // The answer of today, byte for byte, whether the request says `parts` or not, and no member about a base.
    assert_eq!(body, body_parts);
    assert!(body.get("base").is_none(), "{body}");
    let d: SnapshotDownload = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(d.head_sha, HEAD);
    assert_eq!(d.count, Some(3));
    assert_eq!(d.size_bytes, whole(HEAD).len() as u64);
    assert_eq!(d.etag, etag_of(&whole(HEAD)));
    assert!(d.base.is_none());
    // The reads of today: the listing of the two keys, the record, the first bytes of the bundle, one signature. The base is not
    // read, listed or signed.
    for w in [&plain, &with_parts] {
        let calls = w.calls.lock().unwrap().clone();
        let wanted = vec![
            format!("exists {}", s3::snapshot_bundle_key(RUN)),
            format!("exists {}", s3::snapshot_json_key(RUN)),
            format!(
                "read_prefix {} {MAX_RECORD_BYTES}",
                s3::snapshot_json_key(RUN)
            ),
            format!(
                "read_prefix {} {HEADER_BYTES}",
                s3::snapshot_bundle_key(RUN)
            ),
            format!("presign {}", s3::snapshot_bundle_key(RUN)),
        ];
        let object_calls: Vec<String> = calls
            .iter()
            .filter(|c| {
                c.starts_with("exists") || c.starts_with("read_prefix") || c.starts_with("presign")
            })
            .cloned()
            .collect();
        assert_eq!(object_calls, wanted, "{calls:?}");
        assert_eq!(
            w.presigned.lock().unwrap().clone(),
            vec![(s3::snapshot_bundle_key(RUN), DOWNLOAD_URL_TTL)]
        );
    }
}

#[tokio::test]
async fn b860_route_parts_answers_base_and_copy() {
    let w = world(thin(HEAD));
    let (status, body, logs) = post(&w, ME, &path(), &[("parts", "1")], "").await;
    println!(
        "B860 TRIGGER PRESENT: the copy's header names a commit it needs, and the request says parts=1"
    );
    assert_eq!(status, 200, "{body}");
    assert!(logs.is_empty(), "{logs:?}");
    // The copy's members, as for any copy.
    let d: SnapshotDownload = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(d.head_sha, HEAD);
    assert_eq!(d.base_sha.as_deref(), Some(BASE));
    assert_eq!(d.count, Some(3));
    assert_eq!(d.size_bytes, thin(HEAD).len() as u64);
    assert_eq!(d.etag, etag_of(&thin(HEAD)));
    assert!(d.url.contains(&s3::snapshot_bundle_key(RUN)));
    // And the bundle the run started from: where to get it, its size and its ETag.
    let base = d.base.clone().expect("the answer names the base");
    assert!(base.url.contains(&s3::repo_bundle_key(RUN)), "{body}");
    assert_eq!(base.size_bytes, base_bundle().len() as u64);
    assert_eq!(base.etag, etag_of(&base_bundle()));
    let mut members: Vec<&str> = body["base"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    members.sort_unstable();
    assert_eq!(members, vec!["etag", "sizeBytes", "url"], "{body}");
    // Both are signed for the version that was read, for the fifteen minutes of every download.
    assert_eq!(
        w.presigned.lock().unwrap().clone(),
        vec![
            (s3::snapshot_bundle_key(RUN), DOWNLOAD_URL_TTL),
            (s3::repo_bundle_key(RUN), DOWNLOAD_URL_TTL)
        ]
    );
    assert_eq!(
        w.presigned_versions.lock().unwrap().clone(),
        vec![
            Some(version_of(&thin(HEAD))),
            Some(version_of(&base_bundle()))
        ]
    );
    // Whether the base is there is asked by a listing of its exact key, like every key of this route (S3 answers the read of an
    // absent key with 403 for a role that may not list it); then it is read by one byte for its size, its ETag and its version.
    let calls = w.calls.lock().unwrap().clone();
    let listed = calls
        .iter()
        .position(|c| *c == format!("exists {}", s3::repo_bundle_key(RUN)))
        .unwrap_or_else(|| panic!("the base is asked for by a listing: {calls:?}"));
    let read = calls
        .iter()
        .position(|c| *c == format!("read_prefix {} 1", s3::repo_bundle_key(RUN)))
        .unwrap_or_else(|| panic!("the base is read by one byte: {calls:?}"));
    assert!(listed < read, "{calls:?}");
    // The URL of the base is a bearer capability like the copy's: `Debug` hides both.
    let shown = format!("{d:?}");
    assert!(
        !shown.contains("X-Amz-Signature") && shown.contains("<hidden>"),
        "{shown}"
    );

    // A base that is not there (no rule removes it; a hand did), in a stage as it is deployed: the role may not list the bucket
    // at large, so the read of an absent key is refused, and only the listing of the exact key says that it is absent. The copy
    // is intact and is the latest work: it is served alone, with no member `base`, so the client keeps it and says what it needs.
    let gone = world(thin(HEAD));
    gone.objects
        .lock()
        .unwrap()
        .remove(&s3::repo_bundle_key(RUN));
    gone.no_list_permission.store(true, Ordering::SeqCst);
    let (status, body, logs) = post(&gone, ME, &path(), &[("parts", "1")], "").await;
    println!(
        "B860 TRIGGER PRESENT: the copy needs the base, the bucket holds none, and the read of an absent key is refused as for the deployed role"
    );
    assert_eq!(status, 200, "the latest copy still downloads: {body}");
    let d: SnapshotDownload = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(d.head_sha, HEAD);
    assert_eq!(d.count, Some(3));
    assert_eq!(d.size_bytes, thin(HEAD).len() as u64);
    assert_eq!(d.etag, etag_of(&thin(HEAD)));
    assert!(d.base.is_none() && body.get("base").is_none(), "{body}");
    assert!(
        logs.iter()
            .any(|l| l.contains("the bundle the run started from")),
        "{logs:?}"
    );
    assert_eq!(
        gone.presigned.lock().unwrap().clone(),
        vec![(s3::snapshot_bundle_key(RUN), DOWNLOAD_URL_TTL)],
        "the copy is signed, and nothing else"
    );
    let calls = gone.calls.lock().unwrap().clone();
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with(&format!("read_prefix {}", s3::repo_bundle_key(RUN)))),
        "an absent base is not read: {calls:?}"
    );

    // A store that does not answer for the base: the 503 that says to try again, and nothing is signed.
    let down = world(thin(HEAD));
    *down.key_down.lock().unwrap() = Some(s3::repo_bundle_key(RUN));
    let (status, body, _) = post(&down, ME, &path(), &[("parts", "1")], "").await;
    assert_eq!(
        (status, body["code"].as_str().unwrap()),
        (503, "unavailable"),
        "{body}"
    );
    assert!(down.presigned.lock().unwrap().is_empty());
}

#[tokio::test]
async fn b860_route_tells_an_older_client_to_update() {
    let w = world(thin(HEAD));
    // A client that predates the member sends no `parts`; any other word than `1` is no `parts` either.
    for query in [&[][..], &[("parts", "0")][..], &[("parts", "true")][..]] {
        let (status, body, logs) = post(&w, ME, &path(), query, "").await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["code"], "snapshot-needs-update", "{body}");
        assert_eq!(body["message"], NEEDS_UPDATE, "{body}");
        assert!(logs.is_empty(), "{logs:?}");
    }
    println!(
        "B860 TRIGGER PRESENT: the copy's header names a commit it needs, and the request has no parts=1"
    );
    // Nothing is signed, and the base is not read: the answer does not depend on it.
    assert!(w.presigned.lock().unwrap().is_empty());
    assert_eq!(w.count("presign"), 0);
    let calls = w.calls.lock().unwrap().clone();
    assert!(
        !calls.iter().any(|c| c.contains(&s3::repo_bundle_key(RUN))),
        "{calls:?}"
    );
    // The sentence is the same for a run that ended: the work is saved either way.
    let ended = World::new();
    ended.add_run(run_item(
        RUN,
        ME,
        "blocked",
        "2026-11-17T13:00:00Z",
        json!({"mode": "s3-only"}),
    ));
    ended.put_object(&s3::repo_bundle_key(RUN), base_bundle());
    ended.put_object(&s3::snapshot_bundle_key(RUN), thin(HEAD));
    let (status, body, _) = post(&ended, ME, &path(), &[], "").await;
    assert_eq!(
        (
            status,
            body["code"].as_str().unwrap(),
            body["message"].as_str().unwrap()
        ),
        (409, "snapshot-needs-update", NEEDS_UPDATE),
        "{body}"
    );
    // A pair that is being saved keeps its own code and sentence for a client that can take the copy.
    let saving = world(thin(HEAD));
    saving.put_object(&s3::snapshot_json_key(RUN), record(BASE));
    saving.put_object_at(
        &s3::snapshot_bundle_key(RUN),
        thin(HEAD),
        &control_api::ids::now_rfc3339(),
    );
    let (status, body, _) = post(&saving, ME, &path(), &[("parts", "1")], "").await;
    assert_eq!(
        (status, body["code"].as_str().unwrap()),
        (409, "snapshot-updating"),
        "{body}"
    );
    assert_eq!(
        body["message"],
        "a new snapshot is being saved; ask again in a few seconds"
    );
}

#[tokio::test]
async fn b860_route_read_back_is_not_served_in_parts() {
    // The files read back from session 1, in a bundle whose header names a commit it needs (the worker of a read writes none today;
    // the route must not depend on that).
    let read_back = thin(READ_BACK);
    let key = s3::recovery_bundle_key(RUN, 1);
    let make = |chosen: bool| {
        let w = World::new();
        let mut run = run_item(
            RUN,
            ME,
            "active",
            "2026-11-17T13:00:00Z",
            json!({"mode": "s3-only"}),
        );
        run.slice = Some(2);
        if chosen {
            run.recovered_copy = Some(Box::new(RecoveredCopyRef {
                session: 1,
                key: key.clone(),
                version_id: None,
                head_sha: READ_BACK.into(),
                at: "2026-11-17T13:05:00Z".into(),
                newest_write_at: None,
                bundle_bytes: read_back.len() as u64,
            }));
        }
        w.add_run(run);
        w.put_object(&s3::repo_bundle_key(RUN), base_bundle());
        w.put_object(&s3::snapshot_bundle_key_t(RUN, 1), whole(HEAD));
        w.put_object(&s3::snapshot_json_key_t(RUN, 1), record(HEAD));
        w.put_object(&key, read_back.clone());
        w
    };
    // `?readBack=1`, with and without `parts=1`: the answer of today.
    let mut answers = Vec::new();
    for query in [
        &[("readBack", "1")][..],
        &[("readBack", "1"), ("parts", "1")][..],
    ] {
        let w = make(false);
        let (status, body, _) = post(&w, ME, &path(), query, "").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["headSha"], READ_BACK, "{body}");
        assert_eq!(body["recovered"], true, "{body}");
        assert_eq!(body["session"], 1, "{body}");
        assert!(body.get("base").is_none(), "{body}");
        assert_eq!(
            w.presigned.lock().unwrap().clone(),
            vec![(key.clone(), DOWNLOAD_URL_TTL)]
        );
        answers.push(body);
    }
    assert_eq!(answers[0], answers[1]);
    // The files read back as the copy the run goes on from (the plain route): served as before, never refused for a client that
    // sends no `parts`, never with a base.
    for query in [&[][..], &[("parts", "1")][..]] {
        let w = make(true);
        let (status, body, _) = post(&w, ME, &path(), query, "").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["headSha"], READ_BACK, "{body}");
        assert_eq!(body["recovered"], true, "{body}");
        assert!(body.get("base").is_none(), "{body}");
        let calls = w.calls.lock().unwrap().clone();
        assert!(
            !calls.iter().any(|c| c.contains(&s3::repo_bundle_key(RUN))),
            "{calls:?}"
        );
    }
}

#[test]
fn b860_the_answer_for_an_older_client_is_an_answer_that_asks_for_an_update() {
    // The rule of every answer that asks for an update: HTTP 409, a code that ends in `-needs-update`
    // and one sentence that names `run ham update`. The route test above holds the 409, the code and the sentence as a client reads
    // them. This test holds the route's own constants to the rule, so a reworded sentence or a renamed code fails here.
    assert_eq!(
        control_api::snapshot::NEEDS_UPDATE_CODE,
        "snapshot-needs-update"
    );
    assert_eq!(control_api::snapshot::NEEDS_UPDATE, NEEDS_UPDATE);
    assert!(ham_core::update::asks_for_update(
        control_api::snapshot::NEEDS_UPDATE_CODE
    ));
    assert!(ham_core::update::needs_update_sentence_ok(
        control_api::snapshot::NEEDS_UPDATE
    ));
    // TRIGGER PRESENT: the rule refuses the same sentence without the command and a code with another ending, so both checks can fail.
    assert!(!ham_core::update::needs_update_sentence_ok(
        &NEEDS_UPDATE.replace("run ham update", "update ham")
    ));
    assert!(!ham_core::update::asks_for_update("snapshot-unreadable"));
}
