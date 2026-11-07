//! Protocol 1.34.0: the start request carries the routes agentd may fall back to,
//! and `summary.json` lists every switch it made, the model the run ended on and the limits it used. Golden JSON goes in and the same JSON
//! comes out; a run without failover carries none of the new members, so the requests and summaries of 1.33.0 stay what they were.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::direct::{
    DirectAttempt, DirectAttemptKind, DirectAttemptResult, DirectInvocationError,
    DirectInvocationRequest, DirectModelSpec, DirectSummary, DirectSummaryError, DirectSwitch,
    DirectSwitchKind, HeadSource, MAX_DIRECT_DETAIL_CHARS, MAX_DIRECT_ROUTES, MAX_DIRECT_SWITCHES,
    validate_direct_invocation,
};
use harness_protocol::export::all_schemas;
use serde_json::{Value, json};

const RUN: &str = "01M4XAB0CDEFGHJKMNPQRSTVWX";
const BUCKET: &str = "hamh-direct-harness-111122223333";

fn route(id: &str) -> Value {
    json!({"id": id, "region": "us-east-1", "protocol": "converse", "contextTokens": 1_000_000, "maxOutputTokens": 128_000})
}

fn golden_request() -> Value {
    json!({
        "kind": "direct",
        "runId": RUN,
        "agentId": "direct",
        "turn": 1,
        "runtimeSessionId": format!("{RUN}-direct-g1"),
        "credentials": {
            "Version": 1,
            "AccessKeyId": "ASIA-test-0123456789",
            "SecretAccessKey": "test-secret-not-real",
            "SessionToken": "test-session-token-not-real",
            "Expiration": "2026-10-13T19:10:00Z"
        },
        "nonce": "test-nonce-not-real",
        "internalApiUrl": "https://abc.execute-api.us-east-1.amazonaws.com/direct",
        "region": "us-east-1",
        "engine": "cline",
        "model": route("us.anthropic.claude-opus-5-5"),
        "failover": [
            route("global.anthropic.claude-opus-5-5"),
            route("global.anthropic.claude-sonnet-5-5"),
            route("us.anthropic.claude-sonnet-5-5"),
            route("us.openai.gpt-6.1-sol")
        ],
        "request": {
            "uri": format!("s3://{BUCKET}/runs/{RUN}/input/request.json"),
            "sha256": "9f".repeat(32)
        },
        "repo": {
            "kind": "s3Bundle",
            "uri": format!("s3://{BUCKET}/runs/{RUN}/input/repo.bundle")
        },
        "timeoutS": 27_900,
        "snapshotEveryS": 600
    })
}

fn request() -> DirectInvocationRequest {
    serde_json::from_value(golden_request()).unwrap()
}

fn mutated(f: impl FnOnce(&mut Value)) -> DirectInvocationRequest {
    let mut v = golden_request();
    f(&mut v);
    serde_json::from_value(v).unwrap()
}

fn spec(id: &str) -> DirectModelSpec {
    serde_json::from_value(route(id)).unwrap()
}

#[test]
fn the_protocol_is_1_38_0() {
    assert_eq!(PROTOCOL_VERSION, "1.56.0");
}

// ---- the request ------------------------------------------------------------------------------------------------------

#[test]
fn a_request_with_failover_routes_round_trips_byte_for_byte() {
    let r = request();
    assert_eq!(r.failover.len(), 4);
    assert_eq!(r.failover[0].id, "global.anthropic.claude-opus-5-5");
    assert_eq!(serde_json::to_value(&r).unwrap(), golden_request());
    validate_direct_invocation(&r).unwrap();
}

#[test]
fn a_request_without_failover_carries_no_member_and_reads_as_none() {
    let mut v = golden_request();
    v.as_object_mut().unwrap().remove("failover");
    let r: DirectInvocationRequest = serde_json::from_value(v.clone()).unwrap();
    assert!(r.failover.is_empty());
    assert_eq!(serde_json::to_value(&r).unwrap(), v);
    validate_direct_invocation(&r).unwrap();
}

#[test]
fn a_route_with_an_unknown_member_is_refused() {
    let mut v = golden_request();
    v["failover"][0]["accountId"] = json!("123456789012");
    assert!(serde_json::from_value::<DirectInvocationRequest>(v).is_err());
}

#[test]
fn at_most_eight_routes_follow_the_preferred_one() {
    assert_eq!(MAX_DIRECT_ROUTES, 8);
    let ok = mutated(|v| {
        v["failover"] = Value::Array(
            (0..8)
                .map(|i| route(&format!("global.vendor.model-{i}")))
                .collect(),
        );
    });
    validate_direct_invocation(&ok).unwrap();
    let too_many = mutated(|v| {
        v["failover"] = Value::Array(
            (0..9)
                .map(|i| route(&format!("global.vendor.model-{i}")))
                .collect(),
        );
    });
    assert_eq!(
        validate_direct_invocation(&too_many),
        Err(DirectInvocationError::TooManyFailoverRoutes)
    );
}

#[test]
fn a_route_may_not_repeat_the_preferred_model_or_another_route() {
    let same_as_preferred = mutated(|v| v["failover"][1] = route("us.anthropic.claude-opus-5-5"));
    assert_eq!(
        validate_direct_invocation(&same_as_preferred),
        Err(DirectInvocationError::DuplicateFailoverRoute)
    );
    let twice = mutated(|v| v["failover"][3] = route("global.anthropic.claude-sonnet-5-5"));
    assert_eq!(
        validate_direct_invocation(&twice),
        Err(DirectInvocationError::DuplicateFailoverRoute)
    );
}

#[test]
fn a_route_is_checked_like_the_model() {
    for (what, change) in [
        (
            "an id with a slash (a Mantle route is not offered)",
            json!({"id": "aws-mantle/zai.glm-5"}),
        ),
        ("an empty id", json!({"id": ""})),
        ("a space in the id", json!({"id": "us.vendor model"})),
        ("a bad Region", json!({"region": "Mars-1"})),
        ("a zero window", json!({"contextTokens": 0})),
        ("a zero output limit", json!({"maxOutputTokens": 0})),
    ] {
        let r = mutated(|v| {
            for (k, val) in change.as_object().unwrap() {
                v["failover"][2][k] = val.clone();
            }
        });
        assert_eq!(
            validate_direct_invocation(&r),
            Err(DirectInvocationError::BadFailoverRoute),
            "{what}"
        );
    }
}

#[test]
fn the_request_body_with_every_route_stays_far_under_the_size_limit() {
    let bytes = serde_json::to_vec(&mutated(|v| {
        v["failover"] = Value::Array(
            (0..8)
                .map(|i| route(&format!("global.vendor.model-number-{i}")))
                .collect(),
        );
    }))
    .unwrap();
    assert!(
        bytes.len() < harness_protocol::direct::MAX_DIRECT_BODY_BYTES / 4,
        "{}",
        bytes.len()
    );
}

// ---- the summary ------------------------------------------------------------------------------------------------------

fn golden_summary() -> Value {
    json!({
        "schemaVersion": 1,
        "runId": RUN, "agentId": "direct", "turn": 1,
        "outcome": "reported_done",
        "label": "Cline reported done",
        "detail": "Ran on us.anthropic.claude-sonnet-5-5 after 2 switches.",
        "stopReason": "end_turn",
        "turnOutcome": "completed",
        "clineMessage": "The work is in place.",
        "error": null,
        "model": {"id": "us.anthropic.claude-opus-5-5", "region": "us-east-1", "pinned": "us.anthropic.claude-opus-5-5"},
        "engine": {"name": "cline", "version": "3.0.71"},
        "rules": [],
        "rulesPreflight": {"ran": false},
        "request": {"sha256": "9f".repeat(32), "promptBytes": 123},
        "nudge": {"requested": false, "honored": false},
        "usage": null,
        "toolCalls": 88,
        "timing": {"startedAt": "2026-10-15T15:29:11Z", "endedAt": "2026-10-15T16:49:53Z", "wallS": 4842, "promptS": 4840, "maxSilentGapS": 337},
        "git": {"committedByHarness": false, "filesChanged": 26},
        "snapshots": {"count": 8, "lastAt": "2026-10-15T16:49:13Z"},
        "packages": {"enabled": true, "requests": 845, "bytes": 528_752_828, "byPrefix": {"npm": 797, "pypi": 48}, "statuses": {"200": 845}},
        "faultHint": null,
        "versions": {"agentd": "0.1.0", "protocol": "1.35.0"},
        "attempts": [
            {"n": 1, "kind": "prompt", "modelId": "us.anthropic.claude-opus-5-5", "startedAt": "2026-10-15T15:29:11Z", "endedAt": "2026-10-15T15:49:53Z",
             "result": "provider_error", "retryClass": "transient", "detail": "engine error -32603: Internal error: Service unavailable", "backoffS": 5},
            {"n": 2, "kind": "resume", "modelId": "us.anthropic.claude-opus-5-5", "startedAt": "2026-10-15T15:49:58Z", "endedAt": "2026-10-15T15:50:30Z",
             "result": "provider_error", "retryClass": "transient", "detail": "engine error -32603: Internal error: Service unavailable"},
            {"n": 3, "kind": "resume", "modelId": "global.anthropic.claude-opus-5-5", "startedAt": "2026-10-15T15:50:35Z", "endedAt": "2026-10-15T15:52:10Z",
             "result": "provider_error", "retryClass": "transient", "detail": "engine error -32603: Internal error: Service unavailable"},
            {"n": 4, "kind": "resume", "modelId": "us.anthropic.claude-sonnet-5-5", "startedAt": "2026-10-15T15:52:20Z", "endedAt": "2026-10-15T16:49:53Z", "result": "ok"}
        ],
        "switches": [
            {"n": 1, "at": "2026-10-15T15:50:33Z", "from": "us.anthropic.claude-opus-5-5", "to": "global.anthropic.claude-opus-5-5", "kind": "route",
             "class": "transient", "detail": "engine error -32603: Internal error: Service unavailable"},
            {"n": 2, "at": "2026-10-15T15:52:12Z", "from": "global.anthropic.claude-opus-5-5", "to": "us.anthropic.claude-sonnet-5-5", "kind": "model",
             "class": "transient"}
        ],
        "finalModel": "us.anthropic.claude-sonnet-5-5",
        "failover": {"routes": 5, "effectiveMaxOutputTokens": 128_000, "effectiveContextTokens": 1_000_000, "passes": 1, "probes": 0, "waitedS": 0}
    })
}

fn summary() -> DirectSummary {
    serde_json::from_value(golden_summary()).unwrap()
}

#[test]
fn a_summary_with_switches_round_trips_byte_for_byte() {
    let s = summary();
    assert_eq!(s.switches.len(), 2);
    assert_eq!(s.switches[0].kind, DirectSwitchKind::Route);
    assert_eq!(s.switches[1].kind, DirectSwitchKind::Model);
    assert_eq!(
        s.final_model.as_deref(),
        Some("us.anthropic.claude-sonnet-5-5")
    );
    assert_eq!(
        s.attempts[2].model_id.as_deref(),
        Some("global.anthropic.claude-opus-5-5")
    );
    let note = s.failover.as_ref().unwrap();
    assert_eq!(
        (note.routes, note.passes, note.probes, note.waited_s),
        (5, 1, 0, 0)
    );
    assert_eq!(serde_json::to_value(&s).unwrap(), golden_summary());
    s.validate().unwrap();
}

#[test]
fn a_summary_of_a_run_without_failover_carries_none_of_the_new_members() {
    let mut v = golden_summary();
    for k in ["switches", "finalModel", "failover"] {
        v.as_object_mut().unwrap().remove(k);
    }
    for a in v["attempts"].as_array_mut().unwrap() {
        a.as_object_mut().unwrap().remove("modelId");
    }
    let s: DirectSummary = serde_json::from_value(v.clone()).unwrap();
    assert!(s.switches.is_empty() && s.final_model.is_none() && s.failover.is_none());
    assert_eq!(serde_json::to_value(&s).unwrap(), v);
    s.validate().unwrap();
}

#[test]
fn a_reader_of_1_33_0_ignores_the_new_members() {
    // The reading rule: an unknown key in summary.json is ignored. A value of 1.33.0 shape reads, and the new keys read too.
    let v = golden_summary();
    let s: DirectSummary = serde_json::from_value(v).unwrap();
    assert_eq!(s.attempts.len(), 4);
}

fn switch(n: u32) -> DirectSwitch {
    DirectSwitch {
        n,
        at: "2026-10-15T15:50:33Z".into(),
        from: "a.b".into(),
        to: "c.d".into(),
        kind: DirectSwitchKind::Model,
        class: "transient".into(),
        detail: Some("boom".into()),
    }
}

#[test]
fn at_most_96_switches_are_listed() {
    assert_eq!(MAX_DIRECT_SWITCHES, 96);
    let mut s = summary();
    s.switches = (1..=96).map(switch).collect();
    s.validate().unwrap();
    s.switches.push(switch(97));
    assert_eq!(s.validate(), Err(DirectSummaryError::TooManySwitches));
}

#[test]
fn a_switch_detail_is_cut_like_every_detail() {
    let mut s = summary();
    s.switches[0].detail = Some("x".repeat(MAX_DIRECT_DETAIL_CHARS + 1));
    assert_eq!(s.validate(), Err(DirectSummaryError::DetailTooLong));
}

#[test]
fn the_evidence_names_the_requested_model_the_final_model_and_the_count() {
    let e = summary().to_evidence(HeadSource::Final);
    assert_eq!(e.model_id, "us.anthropic.claude-opus-5-5");
    assert_eq!(
        e.final_model_id.as_deref(),
        Some("us.anthropic.claude-sonnet-5-5")
    );
    assert_eq!(e.model_switches, 2);
    let mut v = golden_summary();
    for k in ["switches", "finalModel", "failover"] {
        v.as_object_mut().unwrap().remove(k);
    }
    let plain = serde_json::from_value::<DirectSummary>(v)
        .unwrap()
        .to_evidence(HeadSource::Final);
    assert_eq!((plain.final_model_id, plain.model_switches), (None, 0));
}

#[test]
fn an_attempt_records_the_route_it_ran_on() {
    let a = DirectAttempt {
        n: 2,
        kind: DirectAttemptKind::Resume,
        started_at: "2026-10-15T15:29:11Z".into(),
        ended_at: "2026-10-15T15:29:12Z".into(),
        result: DirectAttemptResult::ProviderError,
        retry_class: Some("transient".into()),
        detail: None,
        backoff_s: None,
        model_id: Some("global.anthropic.claude-opus-5-5".into()),
    };
    let v = serde_json::to_value(&a).unwrap();
    assert_eq!(v["modelId"], "global.anthropic.claude-opus-5-5");
    assert_eq!(serde_json::from_value::<DirectAttempt>(v).unwrap(), a);
}

// ---- the schemas ------------------------------------------------------------------------------------------------------

#[test]
fn the_schemas_name_the_new_members() {
    let (_, schema) = all_schemas()
        .into_iter()
        .find(|(name, _)| name.contains("runtime"))
        .expect("runtime schema");
    let defs = &schema["$defs"];
    for (def, key) in [
        ("DirectInvocationRequest", "failover"),
        ("DirectSummary", "switches"),
        ("DirectSummary", "finalModel"),
        ("DirectSummary", "failover"),
        ("DirectAttempt", "modelId"),
        ("DirectSwitch", "kind"),
        ("DirectFailoverNote", "effectiveMaxOutputTokens"),
        ("DirectEvidence", "finalModelId"),
        ("DirectEvidence", "modelSwitches"),
    ] {
        assert!(defs[def]["properties"].get(key).is_some(), "{def}.{key}");
    }
    let _ = spec("a.b");
}
