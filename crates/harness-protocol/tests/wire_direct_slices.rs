//! Runs past the 8-hour session limit and the paused and stalled statuses: the wire members. A run is a list of slices,
//! each slice is turn n of the one direct agent (its own Turn item, its own S3 prefix and its own microVM session id). A slice that stops at its
//! deadline while time is left says so in turn-complete (handoff); the next slice restores the earlier slice's work and knows the run's original
//! base. A delivered run that stopped at its total time is paused, one that lost its progress is stalled, and both name a reason and how they
//! can be resumed.

#![allow(clippy::unwrap_used)]

use harness_protocol::api::internal::TurnCompleteRequest;
use harness_protocol::api::v1::{RunSummary, StartDirectRunRequest};
use harness_protocol::direct::{
    DEFAULT_MAX_RUN_S, DirectInvocationError, DirectInvocationRequest, DirectSlice, MAX_MAX_RUN_S,
    MAX_SLICE_BRIEF_BYTES, MAX_SLICES, MIN_MAX_RUN_S, max_run_s_ok, s3, validate_direct_invocation,
};
use harness_protocol::goal::{GoalStatus, ResumeMode};
use harness_protocol::store::RunItem;
use serde_json::{Value, json};

const RUN: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const ARN: &str = "arn:aws:sts::111122223333:assumed-role/Builder/example";
const BUCKET: &str = "hamh-direct-harness-111122223333";

fn invocation(turn: u32, repo_key: &str, slice: Value) -> Value {
    let mut v = json!({
        "kind": "direct", "runId": RUN, "agentId": "direct", "turn": turn,
        "runtimeSessionId": format!("{RUN}-direct-g{turn:08}"),
        "credentials": {"Version": 1, "AccessKeyId": "ASIAEXAMPLEEXAMPLE", "SecretAccessKey": "s", "SessionToken": "t",
                        "Expiration": "2026-11-03T18:10:00Z"},
        "nonce": "n", "internalApiUrl": "https://abc.execute-api.us-east-1.amazonaws.com/direct", "region": "us-east-1",
        "engine": "cline",
        "model": {"id": "us.anthropic.claude-sonnet-5-5", "region": "us-east-1", "contextTokens": 200000, "maxOutputTokens": 64000,
                  "protocol": "converse"},
        "request": {"uri": format!("s3://{BUCKET}/runs/{RUN}/input/request.json"), "sha256": "ab".repeat(32)},
        "repo": {"kind": "s3Bundle", "uri": format!("s3://{BUCKET}/{repo_key}")},
        "timeoutS": 600, "snapshotEveryS": 600
    });
    if !slice.is_null() {
        v["slice"] = slice;
    }
    v
}

fn parse(v: Value) -> DirectInvocationRequest {
    serde_json::from_value(v).unwrap()
}

fn input_bundle() -> String {
    format!("runs/{RUN}/input/repo.bundle")
}

fn turn_bundle(n: u32) -> String {
    format!("runs/{RUN}/agents/direct/turns/{n}/bundle")
}

#[test]
fn b688_the_first_slice_is_the_invocation_of_today() {
    let req = parse(invocation(1, &input_bundle(), Value::Null));
    assert_eq!(req.slice, None);
    assert_eq!(validate_direct_invocation(&req), Ok(()));
    let again = serde_json::to_value(&req).unwrap();
    assert!(
        again.get("slice").is_none(),
        "an invocation without a slice member is written without one, so an agentd image that predates it parses it"
    );
}

#[test]
fn b688_a_first_slice_that_is_not_the_last_says_so() {
    let req = parse(invocation(1, &input_bundle(), json!({"last": false})));
    assert_eq!(
        req.slice,
        Some(DirectSlice {
            last: false,
            base_sha: None,
            restore_head: None,
            sessions_before: None,
            brief: None,
        })
    );
    assert_eq!(validate_direct_invocation(&req), Ok(()));
}

#[test]
fn b688_a_later_slice_restores_the_earlier_slices_bundle_and_names_the_original_base() {
    let slice = json!({"last": true, "baseSha": SHA_A, "restoreHead": SHA_B, "sessionsBefore": 3});
    let req = parse(invocation(2, &turn_bundle(1), slice));
    assert_eq!(validate_direct_invocation(&req), Ok(()));
    let slice = req.slice.unwrap();
    assert_eq!(slice.base_sha.as_deref(), Some(SHA_A));
    assert_eq!(slice.restore_head.as_deref(), Some(SHA_B));
    assert_eq!(slice.sessions_before, Some(3));
}

#[test]
fn b688_the_earlier_slices_last_message_rides_along_up_to_four_kib() {
    let mut slice = json!({"last": true, "baseSha": SHA_A, "restoreHead": SHA_B, "brief": "x".repeat(MAX_SLICE_BRIEF_BYTES)});
    let ok = parse(invocation(2, &turn_bundle(1), slice.clone()));
    assert_eq!(validate_direct_invocation(&ok), Ok(()));
    slice["brief"] = json!("x".repeat(MAX_SLICE_BRIEF_BYTES + 1));
    let long = parse(invocation(2, &turn_bundle(1), slice));
    assert_eq!(
        validate_direct_invocation(&long),
        Err(DirectInvocationError::BadSlice)
    );
}

#[test]
fn b688_a_later_slice_may_restore_a_snapshot_bundle() {
    let slice = json!({"last": true, "baseSha": SHA_A, "restoreHead": SHA_B});
    let key = format!("runs/{RUN}/agents/direct/turns/2/snapshot.bundle");
    let req = parse(invocation(3, &key, slice));
    assert_eq!(validate_direct_invocation(&req), Ok(()));
}

#[test]
fn b688_a_later_slice_without_its_slice_member_is_refused() {
    let req = parse(invocation(2, &input_bundle(), Value::Null));
    assert_eq!(
        validate_direct_invocation(&req),
        Err(DirectInvocationError::BadSlice)
    );
}

#[test]
fn b688_a_later_slice_needs_both_shas_to_be_commit_ids() {
    for slice in [
        json!({"last": true}),
        json!({"last": true, "baseSha": SHA_A}),
        json!({"last": true, "baseSha": "main", "restoreHead": SHA_B}),
        json!({"last": true, "baseSha": SHA_A, "restoreHead": "ABC"}),
    ] {
        let req = parse(invocation(2, &turn_bundle(1), slice.clone()));
        assert_eq!(
            validate_direct_invocation(&req),
            Err(DirectInvocationError::BadSlice),
            "{slice}"
        );
    }
}

#[test]
fn b688_a_slice_restores_only_from_an_earlier_slice_of_its_own_run() {
    let slice = json!({"last": true, "baseSha": SHA_A, "restoreHead": SHA_B});
    for key in [
        turn_bundle(2),
        turn_bundle(3),
        format!("runs/{RUN}/agents/direct/turns/1/summary.json"),
        "runs/01M5A2R7K3C9XJ4D8E6FQ0WT1P/agents/direct/turns/1/bundle".to_string(),
        format!("runs/{RUN}/agents/direct/turns/0/bundle"),
    ] {
        let req = parse(invocation(2, &key, slice.clone()));
        assert_eq!(
            validate_direct_invocation(&req),
            Err(DirectInvocationError::BadRepo),
            "{key}"
        );
    }
}

#[test]
fn b688_the_turn_number_is_one_to_the_most_slices_a_run_may_have() {
    let slice = json!({"last": true, "baseSha": SHA_A, "restoreHead": SHA_B});
    let last = parse(invocation(
        MAX_SLICES,
        &turn_bundle(MAX_SLICES - 1),
        slice.clone(),
    ));
    assert_eq!(validate_direct_invocation(&last), Ok(()));
    let past = parse(invocation(
        MAX_SLICES + 1,
        &turn_bundle(MAX_SLICES),
        slice.clone(),
    ));
    assert_eq!(
        validate_direct_invocation(&past),
        Err(DirectInvocationError::BadTurn)
    );
    let zero = parse(invocation(0, &input_bundle(), Value::Null));
    assert_eq!(
        validate_direct_invocation(&zero),
        Err(DirectInvocationError::BadTurn)
    );
}

#[test]
fn b688_every_artifact_of_a_slice_has_its_own_key_and_the_first_slice_keeps_todays() {
    assert_eq!(s3::bundle_key_t(RUN, 1), s3::bundle_key(RUN));
    assert_eq!(s3::summary_key_t(RUN, 1), s3::summary_key(RUN));
    assert_eq!(
        s3::snapshot_bundle_key_t(RUN, 1),
        s3::snapshot_bundle_key(RUN)
    );
    assert_eq!(s3::snapshot_json_key_t(RUN, 1), s3::snapshot_json_key(RUN));
    assert_eq!(s3::test_report_key_t(RUN, 1), s3::test_report_key(RUN));
    assert_eq!(
        s3::bundle_key_t(RUN, 3),
        format!("runs/{RUN}/agents/direct/turns/3/bundle")
    );
    assert_eq!(
        s3::summary_key_t(RUN, 3),
        format!("runs/{RUN}/agents/direct/turns/3/summary.json")
    );
    assert_eq!(
        s3::brief_key_t(RUN, 2),
        format!("runs/{RUN}/agents/direct/turns/2/brief.json")
    );
    assert_eq!(
        s3::slice_record_key(RUN, 2),
        format!("runs/{RUN}/slices/2.json")
    );
}

#[test]
fn b688_turn_complete_carries_the_handoff_only_when_the_slice_ended_at_its_deadline_with_time_left()
{
    let base =
        json!({"runId": RUN, "agentId": "direct", "turn": 1, "finishReason": "stopped_timeout"});
    let plain: TurnCompleteRequest = serde_json::from_value(base.clone()).unwrap();
    assert_eq!(plain.handoff, None);
    assert!(
        serde_json::to_value(&plain)
            .unwrap()
            .get("handoff")
            .is_none()
    );
    let mut v = base;
    v["handoff"] = json!(true);
    let handed: TurnCompleteRequest = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(handed.handoff, Some(true));
    assert_eq!(
        serde_json::to_value(&handed).unwrap()["handoff"],
        json!(true)
    );
}

#[test]
fn b688_the_total_time_of_a_run_is_a_request_member_with_a_default_a_floor_and_a_ceiling() {
    assert_eq!(DEFAULT_MAX_RUN_S, 24 * 3600);
    assert_eq!(MAX_MAX_RUN_S, 7 * 24 * 3600);
    assert_eq!(MIN_MAX_RUN_S, 1_800);
    assert!(max_run_s_ok(DEFAULT_MAX_RUN_S));
    assert!(max_run_s_ok(MIN_MAX_RUN_S));
    assert!(max_run_s_ok(MAX_MAX_RUN_S));
    assert!(!max_run_s_ok(MIN_MAX_RUN_S - 1));
    assert!(!max_run_s_ok(MAX_MAX_RUN_S + 1));
    assert!(!max_run_s_ok(0));
    let v = json!({"prompt": "p", "target": {"mode": "s3-only"}, "model": "us.anthropic.claude-sonnet-5-5", "maxRunS": 36000});
    let req: StartDirectRunRequest = serde_json::from_value(v).unwrap();
    assert_eq!(req.max_run_s, Some(36_000));
    let none: StartDirectRunRequest =
        serde_json::from_value(json!({"prompt": "p", "target": {"mode": "s3-only"}, "model": "m"}))
            .unwrap();
    assert_eq!(none.max_run_s, None);
    assert!(
        serde_json::to_value(&none)
            .unwrap()
            .get("maxRunS")
            .is_none()
    );
}

#[test]
fn b689_a_stalled_run_is_a_status_with_its_own_word() {
    assert_eq!(
        serde_json::to_value(GoalStatus::Stalled).unwrap(),
        json!("stalled")
    );
    let s: GoalStatus = serde_json::from_value(json!("stalled")).unwrap();
    assert_eq!(s, GoalStatus::Stalled);
    assert!(
        !GoalStatus::Stalled.is_terminal() && !GoalStatus::Paused.is_terminal(),
        "paused and stalled admit a follow-up, so neither is a final status"
    );
}

#[test]
fn b689_the_resume_modes_have_their_words() {
    for (mode, word) in [
        (ResumeMode::SameRun, "same_run"),
        (ResumeMode::FollowUp, "follow_up"),
        (ResumeMode::None, "none"),
    ] {
        assert_eq!(serde_json::to_value(mode).unwrap(), json!(word));
        assert_eq!(
            serde_json::from_value::<ResumeMode>(json!(word)).unwrap(),
            mode
        );
    }
}

fn summary_json() -> Value {
    json!({
        "runId": RUN, "status": "paused", "phase": "green", "target": {"mode": "s3-only"},
        "createdAt": "2026-11-03T08:00:00Z", "ownerPrincipalArn": ARN
    })
}

#[test]
fn b689_a_run_summary_carries_the_reason_the_resume_mode_and_the_session_number() {
    let mut v = summary_json();
    v["statusReason"] = json!("total_time: the run used the 24 hours it was given");
    v["resumeMode"] = json!("follow_up");
    v["session"] = json!(4);
    let s: RunSummary = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(s.status, GoalStatus::Paused);
    assert_eq!(
        s.status_reason.as_deref(),
        Some("total_time: the run used the 24 hours it was given")
    );
    assert_eq!(s.resume_mode, Some(ResumeMode::FollowUp));
    assert_eq!(s.session, Some(4));
    assert_eq!(serde_json::to_value(&s).unwrap(), v);
}

#[test]
fn b689_an_ordinary_summary_has_none_of_the_three_members_and_writes_none() {
    let s: RunSummary = serde_json::from_value(summary_json()).unwrap();
    assert_eq!(
        (s.status_reason, s.resume_mode, s.session),
        (None, None, None)
    );
    let out = serde_json::to_value(serde_json::from_value::<RunSummary>(summary_json()).unwrap())
        .unwrap();
    for m in ["statusReason", "resumeMode", "session"] {
        assert!(out.get(m).is_none(), "{m}");
    }
}

#[test]
fn b689_the_run_item_stores_the_status_the_reason_the_resume_mode_and_the_slice() {
    let v = json!({
        "runId": RUN, "ownerPrincipalArn": ARN, "promptSha256": "cd".repeat(32), "target": {"mode": "s3-only"},
        "status": "stalled", "phase": "green", "enginePolicyVersion": 0, "createdAt": "2026-11-03T08:00:00Z",
        "statusReason": "session_lost: the session stopped answering", "resumeMode": "follow_up", "slice": 3,
        "maxRunS": 86400, "version": 9
    });
    let item: RunItem = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(item.status, GoalStatus::Stalled);
    assert_eq!(item.resume_mode, Some(ResumeMode::FollowUp));
    assert_eq!(item.slice, Some(3));
    assert_eq!(item.max_run_s, Some(86_400));
    assert_eq!(serde_json::to_value(&item).unwrap(), v);
}

#[test]
fn b688_the_two_slice_refusals_say_what_is_wrong_in_plain_words() {
    use harness_protocol::direct::{DirectInvocationError as E, MAX_SLICES};
    assert_eq!(
        E::BadTurn.to_string(),
        format!("turn must be 1 to {MAX_SLICES}")
    );
    let slice = E::BadSlice.to_string();
    assert!(
        slice.contains("baseSha")
            && slice.contains("restoreHead")
            && slice.contains("40 character"),
        "{slice}"
    );
}
