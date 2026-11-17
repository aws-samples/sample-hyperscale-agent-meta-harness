//! The wire members of the tokens of the prompts that answered.
//!
//! * `TranscriptSummary.answeredTokens`: what a worker declares at the end of its session beside the last stored chunk and its losses.
//!   The summary travels in `TurnCompleteRequest.transcript` and worker-api stores it on the session's Turn item, so the number needs
//!   no request and no write of its own.
//! * `RunCost.answeredTokens`: the sum that control-api gives over exactly the turns whose usage is in the lines of the cost.
//!
//! Both members are optional and left out when absent, so the form of each type from before the members still reads and still writes
//! byte for byte. A worker that predates the member sends none, and the service then gives no number.

#![allow(clippy::unwrap_used)]

use harness_protocol::api::internal::TurnCompleteRequest;
use harness_protocol::api::v1::RunCost;
use harness_protocol::transcript::TranscriptSummary;
use serde_json::{Value, json};

/// A summary as a worker wrote it before the member existed.
fn summary_before() -> Value {
    json!({"lastTranscriptSeq": 6, "entries": [{"kind": "excluded", "count": 2}]})
}

/// A cost as control-api wrote it before the member existed.
fn cost_before() -> Value {
    json!({
        "pricesVersion": "bedrock-us-west-2-2026-10-05",
        "lines": [{
            "modelId": "global.anthropic.claude-sonnet-5-5", "engine": "cline",
            "usage": {"inputTokens": 52, "outputTokens": 4613, "cacheReadInputTokens": 200_632, "cacheWriteInputTokens": 41_828},
            "usd": 0.19093
        }],
        "turnsWithoutUsage": 0
    })
}

#[test]
fn b846_the_summary_carries_the_answered_tokens() {
    // The form of before: read, and written back byte for byte, with no number.
    let before: TranscriptSummary = serde_json::from_value(summary_before()).unwrap();
    assert_eq!(serde_json::to_value(&before).unwrap(), summary_before());
    assert!(before.is_lossless());
    // With the member: it round-trips, and the summary is as valid and as lossless as without it.
    let mut with = summary_before();
    with["answeredTokens"] = json!(1_727_891u64);
    let s: TranscriptSummary = serde_json::from_value(with.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(&s).unwrap(),
        with,
        "the member is read and written"
    );
    assert!(s.validate() && s.is_lossless(), "{s:?}");
    // The largest number a worker can declare.
    with["answeredTokens"] = json!(u64::MAX);
    let s: TranscriptSummary = serde_json::from_value(with.clone()).unwrap();
    assert_eq!(serde_json::to_value(&s).unwrap(), with);
    // An empty summary writes no member at all.
    assert_eq!(
        serde_json::to_value(TranscriptSummary::default()).unwrap(),
        json!({"entries": []})
    );
}

#[test]
fn b846_the_summary_travels_in_the_turn_complete_request() {
    let req = json!({
        "runId": "01M7HJNPAZVSDBPKFS6FB44ZY4", "agentId": "direct", "turn": 2, "finishReason": "end_turn",
        "transcript": {"lastTranscriptSeq": 6, "entries": [], "answeredTokens": 913_896u64}
    });
    let parsed: TurnCompleteRequest = serde_json::from_value(req.clone()).unwrap();
    let back = serde_json::to_value(&parsed).unwrap();
    assert_eq!(
        back["transcript"], req["transcript"],
        "the request carries the number inside the summary"
    );
}

#[test]
fn b846_the_cost_carries_the_answered_tokens() {
    let before: RunCost = serde_json::from_value(cost_before()).unwrap();
    assert_eq!(serde_json::to_value(&before).unwrap(), cost_before());
    let mut with = cost_before();
    with["answeredTokens"] = json!(1_727_891u64);
    let c: RunCost = serde_json::from_value(with.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(&c).unwrap(),
        with,
        "the member is read and written"
    );
}

/// The exported contract names the members where each reader finds them.
#[test]
fn b846_the_contract_files_name_the_answered_tokens() {
    let docs: std::collections::BTreeMap<&str, Value> = harness_protocol::export::all_schemas()
        .into_iter()
        .collect();
    let internal = &docs["openapi-internal.json"]["components"]["schemas"];
    assert!(
        internal["TranscriptSummary"]["properties"]["answeredTokens"].is_object(),
        "{}",
        internal["TranscriptSummary"]
    );
    let v1 = &docs["openapi-v1.json"]["components"]["schemas"];
    assert!(
        v1["RunCost"]["properties"]["answeredTokens"].is_object(),
        "{}",
        v1["RunCost"]
    );
    // Neither member is required.
    for (schema, name) in [
        (&internal["TranscriptSummary"], "TranscriptSummary"),
        (&v1["RunCost"], "RunCost"),
    ] {
        let required = schema["required"].as_array().cloned().unwrap_or_default();
        assert!(
            !required.contains(&json!("answeredTokens")),
            "{name}: {required:?}"
        );
    }
}
