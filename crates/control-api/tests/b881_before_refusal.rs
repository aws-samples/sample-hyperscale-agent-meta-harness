//! In control-api: `POST /v1/runs/{id}/snapshot/download-url?beforeRefusal=<notice id>`.
//!
//! A session's worker says when it sees the session's storage refuse a write, and its notice names the last copy the session had
//! saved before that: the copy's session, number, time, commit and the version the bucket gave its bundle. A newer copy replaces that
//! bundle at its key, and the bucket keeps the replaced version. The notice's event id is the selector of this route. What this
//! file pins:
//!
//! * the selector is checked before anything is read;
//! * the route reads one event by its key and signs the version the notice names, at a key it builds itself;
//! * an event of another kind, of another agent, or a notice without a copy each has its sentence, and nothing is signed;
//! * a version the store does not give (it is gone, or its read is refused) is said so, and nothing is served in its place;
//! * a version whose header names another commit than the notice is refused;
//! * a copy that needs the bundle the run started from is answered in two parts, or asks a client that takes one file to update;
//! * another caller's run is not found;
//! * a newer object at the same key changes nothing.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, OTHER, World, at, etag_of, post, progress, run_item, ulid, version_of};
use control_api::handler::DOWNLOAD_URL_TTL;
use ham_core::snapshot::{HEADER_BYTES, SNAPSHOT_REF, SnapshotDownload};
use harness_protocol::direct::s3;
use harness_protocol::envelope::{Authority, Envelope, EventType};
use harness_protocol::store::event_shard;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const OTHER_RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Z";
/// The commit of the copy the notice names, and of the copy that replaced it at the key.
const FROZEN: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const NEWER: &str = "4444444444444444444444444444444444444444";
const BASE: &str = "1111111111111111111111111111111111111111";
/// The version the bucket gave the bundle of the copy the notice names.
const VERSION: &str = "version-of-copy-5-as-the-bucket-answered-it";

fn path() -> String {
    format!("/v1/runs/{RUN}/snapshot/download-url")
}

/// The id of the notice: an event id, 26 characters of the id alphabet.
fn notice_id() -> String {
    ulid(7)
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

/// The `storage` member of a notice of session 2 that names copy 5 of the run.
fn storage(copy: Option<Value>) -> Value {
    let mut s = json!({"session": 2, "stretch": 1, "seen": "tool", "usedMb": 165, "partial": false, "quotaMb": 1024});
    if let Some(copy) = copy {
        s["copyBefore"] = copy;
    }
    s
}

fn copy_member() -> Value {
    json!({"session": 2, "count": 5, "at": "2026-11-21T00:37:12Z", "headSha": FROZEN, "versionId": VERSION})
}

/// The worker's notice, as its event.
fn notice(run: &str, id: &str, storage: Value) -> Envelope {
    progress(
        run,
        id,
        &at(60),
        json!({
            "turn": 1, "kind": "notify", "reason": "storage_refused", "outageMinutes": null,
            "detail": "The session's storage refused a write while its files add up to 165 MiB of 1024 MiB (no space left on device).",
            "storage": storage,
        }),
    )
}

/// A run of `ME` in its second session. The bucket holds the copy the notice names as a replaced version of the session's key
/// (`frozen`), a newer copy at the key, and the bundle the run started from. The notice is the event `notice_id()`.
fn world(frozen: Vec<u8>) -> World {
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
    w.put_object(&s3::repo_bundle_key(RUN), base_bundle());
    w.put_object(&s3::snapshot_bundle_key_t(RUN, 2), whole(NEWER));
    w.put_version(&s3::snapshot_bundle_key_t(RUN, 2), VERSION, frozen);
    w.add_events([notice(RUN, &notice_id(), storage(Some(copy_member())))]);
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

fn shard() -> u8 {
    event_shard(Authority::Agent, Some("direct"))
}

#[tokio::test]
async fn b881_the_selector_is_checked_before_anything_is_read() {
    let w = world(whole(FROZEN));
    let id = notice_id();
    let bad_ids = [
        String::new(),
        "5".to_string(),
        "not-an-id".to_string(),
        // 25 and 27 characters.
        id[..25].to_string(),
        format!("{id}X"),
        // The right length with a letter the id alphabet does not have, and with a first character an id cannot start with.
        format!("{}U", &id[..25]),
        format!("8{}", &id[1..]),
        "../../input/repo.bundle".to_string(),
    ];
    for bad in &bad_ids {
        let (status, body, _) = post(&w, ME, &path(), &[("beforeRefusal", bad.as_str())], "").await;
        assert_eq!(
            (status, body["code"].as_str().unwrap()),
            (400, "bad-before-refusal"),
            "{bad:?}: {body}"
        );
        assert_eq!(
            body["message"],
            "beforeRefusal names a notice of the run by its id, as the notice printed it"
        );
    }
    // With `readBack` it is refused too: each names one copy.
    let (status, body, _) = post(
        &w,
        ME,
        &path(),
        &[("beforeRefusal", notice_id().as_str()), ("readBack", "1")],
        "",
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str().unwrap()),
        (400, "bad-before-refusal"),
        "{body}"
    );
    // Nothing was read for any of them: no event and no object.
    assert!(object_calls(&w).is_empty(), "{:?}", object_calls(&w));
    assert!(w.presigned.lock().unwrap().is_empty());
}

#[tokio::test]
async fn b881_the_route_reads_one_event_and_signs_the_version_it_names_at_a_key_it_builds_itself() {
    let w = world(whole(FROZEN));
    // A notice whose member carries a key and a URL of its own: the route takes neither.
    let mut with_key = storage(Some(copy_member()));
    with_key["copyBefore"]["key"] = json!("runs/another/input/repo.bundle");
    with_key["copyBefore"]["url"] = json!("https://example.invalid/x");
    w.events.lock().unwrap().clear();
    w.add_events([notice(RUN, &notice_id(), with_key)]);
    let id = notice_id();
    let (status, body, logs) = post(&w, ME, &path(), &[("beforeRefusal", id.as_str())], "").await;
    assert_eq!(status, 200, "{body}");
    assert!(logs.is_empty(), "{logs:?}");
    println!("B881 TRIGGER PRESENT: the event {id} is a storage_refused notice that names a copy");
    let d: SnapshotDownload = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(d.before_refusal.as_deref(), Some(id.as_str()));
    assert_eq!(d.head_sha, FROZEN);
    assert_eq!(d.count, Some(5));
    assert_eq!(d.taken_at, "2026-11-21T00:37:12Z");
    assert_eq!(d.size_bytes, whole(FROZEN).len() as u64);
    assert_eq!(d.etag, etag_of(&whole(FROZEN)));
    assert!(!d.final_copy && !d.recovered && !d.record_stale);
    assert!(d.base.is_none());
    assert!(d.url.contains(&s3::snapshot_bundle_key_t(RUN, 2)), "{body}");
    // One event read by its exact key, one ranged read of the named version at the key of the session, one signature for that
    // version. No query over the run's events, no listing, no record, and no read of the object that is at the key now.
    let key = s3::snapshot_bundle_key_t(RUN, 2);
    assert_eq!(
        object_calls(&w),
        vec![
            format!("get_event {RUN} {} {id}", shard()),
            format!("read_prefix_version {key} {VERSION} {HEADER_BYTES}"),
            format!("presign {key}"),
        ]
    );
    assert_eq!(
        w.presigned.lock().unwrap().clone(),
        vec![(key, DOWNLOAD_URL_TTL)]
    );
    assert_eq!(
        w.presigned_versions.lock().unwrap().clone(),
        vec![Some(VERSION.to_string())]
    );
    // The URL is a bearer capability: `Debug` hides it.
    assert!(!format!("{d:?}").contains("X-Amz-Signature"));
}

#[tokio::test]
async fn b881_an_event_of_another_kind_another_agent_or_without_a_copy_each_has_its_sentence() {
    let w = world(whole(FROZEN));
    let no_notice = "this run has no notice of a storage refusal with that id";
    let no_copy =
        format!("that notice names no copy; ham pull {RUN} --snapshot downloads the newest copy");
    // No such event.
    let (status, body, _) =
        post(&w, ME, &path(), &[("beforeRefusal", ulid(99).as_str())], "").await;
    assert_eq!(
        (
            status,
            body["code"].as_str().unwrap(),
            body["message"].as_str().unwrap()
        ),
        (404, "no-before-refusal", no_notice),
        "{body}"
    );
    // A notice of another reason, a message, and a saved-copy event that happens to carry a `storage` member.
    let other_reason = progress(
        RUN,
        &ulid(20),
        &at(61),
        json!({"turn": 1, "kind": "notify", "reason": "copy_not_saved", "detail": "x", "storage": storage(Some(copy_member()))}),
    );
    let message = progress(
        RUN,
        &ulid(21),
        &at(62),
        json!({"turn": 1, "kind": "message", "text": "hello", "storage": storage(Some(copy_member()))}),
    );
    // A storage notice of another agent, and one that is no agent's progress event.
    let mut other_agent = notice(RUN, &ulid(22), storage(Some(copy_member())));
    other_agent.agent_id = Some("research".into());
    let mut other_type = notice(RUN, &ulid(23), storage(Some(copy_member())));
    other_type.event_type = EventType::TurnStarted;
    w.add_events([other_reason, message, other_agent, other_type]);
    for n in [20, 21, 22, 23] {
        let (status, body, _) =
            post(&w, ME, &path(), &[("beforeRefusal", ulid(n).as_str())], "").await;
        assert_eq!(
            (
                status,
                body["code"].as_str().unwrap(),
                body["message"].as_str().unwrap()
            ),
            (404, "no-before-refusal", no_notice),
            "event {n}: {body}"
        );
    }
    // A notice that names no copy, and notices whose copy is not in the worker's form: each is "names no copy".
    let mut bad: Vec<Value> = vec![storage(None)];
    for (member, value) in [
        ("session", json!(0)),
        ("session", json!(257)),
        ("session", json!("2")),
        ("count", json!(0)),
        ("at", json!("yesterday")),
        ("headSha", json!("9d2f1a7")),
        ("headSha", json!("ZZ2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b")),
        ("versionId", json!("")),
        ("versionId", json!("v".repeat(1025))),
        ("versionId", json!("a\nb")),
        ("versionId", Value::Null),
    ] {
        let mut copy = copy_member();
        copy[member] = value;
        bad.push(storage(Some(copy)));
    }
    for (i, member) in bad.into_iter().enumerate() {
        let id = ulid(40 + i as u64);
        w.add_events([notice(RUN, &id, member.clone())]);
        let (status, body, _) = post(&w, ME, &path(), &[("beforeRefusal", id.as_str())], "").await;
        assert_eq!(
            (
                status,
                body["code"].as_str().unwrap(),
                body["message"].as_str().unwrap()
            ),
            (404, "no-copy-before-refusal", no_copy.as_str()),
            "{member}: {body}"
        );
    }
    // Nothing was read from the bucket and nothing was signed for any of them.
    assert!(
        !object_calls(&w).iter().any(|c| c.starts_with("read_prefix")
            || c.starts_with("presign")
            || c.starts_with("exists")),
        "{:?}",
        object_calls(&w)
    );
    assert!(w.presigned.lock().unwrap().is_empty());
}

#[tokio::test]
async fn b881_a_version_that_cannot_be_read_is_said_so_and_nothing_is_signed() {
    let sentence = format!(
        "that copy cannot be read; a replaced copy is kept for 30 days; ham pull {RUN} --snapshot downloads the newest copy"
    );
    // The version is gone: the bucket removed it after its rule.
    let gone = world(whole(FROZEN));
    gone.versions.lock().unwrap().clear();
    // The read of the version is refused.
    let denied = world(whole(FROZEN));
    denied.version_denied.store(true, Ordering::SeqCst);
    for (what, w) in [("gone", &gone), ("denied", &denied)] {
        let (status, body, _) = post(
            w,
            ME,
            &path(),
            &[("beforeRefusal", notice_id().as_str())],
            "",
        )
        .await;
        assert_eq!(
            (
                status,
                body["code"].as_str().unwrap(),
                body["message"].as_str().unwrap()
            ),
            (404, "before-refusal-unavailable", sentence.as_str()),
            "{what}: {body}"
        );
        // The newest copy is at the key, and it is not served in its place: nothing is signed, and the key is not read.
        assert!(w.presigned.lock().unwrap().is_empty(), "{what}");
        assert!(
            !object_calls(w)
                .iter()
                .any(|c| c.starts_with("read_prefix ")),
            "{what}: {:?}",
            object_calls(w)
        );
    }
    // A store that does not answer is the 503 that says to try again, never "cannot be read".
    let down = world(whole(FROZEN));
    down.objects_down.store(true, Ordering::SeqCst);
    let (status, body, _) = post(
        &down,
        ME,
        &path(),
        &[("beforeRefusal", notice_id().as_str())],
        "",
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str().unwrap()),
        (503, "unavailable"),
        "{body}"
    );
    assert!(down.presigned.lock().unwrap().is_empty());
}

#[tokio::test]
async fn b881_a_header_that_names_another_commit_is_refused() {
    // The version holds a bundle of another commit than the notice names.
    let w = world(whole(NEWER));
    let (status, body, logs) = post(
        &w,
        ME,
        &path(),
        &[("beforeRefusal", notice_id().as_str())],
        "",
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str().unwrap()),
        (422, "snapshot-unreadable"),
        "{body}"
    );
    assert!(
        logs.iter()
            .any(|l| l.contains("the version names another commit than the notice")),
        "{logs:?}"
    );
    assert!(w.presigned.lock().unwrap().is_empty());
    // The version holds no bundle at all.
    let junk = world(b"not a bundle".to_vec());
    let (status, body, _) = post(
        &junk,
        ME,
        &path(),
        &[("beforeRefusal", notice_id().as_str())],
        "",
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str().unwrap()),
        (422, "snapshot-unreadable"),
        "{body}"
    );
    assert!(junk.presigned.lock().unwrap().is_empty());
}

#[tokio::test]
async fn b881_a_copy_that_needs_the_base_is_answered_in_two_parts_or_asks_for_an_update() {
    // A client that takes two parts: the copy at its version, and the bundle the run started from.
    let w = world(thin(FROZEN));
    let id = notice_id();
    let (status, body, logs) = post(
        &w,
        ME,
        &path(),
        &[("beforeRefusal", id.as_str()), ("parts", "1")],
        "",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(logs.is_empty(), "{logs:?}");
    println!(
        "B881 TRIGGER PRESENT: the copy the notice names needs the bundle the run started from"
    );
    let d: SnapshotDownload = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(d.before_refusal.as_deref(), Some(id.as_str()));
    assert_eq!(d.head_sha, FROZEN);
    assert_eq!(d.size_bytes, thin(FROZEN).len() as u64);
    let base = d.base.clone().expect("the answer names the base");
    assert!(base.url.contains(&s3::repo_bundle_key(RUN)), "{body}");
    assert_eq!(base.size_bytes, base_bundle().len() as u64);
    assert_eq!(base.etag, etag_of(&base_bundle()));
    let key = s3::snapshot_bundle_key_t(RUN, 2);
    assert_eq!(
        w.presigned.lock().unwrap().clone(),
        vec![
            (key, DOWNLOAD_URL_TTL),
            (s3::repo_bundle_key(RUN), DOWNLOAD_URL_TTL)
        ]
    );
    // The copy is signed for the version the notice names, the base for the version that was read.
    assert_eq!(
        w.presigned_versions.lock().unwrap().clone(),
        vec![Some(VERSION.to_string()), Some(version_of(&base_bundle()))]
    );

    // A request that does not say `parts=1` is told to update, in the sentence of every such copy, and nothing is signed.
    let one = world(thin(FROZEN));
    let (status, body, _) = post(&one, ME, &path(), &[("beforeRefusal", id.as_str())], "").await;
    assert_eq!(
        (status, body["code"].as_str().unwrap()),
        (409, "snapshot-needs-update"),
        "{body}"
    );
    assert_eq!(
        body["message"],
        "this copy of the work needs a newer ham to download; the work is saved; run ham update and ask again"
    );
    assert!(one.presigned.lock().unwrap().is_empty());

    // A base that is not there is no refusal: the copy is intact, so it is served alone at its version, with no member `base`, and
    // the log says so. The client keeps it and says what it needs.
    let gone = world(thin(FROZEN));
    gone.objects
        .lock()
        .unwrap()
        .remove(&s3::repo_bundle_key(RUN));
    let (status, body, logs) = post(
        &gone,
        ME,
        &path(),
        &[("beforeRefusal", id.as_str()), ("parts", "1")],
        "",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.get("base").is_none(), "{body}");
    assert_eq!(body["beforeRefusal"], id.as_str());
    assert!(
        logs.iter().any(|l| l.contains("the copy is served alone")),
        "{logs:?}"
    );
    assert_eq!(
        gone.presigned_versions.lock().unwrap().clone(),
        vec![Some(VERSION.to_string())]
    );

    // A copy that clones alone is answered with one part whether the request says `parts` or not.
    let whole_copy = world(whole(FROZEN));
    let (status, with_parts, _) = post(
        &whole_copy,
        ME,
        &path(),
        &[("beforeRefusal", id.as_str()), ("parts", "1")],
        "",
    )
    .await;
    assert_eq!(status, 200, "{with_parts}");
    assert!(with_parts.get("base").is_none(), "{with_parts}");
}

#[tokio::test]
async fn b881_another_callers_run_is_not_found() {
    let w = world(whole(FROZEN));
    let (status, body, _) = post(
        &w,
        OTHER,
        &path(),
        &[("beforeRefusal", notice_id().as_str())],
        "",
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str().unwrap()),
        (404, "not-found"),
        "{body}"
    );
    // A notice of another run under the caller's own run id is not this run's event: the event is read by the key of the run asked
    // for.
    let mine = World::new();
    mine.add_run(run_item(
        OTHER_RUN,
        OTHER,
        "active",
        "2026-11-21T00:00:00Z",
        json!({"mode": "s3-only"}),
    ));
    mine.add_events([notice(RUN, &notice_id(), storage(Some(copy_member())))]);
    mine.put_version(&s3::snapshot_bundle_key_t(RUN, 2), VERSION, whole(FROZEN));
    let (status, body, _) = post(
        &mine,
        OTHER,
        &format!("/v1/runs/{OTHER_RUN}/snapshot/download-url"),
        &[("beforeRefusal", notice_id().as_str())],
        "",
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str().unwrap()),
        (404, "no-before-refusal"),
        "{body}"
    );
    // Neither world signed anything or read an object.
    for world in [&w, &mine] {
        assert!(world.presigned.lock().unwrap().is_empty());
        assert!(
            !object_calls(world)
                .iter()
                .any(|c| c.starts_with("read_prefix")),
            "{:?}",
            object_calls(world)
        );
    }
}

#[tokio::test]
async fn b881_a_newer_object_at_the_same_key_changes_nothing() {
    let w = world(whole(FROZEN));
    let id = notice_id();
    let (status, first, _) = post(&w, ME, &path(), &[("beforeRefusal", id.as_str())], "").await;
    assert_eq!(status, 200, "{first}");
    // The session saves two more copies at the key, and the run moves to a third session that saves its own.
    w.put_object(&s3::snapshot_bundle_key_t(RUN, 2), whole(BASE));
    w.put_object(&s3::snapshot_bundle_key_t(RUN, 3), whole(NEWER));
    w.runs.lock().unwrap().get_mut(RUN).unwrap().slice = Some(3);
    let (status, second, _) = post(&w, ME, &path(), &[("beforeRefusal", id.as_str())], "").await;
    assert_eq!(status, 200, "{second}");
    println!(
        "B881 TRIGGER PRESENT: the key of the session holds another object than the version the notice names"
    );
    assert_eq!(first, second, "the answer is the same copy");
    let d: SnapshotDownload = serde_json::from_value(second).unwrap();
    assert_eq!(d.head_sha, FROZEN);
    assert_eq!(d.etag, etag_of(&whole(FROZEN)));
    assert_eq!(
        w.presigned_versions.lock().unwrap().clone(),
        vec![Some(VERSION.to_string()), Some(VERSION.to_string())]
    );
    // The plain route still answers with the run's newest copy, with no member about a refusal.
    let (status, newest, _) = post(&w, ME, &path(), &[("parts", "1")], "").await;
    assert_eq!(status, 200, "{newest}");
    assert_eq!(newest["headSha"], NEWER);
    assert!(newest.get("beforeRefusal").is_none(), "{newest}");
}
