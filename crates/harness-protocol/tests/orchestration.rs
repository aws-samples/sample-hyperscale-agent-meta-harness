//! Orchestration contracts: gate reports and the
//! worker's redacted view of them, evaluator verdicts, harness event payloads (the fields `ham`
//! reads), Step Functions task inputs and the repository profile.
#![allow(clippy::unwrap_used)]

use harness_protocol::goal::GoalPhase;
use serde_json::json;

#[test]
fn goal_phases_are_the_fr_13_set() {
    let phases: Vec<GoalPhase> = serde_json::from_value(json!([
        "planning",
        "red",
        "green",
        "refactor",
        "gating",
        "evaluating",
        "integrating",
        "delivering"
    ]))
    .unwrap();
    assert_eq!(phases.len(), 8);
    assert!(serde_json::from_value::<GoalPhase>(json!("working")).is_err());
}
