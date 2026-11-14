//! A delivery that is refused never removes or hides the work. `mark-blocked` ends every blocked reason with where the work is:
//! the snapshot command when the run saved a copy (the copy of the session's end holds the work as the engine left it; an older copy is
//! named with its time), or the fact that there is none. Nothing that judges or refuses a delivery writes the two snapshot objects.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::common::Engine;
use harness_protocol::direct::{DirectOutcome, s3};
use harness_protocol::goal::GoalStatus;
use harness_protocol::target::RunTarget;
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

async fn call(mem: &Mem, v: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(v).unwrap();
    handle(&mem.deps(), &op).await
}

fn seeded(target: RunTarget) -> Mem {
    let mem = Mem::default();
    mem.seed(target, "Build a market terminal", MODEL);
    mem.put_bundle();
    mem.put_summary(&summary(
        DirectOutcome::ReportedDone,
        Some("Built it."),
        true,
    ));
    mem
}

/// The copy of the session's end, as agentd writes its record: `final` and the commit of the final head.
fn put_final_copy(mem: &Mem, head: &str) {
    mem.put_blob(&s3::snapshot_bundle_key(RUN), b"SNAP".to_vec());
    mem.put_blob(
        &s3::snapshot_json_key(RUN),
        serde_json::to_vec(
            &json!({"schemaVersion": 1, "headSha": head, "baseSha": BASE,
            "at": "2026-11-12T00:47:40Z", "count": 9, "final": true}),
        )
        .unwrap(),
    );
}

async fn blocked(mem: &Mem, state: Value) -> String {
    let out = call(
        mem,
        json!({"op": "mark-blocked", "runId": RUN, "state": state}),
    )
    .await
    .unwrap();
    let reason = out["reason"].as_str().unwrap().to_string();
    let event = mem
        .events()
        .into_iter()
        .find(|(t, _)| t == "goal.blocked")
        .unwrap()
        .1;
    assert_eq!(event["reason"], reason, "the event carries the same words");
    assert_eq!(mem.run().status, GoalStatus::Blocked);
    reason
}

fn refused(violations: Value) -> Value {
    json!({"runId": RUN, "validated": {"result": {"pass": false, "violations": violations}}})
}

fn final_words() -> String {
    format!(
        "The run's work is saved as Cline left it. Download it with `ham pull {RUN} --snapshot`."
    )
}

fn older_words() -> String {
    format!(
        "The run's last snapshot was saved at 2026-10-13 18:50 UTC. Download it with `ham pull {RUN} --snapshot`; it does not hold work done after that time."
    )
}

const NO_COPY: &str = "The run has no snapshot to download.";

/// The run of 2026-11-11: a zip run whose delivery was refused. Its work was in the bucket and the reason named no way to it.
#[tokio::test]
async fn b823_a_refused_delivery_names_the_command_that_downloads_the_work_as_the_engine_left_it() {
    // Every kind of refusal, not only a protected path or a secret.
    for violations in [
        json!([{"rule": "workflow-file", "path": ".github/workflows/ci.yml", "detail": "refused"}]),
        json!([{"rule": "path-glob", "path": "x/y", "detail": "x/y is outside the allowed paths"}]),
        json!([{"rule": "bundle-size", "detail": "the bundle is 3 bytes; the limit is 2 bytes"}]),
        json!([{"rule": "file-size", "path": "big.bin", "detail": "big.bin is over 2 GB"}]),
    ] {
        let mem = seeded(RunTarget::S3Only);
        put_final_copy(&mem, HEAD);
        let reason = blocked(&mem, refused(violations.clone())).await;
        assert!(reason.starts_with("bundle validation failed: "), "{reason}");
        assert!(
            reason.ends_with(&format!(". {}", final_words())),
            "{violations}: {reason}"
        );
        assert!(
            !reason.contains("does not hold work done after"),
            "{reason}"
        );
    }
}

#[tokio::test]
async fn b823_a_step_that_failed_after_the_engine_stopped_names_the_command_too() {
    let mem = seeded(RunTarget::S3Only);
    put_final_copy(&mem, HEAD);
    let reason = blocked(
        &mem,
        json!({"runId": RUN, "failure": {"Error": "Packager.Failed", "Cause": "the zip could not be written"}}),
    )
    .await;
    assert!(reason.ends_with(&final_words()), "{reason}");
}

#[tokio::test]
async fn b823_the_sentence_names_the_engine_that_ran() {
    let mem = seeded(RunTarget::S3Only);
    mem.set_run_engine(Engine::OPENCODE);
    put_final_copy(&mem, HEAD);
    let reason = blocked(
        &mem,
        refused(json!([{"rule": "path-glob", "path": "x/y", "detail": "refused"}])),
    )
    .await;
    assert!(
        reason.ends_with(&format!(
            "The run's work is saved as OpenCode left it. Download it with `ham pull {RUN} --snapshot`."
        )),
        "{reason}"
    );
}

#[tokio::test]
async fn b823_an_older_copy_is_named_with_its_time_and_not_as_the_end_state() {
    // A worker older than the copy of the end, or a session that died before it stored one: the last copy made while the run worked.
    let mem = seeded(RunTarget::S3Only);
    mem.put_snapshot(HEAD);
    let reason = blocked(
        &mem,
        refused(json!([{"rule": "bundle-size", "detail": "the bundle is 3 bytes; the limit is 2 bytes"}])),
    )
    .await;
    assert!(
        reason.ends_with(&format!(". {}", older_words())),
        "{reason}"
    );
    assert!(!reason.contains("as Cline left it"), "{reason}");
}

#[tokio::test]
async fn b823_a_run_that_saved_nothing_names_no_command_that_has_nothing_to_download() {
    // The session ended before any work was saved: the reason says so itself, and names no command.
    let mem = seeded(RunTarget::S3Only);
    let reason = blocked(
        &mem,
        json!({"runId": RUN, "plan": {"result": {"deliver": false, "reason": "the session ended before any work was saved"}}}),
    )
    .await;
    assert_eq!(reason, "the session ended before any work was saved");
    // A refused delivery of a run without a copy says that there is none, and still names no command.
    let mem = seeded(RunTarget::S3Only);
    let reason = blocked(
        &mem,
        refused(json!([{"rule": "engine-config", "path": "opencode.json", "detail": "refused"}])),
    )
    .await;
    assert!(
        reason.ends_with(&format!(": refused. {NO_COPY}")),
        "{reason}"
    );
    assert!(!reason.contains("ham pull"), "{reason}");
}

#[tokio::test]
async fn b823_a_copy_of_the_end_that_equals_the_start_is_no_work_to_download() {
    // The engine never changed a file: the copy of the end is the base itself.
    let mem = seeded(RunTarget::S3Only);
    put_final_copy(&mem, BASE);
    let reason = blocked(
        &mem,
        json!({"runId": RUN, "plan": {"result": {"deliver": false, "reason": "Cline did not start"}}}),
    )
    .await;
    assert_eq!(
        reason,
        "Cline did not start. The run changed no file, so there is no work to download."
    );
}

#[tokio::test]
async fn b823_a_bucket_that_cannot_answer_does_not_keep_the_run_from_ending_blocked() {
    let mem = seeded(RunTarget::S3Only);
    put_final_copy(&mem, HEAD);
    mem.deny_read(&s3::snapshot_json_key_t(RUN, 1));
    let reason = blocked(
        &mem,
        refused(json!([{"rule": "bundle-size", "detail": "refused"}])),
    )
    .await;
    assert!(
        reason.ends_with(&format!(
            "Whether the run saved a snapshot could not be read; `ham pull {RUN} --snapshot` downloads it if it did."
        )),
        "{reason}"
    );
    assert!(
        !reason.contains(NO_COPY) && !reason.contains("as Cline left it"),
        "{reason}"
    );
}

/// Nothing that plans, judges, refuses or ends a delivery writes or removes the copy: the two objects are byte for byte what agentd stored.
#[tokio::test]
async fn b823_no_step_of_a_refused_delivery_touches_the_copy() {
    let mem = seeded(RunTarget::S3Only);
    put_final_copy(&mem, HEAD);
    let before = (
        mem.blob(&s3::snapshot_json_key(RUN)).unwrap(),
        mem.blob(&s3::snapshot_bundle_key(RUN)).unwrap(),
    );
    let plan = call(
        &mem,
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged(),
            "session": {"turn": turn_output(true), "failure": {}}}),
    )
    .await
    .unwrap();
    assert_eq!(plan["deliver"], true, "{plan}");
    blocked(
        &mem,
        json!({"runId": RUN, "plan": {"result": plan},
            "validated": {"result": {"pass": false, "violations": [{"rule": "workflow-file", "path": ".github/workflows/ci.yml", "detail": "refused"}]}}}),
    )
    .await;
    let after = (
        mem.blob(&s3::snapshot_json_key(RUN)).unwrap(),
        mem.blob(&s3::snapshot_bundle_key(RUN)).unwrap(),
    );
    assert_eq!(before, after);
}

/// The run renewed its session once: its current session is the second.
fn second_session(mem: &Mem) {
    mem.0.runs.lock().unwrap().get_mut(RUN).unwrap().slice = Some(2);
}

#[tokio::test]
async fn b823_the_copy_of_the_end_of_an_earlier_session_is_named_as_a_last_copy() {
    // Session 1 ended with its copy of the end; session 2 worked on and saved nothing before the run was blocked.
    let mem = seeded(RunTarget::S3Only);
    second_session(&mem);
    put_final_copy(&mem, HEAD);
    let reason = blocked(
        &mem,
        refused(json!([{"rule": "bundle-size", "detail": "the bundle is 3 bytes; the limit is 2 bytes"}])),
    )
    .await;
    assert!(!reason.contains("as Cline left it"), "{reason}");
    assert!(
        reason.ends_with(&format!(
            "The run's last snapshot was saved at 2026-11-12 00:47 UTC. Download it with `ham pull {RUN} --snapshot`; it does not hold work done after that time."
        )),
        "{reason}"
    );
    // The same when session 1 changed no file: what session 2 did is not known from its copy.
    let mem = seeded(RunTarget::S3Only);
    second_session(&mem);
    put_final_copy(&mem, BASE);
    let reason = blocked(
        &mem,
        refused(json!([{"rule": "bundle-size", "detail": "the bundle is 3 bytes; the limit is 2 bytes"}])),
    )
    .await;
    assert!(!reason.contains("changed no file"), "{reason}");
    assert!(
        reason.contains(&format!("`ham pull {RUN} --snapshot`")),
        "{reason}"
    );
}

#[tokio::test]
async fn b823_a_session_whose_record_has_no_bundle_does_not_hide_the_copy_of_the_session_before() {
    let mem = seeded(RunTarget::S3Only);
    second_session(&mem);
    mem.put_snapshot_t(1, HEAD);
    mem.put_blob(
        &s3::snapshot_json_key_t(RUN, 2),
        serde_json::to_vec(
            &json!({"schemaVersion": 1, "headSha": HEAD, "baseSha": BASE,
            "at": "2026-11-12T00:47:40Z", "count": 1}),
        )
        .unwrap(),
    );
    let reason = blocked(
        &mem,
        refused(json!([{"rule": "bundle-size", "detail": "the bundle is 3 bytes; the limit is 2 bytes"}])),
    )
    .await;
    assert!(
        reason.ends_with(&format!(". {}", older_words())),
        "{reason}"
    );
}

#[tokio::test]
async fn b823_a_bundle_whose_record_never_came_is_still_named() {
    // The bundle of a copy went up and its record did not: the route serves it from the commit its header names, so the reason names
    // the command. The header is the one git writes (a bundle whose header cannot be read is no copy the route serves).
    let mem = seeded(RunTarget::S3Only);
    mem.put_blob(
        &s3::snapshot_bundle_key(RUN),
        format!("# v2 git bundle\n{HEAD} refs/heads/snapshot\n\nPACK").into_bytes(),
    );
    let reason = blocked(
        &mem,
        refused(json!([{"rule": "bundle-size", "detail": "the bundle is 3 bytes; the limit is 2 bytes"}])),
    )
    .await;
    assert!(
        reason.ends_with(&format!(
            "Download the run's last snapshot with `ham pull {RUN} --snapshot`; it does not hold work done after it was saved."
        )),
        "{reason}"
    );
}
