//! Protocol 1.32.0: `summary.json` of a direct run lists every try of the
//! prompt when agentd retried after a provider failure. Golden JSON goes in and the same JSON comes out; a run with one try carries no
//! member at all, so the summaries written before this version stay what they were; an older reader ignores the member.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::direct::{
    DirectAttempt, DirectAttemptKind, DirectAttemptResult, DirectSummary, DirectSummaryError,
    MAX_DIRECT_ATTEMPTS, MAX_DIRECT_DETAIL_CHARS,
};
use harness_protocol::export::all_schemas;
use serde_json::{Value, json};

const RUN: &str = "01M4XAB0CDEFGHJKMNPQRSTVWX";

fn golden() -> Value {
    json!({
        "schemaVersion": 1,
        "runId": RUN, "agentId": "direct", "turn": 1,
        "outcome": "reported_done",
        "label": "Cline reported done",
        "detail": null,
        "stopReason": "end_turn",
        "turnOutcome": "completed",
        "clineMessage": "Checked the tree; the work is in place.",
        "error": null,
        "model": {"id": "us.openai.gpt-6.1-sol", "region": "us-east-1", "pinned": "us.openai.gpt-6.1-sol"},
        "engine": {"name": "cline", "version": "3.0.71"},
        "rules": [],
        "rulesPreflight": {"ran": false},
        "request": {"sha256": "9f".repeat(32), "promptBytes": 123},
        "nudge": {"requested": false, "honored": false},
        "usage": null,
        "toolCalls": 88,
        "timing": {"startedAt": "2026-10-14T15:29:11Z", "endedAt": "2026-10-14T15:49:53Z", "wallS": 1241, "promptS": 1239, "maxSilentGapS": 337},
        "git": {"committedByHarness": false, "filesChanged": 26},
        "snapshots": {"count": 2, "lastAt": "2026-10-14T15:49:13Z"},
        "packages": {"enabled": true, "requests": 845, "bytes": 528_752_828, "byPrefix": {"npm": 797, "pypi": 48}, "statuses": {"200": 837, "502": 8}},
        "faultHint": null,
        "versions": {"agentd": "0.1.0", "protocol": "1.32.0"},
        "attempts": [
            {"n": 1, "kind": "prompt", "startedAt": "2026-10-14T15:29:11Z", "endedAt": "2026-10-14T15:49:53Z", "result": "provider_error",
             "retryClass": "transient", "detail": "engine error -32603: Internal error: The operation timed out.", "backoffS": 5},
            {"n": 2, "kind": "resume", "startedAt": "2026-10-14T15:49:58Z", "endedAt": "2026-10-14T15:50:30Z", "result": "engine_cancelled",
             "retryClass": "permanent", "detail": "the engine ended the prompt cancelled with no cancel request", "backoffS": 5},
            {"n": 3, "kind": "resume", "startedAt": "2026-10-14T15:50:35Z", "endedAt": "2026-10-14T15:52:10Z", "result": "ok"}
        ]
    })
}

fn summary() -> DirectSummary {
    serde_json::from_value(golden()).unwrap()
}

fn attempt(n: u32) -> DirectAttempt {
    DirectAttempt {
        n,
        kind: if n == 1 {
            DirectAttemptKind::Prompt
        } else {
            DirectAttemptKind::Resume
        },
        started_at: "2026-10-14T15:29:11Z".into(),
        ended_at: "2026-10-14T15:29:12Z".into(),
        result: DirectAttemptResult::ProviderError,
        retry_class: Some("transient".into()),
        detail: Some("boom".into()),
        backoff_s: Some(5),
        model_id: None,
    }
}

#[test]
fn the_protocol_is_1_38_0() {
    assert_eq!(PROTOCOL_VERSION, "1.59.0");
}

#[test]
fn a_summary_with_attempts_round_trips_byte_for_byte() {
    let s = summary();
    assert_eq!(s.attempts.len(), 3);
    assert_eq!(s.attempts[1].result, DirectAttemptResult::EngineCancelled);
    assert_eq!(s.attempts[0].kind, DirectAttemptKind::Prompt);
    assert_eq!(s.attempts[0].result, DirectAttemptResult::ProviderError);
    assert_eq!(s.attempts[0].backoff_s, Some(5));
    assert_eq!(s.attempts[1].kind, DirectAttemptKind::Resume);
    assert_eq!(s.attempts[1].retry_class.as_deref(), Some("permanent"));
    assert_eq!(s.attempts[2].kind, DirectAttemptKind::Resume);
    assert_eq!(s.attempts[2].result, DirectAttemptResult::Ok);
    assert_eq!(
        (
            s.attempts[2].retry_class.clone(),
            s.attempts[2].detail.clone(),
            s.attempts[2].backoff_s
        ),
        (None, None, None)
    );
    assert_eq!(serde_json::to_value(&s).unwrap(), golden());
    assert_eq!(s.validate(), Ok(()));
}

#[test]
fn a_summary_without_the_member_reads_as_no_attempts_and_writes_none_back() {
    let mut v = golden();
    v.as_object_mut().unwrap().remove("attempts");
    let s: DirectSummary = serde_json::from_value(v.clone()).unwrap();
    assert!(s.attempts.is_empty());
    assert_eq!(
        serde_json::to_value(&s).unwrap(),
        v,
        "an empty list is not written"
    );
}

#[test]
fn validate_refuses_too_many_tries_and_a_long_detail() {
    let mut s = summary();
    s.attempts = (1..=MAX_DIRECT_ATTEMPTS as u32 + 1).map(attempt).collect();
    assert_eq!(s.validate(), Err(DirectSummaryError::TooManyAttempts));
    let mut s = summary();
    s.attempts = (1..=MAX_DIRECT_ATTEMPTS as u32).map(attempt).collect();
    assert_eq!(s.validate(), Ok(()), "the limit itself passes");
    let mut s = summary();
    s.attempts[0].detail = Some("d".repeat(MAX_DIRECT_DETAIL_CHARS + 1));
    assert_eq!(s.validate(), Err(DirectSummaryError::DetailTooLong));
    s.attempts[0].detail = Some("d".repeat(MAX_DIRECT_DETAIL_CHARS));
    assert_eq!(s.validate(), Ok(()));
}

#[test]
fn a_kind_or_result_the_contract_does_not_name_is_refused() {
    for (field, value) in [("kind", "nudge"), ("result", "great")] {
        let mut v = golden();
        v["attempts"][0][field] = json!(value);
        assert!(
            serde_json::from_value::<DirectSummary>(v).is_err(),
            "{field}={value}"
        );
    }
}

#[test]
fn the_runtime_schema_declares_the_attempt() {
    let (_, schema) = all_schemas()
        .into_iter()
        .find(|(name, _)| name.contains("runtime"))
        .expect("runtime schema");
    let text = serde_json::to_string(&schema).unwrap();
    assert!(text.contains("DirectAttempt"), "DirectAttempt is declared");
    let attempt = &schema["$defs"]["DirectAttempt"]["properties"];
    for key in [
        "n",
        "kind",
        "startedAt",
        "endedAt",
        "result",
        "retryClass",
        "detail",
        "backoffS",
    ] {
        assert!(attempt.get(key).is_some(), "{key}: {attempt}");
    }
}
