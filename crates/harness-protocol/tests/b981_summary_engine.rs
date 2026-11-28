//! The run summary names the engine the run is on.
//!
//! `RunSummary.engine` is optional and left out when absent. The service leaves it out for a run on the default engine, so the answer
//! about such a run is the one every earlier client read; a summary without the member means the default engine or a service that
//! predates the member. The 1.64.0 form of the summary still reads and still writes as it did, alone, in the run list and in the
//! answer about one run.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::api::v1::{ListRunsResponse, RunDetail, RunSummary};
use serde_json::{Value, json};

const RUN: &str = "01M7HJNPAZVSDBPKFS6FB44ZY4";

/// A row of the run list as protocol 1.64.0 wrote it.
fn summary_1_64() -> Value {
    json!({
        "runId": RUN, "status": "complete", "phase": "delivering", "target": {"mode": "s3-only"},
        "createdAt": "2026-11-27T07:00:00Z", "ownerPrincipalArn": "arn:aws:sts::123456789012:assumed-role/Admin/me"
    })
}

/// The answer about one run as protocol 1.64.0 wrote it.
fn detail_1_64() -> Value {
    json!({"summary": summary_1_64(), "tokensUsed": 0, "costUsd": 0.0, "links": {}})
}

fn docs() -> std::collections::BTreeMap<&'static str, Value> {
    harness_protocol::export::all_schemas()
        .into_iter()
        .collect()
}

#[test]
fn b981_the_protocol_is_1_65_0_in_both_openapi_documents() {
    assert_eq!(PROTOCOL_VERSION, "1.65.0");
    let docs = docs();
    assert_eq!(docs["openapi-v1.json"]["info"]["version"], "1.65.0");
    assert_eq!(docs["openapi-internal.json"]["info"]["version"], "1.65.0");
}

#[test]
fn b981_a_summary_without_the_engine_reads_and_writes_as_it_did() {
    let old: RunSummary = serde_json::from_value(summary_1_64()).unwrap();
    assert_eq!(serde_json::to_value(&old).unwrap(), summary_1_64());
    let list: ListRunsResponse = serde_json::from_value(json!({"runs": [summary_1_64()]})).unwrap();
    assert_eq!(
        serde_json::to_value(&list).unwrap(),
        json!({"runs": [summary_1_64()]})
    );
    let detail: RunDetail = serde_json::from_value(detail_1_64()).unwrap();
    assert_eq!(serde_json::to_value(&detail).unwrap(), detail_1_64());
}

#[test]
fn b981_a_summary_carries_the_engine_under_the_name_engine() {
    // Any engine name is carried: the name of an engine's profile, as in the start request and on the Run item.
    for name in ["opencode", "cline", "pi"] {
        let mut with = summary_1_64();
        with["engine"] = json!(name);
        let row: RunSummary = serde_json::from_value(with.clone()).unwrap();
        assert_eq!(serde_json::to_value(&row).unwrap(), with, "{name}");
        // The same summary in the run list and in the answer about one run.
        let list: ListRunsResponse = serde_json::from_value(json!({"runs": [with]})).unwrap();
        assert_eq!(
            serde_json::to_value(&list).unwrap()["runs"][0]["engine"],
            json!(name)
        );
        let mut one = detail_1_64();
        one["summary"]["engine"] = json!(name);
        let detail: RunDetail = serde_json::from_value(one.clone()).unwrap();
        assert_eq!(serde_json::to_value(&detail).unwrap(), one, "{name}");
    }
    // The member is an engine name: a value of another shape is refused, as it is in the start request.
    let mut odd = summary_1_64();
    odd["engine"] = json!("Not An Engine");
    assert!(serde_json::from_value::<RunSummary>(odd).is_err());
}

/// The exported contract names the member where a client reads it and does not require it.
#[test]
fn b981_the_contract_names_the_member_and_does_not_require_it() {
    let docs = docs();
    let summary = &docs["openapi-v1.json"]["components"]["schemas"]["RunSummary"];
    assert!(
        summary["properties"]["engine"].is_object(),
        "RunSummary.engine: {summary}"
    );
    let required = summary["required"].as_array().unwrap();
    assert!(!required.contains(&json!("engine")), "{required:?}");
    for member in ["runId", "status", "phase", "target", "createdAt"] {
        assert!(required.contains(&json!(member)), "{member}: {required:?}");
    }
}
