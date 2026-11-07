//! Who the engine ran as, on the wire: the optional `engine.identity` of `summary.json` is `nobody` (the nobody route of an
//! Instances session), `agentd` (the engine as agentd's own user) or the name of the engine user (the microVM). The member is additive: a summary written before
//! this version reads as no identity and writes none back, and the reader ignores it when it is older.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::direct::{DirectSummary, DirectSummaryError};
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
        "timing": {"startedAt": "2026-10-27T15:29:11Z", "endedAt": "2026-10-27T15:49:53Z", "wallS": 1241, "promptS": 1239, "maxSilentGapS": 337},
        "git": {"committedByHarness": false, "filesChanged": 26},
        "snapshots": {"count": 2, "lastAt": "2026-10-27T15:49:13Z"},
        "packages": {"enabled": true, "requests": 845, "bytes": 528_752_828, "byPrefix": {"npm": 797, "pypi": 48}, "statuses": {"200": 837, "502": 8}},
        "faultHint": null,
        "versions": {"agentd": "0.1.0", "protocol": "1.49.0"}
    })
}

fn with_identity(word: &str) -> Value {
    let mut v = golden();
    v["engine"]["identity"] = json!(word);
    v
}

#[test]
fn the_protocol_is_1_49_0_or_later() {
    let mut parts = PROTOCOL_VERSION
        .split('.')
        .map(|p| p.parse::<u32>().unwrap());
    let (major, minor) = (parts.next().unwrap(), parts.next().unwrap());
    assert!(major == 1 && minor >= 49, "{PROTOCOL_VERSION}");
}

#[test]
fn a_summary_without_the_member_reads_as_no_identity_and_writes_none_back() {
    let s: DirectSummary = serde_json::from_value(golden()).unwrap();
    assert_eq!(s.engine.identity, None);
    assert_eq!(serde_json::to_value(&s).unwrap(), golden());
    assert_eq!(s.validate(), Ok(()));
}

#[test]
fn the_identity_round_trips_in_camel_case_under_engine() {
    for word in ["nobody", "agentd", "agent"] {
        let v = with_identity(word);
        let s: DirectSummary = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(s.engine.identity.as_deref(), Some(word));
        assert_eq!(serde_json::to_value(&s).unwrap(), v);
        assert_eq!(s.validate(), Ok(()), "{word}");
    }
}

#[test]
fn validate_refuses_a_word_that_is_not_a_plain_name() {
    for bad in [
        "",
        "Nobody",
        "no body",
        "nobody\n",
        "root!",
        "a/b",
        &"a".repeat(33),
    ] {
        let s: DirectSummary = serde_json::from_value(with_identity(bad)).unwrap();
        assert_eq!(
            s.validate(),
            Err(DirectSummaryError::EngineIdentityInvalid),
            "{bad:?}"
        );
    }
    assert!(
        DirectSummaryError::EngineIdentityInvalid
            .to_string()
            .contains("identity")
    );
}

#[test]
fn only_the_runtime_schema_carries_the_member() {
    for (name, value) in all_schemas() {
        let has = serde_json::to_string(&value)
            .unwrap()
            .contains("\"identity\"");
        assert_eq!(has, name == "runtime.schema.json", "{name}");
    }
}
