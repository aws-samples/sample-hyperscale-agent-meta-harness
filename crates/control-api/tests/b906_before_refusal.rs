//! In control-api: `?beforeRefusal=<notice id>` for a notice that names the copy its session started from.
//!
//! A session after the first starts from a copy an earlier session saved. When its storage refuses a write before it stored a copy
//! of its own, its notice names that copy: the session that saved it, its number, time and commit, and the version the bucket
//! answered when the later session read the bundle. The route builds the key from the session the COPY names, which is not the
//! session of the notice, and signs that version there. What the later session stored at its own key meanwhile changes nothing.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, World, at, etag_of, post, progress, run_item, ulid};
use control_api::handler::DOWNLOAD_URL_TTL;
use ham_core::snapshot::{HEADER_BYTES, SNAPSHOT_REF, SnapshotDownload};
use harness_protocol::direct::s3;
use harness_protocol::envelope::{Authority, Envelope};
use harness_protocol::store::event_shard;
use serde_json::{Value, json};

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
/// The commit of copy 7, which session 1 saved and session 2 started from.
const FROM: &str = "7d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
/// The commit of the copy session 2 stored at its own key after the refusal.
const OWN: &str = "8888888888888888888888888888888888888888";
/// The commit of a copy session 1 stored at its key after session 2 had started.
const REPLACED: &str = "9999999999999999999999999999999999999999";
/// The version the bucket answered when session 2 read the bundle of copy 7.
const VERSION: &str = "version-of-copy-7-as-the-bucket-answered-the-read";

fn path() -> String {
    format!("/v1/runs/{RUN}/snapshot/download-url")
}

fn notice_id() -> String {
    ulid(9)
}

/// A copy that holds the whole history: its header names no commit it needs.
fn whole(head: &str) -> Vec<u8> {
    let mut b = format!("# v2 git bundle\n{head} {SNAPSHOT_REF}\n\nPACK").into_bytes();
    b.extend(std::iter::repeat_n(7u8, 4_000));
    b
}

/// The first notice of session 2, at the marker of its start: it names copy 7 of session 1.
fn notice(copy_session: u32) -> Envelope {
    let storage: Value = json!({
        "session": 2, "stretch": 1, "seen": "start", "usedMb": 165, "partial": false, "quotaMb": 1024,
        "copyBefore": {"session": copy_session, "count": 7, "at": "2026-11-21T00:37:12Z", "headSha": FROM, "versionId": VERSION},
    });
    progress(
        RUN,
        &notice_id(),
        &at(60),
        json!({
            "turn": 1, "kind": "notify", "reason": "storage_refused", "outageMinutes": null,
            "detail": "The session's storage refused a write while its files add up to 165 MiB of 1024 MiB (no space left on device).",
            "storage": storage,
        }),
    )
}

/// A run of `ME` in its second session. The key of session 1 holds copy 7 at the version session 2 read; the key of session 2
/// holds the copy that session stored later.
fn world(copy_session: u32) -> World {
    world_with(copy_session, whole(FROM))
}

/// [`world`], with `at_version` as the bytes the bucket holds at the version the notice names.
fn world_with(copy_session: u32, at_version: Vec<u8>) -> World {
    let w = World::new();
    let mut run = run_item(
        RUN,
        ME,
        "active",
        "2026-11-21T00:00:00Z",
        json!({"mode": "s3-only"}),
    );
    run.slice = Some(2);
    w.add_run(run);
    w.put_object(&s3::snapshot_bundle_key_t(RUN, 1), whole(FROM));
    w.put_version(&s3::snapshot_bundle_key_t(RUN, 1), VERSION, at_version);
    w.put_object(&s3::snapshot_bundle_key_t(RUN, 2), whole(OWN));
    w.add_events([notice(copy_session)]);
    w
}

fn object_calls(w: &World) -> Vec<String> {
    w.calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| {
            c.starts_with("exists")
                || c.starts_with("read_prefix")
                || c.starts_with("presign")
                || c.starts_with("get_event")
                || c.starts_with("events")
        })
        .cloned()
        .collect()
}

#[tokio::test]
async fn b906_the_route_serves_the_copy_a_later_session_started_from_at_the_key_of_the_session_that_saved_it()
 {
    let w = world(1);
    let id = notice_id();
    let (status, body, logs) = post(&w, ME, &path(), &[("beforeRefusal", id.as_str())], "").await;
    assert_eq!(status, 200, "{body}");
    assert!(logs.is_empty(), "{logs:?}");
    println!(
        "B906 TRIGGER PRESENT: the event {id} is a notice of session 2 whose copy names session 1"
    );
    let d: SnapshotDownload = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(d.before_refusal.as_deref(), Some(id.as_str()));
    assert_eq!(d.head_sha, FROM);
    assert_eq!(d.count, Some(7));
    assert_eq!(d.taken_at, "2026-11-21T00:37:12Z");
    assert_eq!(d.size_bytes, whole(FROM).len() as u64);
    assert_eq!(d.etag, etag_of(&whole(FROM)));
    assert!(!d.final_copy && !d.recovered && !d.record_stale);
    // The key is the one of the session that saved the copy. Nothing at the key of the notice's own session is read or signed.
    let saved_at = s3::snapshot_bundle_key_t(RUN, 1);
    let own_key = s3::snapshot_bundle_key_t(RUN, 2);
    assert!(d.url.contains(&saved_at), "{body}");
    assert!(!d.url.contains(&own_key), "{body}");
    let shard = event_shard(Authority::Agent, Some("direct"));
    assert_eq!(
        object_calls(&w),
        vec![
            format!("get_event {RUN} {shard} {id}"),
            format!("read_prefix_version {saved_at} {VERSION} {HEADER_BYTES}"),
            format!("presign {saved_at}"),
        ]
    );
    assert_eq!(
        w.presigned.lock().unwrap().clone(),
        vec![(saved_at, DOWNLOAD_URL_TTL)]
    );
    assert_eq!(
        w.presigned_versions.lock().unwrap().clone(),
        vec![Some(VERSION.to_string())]
    );
}

#[tokio::test]
async fn b906_a_version_that_is_not_at_the_key_of_the_session_the_copy_names_is_said_so_and_nothing_is_signed()
 {
    // The same notice, except that its copy names session 2, at whose key the bucket has no such version: the route does not look
    // for the version at another session's key, and nothing is signed.
    let w = world(2);
    let id = notice_id();
    let (status, body, _) = post(&w, ME, &path(), &[("beforeRefusal", id.as_str())], "").await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["code"], "before-refusal-unavailable", "{body}");
    assert!(w.presigned.lock().unwrap().is_empty(), "{body}");
}

#[tokio::test]
async fn b906_a_version_that_holds_another_commit_than_the_notice_names_is_refused_and_nothing_is_signed()
 {
    // A later session that is invoked a second time keeps the checkout of its first invocation and downloads the object again.
    // When the earlier session was still writing and replaced its bundle in between, the version of that second download holds
    // another commit than the one the session works on, and the notice names that version with the commit of the checkout. The
    // route reads the header at the version the notice names, finds another commit there, says so and serves nothing: not that
    // version and no other.
    let w = world_with(1, whole(REPLACED));
    let id = notice_id();
    let (status, body, logs) = post(&w, ME, &path(), &[("beforeRefusal", id.as_str())], "").await;
    let saved_at = s3::snapshot_bundle_key_t(RUN, 1);
    let shard = event_shard(Authority::Agent, Some("direct"));
    assert_eq!(
        object_calls(&w),
        vec![
            format!("get_event {RUN} {shard} {id}"),
            format!("read_prefix_version {saved_at} {VERSION} {HEADER_BYTES}"),
        ],
        "TRIGGER PRESENT is not shown: the route did not read the header at the version the notice names"
    );
    println!(
        "B906 TRIGGER PRESENT: the version {VERSION} at {saved_at} holds {REPLACED}; the notice names {FROM}"
    );
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["code"], "snapshot-unreadable", "{body}");
    assert!(
        logs.iter().any(|l| l.contains(&format!(
            "copy before the storage refusal {id} of run {RUN}: the version names another commit than the notice"
        ))),
        "{logs:?}"
    );
    assert!(body.get("url").is_none(), "{body}");
    assert!(w.presigned.lock().unwrap().is_empty(), "{body}");
    assert!(w.presigned_versions.lock().unwrap().is_empty(), "{body}");
}
