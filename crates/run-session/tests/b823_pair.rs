//! The two objects of a copy are read as one. agentd stores `snapshot.bundle` and then `snapshot.json`; a session that is
//! stopped between the two leaves the bundle of its newest copy beside the record of the copy before it, or beside no record. The bundle is
//! the work: a delivery from the last copy, the restore of the next session and the blocked reason take the commit the bundle itself names,
//! as the download route does. Before, they took the record's commit, which the bundle does not hold: the validator refused the delivery
//! ("headSha ... is not in the source bundle"), the next session could not start, and a bundle without a record was no copy to deliver.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::direct::{DirectOutcome, s3};
use harness_protocol::target::RunTarget;
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use run_session::slices::{RestoreKind, best_head};
use serde_json::{Value, json};
use support::fakes::*;

/// The copy the record names: the one before the newest.
const COPY_BEFORE: &str = "3333333333333333333333333333333333333333";
/// The copy the bundle holds: the newest.
const COPY_NEWEST: &str = "4444444444444444444444444444444444444444";

async fn call(mem: &Mem, v: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(v).unwrap();
    handle(&mem.deps(), &op).await
}

async fn plan(mem: &Mem, session: Value) -> Value {
    call(
        mem,
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged(), "session": session}),
    )
    .await
    .unwrap()
}

/// A run whose one session started 662 s before the plan is made and then missed its heartbeat (`session_lost.rs`).
async fn lost_session() -> Mem {
    let mem = Mem::default();
    mem.seed(
        RunTarget::S3Only,
        "Add percent_change(a, b) with tests",
        MODEL,
    );
    call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
    mem.0
        .turns
        .lock()
        .unwrap()
        .get_mut(&("direct".to_string(), 1))
        .unwrap()
        .started_at_ms = Some(T0 - 662 * 1000);
    mem.clear_events();
    mem
}

fn missed_heartbeat() -> Value {
    json!({"turn": {}, "failure": {"Error": "States.Timeout"}})
}

/// The first bytes of a bundle of `refs/heads/snapshot` at `commit`, as git writes them.
fn bundle_of(commit: &str) -> Vec<u8> {
    format!("# v2 git bundle\n{commit} refs/heads/snapshot\n\nPACK").into_bytes()
}

fn record_of(commit: &str) -> Vec<u8> {
    serde_json::to_vec(
        &json!({"schemaVersion": 1, "headSha": commit, "baseSha": BASE,
        "at": "2026-10-13T18:50:00Z", "count": 5}),
    )
    .unwrap()
}

#[tokio::test]
async fn b823_a_session_stopped_between_the_two_uploads_delivers_the_commit_its_bundle_holds() {
    let mem = lost_session().await;
    mem.put_blob(&s3::snapshot_bundle_key_t(RUN, 1), bundle_of(COPY_NEWEST));
    mem.put_blob(&s3::snapshot_json_key_t(RUN, 1), record_of(COPY_BEFORE));
    let p = plan(&mem, missed_heartbeat()).await;
    assert_eq!(p["deliver"], true, "{p}");
    assert_eq!(p["headSource"], "snapshot");
    assert_eq!(
        p["headSha"], COPY_NEWEST,
        "TRIGGER PRESENT: the plan names the record's commit, which the bundle does not hold: {p}"
    );
    assert_eq!(p["validate"]["headSha"], COPY_NEWEST);
    // The record's time is the time of the copy before, so the detail names none.
    assert_eq!(
        p["detail"],
        "no heartbeat for 600 s. The work is the last saved snapshot."
    );
}

#[tokio::test]
async fn b823_a_bundle_whose_record_never_came_is_delivered() {
    let mem = lost_session().await;
    mem.put_blob(&s3::snapshot_bundle_key_t(RUN, 1), bundle_of(COPY_NEWEST));
    let p = plan(&mem, missed_heartbeat()).await;
    assert_eq!(
        p["deliver"], true,
        "TRIGGER PRESENT: a bundle the route serves is not delivered: {p}"
    );
    assert_eq!(p["headSha"], COPY_NEWEST);
    assert_eq!(p["headSource"], "snapshot");
}

#[tokio::test]
async fn b823_a_pair_that_agrees_keeps_the_time_of_its_record() {
    // Regression: passes before the change.
    let mem = lost_session().await;
    mem.put_blob(&s3::snapshot_bundle_key_t(RUN, 1), bundle_of(COPY_NEWEST));
    mem.put_blob(&s3::snapshot_json_key_t(RUN, 1), record_of(COPY_NEWEST));
    let p = plan(&mem, missed_heartbeat()).await;
    assert_eq!(p["headSha"], COPY_NEWEST);
    assert_eq!(
        p["detail"],
        "no heartbeat for 600 s. The work is the last saved snapshot, taken at 2026-10-13 18:50 UTC."
    );
}

#[tokio::test]
async fn b823_a_bundle_whose_header_cannot_be_read_is_judged_by_its_record_as_before() {
    // Regression: passes before the change. The validator is the judge of such a bundle, as it always was.
    let mem = lost_session().await;
    mem.put_snapshot(COPY_BEFORE);
    let p = plan(&mem, missed_heartbeat()).await;
    assert_eq!(p["deliver"], true, "{p}");
    assert_eq!(p["headSha"], COPY_BEFORE);
}

#[tokio::test]
async fn b823_the_next_session_restores_the_commit_the_bundle_holds() {
    let mem = Mem::default();
    mem.put_blob(&s3::snapshot_bundle_key_t(RUN, 1), bundle_of(COPY_NEWEST));
    mem.put_blob(&s3::snapshot_json_key_t(RUN, 1), record_of(COPY_BEFORE));
    let restore = best_head(&mem, BUCKET, RUN, 1, None)
        .await
        .unwrap()
        .expect("session 1 left a copy");
    assert_eq!(restore.kind, RestoreKind::Snapshot);
    assert_eq!(restore.key, s3::snapshot_bundle_key_t(RUN, 1));
    assert_eq!(
        restore.head.as_deref(),
        Some(COPY_NEWEST),
        "TRIGGER PRESENT: the restored repository would never be at the commit the service names"
    );

    // A bundle without a record is a copy to restore too.
    let alone = Mem::default();
    alone.put_blob(&s3::snapshot_bundle_key_t(RUN, 1), bundle_of(COPY_NEWEST));
    let restore = best_head(&alone, BUCKET, RUN, 1, None).await.unwrap();
    assert_eq!(restore.and_then(|r| r.head).as_deref(), Some(COPY_NEWEST));
}

/// A blocked run whose record names another commit than its bundle: the reason names the command without the record's time, which is the
/// time of an older copy.
#[tokio::test]
async fn b823_a_blocked_run_whose_record_lags_names_the_command_without_the_older_time() {
    let mem = Mem::default();
    mem.seed(RunTarget::S3Only, "Build a market terminal", MODEL);
    mem.put_bundle();
    mem.put_summary(&summary(
        DirectOutcome::ReportedDone,
        Some("Built it."),
        true,
    ));
    mem.put_blob(&s3::snapshot_bundle_key(RUN), bundle_of(COPY_NEWEST));
    mem.put_blob(&s3::snapshot_json_key(RUN), record_of(COPY_BEFORE));
    let out = call(
        &mem,
        json!({"op": "mark-blocked", "runId": RUN, "state": {"runId": RUN, "validated": {"result": {"pass": false,
            "violations": [{"rule": "bundle-size", "detail": "the bundle is 3 bytes; the limit is 2 bytes"}]}}}}),
    )
    .await
    .unwrap();
    let reason = out["reason"].as_str().unwrap();
    assert!(
        reason.ends_with(&format!(
            "Download the run's last snapshot with `ham pull {RUN} --snapshot`; it does not hold work done after it was saved."
        )),
        "TRIGGER PRESENT: {reason}"
    );
}

/// A bundle whose header cannot be read and that has no record is no copy the route can serve (it answers 422): the reason of a run
/// whose session was lost then says that no work was saved and names no command.
#[tokio::test]
async fn b823_a_reason_never_says_both_that_no_work_was_saved_and_where_to_download_it() {
    let mem = lost_session().await;
    mem.put_blob(&s3::snapshot_bundle_key(RUN), b"SNAP".to_vec());
    let p = plan(&mem, missed_heartbeat()).await;
    let out = call(
        &mem,
        json!({"op": "mark-blocked", "runId": RUN,
               "state": {"runId": RUN, "session": missed_heartbeat(), "plan": {"result": p.clone()}}}),
    )
    .await
    .unwrap();
    let reason = out["reason"].as_str().unwrap_or_default().to_string();
    assert!(reason.contains("before any work was saved"), "{p} {reason}");
    assert!(
        !reason.contains("--snapshot"),
        "TRIGGER PRESENT: the reason says no work was saved and names a copy to download: {reason}"
    );
}
