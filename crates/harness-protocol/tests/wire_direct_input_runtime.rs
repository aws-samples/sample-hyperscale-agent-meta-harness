//! Protocol 1.37.0: the `inputWaitS` request members, the attempt kinds `fresh_session` and `instruction`, the `interventions` and
//! `freshSessions` members of `summary.json`, and the S3 keys of the stop digest and the fresh-session audit copy.

#![allow(clippy::unwrap_used)]

use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{
    self, DirectAttemptKind, DirectInvocationError, DirectInvocationRequest, DirectSummary,
    DirectSummaryError, MAX_DIRECT_INTERVENTIONS, MAX_INPUT_WAIT_S, validate_direct_invocation,
};
use harness_protocol::store;
use serde_json::{Value, json};

const RUN: &str = "01M4XAB0CDEFGHJKMNPQRSTVWX";
const BUCKET: &str = "hamh-direct-harness-111122223333";
const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";

fn golden_request() -> Value {
    json!({
        "kind": "direct", "runId": RUN, "agentId": "direct", "turn": 1,
        "runtimeSessionId": format!("{RUN}-direct-g1"),
        "credentials": {
            "Version": 1, "AccessKeyId": "ASIA-test-0123456789", "SecretAccessKey": "test-secret-not-real",
            "SessionToken": "test-session-token-not-real", "Expiration": "2026-10-13T19:10:00Z"
        },
        "nonce": "test-nonce-not-real",
        "internalApiUrl": "https://abc.execute-api.us-east-1.amazonaws.com/direct",
        "region": "us-east-1", "engine": "cline",
        "model": {"id": "us.anthropic.claude-opus-5-5", "region": "us-east-1", "protocol": "converse",
                  "contextTokens": 1_000_000, "maxOutputTokens": 128_000},
        "request": {"uri": format!("s3://{BUCKET}/runs/{RUN}/input/request.json"), "sha256": "9f".repeat(32)},
        "repo": {"kind": "s3Bundle", "uri": format!("s3://{BUCKET}/runs/{RUN}/input/repo.bundle")},
        "timeoutS": 27_900, "snapshotEveryS": 600
    })
}

fn request_with(wait: Option<u64>) -> Value {
    let mut v = golden_request();
    if let Some(w) = wait {
        v["inputWaitS"] = json!(w);
    }
    v
}

#[test]
fn an_invocation_without_input_wait_is_what_it_was_and_with_it_round_trips() {
    let old: DirectInvocationRequest = serde_json::from_value(golden_request()).unwrap();
    assert_eq!(old.input_wait_s, None);
    assert!(
        serde_json::to_value(&old)
            .unwrap()
            .get("inputWaitS")
            .is_none()
    );
    assert_eq!(validate_direct_invocation(&old), Ok(()));

    let v = request_with(Some(1800));
    let r: DirectInvocationRequest = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(r.input_wait_s, Some(1800));
    assert_eq!(serde_json::to_value(&r).unwrap(), v);
    assert_eq!(validate_direct_invocation(&r), Ok(()));
    assert!(format!("{r:?}").contains("input_wait_s: Some(1800)"));
}

#[test]
fn validate_refuses_a_zero_or_oversized_input_wait_and_names_the_rule() {
    for (wait, ok) in [
        (0, false),
        (1, true),
        (u64::from(MAX_INPUT_WAIT_S), true),
        (u64::from(MAX_INPUT_WAIT_S) + 1, false),
    ] {
        let r: DirectInvocationRequest = serde_json::from_value(request_with(Some(wait))).unwrap();
        let got = validate_direct_invocation(&r);
        if ok {
            assert_eq!(got, Ok(()), "inputWaitS {wait}");
        } else {
            assert_eq!(
                got,
                Err(DirectInvocationError::BadInputWait),
                "inputWaitS {wait}"
            );
        }
    }
    let msg = DirectInvocationError::BadInputWait.to_string();
    assert!(
        msg.contains("inputWaitS") && msg.contains("2592000"),
        "{msg}"
    );
}

#[test]
fn a_start_request_carries_input_wait_only_when_the_client_sets_it() {
    let base = json!({"prompt": "p", "target": {"mode": "s3-only"}, "model": "m"});
    let none: StartDirectRunRequest = serde_json::from_value(base.clone()).unwrap();
    assert_eq!(none.input_wait_s, None);
    assert_eq!(serde_json::to_value(&none).unwrap().get("inputWaitS"), None);

    let mut with = base.clone();
    with["inputWaitS"] = json!(1800);
    let r: StartDirectRunRequest = serde_json::from_value(with.clone()).unwrap();
    assert_eq!(r.input_wait_s, Some(1800));
    assert_eq!(serde_json::to_value(&r).unwrap()["inputWaitS"], 1800);

    let mut unknown = base;
    unknown["inputWait"] = json!(1);
    assert!(serde_json::from_value::<StartDirectRunRequest>(unknown).is_err());
}

fn golden_summary() -> Value {
    json!({
        "schemaVersion": 1, "runId": RUN, "agentId": "direct", "turn": 1,
        "outcome": "reported_done", "label": "Cline reported done", "detail": null,
        "stopReason": "end_turn", "turnOutcome": "completed",
        "clineMessage": "Done.", "error": null,
        "model": {"id": "us.openai.gpt-6.1-sol", "region": "us-east-1", "pinned": "us.openai.gpt-6.1-sol"},
        "engine": {"name": "cline", "version": "3.0.71"},
        "rules": [], "rulesPreflight": {"ran": false},
        "request": {"sha256": "9f".repeat(32), "promptBytes": 123},
        "nudge": {"requested": false, "honored": false},
        "usage": {"inputTokens": 1204331, "outputTokens": 88120, "cacheReadInputTokens": 801220, "cacheWriteInputTokens": 90211},
        "toolCalls": 88,
        "timing": {"startedAt": "2026-10-15T13:30:00Z", "endedAt": "2026-10-15T14:10:00Z", "wallS": 2400},
        "git": {"committedByHarness": false, "filesChanged": 4},
        "snapshots": {"count": 2},
        "packages": {"enabled": false, "requests": 0, "bytes": 0, "byPrefix": {}, "statuses": {}},
        "faultHint": null,
        "versions": {"agentd": "0.1.0", "protocol": "1.37.0"},
        "attempts": [
            {"n": 1, "kind": "prompt", "startedAt": "2026-10-15T13:30:00Z", "endedAt": "2026-10-15T13:41:07Z", "result": "engine_cancelled",
             "retryClass": "permanent", "detail": "the engine ended the prompt cancelled with no cancel request"},
            {"n": 2, "kind": "resume", "startedAt": "2026-10-15T13:41:12Z", "endedAt": "2026-10-15T13:41:30Z", "result": "engine_cancelled"},
            {"n": 3, "kind": "instruction", "startedAt": "2026-10-15T13:42:40Z", "endedAt": "2026-10-15T13:43:10Z", "result": "ok"},
            {"n": 4, "kind": "fresh_session", "startedAt": "2026-10-15T13:50:00Z", "endedAt": "2026-10-15T14:10:00Z", "result": "ok"}
        ],
        "interventions": [
            {"n": 1, "requestId": REQ, "reason": "engine_stop", "openedAt": "2026-10-15T13:41:30Z", "closedAt": "2026-10-15T13:42:40Z",
             "waitedS": 70, "outcome": "answered", "action": "continue_with_prompt", "origin": "user", "textBytes": 88, "atAttempt": 3}
        ],
        "freshSessions": [
            {"n": 2, "trigger": "harness", "reason": "replay_rejected", "startedAt": "2026-10-15T13:50:00Z", "restartedProcess": false,
             "digestBytes": 9210, "digestSha256": "ab".repeat(32), "atAttempt": 4}
        ]
    })
}

#[test]
fn a_summary_with_the_new_attempt_kinds_and_members_parses_and_writes_back() {
    let s: DirectSummary = serde_json::from_value(golden_summary()).unwrap();
    assert_eq!(s.attempts[2].kind, DirectAttemptKind::Instruction);
    assert_eq!(s.attempts[3].kind, DirectAttemptKind::FreshSession);
    assert_eq!(s.interventions.len(), 1);
    assert_eq!(s.fresh_sessions[0].trigger, "harness");
    assert_eq!(s.fresh_sessions[0].request_id, None);
    assert_eq!(serde_json::to_value(&s).unwrap(), golden_summary());
    assert_eq!(s.validate(), Ok(()));
}

#[test]
fn a_summary_written_before_1_37_0_reads_without_the_members_and_writes_none_back() {
    let mut v = golden_summary();
    let o = v.as_object_mut().unwrap();
    o.remove("interventions");
    o.remove("freshSessions");
    let s: DirectSummary = serde_json::from_value(v.clone()).unwrap();
    assert!(s.interventions.is_empty() && s.fresh_sessions.is_empty());
    assert_eq!(serde_json::to_value(&s).unwrap(), v);
}

#[test]
fn validate_refuses_more_than_32_interventions_or_fresh_sessions() {
    let base: DirectSummary = serde_json::from_value(golden_summary()).unwrap();
    let mut s = base.clone();
    s.interventions = vec![base.interventions[0].clone(); MAX_DIRECT_INTERVENTIONS];
    assert_eq!(s.validate(), Ok(()), "the limit itself passes");
    s.interventions.push(base.interventions[0].clone());
    assert_eq!(s.validate(), Err(DirectSummaryError::TooManyInterventions));

    let mut s = base.clone();
    s.fresh_sessions = vec![base.fresh_sessions[0].clone(); MAX_DIRECT_INTERVENTIONS + 1];
    assert_eq!(s.validate(), Err(DirectSummaryError::TooManyInterventions));
    assert!(
        DirectSummaryError::TooManyInterventions
            .to_string()
            .contains("32")
    );
}

#[test]
fn the_stop_digest_and_the_fresh_session_copy_have_their_own_keys_under_the_turn_prefix() {
    let prefix = format!("runs/{RUN}/agents/direct/turns/1/");
    assert_eq!(
        direct::s3::stop_digest_key(RUN, 3),
        format!("{prefix}stop-digest-3.json")
    );
    assert_eq!(
        direct::s3::fresh_session_key(RUN, 2),
        format!("{prefix}fresh-session-2.md")
    );
    assert_eq!(
        store::s3::direct_stop_digest_key(RUN, 3),
        direct::s3::stop_digest_key(RUN, 3)
    );
    assert_eq!(
        store::s3::direct_fresh_session_key(RUN, 2),
        direct::s3::fresh_session_key(RUN, 2)
    );
    assert!(direct::s3::stop_digest_key(RUN, 1).starts_with(&direct::s3::turn_prefix(RUN)));
}
