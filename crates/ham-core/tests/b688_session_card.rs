//! The delivery card names the session a run ended in when it continued in a new session after the first one ended. A run of one session
//! has no `Session` row, and the row never changes the label.

#![allow(clippy::unwrap_used)]

use ham_core::render::{FinalCard, final_card, final_card_shown, session_text};
use ham_core::state::RunState;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use serde_json::json;

fn ev(n: u8, t: EventType, v: serde_json::Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: format!("01J9ZZZZZZ00000000000000{n:02}"),
        run_id: "R1".into(),
        task_id: None,
        agent_id: None,
        seq: u64::from(n),
        ts: "2026-09-25T02:00:00Z".into(),
        event_type: t,
        authority: Authority::Harness,
        payload: Payload::Inline { value: v },
    }
}

fn delivered() -> RunState {
    let mut s = RunState::new("R1");
    s.apply(ev(1, EventType::RunCreated, json!({})));
    s.apply(ev(
        13,
        EventType::GoalStatus,
        json!({"status": "paused", "phase": "delivering"}),
    ));
    s.apply(ev(
        14,
        EventType::DeliveryReady,
        json!({"artifactId": "ART1", "sha256": "ab".repeat(32), "sizeBytes": 2048}),
    ));
    s.apply(ev(
        15,
        EventType::RunCompleted,
        json!({"status": "paused", "outcome": "stopped_timeout", "label": "Cline stopped: the run reached its time limit"}),
    ));
    s
}

fn card(session: Option<u32>) -> FinalCard {
    FinalCard {
        local_path: "/downloads/r1.zip".into(),
        session,
        ..FinalCard::default()
    }
}

#[test]
fn b688_a_run_that_continued_in_a_new_session_names_the_session_it_ended_in() {
    let v = delivered().view();
    for text in [
        final_card(&v, &card(Some(3))),
        final_card_shown(&v, &card(Some(3))),
    ] {
        assert!(
            text.contains("\nSession   3 (the run continued in a new session 2 times)\n"),
            "{text}"
        );
        // the row sits after the model rows and before the pull request, and the label stays as the harness wrote it
        let (outcome, session, pr) = (
            text.find("\nOutcome ").unwrap(),
            text.find("\nSession ").unwrap(),
            text.find("\nPR ").unwrap(),
        );
        assert!(outcome < session && session < pr, "{text}");
        assert!(
            text.contains("Outcome   Cline stopped: the run reached its time limit"),
            "{text}"
        );
    }
}

#[test]
fn b688_a_run_of_one_session_and_a_detail_that_names_none_show_no_session_row() {
    let v = delivered().view();
    for session in [None, Some(0), Some(1)] {
        let text = final_card(&v, &card(session));
        assert!(!text.contains("Session"), "{session:?}: {text}");
    }
}

#[test]
fn b688_the_session_words_count_the_renewals() {
    assert_eq!(
        session_text(2),
        "2 (the run continued in a new session 1 time)"
    );
    assert_eq!(
        session_text(5),
        "5 (the run continued in a new session 4 times)"
    );
}
