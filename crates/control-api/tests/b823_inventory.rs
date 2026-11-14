//! However a run ended, `POST /v1/runs/{id}/snapshot/download-url` returns the latest saved copy of its work. One test per kind of
//! copy (the copy of the end, a copy made while the run worked, none) over every status a run can have, on a store that behaves like the
//! deployed bucket: the route's role may read the two snapshot keys and may not read a key that does not exist.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, World, post, run_item};
use ham_core::snapshot::{SNAPSHOT_REF, SnapshotDownload};
use harness_protocol::direct::s3;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

const RUN: &str = "01M76EJVX4BSZ814X2DVVN834F";
const BASE: &str = "f6dc8e0f72e20000000000000000000000000000";
const FINAL: &str = "ad2645e679230000000000000000000000000000";
const EARLIER: &str = "cced2ef89ca70000000000000000000000000000";

/// Every status a run can have (`GoalStatus`).
const STATUSES: [&str; 8] = [
    "active",
    "paused",
    "blocked",
    "usage_limited",
    "budget_limited",
    "complete",
    "cleared",
    "stalled",
];

fn path() -> String {
    format!("/v1/runs/{RUN}/snapshot/download-url")
}

fn bundle(head: &str) -> Vec<u8> {
    let mut b = format!("# v2 git bundle\n{head} {SNAPSHOT_REF}\n\nPACK").into_bytes();
    b.extend(std::iter::repeat_n(7u8, 4_000));
    b
}

/// `snapshot.json` as agentd writes it: the copy of the end carries `final`, a copy made while the run worked does not.
fn record(head: &str, count: u32, last: bool) -> Vec<u8> {
    let mut v = json!({"schemaVersion": 1, "headSha": head, "baseSha": BASE, "at": "2026-11-12T00:47:40Z", "count": count});
    if last {
        v["final"] = json!(true);
    }
    v.to_string().into_bytes()
}

/// A run of `ME` in `status` on a store that refuses the read of a key that does not exist, as the deployed bucket does.
fn world(status: &str) -> World {
    let w = World::new();
    w.no_list_permission.store(true, Ordering::SeqCst);
    w.add_run(run_item(
        RUN,
        ME,
        status,
        "2026-11-11T23:20:13Z",
        json!({"mode": "s3-only"}),
    ));
    w
}

/// [`world`] for a run that renewed its session once: its current session is the second.
fn two_sessions(status: &str) -> World {
    let w = World::new();
    w.no_list_permission.store(true, Ordering::SeqCst);
    let mut run: Value = serde_json::to_value(run_item(
        RUN,
        ME,
        status,
        "2026-11-11T23:20:13Z",
        json!({"mode": "s3-only"}),
    ))
    .unwrap();
    run["slice"] = json!(2);
    w.add_run(serde_json::from_value(run).unwrap());
    w
}

fn save(w: &World, turn: u32, head: &str, count: u32, last: bool) {
    w.put_object(
        &s3::snapshot_json_key_t(RUN, turn),
        record(head, count, last),
    );
    w.put_object(&s3::snapshot_bundle_key_t(RUN, turn), bundle(head));
}

#[tokio::test]
async fn b823_however_a_run_ended_the_route_returns_the_copy_of_its_end() {
    for status in STATUSES {
        let w = world(status);
        save(&w, 1, FINAL, 9, true);
        let (code, body, _) = post(&w, ME, &path(), &[], "").await;
        assert_eq!(code, 200, "{status}: {body}");
        let d: SnapshotDownload = serde_json::from_value(body.clone()).unwrap();
        assert_eq!(d.head_sha, FINAL, "{status}");
        assert_eq!(d.count, Some(9));
        assert_eq!(body["finalCopy"], true, "{status}: {body}");
        assert_eq!(body["runStatus"], status);
        assert_eq!(
            body["engine"], "cline",
            "{status}: the engine of the run is named"
        );
        assert!(d.url.contains(&s3::snapshot_bundle_key(RUN)));
    }
}

#[tokio::test]
async fn b823_a_copy_made_while_the_run_worked_is_returned_and_is_not_called_the_end() {
    for status in STATUSES {
        let w = world(status);
        save(&w, 1, EARLIER, 8, false);
        let (code, body, _) = post(&w, ME, &path(), &[], "").await;
        assert_eq!(code, 200, "{status}: {body}");
        assert_eq!(body["headSha"], EARLIER);
        assert!(body.get("finalCopy").is_none(), "{status}: {body}");
        // A client reads the missing member as "not the copy of the end".
        let d: SnapshotDownload = serde_json::from_value(body).unwrap();
        assert_eq!(d.count, Some(8));
    }
}

#[tokio::test]
async fn b823_a_run_without_a_copy_is_a_404_that_states_only_the_fact() {
    for status in STATUSES {
        let w = world(status);
        assert!(
            w.no_list_permission.load(Ordering::SeqCst),
            "TRIGGER PRESENT: the read of a key that does not exist is refused"
        );
        let (code, body, logs) = post(&w, ME, &path(), &[], "").await;
        assert_eq!(code, 404, "{status}: {body} {logs:?}");
        assert_eq!(body["code"], "no-snapshot");
        let message = body["message"].as_str().unwrap();
        if status == "active" {
            assert_eq!(
                message,
                "no copy of this run's work is saved yet; ask again when the run has changed a file"
            );
        } else {
            assert_eq!(message, "no copy of this run's work was saved", "{status}");
        }
        // No component name and no interval of an earlier release.
        assert!(
            !message.contains("agentd") && !message.contains("10 minutes"),
            "{message}"
        );
        assert!(w.presigned.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn b823_a_record_without_its_bundle_is_no_copy_and_a_bundle_whose_record_never_came_is_served()
 {
    let w = world("blocked");
    w.put_object(&s3::snapshot_json_key(RUN), record(FINAL, 9, true));
    assert_eq!(post(&w, ME, &path(), &[], "").await.0, 404);
    // The bundle went up and its record did not (the session ended between the two, or the record's upload failed). A bundle that a run
    // that works wrote a moment ago may still get its record.
    let w = world("active");
    w.put_object_at(
        &s3::snapshot_bundle_key(RUN),
        bundle(FINAL),
        &control_api::ids::now_rfc3339(),
    );
    let (code, body, _) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(code, 409, "{body}");
    assert_eq!(body["code"], "snapshot-updating");
    // An old one is the copy there is: the work is served from the bundle's own commit, and the answer says the record is missing.
    let w = world("blocked");
    w.put_object_at(
        &s3::snapshot_bundle_key(RUN),
        bundle(FINAL),
        "2026-10-15T13:30:00Z",
    );
    let (code, body, _) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["headSha"], FINAL);
    assert_eq!(body["recordStale"], true);
    assert_eq!(body["takenAt"], "2026-10-15T13:30:00Z");
    assert!(body.get("finalCopy").is_none(), "{body}");
    assert!(body.get("count").is_none(), "{body}");
}

#[tokio::test]
async fn b823_a_later_session_without_a_copy_falls_back_to_the_session_before_it() {
    // Session 2 has saved nothing yet; session 1 ended with its final copy.
    let w = World::new();
    w.no_list_permission.store(true, Ordering::SeqCst);
    let mut run: Value = serde_json::to_value(run_item(
        RUN,
        ME,
        "active",
        "2026-11-11T23:20:13Z",
        json!({"mode": "s3-only"}),
    ))
    .unwrap();
    run["slice"] = json!(2);
    w.add_run(serde_json::from_value(run).unwrap());
    save(&w, 1, FINAL, 9, true);
    let (code, body, _) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["headSha"], FINAL);
    assert!(
        body["url"]
            .as_str()
            .unwrap()
            .contains(&s3::snapshot_bundle_key_t(RUN, 1))
    );
    // Once session 2 saves, its copy is the newest.
    save(&w, 2, EARLIER, 1, false);
    let (_, body, _) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(body["headSha"], EARLIER);
    assert!(
        body["url"]
            .as_str()
            .unwrap()
            .contains(&s3::snapshot_bundle_key_t(RUN, 2))
    );
}

#[tokio::test]
async fn b823_a_bucket_that_cannot_answer_is_a_503_and_never_no_copy() {
    let w = world("blocked");
    save(&w, 1, FINAL, 9, true);
    w.objects_down.store(true, Ordering::SeqCst);
    let (code, body, _) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(code, 503, "{body}");
    assert_ne!(body["code"], "no-snapshot");
}

#[tokio::test]
async fn b823_the_engine_the_run_used_is_the_one_the_answer_names() {
    let w = World::new();
    let mut run: Value = serde_json::to_value(run_item(
        RUN,
        ME,
        "blocked",
        "2026-11-11T23:20:13Z",
        json!({"mode": "s3-only"}),
    ))
    .unwrap();
    run["engine"] = json!("opencode");
    w.add_run(serde_json::from_value(run).unwrap());
    save(&w, 1, FINAL, 3, true);
    let (code, body, _) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["engine"], "opencode");
}

/// `ham resume` refuses a run that delivered nothing; the refusal names the command for its saved work.
#[tokio::test]
async fn b823_the_resume_refusal_of_a_blocked_or_cleared_run_names_the_snapshot_command() {
    for (status, lead) in [
        (
            "blocked",
            "is blocked and delivered no zip; a retry starts it again",
        ),
        ("cleared", "was cleared; it cannot be resumed"),
    ] {
        let w = world(status);
        let mut r = chat_support::request(ME, "POST", &format!("/v1/runs/{RUN}/resume"), &[], "{}");
        r.headers
            .insert("idempotency-key".into(), format!("resume-{RUN}"));
        let out = control_api::handler::handle(r, &chat_support::deps(&w)).await;
        let body: Value = serde_json::from_str(&out.body).unwrap();
        assert_eq!(out.status, 409, "{status}: {body}");
        assert_eq!(body["code"], "run-not-resumable");
        let message = body["message"].as_str().unwrap();
        assert!(message.contains(lead), "{status}: {message}");
        assert!(
            message.ends_with(&format!(
                "`ham pull {RUN} --snapshot` downloads the latest saved copy of its work"
            )),
            "{status}: {message}"
        );
    }
}

#[tokio::test]
async fn b823_the_copy_of_the_end_of_an_earlier_session_is_not_called_the_end_of_the_run() {
    // Session 1 ended with its copy of the end; session 2 worked on and saved nothing before the run was blocked.
    let w = two_sessions("blocked");
    save(&w, 1, FINAL, 9, true);
    let (code, body, _) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["headSha"], FINAL, "the newest copy there is");
    assert!(
        body.get("finalCopy").is_none(),
        "what session 2 did is not in it: {body}"
    );
    // The copy of the end of the run's last session is the end of the run.
    save(&w, 2, EARLIER, 3, true);
    let (_, body, _) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(body["headSha"], EARLIER);
    assert_eq!(body["finalCopy"], true, "{body}");
}

#[tokio::test]
async fn b823_a_session_whose_record_has_no_bundle_does_not_hide_the_copy_of_the_session_before() {
    let w = two_sessions("blocked");
    save(&w, 1, FINAL, 9, false);
    w.put_object(&s3::snapshot_json_key_t(RUN, 2), record(EARLIER, 1, false));
    let (code, body, _) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["headSha"], FINAL);
    assert!(
        body["url"]
            .as_str()
            .unwrap()
            .contains(&s3::snapshot_bundle_key_t(RUN, 1))
    );
}

/// A run that ended saves nothing more: a bundle whose record lags is the copy there is, served at once from the bundle's own commit. Before,
/// the route answered 409 "a new snapshot is being saved" for two minutes to a run that saves nothing.
#[tokio::test]
async fn b823_an_ended_run_whose_record_lags_is_served_at_once() {
    for status in STATUSES.into_iter().filter(|s| *s != "active") {
        let w = world(status);
        w.put_object_at(
            &s3::snapshot_bundle_key(RUN),
            bundle(FINAL),
            &control_api::ids::now_rfc3339(),
        );
        w.put_object(&s3::snapshot_json_key(RUN), record(EARLIER, 3, false));
        let (code, body, _) = post(&w, ME, &path(), &[], "").await;
        assert_eq!(code, 200, "TRIGGER PRESENT: {status}: {body}");
        assert_eq!(body["headSha"], FINAL, "{status}");
        assert_eq!(body["recordStale"], true, "{status}");
        assert!(body.get("finalCopy").is_none(), "{status}: {body}");
    }
    // A run that works may be saving the pair right now: it is asked again in a few seconds.
    let w = world("active");
    w.put_object_at(
        &s3::snapshot_bundle_key(RUN),
        bundle(FINAL),
        &control_api::ids::now_rfc3339(),
    );
    w.put_object(&s3::snapshot_json_key(RUN), record(EARLIER, 3, false));
    let (code, body, _) = post(&w, ME, &path(), &[], "").await;
    assert_eq!(code, 409, "{body}");
    assert_eq!(body["code"], "snapshot-updating");
}
