//! Protocol 1.38.0: `RunSummary.continueFrom` and `RunItem.continueFrom`, the id of the run a follow-up run continues from. The member is
//! a plain run id, absent for an ordinary run, so a client that lists runs can show which run a follow-up came from without reading the stored
//! start request. `RunDetail` carries it through its `summary`.

#![allow(clippy::unwrap_used)]

use harness_protocol::api::v1::{RunDetail, RunSummary};
use harness_protocol::store::RunItem;
use serde_json::{Value, json};

const RUN: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const BASE: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const ARN: &str = "arn:aws:sts::700000000011:assumed-role/Admin/testuser";

fn summary_json() -> Value {
    json!({
        "runId": RUN, "status": "complete", "phase": "green", "target": {"mode": "s3-only"},
        "createdAt": "2026-10-17T08:00:00Z", "ownerPrincipalArn": ARN
    })
}

fn item_json() -> Value {
    json!({
        "runId": RUN, "ownerPrincipalArn": ARN, "promptSha256": "cd".repeat(32), "target": {"mode": "s3-only"},
        "status": "complete", "phase": "green", "enginePolicyVersion": 0, "createdAt": "2026-10-17T08:00:00Z",
        "version": 3
    })
}

#[test]
fn a_summary_names_the_run_it_continues_from() {
    let mut v = summary_json();
    v["continueFrom"] = json!(BASE);
    let s: RunSummary = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(s.continue_from.as_deref(), Some(BASE));
    assert_eq!(
        serde_json::to_value(&s).unwrap(),
        v,
        "the wire form is the member as sent"
    );
}

#[test]
fn an_ordinary_summary_has_no_member_and_writes_none() {
    let s: RunSummary = serde_json::from_value(summary_json()).unwrap();
    assert_eq!(s.continue_from, None);
    assert!(
        serde_json::to_value(&s)
            .unwrap()
            .get("continueFrom")
            .is_none()
    );
}

#[test]
fn the_member_is_a_run_id_string_and_not_the_request_object() {
    // The request member `StartDirectRunRequest.continueFrom` is an object; the response member is only the id.
    let mut v = summary_json();
    v["continueFrom"] = json!({"runId": BASE});
    assert!(serde_json::from_value::<RunSummary>(v).is_err());
}

#[test]
fn the_detail_carries_it_through_its_summary() {
    let mut s = summary_json();
    s["continueFrom"] = json!(BASE);
    let d: RunDetail = serde_json::from_value(json!({
        "summary": s, "tokensUsed": 12, "costUsd": 0.5
    }))
    .unwrap();
    assert_eq!(d.summary.continue_from.as_deref(), Some(BASE));
    let back = serde_json::to_value(&d).unwrap();
    assert_eq!(back["summary"]["continueFrom"], json!(BASE));
    assert!(
        back.get("continueFrom").is_none(),
        "the detail has no member of its own"
    );
}

#[test]
fn the_run_item_keeps_the_member_and_an_old_item_reads_as_ordinary() {
    let mut v = item_json();
    v["continueFrom"] = json!(BASE);
    let r: RunItem = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(r.continue_from.as_deref(), Some(BASE));
    assert_eq!(serde_json::to_value(&r).unwrap(), v);

    let old: RunItem = serde_json::from_value(item_json()).unwrap();
    assert_eq!(old.continue_from, None);
    assert!(
        serde_json::to_value(&old)
            .unwrap()
            .get("continueFrom")
            .is_none()
    );
}
