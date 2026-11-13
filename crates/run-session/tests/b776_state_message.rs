//! Cline's final message stays whole in the evidence file (an S3 object) and rides in Step Functions state only as a preview inside a
//! byte budget, so a long message can neither be cut to its last 8,000 characters nor push the state over 256 KiB.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::direct::{DirectOutcome, MAX_STATE_MESSAGE_BYTES, json_len, s3};
use harness_protocol::target::RunTarget;
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

async fn plan(mem: &Mem, session: Value) -> Value {
    let op: Op = serde_json::from_value(
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged(), "session": session}),
    )
    .unwrap();
    handle(&mem.deps(), &op).await.unwrap()
}

fn seeded(target: RunTarget) -> Mem {
    let mem = Mem::default();
    mem.seed(target, "Add percent_change(a, b) with tests", MODEL);
    mem
}

fn long_report(lines: usize) -> String {
    (0..lines)
        .map(|i| format!("finding {i}: the check passed"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn b776_a_long_message_is_whole_in_the_evidence_file_and_a_preview_in_the_package() {
    let mem = seeded(repo_target());
    mem.put_bundle();
    let long = long_report(5_000);
    assert!(json_len(&long) > 4 * MAX_STATE_MESSAGE_BYTES);
    mem.put_summary(&summary(DirectOutcome::ReportedDone, Some(&long), true));
    let p = plan(&mem, json!({"turn": turn_output(true), "failure": {}})).await;
    assert_eq!(p["deliver"], true);

    let written: Value =
        serde_json::from_slice(&mem.blob(&s3::evidence_key(RUN)).unwrap()).unwrap();
    assert_eq!(
        ham_core::head::cline_message_in_evidence(&written),
        Some(long.as_str()),
        "the S3 evidence file holds the whole message"
    );

    let preview = p["package"]["direct"]["clineMessage"].as_str().unwrap();
    assert!(
        json_len(preview) <= MAX_STATE_MESSAGE_BYTES,
        "{}",
        json_len(preview)
    );
    assert!(preview.starts_with("finding 0: the check passed"));
    assert!(
        preview.ends_with("finding 4999: the check passed"),
        "the end is kept"
    );
    assert!(
        preview.contains("characters omitted (the whole text is in evidence/direct.json)"),
        "{}",
        &preview[preview.len() / 4..preview.len() / 4 + 200]
    );
}

#[tokio::test]
async fn b776_a_message_inside_the_budget_is_passed_on_unchanged() {
    let mem = seeded(repo_target());
    mem.put_bundle();
    let message = long_report(100);
    assert!(json_len(&message) < MAX_STATE_MESSAGE_BYTES);
    mem.put_summary(&summary(DirectOutcome::ReportedDone, Some(&message), true));
    let p = plan(&mem, json!({"turn": turn_output(true), "failure": {}})).await;
    assert_eq!(p["package"]["direct"]["clineMessage"], message);
}

#[tokio::test]
async fn b776_the_planned_delivery_with_the_longest_message_leaves_room_in_the_state() {
    let mem = seeded(repo_target());
    mem.put_bundle();
    // Quotes and new lines take two bytes each in JSON; emoji take four. The preview is held to the budget whatever the text is.
    for unit in ["\"", "\n", "\u{1F680}", "a"] {
        let long = unit.repeat(150_000);
        mem.put_summary(&summary(DirectOutcome::ReportedDone, Some(&long), true));
        let p = plan(&mem, json!({"turn": turn_output(true), "failure": {}})).await;
        let preview = p["package"]["direct"]["clineMessage"].as_str().unwrap();
        assert!(json_len(preview) <= MAX_STATE_MESSAGE_BYTES, "{unit:?}");
        // The whole answer is the largest thing the plan adds to the state. The turn result holds a second copy of at most the same size.
        let plan_bytes = serde_json::to_vec(&p).unwrap().len();
        assert!(
            plan_bytes + MAX_STATE_MESSAGE_BYTES * 2 < 192 * 1024,
            "{unit:?}: the plan is {plan_bytes} bytes"
        );
    }
}
