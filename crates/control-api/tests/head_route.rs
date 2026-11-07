//! `GET /v1/runs/{id}/head`: the commit a finished run delivered, for the follow-up's header, its confirmation and
//! its start key. The route reads the first 8 KiB of the verified bundle (the one harness-made object that names the delivered head) and, when
//! the harness wrote it, Cline's last message out of `evidence/direct.json`. Only the owner gets an answer, a run that is still going has no
//! delivered head, and nothing in a bundle or in the evidence reaches the access line.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, OTHER, World, get_logged, request, run_item};
use ham_core::head::DeliveredHead;
use harness_protocol::direct::s3 as direct_s3;
use harness_protocol::store::s3;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const BASE: &str = "1111111111111111111111111111111111111111";

fn path() -> String {
    format!("/v1/runs/{RUN}/head")
}

/// A v2 bundle header that names the harness ref at `head`, then a pack of `filler` bytes.
fn bundle(head: &str, filler: usize) -> Vec<u8> {
    let mut b =
        format!("# v2 git bundle\n-{BASE} base subject\n{head} refs/heads/harness/{RUN}\n\nPACK")
            .into_bytes();
    b.extend(std::iter::repeat_n(7u8, filler));
    b
}

/// `runs/<run>/evidence/direct.json` as run-session's `plan-delivery` writes it (`EvidenceFile`): an envelope of the harness's own members and,
/// in `summary`, agentd's `summary.json` with Cline's last message in `summary.clineMessage`. The envelope has no `clineMessage` of its own.
/// run-session's test `the_evidence_file_holds_clines_message_where_the_head_route_reads_it` feeds the writer's real output to the same reader.
fn evidence(message: Option<&str>) -> Vec<u8> {
    let summary = json!({
        "schemaVersion": 1, "runId": RUN, "agentId": "direct", "turn": 1,
        "outcome": "reported_done", "label": "Cline reported done", "detail": null,
        "stopReason": "end_turn", "turnOutcome": "completed",
        "clineMessage": message, "error": null,
        "model": {"id": "global.anthropic.claude-sonnet-5-5", "region": "us-east-1", "pinned": "global.anthropic.claude-sonnet-5-5"},
        "engine": {"name": "cline", "version": "3.0.71", "binarySha256": "ab".repeat(32)},
        "rules": []
    });
    json!({
        "schemaVersion": 1, "runId": RUN, "outcome": "reported_done", "label": "Cline reported done", "detail": null,
        "headSource": "final", "headSha": HEAD, "baseSha": BASE,
        "plannedAt": "2026-10-15T13:00:00Z", "plannedAtMs": 1_792_069_200_000u64,
        "failure": null, "summary": summary
    })
    .to_string()
    .into_bytes()
}

/// Cline's last message of a live run of scenario T60: backticks, a path, a line
/// break and the reply marker on the last line.
const LIVE_MESSAGE: &str = "Yes, the goal is complete. I created `hello.py`, which prints 55, and ran it with python3. I also ran the one shell command you gave. docs/lifecycle/GOAL.md doesn't exist, so there were no extra checks to run.\n55 T60CMARK4EBC5522B5T60DMARKA5799E2FE7";

/// A finished run of `ME` whose verified bundle names `HEAD`.
fn world(status: &str) -> World {
    let w = World::new();
    w.add_run(run_item(
        RUN,
        ME,
        status,
        "2026-10-15T13:00:00Z",
        json!({"mode": "s3-only"}),
    ));
    w.put_object(&s3::verified_bundle_key(RUN), bundle(HEAD, 3_000_000));
    w
}

async fn head(w: &World, caller: &str) -> (u16, Value, Vec<String>) {
    get_logged(w, caller, &path(), &[]).await
}

#[tokio::test]
async fn the_owner_reads_the_delivered_head_and_the_state_of_the_run() {
    let w = world("complete");
    let (status, body, logs) = head(&w, ME).await;
    assert_eq!(status, 200, "{body}");
    let h: DeliveredHead = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(h.run_id, RUN);
    assert_eq!(h.head_sha, HEAD, "the commit the verified bundle names");
    assert_eq!(body["runStatus"], "complete");
    assert_eq!(body["headSha"], HEAD, "the wire names are camelCase");
    assert!(logs.is_empty(), "{logs:?}");
}

#[tokio::test]
async fn the_route_reads_a_few_kilobytes_never_the_bundle() {
    let w = world("complete");
    let (status, _, _) = head(&w, ME).await;
    assert_eq!(status, 200);
    let calls = w.calls.lock().unwrap().clone();
    let reads: Vec<&String> = calls
        .iter()
        .filter(|c| c.starts_with("read_prefix"))
        .collect();
    assert!(
        reads.iter().any(|c| c.ends_with(&format!(
            "verified.bundle {}",
            ham_core::snapshot::HEADER_BYTES
        ))),
        "{calls:?}"
    );
    assert!(
        reads.iter().all(|c| !c.contains("3000")),
        "no read is as long as the bundle: {calls:?}"
    );
}

#[tokio::test]
async fn another_principals_run_and_a_missing_run_are_both_not_found_before_anything_is_read() {
    let w = world("complete");
    let (other, body, _) = head(&w, OTHER).await;
    assert_eq!(other, 404, "{body}");
    let (missing, _, _) = get_logged(&w, ME, "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Z/head", &[]).await;
    assert_eq!(missing, 404);
    assert_eq!(
        w.count("read_prefix") + w.count("exists"),
        0,
        "no object is touched for a run the caller does not own: {:?}",
        w.calls.lock().unwrap()
    );
}

#[tokio::test]
async fn a_run_that_is_still_going_has_no_delivered_head() {
    let w = world("active");
    let (status, body, _) = head(&w, ME).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], "run-active");
    assert_eq!(w.count("read_prefix"), 0);
}

#[tokio::test]
async fn a_run_that_delivered_nothing_says_so() {
    let w = World::new();
    w.add_run(run_item(
        RUN,
        ME,
        "complete",
        "2026-10-15T13:00:00Z",
        json!({"mode": "s3-only"}),
    ));
    let (status, body, _) = head(&w, ME).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], "no-delivered-head");
}

/// The role may list the bucket for the bundle's key only: a GET of an absent key is 403 for it. The route asks by a listing, as the
/// follow-up start does, so a run that delivered nothing never looks like an outage.
#[tokio::test]
async fn the_absence_of_a_bundle_is_told_by_a_listing_not_by_a_refused_read() {
    let w = World::new();
    w.no_list_permission.store(true, Ordering::SeqCst);
    w.add_run(run_item(
        RUN,
        ME,
        "complete",
        "2026-10-15T13:00:00Z",
        json!({"mode": "s3-only"}),
    ));
    let (status, body, _) = head(&w, ME).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], "no-delivered-head");
    assert_eq!(
        w.count(&format!("exists {}", s3::verified_bundle_key(RUN))),
        1
    );
}

#[tokio::test]
async fn a_header_that_cannot_be_read_as_a_head_is_422_and_the_body_keeps_the_cause_out() {
    for bytes in [
        b"not a bundle at all\n".to_vec(),
        format!("# v2 git bundle\n{HEAD} refs/heads/harness/OTHER\n\nPACK").into_bytes(),
        format!("# v2 git bundle\n{HEAD} refs/heads/harness/{RUN}").into_bytes(),
    ] {
        let w = World::new();
        w.add_run(run_item(
            RUN,
            ME,
            "complete",
            "2026-10-15T13:00:00Z",
            json!({"mode": "s3-only"}),
        ));
        w.put_object(&s3::verified_bundle_key(RUN), bytes);
        let (status, body, logs) = head(&w, ME).await;
        assert_eq!(status, 422, "{body}");
        assert_eq!(body["code"], "delivered-head-unreadable");
        assert!(
            !body.to_string().contains("refs/heads"),
            "the cause goes to the log: {body}"
        );
        assert!(!logs.is_empty(), "the cause is logged");
    }
}

#[tokio::test]
async fn clines_last_message_comes_from_the_evidence_the_harness_wrote() {
    let w = world("complete");
    w.put_object(
        &direct_s3::evidence_key(RUN),
        evidence(Some("hello.py printed 55.")),
    );
    let (status, body, _) = head(&w, ME).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["clineMessage"], "hello.py printed 55.");
}

/// The route read a top-level `clineMessage` that no writer makes; the message is in `summary.clineMessage`.
#[tokio::test]
async fn the_message_of_a_live_evidence_file_reaches_the_follow_up() {
    let w = world("complete");
    w.put_object(&direct_s3::evidence_key(RUN), evidence(Some(LIVE_MESSAGE)));
    let (status, body, _) = head(&w, ME).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["clineMessage"], LIVE_MESSAGE,
        "the message in summary.clineMessage of the evidence file: {body}"
    );
}

/// A `clineMessage` at the top of the file is not what `plan-delivery` writes; the reader looks at `summary.clineMessage` only (the same place
/// `ham_core::chat::final_message_from_evidence` reads).
#[tokio::test]
async fn a_message_at_the_top_of_the_file_is_not_the_one_the_writer_makes() {
    let w = world("complete");
    let mut v: Value = serde_json::from_slice(&evidence(None)).unwrap();
    v["clineMessage"] = json!("not where the writer puts it");
    w.put_object(&direct_s3::evidence_key(RUN), v.to_string().into_bytes());
    let (status, body, _) = head(&w, ME).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.get("clineMessage").is_none_or(Value::is_null),
        "{body}"
    );
}

#[tokio::test]
async fn the_message_is_cleaned_before_it_leaves() {
    let w = world("complete");
    let secret = format!("AKIA{}", "IOSFODNN7EXAMPLE");
    let text = format!("done\u{1b}[31m red {secret} and \u{202e}evil");
    w.put_object(&direct_s3::evidence_key(RUN), evidence(Some(&text)));
    let (status, body, _) = head(&w, ME).await;
    assert_eq!(status, 200, "{body}");
    let m = body["clineMessage"].as_str().unwrap();
    assert!(!m.contains('\u{1b}'), "no escape byte: {m:?}");
    assert!(
        !m.contains(&secret),
        "a token-shaped string is hidden: {m:?}"
    );
    assert!(!m.contains('\u{202e}'), "no bidi override: {m:?}");
}

#[tokio::test]
async fn evidence_that_is_missing_or_unreadable_does_not_hide_the_head() {
    // Missing, and the role not allowed to tell absent from refused: the message is left out, the head still answers.
    let w = world("complete");
    w.no_list_permission.store(true, Ordering::SeqCst);
    let (status, body, _) = head(&w, ME).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.get("clineMessage").is_none_or(Value::is_null),
        "{body}"
    );
    // Not JSON.
    let w = world("complete");
    w.put_object(&direct_s3::evidence_key(RUN), b"{ not json".to_vec());
    let (status, body, logs) = head(&w, ME).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.get("clineMessage").is_none_or(Value::is_null),
        "{body}"
    );
    assert!(!logs.is_empty(), "the unreadable evidence is logged");
    // A message that is not a string.
    let w = world("complete");
    w.put_object(
        &direct_s3::evidence_key(RUN),
        json!({"summary": {"clineMessage": 7}})
            .to_string()
            .into_bytes(),
    );
    let (status, body, _) = head(&w, ME).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.get("clineMessage").is_none_or(Value::is_null),
        "{body}"
    );
}

#[tokio::test]
async fn nothing_from_the_bucket_reaches_the_access_line_or_the_log() {
    let w = world("complete");
    w.put_object(
        &direct_s3::evidence_key(RUN),
        evidence(Some("PRIVATE-MESSAGE-TEXT")),
    );
    let (status, _, logs) = head(&w, ME).await;
    assert_eq!(status, 200);
    assert!(
        logs.iter().all(|l| !l.contains("PRIVATE-MESSAGE-TEXT")),
        "{logs:?}"
    );
}

#[tokio::test]
async fn only_get_is_allowed() {
    let w = world("complete");
    for method in ["POST", "PUT", "DELETE"] {
        let out = control_api::handler::handle(
            request(ME, method, &path(), &[], ""),
            &chat_support::deps(&w),
        )
        .await;
        assert_eq!(out.status, 405, "{method}");
    }
}

#[tokio::test]
async fn a_store_that_is_down_is_a_retry_not_a_missing_head() {
    let w = world("complete");
    w.objects_down.store(true, Ordering::SeqCst);
    let (status, body, _) = head(&w, ME).await;
    assert_eq!(status, 503, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("retry"),
        "{body}"
    );
}

// ---- what the delivery changed (the validator's record) -------------------------------------------------------------------------------

fn record(head: &str, total: u64, omitted: u64, files: Value) -> Vec<u8> {
    json!({"schemaVersion": 1, "runId": RUN, "headSha": head, "base": {"kind": "empty-tree"},
           "total": total, "omitted": omitted, "files": files})
    .to_string()
    .into_bytes()
}

#[tokio::test]
async fn the_validators_record_of_this_head_comes_with_the_answer() {
    let w = world("complete");
    w.put_object(
        &ham_core::head::changes_key(RUN),
        record(
            HEAD,
            3,
            1,
            json!([{"path": "README.md", "status": "A"}, {"path": "hello.py", "status": "A"}]),
        ),
    );
    let (status, body, logs) = head(&w, ME).await;
    assert_eq!(status, 200, "{body}");
    let h: DeliveredHead = serde_json::from_value(body.clone()).unwrap();
    let ham_core::head::Changes::Recorded(list) = h.changes else {
        panic!("{body}")
    };
    assert_eq!((list.total, list.omitted), (3, 1));
    assert_eq!(list.files.len(), 2);
    assert_eq!(body["changes"]["state"], "recorded");
    assert!(logs.is_empty(), "{logs:?}");
}

#[tokio::test]
async fn a_delivery_that_changed_nothing_is_a_recorded_empty_list_not_a_missing_one() {
    let w = world("complete");
    w.put_object(
        &ham_core::head::changes_key(RUN),
        record(HEAD, 0, 0, json!([])),
    );
    let (status, body, _) = head(&w, ME).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["changes"]["state"], "recorded");
    assert_eq!(body["changes"]["total"], 0);
    let missing = world("complete");
    let (_, body, _) = head(&missing, ME).await;
    assert_eq!(body["changes"]["state"], "unavailable");
}

#[tokio::test]
async fn a_record_of_another_head_is_never_shown_as_this_deliverys() {
    let w = world("complete");
    w.put_object(
        &ham_core::head::changes_key(RUN),
        record(
            &"2".repeat(40),
            1,
            0,
            json!([{"path": "SECRET-OTHER-PATH", "status": "A"}]),
        ),
    );
    let (status, body, logs) = head(&w, ME).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["changes"]["state"], "unavailable");
    assert!(!body.to_string().contains("SECRET-OTHER-PATH"), "{body}");
    assert!(
        logs.iter().any(|l| l.contains("names another head")),
        "the cause is logged: {logs:?}"
    );
    assert!(
        logs.iter().all(|l| !l.contains("SECRET-OTHER-PATH")),
        "{logs:?}"
    );
}

#[tokio::test]
async fn a_record_that_is_unreadable_oversized_or_of_another_version_is_unavailable_and_logged() {
    let big = vec![b' '; ham_core::head::MAX_CHANGES_RECORD_BYTES + 10];
    let wrong_schema =
        json!({"schemaVersion": 7, "runId": RUN, "headSha": HEAD, "base": {"kind": "empty-tree"},
                              "total": 0, "omitted": 0, "files": []})
        .to_string()
        .into_bytes();
    let off = record(HEAD, 9, 0, json!([{"path": "a", "status": "A"}]));
    for bytes in [b"{ not json".to_vec(), big, wrong_schema, off] {
        let w = world("complete");
        w.put_object(&ham_core::head::changes_key(RUN), bytes);
        let (status, body, logs) = head(&w, ME).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["changes"]["state"], "unavailable", "{body}");
        assert!(!logs.is_empty(), "the cause is logged");
    }
}

#[tokio::test]
async fn a_record_the_role_cannot_tell_absent_from_refused_is_unavailable_never_an_error() {
    let w = world("complete");
    w.no_list_permission.store(true, Ordering::SeqCst);
    let (status, body, _) = head(&w, ME).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["changes"]["state"], "unavailable");
}

#[tokio::test]
async fn the_record_is_read_whole_in_one_ranged_read_bounded_by_its_limit() {
    let w = world("complete");
    w.put_object(
        &ham_core::head::changes_key(RUN),
        record(HEAD, 1, 0, json!([{"path": "a", "status": "A"}])),
    );
    let (status, _, _) = head(&w, ME).await;
    assert_eq!(status, 200);
    let calls = w.calls.lock().unwrap().clone();
    let want = format!(
        "read_prefix {} {}",
        ham_core::head::changes_key(RUN),
        ham_core::head::MAX_CHANGES_RECORD_BYTES + 1
    );
    assert!(calls.contains(&want), "{calls:?}");
}
