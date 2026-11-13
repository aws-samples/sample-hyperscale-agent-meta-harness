//! The research lines of the run screen: agentd writes `turn.progress` events of kind `research`. At startup, when the user
//! switched research off for the run, one event has `phase: "off"` and a sentence; when the run made research calls, one event at the end carries
//! the counts. Each is one plain line that says what happened and never judges the work.

#![allow(clippy::unwrap_used)]

use ham_core::notice::{NoticeKind, notice_of};
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

fn research(n: u32, extra: Value) -> Envelope {
    let mut v = json!({"turn": 1, "kind": "research"});
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": format!("01M5A2R7K3C9XJ4D8E6F{n:06}"), "runId": "01M4ZHFBV5SDX7J92E6YRH4QTW", "agentId": "direct", "seq": n,
        "ts": "2026-10-16T08:15:00Z", "type": "turn.progress", "authority": "agent",
        "payload": {"kind": "inline", "value": v}
    }))
    .unwrap()
}

#[test]
fn the_users_switch_is_one_plain_line() {
    let n = notice_of(&research(
        1,
        json!({"phase": "off", "text": "Research off: you switched it off for this run."}),
    ))
    .unwrap();
    assert_eq!(n.kind, NoticeKind::Notify);
    assert_eq!(n.text, "Research off: you switched it off for this run.");
    println!("RESEARCH-NOTICE TRIGGER PRESENT: phase=off -> {}", n.text);
}

#[test]
fn the_counts_at_the_end_are_a_line_built_from_the_numbers_only() {
    let n = notice_of(&research(
        2,
        json!({"searches": 3, "fetches": 2, "repos": 1, "placed": 3, "refused": 0, "errors": 1, "placedBytes": 52000}),
    ))
    .unwrap();
    assert_eq!(n.kind, NoticeKind::Notify);
    assert_eq!(
        n.text,
        "Research: 3 searches, 2 page fetches, 1 repository download, 3 items stored, 1 error."
    );
    let one = notice_of(&research(
        3,
        json!({"searches": 1, "fetches": 0, "repos": 0, "placed": 0, "refused": 0, "errors": 0, "placedBytes": 0}),
    ))
    .unwrap();
    assert_eq!(
        one.text,
        "Research: 1 search, 0 page fetches, 0 repository downloads, 0 items stored, 0 errors."
    );
}

#[test]
fn a_hostile_sentence_is_made_safe_and_no_line_claims_the_work_was_checked() {
    let n = notice_of(&research(
        4,
        json!({"phase": "off", "text": "Research off: \u{1b}[31mred\u{7} you switched it off."}),
    ))
    .unwrap();
    assert!(n.text.chars().all(|c| !c.is_control()), "{:?}", n.text);
    for line in [
        &n.text,
        "Research: 3 searches, 2 page fetches, 1 repository download, 3 items stored, 1 error.",
    ] {
        let lower = line.to_lowercase();
        for w in ["verified", "tested", "gated", "evaluated"] {
            assert!(!lower.contains(w), "{line}");
        }
    }
}

#[test]
fn an_event_of_kind_research_with_neither_phase_nor_counts_is_still_a_line() {
    let n = notice_of(&research(5, json!({}))).unwrap();
    assert_eq!(n.kind, NoticeKind::Notify);
    assert_eq!(n.text, "Research.");
}
