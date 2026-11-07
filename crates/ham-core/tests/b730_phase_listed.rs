//! The phase of a finished run on the command line. The service writes a run's phase `delivering` once and moves the status to
//! `complete` without writing a phase again, so `ham runs`, `ham status`, `ham watch` and the card the command prints said
//! `complete (delivering)` hours after the zip was in the bucket. A `complete` run now reads `complete (delivered)` there; a run in any other
//! status keeps the phase as stored, and the JSON keeps the stored word.
#![allow(clippy::unwrap_used)]

use ham_core::render::{FinalCard, final_card, phase_listed, status_line};
use ham_core::state::{RunState, RunView};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use harness_protocol::goal::{GoalPhase, GoalStatus};
use serde_json::{Value, json};

fn harness(n: u8, t: EventType, v: Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: format!("01J9ZZZZZZ00000000000000{n:02}"),
        run_id: "R1".into(),
        task_id: None,
        agent_id: None,
        seq: u64::from(n),
        ts: "2026-11-01T12:00:00Z".into(),
        event_type: t,
        authority: Authority::Harness,
        payload: Payload::Inline { value: v },
    }
}

/// An s3-only run whose last stored status and phase are `status` and `phase`, with its zip delivered.
fn run_with(status: &str, phase: &str) -> RunView {
    let mut s = RunState::new("R1");
    s.apply(harness(1, EventType::RunCreated, json!({})));
    s.apply(harness(
        2,
        EventType::GoalStatus,
        json!({"status": status, "phase": phase}),
    ));
    s.apply(harness(
        3,
        EventType::DeliveryReady,
        json!({"artifactId": "zip-1", "sha256": "0123456789abcdef0123456789abcdef", "sizeBytes": 3000}),
    ));
    s.view()
}

fn card() -> FinalCard {
    FinalCard {
        local_path: "/downloads/s3-only-R1-abc1234.zip".into(),
        presigned_url: None,
        expires_at: None,
        usage: None,
        tests: None,
        flow: None,
        label_missing_after: None,
        session: None,
    }
}

#[test]
fn b730_a_complete_run_with_the_stored_phase_delivering_is_listed_as_delivered() {
    assert_eq!(
        phase_listed(Some(GoalStatus::Complete), Some(GoalPhase::Delivering)),
        "delivered"
    );
}

#[test]
fn b730_every_other_pair_keeps_the_phase_as_the_service_stored_it() {
    for (status, phase, want) in [
        (GoalStatus::Blocked, GoalPhase::Delivering, "delivering"),
        (GoalStatus::Active, GoalPhase::Delivering, "delivering"),
        (GoalStatus::Active, GoalPhase::Green, "green"),
        (GoalStatus::Cleared, GoalPhase::Green, "green"),
        (GoalStatus::Complete, GoalPhase::Green, "green"),
    ] {
        assert_eq!(
            phase_listed(Some(status), Some(phase)),
            want,
            "{status:?} {phase:?}"
        );
    }
    assert_eq!(phase_listed(None, None), "-");
    assert_eq!(phase_listed(Some(GoalStatus::Complete), None), "-");
}

#[test]
fn b730_the_status_line_of_a_finished_s3_only_run_says_delivered() {
    let line = status_line(&run_with("complete", "delivering"));
    assert!(
        line.starts_with("run R1 complete (delivered) agents"),
        "{line}"
    );
    assert!(!line.contains("delivering"), "{line}");
    // A run that is still delivering says so.
    let going = status_line(&run_with("active", "delivering"));
    assert!(
        going.starts_with("run R1 active (delivering) agents"),
        "{going}"
    );
}

#[test]
fn b730_the_card_the_command_prints_says_delivered_and_keeps_the_zip_line() {
    let text = final_card(&run_with("complete", "delivering"), &card());
    assert!(text.contains("Run       R1 complete (delivered)"), "{text}");
    assert!(!text.contains("(delivering)"), "{text}");
    assert!(
        text.contains("Zip       /downloads/s3-only-R1-abc1234.zip"),
        "{text}"
    );
    // A blocked run keeps the phase where it stopped.
    let blocked = final_card(&run_with("blocked", "delivering"), &card());
    assert!(
        blocked.contains("Run       R1 blocked (delivering)"),
        "{blocked}"
    );
}

#[test]
fn b730_the_stored_value_is_not_changed_by_the_listing() {
    let v = run_with("complete", "delivering");
    assert_eq!(v.phase, Some(GoalPhase::Delivering));
    assert_eq!(v.status, Some(GoalStatus::Complete));
}
