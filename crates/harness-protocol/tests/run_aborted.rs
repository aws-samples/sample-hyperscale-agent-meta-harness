//! `run.aborted` and the Run item's `aborted`: a run whose `run` execution ended ABORTED, FAILED
//! or TIMED_OUT without the run finishing is cleared by the harness, and the event, the Run item
//! and `GET /v1/runs` carry the execution's status, error and cause, so the TUI and `ham runs`
//! show why the run ended.

use harness_protocol::api::v1::RunSummary;
use harness_protocol::envelope::{
    Authority, Envelope, EventType, MAX_INLINE_PAYLOAD_BYTES, Payload, SCHEMA_VERSION, validate,
};
use harness_protocol::orchestration::{
    ExecutionEnd, MAX_ABORT_CAUSE_BYTES, MAX_ABORT_ERROR_BYTES, RunAbortedPayload,
};
use harness_protocol::store::RunItem;
use serde_json::json;

const RUN: &str = "01M3QA3YCXGFW428XNGR7R8VWD";

fn live_payload() -> RunAbortedPayload {
    RunAbortedPayload::new(
        ExecutionEnd::Aborted,
        Some("HarnessOperatorStop"),
        Some("operator: the new-repo leg already recorded FAIL (watch 7200 s)"),
        "2026-09-29T21:30:26Z",
    )
}

#[test]
fn the_payload_carries_the_step_functions_status_error_and_cause() {
    let v = serde_json::to_value(live_payload()).unwrap();
    assert_eq!(
        v,
        json!({
            "status": "ABORTED",
            "error": "HarnessOperatorStop",
            "cause": "operator: the new-repo leg already recorded FAIL (watch 7200 s)",
            "stoppedAt": "2026-09-29T21:30:26Z",
        })
    );
    let back: RunAbortedPayload = serde_json::from_value(v).unwrap();
    assert_eq!(back, live_payload());
    // An execution that failed without an error or cause omits both.
    let bare = RunAbortedPayload::new(ExecutionEnd::TimedOut, None, None, "2026-09-29T21:30:26Z");
    assert_eq!(
        serde_json::to_value(bare).unwrap(),
        json!({"status": "TIMED_OUT", "stoppedAt": "2026-09-29T21:30:26Z"})
    );
}

#[test]
fn execution_end_is_one_of_the_three_ends_that_leave_a_run_unfinished() {
    for (word, end) in [
        ("ABORTED", ExecutionEnd::Aborted),
        ("FAILED", ExecutionEnd::Failed),
        ("TIMED_OUT", ExecutionEnd::TimedOut),
    ] {
        assert_eq!(ExecutionEnd::from_status(word), Some(end));
        assert_eq!(end.as_str(), word);
        assert_eq!(serde_json::to_value(end).unwrap(), json!(word));
    }
    // A run that succeeded ended through MarkComplete or RunBlocked; RUNNING and PENDING_REDRIVE
    // have not ended.
    for word in ["SUCCEEDED", "RUNNING", "PENDING_REDRIVE", "aborted", ""] {
        assert_eq!(ExecutionEnd::from_status(word), None, "{word}");
    }
}

#[test]
fn error_and_cause_are_bounded_single_line_text_that_fits_an_inline_event() {
    // StopExecution takes a cause of up to 32,768 characters; the event holds at most 4,096 bytes.
    let cause = format!("{}\n\u{7}{}", "é".repeat(700), "\"\\".repeat(2000));
    let error = "E".repeat(400);
    let p = RunAbortedPayload::new(
        ExecutionEnd::Failed,
        Some(&error),
        Some(&cause),
        "2026-09-29T21:30:26Z",
    );
    let c = p.cause.clone().unwrap();
    let e = p.error.clone().unwrap();
    assert!(c.len() <= MAX_ABORT_CAUSE_BYTES, "{}", c.len());
    assert!(e.len() <= MAX_ABORT_ERROR_BYTES, "{}", e.len());
    assert_eq!(MAX_ABORT_CAUSE_BYTES, 1024);
    assert_eq!(MAX_ABORT_ERROR_BYTES, 256);
    // Cut on a character boundary, control characters replaced by spaces.
    assert!(c.starts_with(&"é".repeat(512)), "{c}");
    assert!(!c.chars().any(char::is_control), "{c:?}");
    assert!(!e.chars().any(char::is_control));
    let envelope = Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: "01M3QA3YCXGFW428XNGR7R8VWE".into(),
        run_id: RUN.into(),
        task_id: None,
        agent_id: None,
        seq: 0,
        ts: "2026-09-29T21:30:27Z".into(),
        event_type: EventType::RunAborted,
        authority: Authority::Harness,
        payload: Payload::Inline {
            value: serde_json::to_value(&p).unwrap(),
        },
    };
    validate(&envelope).unwrap();
    let bytes = serde_json::to_vec(&serde_json::to_value(&p).unwrap()).unwrap();
    assert!(bytes.len() <= MAX_INLINE_PAYLOAD_BYTES, "{}", bytes.len());
}

#[test]
fn run_aborted_is_a_harness_event_that_changes_goal_status() {
    assert_eq!(EventType::RunAborted.as_str(), "run.aborted");
    assert_eq!(
        serde_json::to_value(EventType::RunAborted).unwrap(),
        json!("run.aborted")
    );
    assert_eq!(
        EventType::RunAborted.required_authority(),
        Authority::Harness
    );
    assert!(EventType::RunAborted.changes_goal_status());
    assert!(EventType::ALL.contains(&EventType::RunAborted));
}

fn run_json() -> serde_json::Value {
    json!({
        "runId": RUN, "ownerPrincipalArn": "arn:aws:sts::800000000022:assumed-role/Admin/testuser",
        "promptSha256": "cd".repeat(32), "target": {"mode": "s3-only"}, "status": "cleared",
        "phase": "green", "enginePolicyVersion": 1, "createdAt": "2026-09-29T19:26:27Z",
        "version": 2
    })
}

#[test]
fn the_run_item_and_the_run_summary_carry_why_the_run_ended() {
    let mut v = run_json();
    v["aborted"] = serde_json::to_value(live_payload()).unwrap();
    let r: RunItem = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(r.aborted.as_deref(), Some(&live_payload()));
    assert_eq!(serde_json::to_value(&r).unwrap(), v);
    // A Run item written before the field exists reads as not aborted and writes no field.
    let old: RunItem = serde_json::from_value(run_json()).unwrap();
    assert_eq!(old.aborted, None);
    assert!(serde_json::to_value(&old).unwrap().get("aborted").is_none());

    let s: RunSummary = serde_json::from_value(json!({
        "runId": RUN, "status": "cleared", "phase": "green", "target": {"mode": "s3-only"},
        "createdAt": "2026-09-29T19:26:27Z",
        "ownerPrincipalArn": "arn:aws:sts::800000000022:assumed-role/Admin/testuser",
        "aborted": serde_json::to_value(live_payload()).unwrap(),
    }))
    .unwrap();
    assert_eq!(s.aborted.as_deref(), Some(&live_payload()));
}
