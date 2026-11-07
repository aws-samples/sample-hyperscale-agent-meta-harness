//! The flow a run declared on the delivery card: one `Flow` row with the word `quick`, `full` or `none`, read by the client from
//! `evidence/direct.json` of the delivered zip (`summary.flow`). The row is a fact beside the label: it changes no label, outcome or other row.

#![allow(clippy::unwrap_used)]

use ham_core::head::{FLOW_POINTER, flow_in_evidence};
use ham_core::render::{FLOW_LABEL, FinalCard, TESTS_LABEL, final_card, final_card_shown};
use ham_core::state::RunState;
use harness_protocol::direct::DirectFlow;
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
        ts: "2026-10-27T02:00:00Z".into(),
        event_type: t,
        authority: Authority::Harness,
        payload: Payload::Inline { value: v },
    }
}

fn done() -> RunState {
    let mut s = RunState::new("R1");
    s.apply(ev(1, EventType::RunCreated, json!({})));
    s.apply(ev(
        2,
        EventType::SessionStarted,
        json!({"agentId": "direct", "engine": "cline", "modelId": "global.anthropic.claude-sonnet-5-5"}),
    ));
    s.apply(ev(
        14,
        EventType::DeliveryReady,
        json!({"artifactId": "ART1", "sha256": "ab".repeat(32), "sizeBytes": 2048}),
    ));
    s.apply(ev(
        15,
        EventType::RunCompleted,
        json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
    ));
    s
}

fn card() -> FinalCard {
    FinalCard {
        local_path: "/downloads/calc.zip".into(),
        tests: Some("repository tests pass in a clean checkout: 12 of 12.".into()),
        ..FinalCard::default()
    }
}

#[test]
fn the_label_is_a_fixed_ten_character_column_like_its_neighbours() {
    assert_eq!(FLOW_LABEL, "Flow      ");
    assert_eq!(FLOW_LABEL.len(), TESTS_LABEL.len());
}

#[test]
fn a_card_with_a_flow_has_one_flow_row_after_the_tests_row() {
    for word in ["quick", "full", "none"] {
        let c = FinalCard {
            flow: Some(word.into()),
            ..card()
        };
        for text in [
            final_card(&done().view(), &c),
            final_card_shown(&done().view(), &c),
        ] {
            let row = format!("\nFlow      {word}");
            assert_eq!(text.matches("\nFlow ").count(), 1, "{text}");
            assert!(text.ends_with(&row), "{text}");
            assert!(
                text.find("\nTests ").unwrap() < text.find("\nFlow ").unwrap(),
                "{text}"
            );
            // The label and the rows around it are untouched.
            assert!(text.contains("\nOutcome   Cline reported done\n"), "{text}");
        }
    }
}

#[test]
fn a_card_without_a_flow_is_the_card_it_was() {
    let with_none = final_card(&done().view(), &card());
    assert!(!with_none.contains("Flow"), "{with_none}");
    let blank = FinalCard {
        flow: Some("  ".into()),
        ..card()
    };
    assert_eq!(final_card(&done().view(), &blank), with_none);
    // Only the one line differs from the same card with a flow.
    let c = FinalCard {
        flow: Some("quick".into()),
        ..card()
    };
    let text = final_card(&done().view(), &c);
    assert_eq!(
        text.strip_suffix("\nFlow      quick").unwrap(),
        with_none.as_str()
    );
}

// ---- the place in evidence/direct.json ----------------------------------------------------------------------------------------

#[test]
fn the_flow_is_read_from_the_summary_of_the_evidence_file() {
    assert_eq!(FLOW_POINTER, "/summary/flow");
    for (word, flow) in [
        ("quick", DirectFlow::Quick),
        ("full", DirectFlow::Full),
        ("none", DirectFlow::Undeclared),
    ] {
        let v = json!({"schemaVersion": 1, "summary": {"flow": word}});
        assert_eq!(flow_in_evidence(&v), Some(flow), "{word}");
    }
}

#[test]
fn a_file_with_no_flow_or_one_this_build_does_not_know_gives_none() {
    for v in [
        json!({}),
        json!({"summary": null}),
        json!({"summary": {}}),
        json!({"summary": {"flow": null}}),
        json!({"summary": {"flow": 3}}),
        json!({"summary": {"flow": "light"}}),
        json!({"summary": {"flow": "Quick"}}),
        // The envelope has no flow of its own: only the summary's counts.
        json!({"flow": "quick"}),
    ] {
        assert_eq!(flow_in_evidence(&v), None, "{v}");
    }
}
