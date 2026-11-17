//! In control-api: the switch of a run, and the download of the files the service read back from a stopped session's storage.
//!
//! The download (`POST /v1/runs/{id}/snapshot/download-url`): the plain route answers with the files read back when the service chose
//! them and no later session has saved a copy; with `?readBack=<n>` it answers with the files read back from session n whether the
//! service used them or not. A check may refuse to use work, it never hides it: every copy the service read back and did not use is
//! downloaded by the number of its session.
//!
//! `POST /v1/direct/runs` with the switch of reading a stopped session's storage back: `recoverAfterStop: false`,
//! from the user's `run.recover_after_stop`, is stored as sent, so `start-session` copies it to the Agent item and the service makes no
//! read and does not act on the worker's notice of an end. `true` is the default said out loud and is stored as no member: the stored
//! request of a default run is byte for byte the one of protocol 1.62.0. A value that is no boolean is refused before anything is written.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, OTHER, SONNET, World, deps, post, request, run_item, version_of};
use control_api::handler::DOWNLOAD_URL_TTL;
use ham_core::snapshot::SNAPSHOT_REF;
use harness_protocol::store::{RecoveredCopyRef, s3};
use serde_json::{Value, json};

fn body(extra: Value) -> Value {
    let mut b = json!({
        "prompt": "Add percent_change(a, b) with tests",
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

#[tokio::test]
async fn b854_recover_after_stop_false_is_stored_and_true_is_dropped() {
    let w = World::new();
    // Off is stored as sent.
    let (status, off) = start(&w, "k-1", &body(json!({"recoverAfterStop": false}))).await;
    assert_eq!(status, 201, "{off}");
    let kept: Value =
        serde_json::from_slice(&stored_bytes(&w, off["runId"].as_str().unwrap())).unwrap();
    assert_eq!(kept["recoverAfterStop"], json!(false), "{kept}");
    // The default, said or not, is stored as no member, and the two stored requests are the same bytes.
    let (status, plain) = start(&w, "k-2", &body(json!({}))).await;
    assert_eq!(status, 201, "{plain}");
    let (status, said) = start(&w, "k-3", &body(json!({"recoverAfterStop": true}))).await;
    assert_eq!(status, 201, "{said}");
    let a = stored_bytes(&w, plain["runId"].as_str().unwrap());
    let b = stored_bytes(&w, said["runId"].as_str().unwrap());
    println!(
        "B854 TRIGGER PRESENT: three runs were stored: {}",
        plain["runId"] != said["runId"] && plain["runId"] != off["runId"]
    );
    assert!(
        serde_json::from_slice::<Value>(&b)
            .unwrap()
            .get("recoverAfterStop")
            .is_none(),
        "`true` is the default and is stored as no member"
    );
    assert_eq!(
        a, b,
        "the stored request of a default run does not depend on whether the client said the default"
    );
}

#[tokio::test]
async fn b854_a_value_that_is_no_boolean_is_refused_and_nothing_is_written() {
    for bad in [json!("off"), json!(0), json!({"enabled": false})] {
        let w = World::new();
        let (status, v) = start(&w, "k-4", &body(json!({"recoverAfterStop": bad}))).await;
        assert_eq!(status, 400, "{bad}: {v}");
        assert_eq!(v["code"], "bad-request", "{v}");
        assert!(w.runs.lock().unwrap().is_empty(), "no run was created");
        assert_eq!(w.count("blob_put"), 0, "no blob was written");
    }
}

// ---- the download ------------------------------------------------------------------------------------------------------------------

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
/// The commit of the copy session 1 saved while it worked.
const SAVED: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
/// The commit of the files the service read back from the storage of session 1.
const READ_BACK: &str = "3333333333333333333333333333333333333333";
const BASE: &str = "1111111111111111111111111111111111111111";

fn path() -> String {
    format!("/v1/runs/{RUN}/snapshot/download-url")
}

fn bundle(head: &str) -> Vec<u8> {
    format!("# v2 git bundle\n-{BASE} base subject\n{head} {SNAPSHOT_REF}\n\nPACK-{head}")
        .into_bytes()
}

fn record(head: &str) -> Vec<u8> {
    json!({"schemaVersion": 1, "headSha": head, "baseSha": BASE, "at": "2026-11-16T14:02:51Z", "count": 3})
        .to_string()
        .into_bytes()
}

fn read_back_key(n: u32) -> String {
    harness_protocol::direct::s3::recovery_bundle_key(RUN, n)
}

/// A run of `ME` that is in its session `slice`. Session 1 saved a copy at `SAVED` and was lost; the service read its storage back
/// into a copy at `READ_BACK`. `chosen`: the service chose to go on from the files read back, and the Run item names them.
fn world(status: &str, slice: u32, chosen: bool) -> World {
    use harness_protocol::direct::s3 as keys;
    let w = World::new();
    let mut run = run_item(
        RUN,
        ME,
        status,
        "2026-11-16T14:00:00Z",
        json!({"mode": "s3-only"}),
    );
    run.slice = Some(slice);
    if chosen {
        run.recovered_copy = Some(Box::new(RecoveredCopyRef {
            session: 1,
            key: read_back_key(1),
            version_id: None,
            head_sha: READ_BACK.into(),
            at: "2026-11-16T14:05:00Z".into(),
            newest_write_at: None,
            bundle_bytes: bundle(READ_BACK).len() as u64,
        }));
    }
    w.add_run(run);
    w.put_object(&keys::snapshot_bundle_key_t(RUN, 1), bundle(SAVED));
    w.put_object(&keys::snapshot_json_key_t(RUN, 1), record(SAVED));
    w.put_object(&read_back_key(1), bundle(READ_BACK));
    w
}

#[tokio::test]
async fn b854_pull_snapshot_serves_the_files_read_back_when_they_are_the_newest_copy() {
    // The service chose the files read back from session 1, and session 2 has saved no copy yet: they are the run's newest copy.
    let w = world("active", 2, true);
    let (status, body, logs) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["headSha"], READ_BACK,
        "the files read back, not the older saved copy: {body}"
    );
    assert_eq!(body["recovered"], true, "{body}");
    assert_eq!(body["session"], 1, "{body}");
    assert_eq!(
        body["takenAt"], "2026-11-16T14:05:00Z",
        "the time the service chose them"
    );
    assert!(
        body.get("count").is_none(),
        "a copy read back has no copy number: {body}"
    );
    assert!(body.get("finalCopy").is_none(), "{body}");
    assert_eq!(body["sizeBytes"], bundle(READ_BACK).len());
    assert!(
        body["url"].as_str().unwrap().contains(&read_back_key(1)),
        "{body}"
    );
    assert!(logs.is_empty(), "{logs:?}");
    // Exactly the read-back object is signed, pinned to the version whose header was read.
    assert_eq!(
        w.presigned.lock().unwrap().clone(),
        vec![(read_back_key(1), DOWNLOAD_URL_TTL)]
    );
    assert_eq!(
        w.presigned_versions.lock().unwrap().clone(),
        vec![Some(version_of(&bundle(READ_BACK)))]
    );

    // Files that were read back and not used are never served as the run's newest copy: the saved copy is.
    let w = world("active", 2, false);
    assert!(
        w.objects.lock().unwrap().contains_key(&read_back_key(1)),
        "TRIGGER PRESENT: the bucket holds files read back from session 1"
    );
    let (status, body, _) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["headSha"], SAVED, "{body}");
    assert!(body.get("recovered").is_none(), "{body}");
    assert!(body.get("session").is_none(), "{body}");
    assert_eq!(body["count"], 3);
}

#[tokio::test]
async fn b854_a_later_sessions_copy_is_served_once_it_exists() {
    use harness_protocol::direct::s3 as keys;
    let later = "5555555555555555555555555555555555555555";
    let w = world("active", 2, true);
    assert_eq!(
        post(&w, ME, &path(), &[], "").await.1["headSha"],
        READ_BACK,
        "TRIGGER PRESENT: before session 2 saves a copy, the files read back are served"
    );
    w.put_object(&keys::snapshot_bundle_key_t(RUN, 2), bundle(later));
    w.put_object(&keys::snapshot_json_key_t(RUN, 2), record(later));
    let (status, body, _) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["headSha"], later,
        "the copy session 2 saved is newer: {body}"
    );
    assert!(body.get("recovered").is_none(), "{body}");
}

#[tokio::test]
async fn b854_a_recovery_object_that_names_another_commit_is_unreadable() {
    // The object under the chosen key names another commit than the one the service chose: nothing is signed, nothing is removed.
    let w = world("active", 2, true);
    let other = "7777777777777777777777777777777777777777";
    w.put_object(&read_back_key(1), bundle(other));
    let (status, body, logs) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["code"], "snapshot-unreadable");
    assert!(w.presigned.lock().unwrap().is_empty());
    assert!(
        logs.iter().any(|l| l.contains("another commit")),
        "the diagnostic says why: {logs:?}"
    );
    assert!(
        w.objects.lock().unwrap().contains_key(&read_back_key(1)),
        "the check removes nothing"
    );

    // A chosen object that is gone is no refusal: the route answers with the saved copy.
    let w = world("active", 2, true);
    w.objects.lock().unwrap().remove(&read_back_key(1));
    let (status, body, _) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["headSha"], SAVED, "{body}");
    assert!(body.get("recovered").is_none(), "{body}");
}

/// The gate of the maintainers' rule: files the service read back and did not use stay retrievable with one command. The service did
/// not choose them (the Run item names none), a later session has saved its own copy, and the run may have ended blocked: the files
/// read back from session n are still answered for `readBack=n`.
#[tokio::test]
async fn b854_files_that_were_read_back_and_not_used_are_downloaded_by_the_number_of_their_session()
{
    use harness_protocol::direct::s3 as keys;
    let (second, later) = (
        "4444444444444444444444444444444444444444",
        "5555555555555555555555555555555555555555",
    );
    let w = world("active", 3, false);
    w.put_object(&read_back_key(2), bundle(second));
    w.put_object(&keys::snapshot_bundle_key_t(RUN, 3), bundle(later));
    w.put_object(&keys::snapshot_json_key_t(RUN, 3), record(later));
    assert_eq!(
        post(&w, ME, &path(), &[], "").await.1["headSha"],
        later,
        "TRIGGER PRESENT: the plain route answers with the newest saved copy, not with files read back"
    );

    // Session 1's files, which the service did not use.
    let (status, body, logs) = post(&w, ME, &path(), &[("readBack", "1")], "").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["headSha"], READ_BACK, "{body}");
    assert_eq!(
        (body["recovered"].clone(), body["session"].clone()),
        (json!(true), json!(1))
    );
    assert!(
        body["url"].as_str().unwrap().contains(&read_back_key(1)),
        "{body}"
    );
    assert!(
        body.get("count").is_none() && body.get("finalCopy").is_none(),
        "{body}"
    );
    assert!(logs.is_empty(), "{logs:?}");
    // The named session is served, not the newest read-back object.
    let (status, body, _) = post(&w, ME, &path(), &[("readBack", "2")], "").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["headSha"], second, "{body}");
    assert_eq!(body["session"], 2);
    assert_eq!(
        w.presigned.lock().unwrap().clone(),
        vec![
            (keys::snapshot_bundle_key_t(RUN, 3), DOWNLOAD_URL_TTL),
            (read_back_key(1), DOWNLOAD_URL_TTL),
            (read_back_key(2), DOWNLOAD_URL_TTL)
        ]
    );
    assert_eq!(
        w.presigned_versions.lock().unwrap()[1..],
        [
            Some(version_of(&bundle(READ_BACK))),
            Some(version_of(&bundle(second)))
        ],
        "each is pinned to the version whose header was read"
    );

    // A session whose storage was not read back: a 404 that names the session.
    let (status, body, _) = post(&w, ME, &path(), &[("readBack", "3")], "").await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["code"], "no-read-back");
    assert_eq!(
        body["message"],
        "no files were read back from the storage of session 3 of this run"
    );

    // A value that is no session number is refused before anything is read.
    for bad in ["0", "257", "x", "-1", "01", " 1", ""] {
        let w = world("active", 2, false);
        let (status, body, _) = post(&w, ME, &path(), &[("readBack", bad)], "").await;
        assert_eq!(status, 400, "{bad:?}: {body}");
        assert_eq!(body["code"], "bad-read-back", "{bad:?}");
        assert_eq!(
            body["message"],
            "readBack names a session of the run by its number, 1 to 256"
        );
        assert!(
            !w.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with("read_prefix")),
            "{bad:?}: nothing is read"
        );
    }

    // An object that is no bundle of the snapshot ref is never signed.
    let w = world("active", 2, false);
    w.put_object(&read_back_key(1), b"not a bundle".to_vec());
    let (status, body, _) = post(&w, ME, &path(), &[("readBack", "1")], "").await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["code"], "snapshot-unreadable");
    assert!(w.presigned.lock().unwrap().is_empty());

    // Only the owner: another principal gets a 404 and nothing is read or signed.
    let w = world("active", 2, false);
    let (status, body, _) = post(&w, OTHER, &path(), &[("readBack", "1")], "").await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["code"], "not-found");
    assert!(w.presigned.lock().unwrap().is_empty());

    // A run that ended blocked (its delivery was refused) keeps the files read back downloadable, chosen or not.
    for chosen in [true, false] {
        let w = world("blocked", 2, chosen);
        let (status, body, _) = post(&w, ME, &path(), &[("readBack", "1")], "").await;
        assert_eq!(status, 200, "{chosen}: {body}");
        assert_eq!(body["headSha"], READ_BACK);
        assert_eq!(body["runStatus"], "blocked");
    }
}

/// A run of `ME` in `status` whose keep-the-work cancel is recorded. Session 1 saved a copy at `SAVED`, and the service read its
/// storage back into a copy at `READ_BACK`; `chosen`: the Run item names the files read back as the copy the run ended with.
fn cancelled_world(status: &str, chosen: bool) -> World {
    use harness_protocol::direct::s3 as keys;
    let w = World::new();
    let mut run = run_item(
        RUN,
        ME,
        status,
        "2026-11-16T14:00:00Z",
        json!({"mode": "s3-only"}),
    );
    run.slice = Some(1);
    run.cancel_requested_at = Some("2026-11-16T14:04:00Z".into());
    if chosen {
        run.recovered_copy = Some(Box::new(RecoveredCopyRef {
            session: 1,
            key: read_back_key(1),
            version_id: None,
            head_sha: READ_BACK.into(),
            at: "2026-11-16T14:05:00Z".into(),
            newest_write_at: None,
            bundle_bytes: bundle(READ_BACK).len() as u64,
        }));
    }
    w.add_run(run);
    w.put_object(&keys::snapshot_bundle_key_t(RUN, 1), bundle(SAVED));
    w.put_object(&keys::snapshot_json_key_t(RUN, 1), record(SAVED));
    w.put_object(&read_back_key(1), bundle(READ_BACK));
    w
}

/// With: a discard wins over the read of a stopped session's storage and delivers nothing, and a cancelled run ends with
/// the copy the service chose. Neither removes or hides what was read: the files read back from session 1 are answered for
/// `readBack=1` for a run that was discarded (`cleared`) and for a cancelled run that has ended, whether the service chose them or not.
#[tokio::test]
async fn b853_b854_files_read_back_stay_downloadable_after_a_discard_and_after_a_cancelled_run_ended()
 {
    for status in ["cleared", "complete", "blocked"] {
        for chosen in [true, false] {
            let w = cancelled_world(status, chosen);
            let (code, body, logs) = post(&w, ME, &path(), &[("readBack", "1")], "").await;
            assert_eq!(code, 200, "{status} chosen={chosen}: {body}");
            assert_eq!(
                body["headSha"], READ_BACK,
                "{status} chosen={chosen}: {body}"
            );
            assert_eq!(
                (body["recovered"].clone(), body["session"].clone()),
                (json!(true), json!(1)),
                "{status} chosen={chosen}"
            );
            assert_eq!(body["runStatus"], status, "{body}");
            assert!(logs.is_empty(), "{logs:?}");
            assert!(
                w.objects.lock().unwrap().contains_key(&read_back_key(1)),
                "the route removes nothing"
            );
        }
        // The plain route of a cancelled run that ended with the files read back answers with them: `ham pull <runId> --snapshot`
        // downloads the copy the run ended with.
        let w = cancelled_world(status, true);
        let (code, body, _) = post(&w, ME, &path(), &[], "").await;
        assert_eq!(code, 200, "{status}: {body}");
        assert_eq!(body["headSha"], READ_BACK, "{status}: {body}");
    }
}
