//! The Runs list on the command line: `ham runs` and the `/runs` panel of the terminal app wrote `complete (delivering)` for every
//! finished run, hours after its zip was in the bucket. A finished s3-only run now reads `complete (delivered)`. A run in another status keeps
//! the phase the service stored, and the JSON of the API keeps the stored word.
#![allow(clippy::unwrap_used)]

use harness_protocol::api::v1::RunSummary;
use serde_json::{Value, json};

const ARN: &str = "arn:aws:sts::111122223333:assumed-role/Admin/tester";

fn summary(status: &str, phase: &str) -> RunSummary {
    serde_json::from_value(json!({"runId": "01M6C3D78J6WJM6ADMEPMRMJ38", "status": status, "phase": phase,
        "target": {"mode": "s3-only"}, "createdAt": "2026-11-01T12:00:00Z", "ownerPrincipalArn": ARN}))
    .unwrap()
}

#[test]
fn b730_a_finished_s3_only_run_is_listed_as_delivered() {
    let line = ham::cli::runs_line(&summary("complete", "delivering"));
    assert!(
        line.starts_with(
            "01M6C3D78J6WJM6ADMEPMRMJ38  complete (delivered)  s3-only  2026-11-01T12:00:00Z"
        ),
        "{line}"
    );
    assert!(!line.contains("delivering"), "{line}");
}

#[test]
fn b730_a_run_that_is_not_finished_keeps_the_phase_it_stored() {
    for (status, phase) in [
        ("active", "delivering"),
        ("active", "green"),
        ("blocked", "delivering"),
        ("cleared", "green"),
    ] {
        let line = ham::cli::runs_line(&summary(status, phase));
        assert!(line.contains(&format!("{status} ({phase})")), "{line}");
    }
}

#[test]
fn b730_the_json_of_the_api_keeps_the_stored_phase() {
    let v: Value = serde_json::to_value(summary("complete", "delivering")).unwrap();
    assert_eq!(v["phase"], "delivering");
    assert_eq!(v["status"], "complete");
}
