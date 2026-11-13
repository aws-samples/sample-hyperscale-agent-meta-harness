//! The `ham-core` readers of the run list pass `continueFrom` on. The run-index row (the `GET /v1/runs` answer) keeps it
//! in the flattened summary and writes it back; the Runs screen row (`runfilter::RunRow`) holds it as display text, cleaned like every other
//! word of a row, and holds nothing for an ordinary run.

#![allow(clippy::unwrap_used)]

use ham_core::run_index::RunRow as WireRow;
use ham_core::runfilter::RunRow;
use harness_protocol::api::v1::RunSummary;
use serde_json::{Value, json};

const RUN: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const BASE: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const ARN: &str = "arn:aws:sts::700000000011:assumed-role/Admin/testuser";

fn summary(extra: Value) -> RunSummary {
    let mut v = json!({
        "runId": RUN, "status": "complete", "phase": "green", "target": {"mode": "s3-only"},
        "createdAt": "2026-10-17T08:00:00Z", "ownerPrincipalArn": ARN
    });
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    serde_json::from_value(v).unwrap()
}

#[test]
fn the_wire_row_keeps_continue_from_in_its_flattened_summary() {
    let row = WireRow::new(summary(json!({"continueFrom": BASE})), None);
    assert_eq!(row.summary.continue_from.as_deref(), Some(BASE));
    let v = serde_json::to_value(&row).unwrap();
    assert_eq!(
        v["continueFrom"],
        json!(BASE),
        "flattened beside runId: {v}"
    );
    let back: WireRow = serde_json::from_value(v).unwrap();
    assert_eq!(back, row);
}

#[test]
fn the_wire_row_of_an_ordinary_run_has_no_member() {
    let row = WireRow::new(summary(json!({})), None);
    assert!(
        serde_json::to_value(&row)
            .unwrap()
            .get("continueFrom")
            .is_none()
    );
}

#[test]
fn the_screen_row_holds_the_base_run_id() {
    let r = RunRow::from_summary(&summary(json!({"continueFrom": BASE})));
    assert_eq!(r.continue_from.as_deref(), Some(BASE));
}

#[test]
fn the_screen_row_of_an_ordinary_run_holds_nothing() {
    let r = RunRow::from_summary(&summary(json!({})));
    assert_eq!(r.continue_from, None);
}

#[test]
fn the_screen_row_cleans_the_member_like_the_other_words() {
    // A server cannot make the screen draw a control sequence through this member.
    let r = RunRow::from_summary(&summary(
        json!({"continueFrom": format!("\u{1b}[31m{BASE}\u{7}")}),
    ));
    assert_eq!(r.continue_from.as_deref(), Some(BASE));
    // A member that cleans to nothing is no member.
    let blank = RunRow::from_summary(&summary(json!({"continueFrom": "\u{1b}[0m"})));
    assert_eq!(blank.continue_from, None);
}
