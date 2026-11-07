//! Protocol 1.47.0: reasoning is on by default. The model catalog gives each model the level it reasons at, run-session
//! puts that level on the model spec of the invocation (`DirectModelSpec.defaultReasoningEffort`), agentd launches the engine with it and records it in
//! `summary.json` (`engine.reasoningEffort`, `engine.reasoningSource`). The user's `reasoning.effort` key gains `off` (no reasoning argument) and `xhigh`. A spec or summary
//! without the new members is what 1.45.0 wrote, byte for byte.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::direct::{
    CLINE_KEYS, DirectInvocationError, DirectInvocationRequest, DirectSummary, EngineConfig,
    KeyKind, REASONING_LEVELS, check_engine_config, table_problems, validate_direct_invocation,
};
use serde_json::{Value, json};

const RUN: &str = "01M4XAB0CDEFGHJKMNPQRSTVWX";
const BUCKET: &str = "hamh-direct-harness-111122223333";

fn model(id: &str) -> Value {
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
            "AccessKeyId": concat!("ASIA", "-test-0123456789"),
            "SecretAccessKey": concat!("test-", "secret-not-real"),
            "SessionToken": concat!("test-session-", "token-not-real"),
            "Expiration": "2026-10-27T19:10:00Z"
        },
        "nonce": "test-nonce-not-real",
        "internalApiUrl": "https://abc.execute-api.us-east-1.amazonaws.com/direct",
        "region": "us-east-1",
        "engine": "cline",
        "model": model("us.openai.gpt-6.1-sol"),
        "failover": [model("global.openai.gpt-6.1-sol"), model("global.anthropic.claude-sonnet-5-5")],
        "request": {"uri": format!("s3://{BUCKET}/runs/{RUN}/input/request.json"), "sha256": "9f".repeat(32)},
        "repo": {"kind": "s3Bundle", "uri": format!("s3://{BUCKET}/runs/{RUN}/input/repo.bundle")},
        "timeoutS": 27_900,
        "snapshotEveryS": 600
    })
}

fn mutated(f: impl FnOnce(&mut Value)) -> Result<DirectInvocationRequest, serde_json::Error> {
    let mut v = golden_request();
    f(&mut v);
    serde_json::from_value(v)
}

fn cfg(settings: Value) -> EngineConfig {
    EngineConfig {
        engine: "cline".into(),
        schema: 1,
        settings: settings.as_object().unwrap().clone(),
    }
}

#[test]
fn the_protocol_is_1_46_0() {
    assert_eq!(PROTOCOL_VERSION, "1.56.0");
}

// ---- the user's key ---------------------------------------------------------------------------------------------------------

#[test]
fn reasoning_effort_offers_default_off_and_the_four_launch_levels_in_that_order() {
    let row = CLINE_KEYS
        .iter()
        .find(|r| r.path == "reasoning.effort")
        .unwrap();
    let KeyKind::Choice(levels) = row.kind else {
        panic!("reasoning.effort is a choice");
    };
    assert_eq!(levels, ["default", "off", "low", "medium", "high", "xhigh"]);
    assert_eq!(row.default, "default");
    assert!(table_problems(CLINE_KEYS).is_empty());
}

#[test]
fn every_offered_level_passes_the_one_check_and_the_words_the_route_refuses_stay_refused() {
    for ok in ["default", "off", "low", "medium", "high", "xhigh"] {
        check_engine_config(&cfg(json!({"reasoning": {"effort": ok}})))
            .unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
    for bad in [
        "none", "max", "minimal", "OFF", "Off", " off", "xhigh ", "on", "true", "",
    ] {
        let p = check_engine_config(&cfg(json!({"reasoning": {"effort": bad}}))).unwrap_err();
        assert_eq!(p.code(), "out_of_range", "{bad:?}");
    }
    // A value is never echoed in the words of the refusal.
    const SENTINEL: &str = "VALUE-SENTINEL-77c1";
    let p = check_engine_config(&cfg(json!({"reasoning": {"effort": SENTINEL}}))).unwrap_err();
    assert_eq!(p.code(), "out_of_range");
    assert!(!p.to_string().contains(SENTINEL), "{p}");
}

// ---- the launch levels ---------------------------------------------------------------------------------------------------------

#[test]
fn the_launch_levels_are_the_four_the_engine_flag_takes() {
    assert_eq!(REASONING_LEVELS, ["low", "medium", "high", "xhigh"]);
}

// ---- the model spec -------------------------------------------------------------------------------------------------------------

#[test]
fn a_model_spec_carries_the_level_and_round_trips_byte_for_byte() {
    let mut want = golden_request();
    want["model"]["defaultReasoningEffort"] = json!("xhigh");
    want["failover"][1]["defaultReasoningEffort"] = json!("high");
    let r = mutated(|v| {
        v["model"]["defaultReasoningEffort"] = json!("xhigh");
        v["failover"][1]["defaultReasoningEffort"] = json!("high");
    })
    .unwrap();
    assert_eq!(r.model.default_reasoning_effort.as_deref(), Some("xhigh"));
    assert_eq!(
        r.failover[1].default_reasoning_effort.as_deref(),
        Some("high")
    );
    assert_eq!(r.failover[0].default_reasoning_effort, None);
    assert_eq!(serde_json::to_value(&r).unwrap(), want);
    validate_direct_invocation(&r).unwrap();
}

#[test]
fn a_spec_without_the_member_reads_as_none_and_writes_what_1_45_0_wrote() {
    let r = mutated(|_| {}).unwrap();
    assert_eq!(r.model.default_reasoning_effort, None);
    assert_eq!(serde_json::to_value(&r).unwrap(), golden_request());
    validate_direct_invocation(&r).unwrap();
}

#[test]
fn only_a_launch_level_is_valid_on_the_model_or_on_a_route() {
    for level in REASONING_LEVELS {
        let r = mutated(|v| v["model"]["defaultReasoningEffort"] = json!(level)).unwrap();
        validate_direct_invocation(&r).unwrap();
    }
    for bad in [
        "default", "off", "none", "max", "minimal", "HIGH", "", "high ",
    ] {
        let on_model = mutated(|v| v["model"]["defaultReasoningEffort"] = json!(bad)).unwrap();
        assert_eq!(
            validate_direct_invocation(&on_model),
            Err(DirectInvocationError::BadReasoningEffort),
            "model {bad:?}"
        );
        let on_route =
            mutated(|v| v["failover"][0]["defaultReasoningEffort"] = json!(bad)).unwrap();
        assert_eq!(
            validate_direct_invocation(&on_route),
            Err(DirectInvocationError::BadReasoningEffort),
            "route {bad:?}"
        );
    }
    let text = DirectInvocationError::BadReasoningEffort.to_string();
    assert!(
        text.contains("defaultReasoningEffort") && text.contains("xhigh"),
        "{text}"
    );
}

#[test]
fn a_spec_with_a_non_text_level_is_refused_at_the_parse() {
    assert!(mutated(|v| v["model"]["defaultReasoningEffort"] = json!(3)).is_err());
    assert!(mutated(|v| v["model"]["defaultReasoningEffort"] = json!({"level": "high"})).is_err());
}

// ---- the summary ----------------------------------------------------------------------------------------------------------------

fn golden_summary() -> Value {
    json!({
        "schemaVersion": 1,
        "runId": RUN, "agentId": "direct", "turn": 1,
        "outcome": "reported_done",
        "label": "Cline reported done",
        "detail": "Added the feature.",
        "stopReason": "end_turn",
        "turnOutcome": "completed",
        "clineMessage": "The work is in place.",
        "error": null,
        "model": {"id": "us.openai.gpt-6.1-sol", "region": "us-east-1", "pinned": "us.openai.gpt-6.1-sol"},
        "engine": {"name": "cline", "version": "3.0.71"},
        "rules": [],
        "rulesPreflight": {"ran": false},
        "request": {"sha256": "9f".repeat(32), "promptBytes": 123},
        "nudge": {"requested": false, "honored": false},
        "usage": null,
        "toolCalls": 88,
        "timing": {"startedAt": "2026-10-27T15:29:11Z", "endedAt": "2026-10-27T16:49:53Z", "wallS": 4842, "promptS": 4840, "maxSilentGapS": 337},
        "git": {"committedByHarness": false, "filesChanged": 26},
        "snapshots": {"count": 8, "lastAt": "2026-10-27T16:49:13Z"},
        "packages": {"enabled": false, "requests": 0, "bytes": 0, "byPrefix": {}, "statuses": {}},
        "faultHint": null,
        "versions": {"agentd": "0.1.0", "protocol": "1.47.0"}
    })
}

#[test]
fn the_summary_names_the_level_the_engine_was_launched_with_and_where_it_came_from() {
    for (level, source) in [("xhigh", "default"), ("high", "explicit")] {
        let mut v = golden_summary();
        v["engine"]["reasoningEffort"] = json!(level);
        v["engine"]["reasoningSource"] = json!(source);
        let s: DirectSummary = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(s.engine.reasoning_effort.as_deref(), Some(level));
        assert_eq!(s.engine.reasoning_source.as_deref(), Some(source));
        assert_eq!(serde_json::to_value(&s).unwrap(), v);
        s.validate().unwrap();
    }
}

#[test]
fn a_run_with_reasoning_off_names_the_source_and_no_level() {
    let mut v = golden_summary();
    v["engine"]["reasoningSource"] = json!("off");
    let s: DirectSummary = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(s.engine.reasoning_effort, None);
    assert_eq!(s.engine.reasoning_source.as_deref(), Some("off"));
    assert_eq!(serde_json::to_value(&s).unwrap(), v);
    s.validate().unwrap();
}

#[test]
fn a_summary_without_either_member_is_what_1_45_0_wrote() {
    let v = golden_summary();
    let s: DirectSummary = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(
        (
            s.engine.reasoning_effort.as_deref(),
            s.engine.reasoning_source.as_deref()
        ),
        (None, None)
    );
    assert_eq!(serde_json::to_value(&s).unwrap(), v);
    s.validate().unwrap();
}

#[test]
fn the_summary_words_are_plain_so_a_reader_older_than_the_writer_does_not_fail_on_a_new_one() {
    // A level or a source added by a later version reads and validates; only a word that is not plain text is refused.
    for (key, ok) in [
        ("reasoningEffort", "max"),
        ("reasoningEffort", "ultra-high"),
        ("reasoningSource", "catalog-v2"),
    ] {
        let mut v = golden_summary();
        v["engine"][key] = json!(ok);
        let s: DirectSummary = serde_json::from_value(v).unwrap();
        s.validate().unwrap_or_else(|e| panic!("{key} {ok:?}: {e}"));
    }
    for (key, bad) in [
        ("reasoningEffort", ""),
        ("reasoningEffort", "High"),
        ("reasoningEffort", "high\nlow"),
        ("reasoningEffort", "a-very-long-reasoning-word"),
        ("reasoningSource", ""),
        ("reasoningSource", "from the user"),
    ] {
        let mut v = golden_summary();
        v["engine"][key] = json!(bad);
        let s: DirectSummary = serde_json::from_value(v).unwrap();
        assert!(s.validate().is_err(), "{key} {bad:?}");
    }
}
