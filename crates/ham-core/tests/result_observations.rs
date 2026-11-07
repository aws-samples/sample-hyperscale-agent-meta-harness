//! The facts the harness read at the end of a run, beside the label, and the card of the brake inside one round. The brake rule
//! `round-no-progress` has plain words of its own.
#![allow(clippy::unwrap_used)]

use ham_core::goal_card::from_events;
use ham_core::render::{FinalCard, final_card};
use ham_core::state::{RunState, RunView};
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const RED: &str = "The last entry of the run log is a red run: pytest exited 1 and 2 failed.";

fn ev(n: u32, ty: &str, agent: Option<&str>, payload: Value) -> Envelope {
    let authority = if agent.is_some() { "agent" } else { "harness" };
    let mut v = json!({
        "schemaVersion": 1, "eventId": format!("01M5A2R7K3C9XJ4D8E6F{n:06}"), "runId": RUN, "seq": n,
        "ts": "2026-10-16T08:15:00Z", "type": ty, "authority": authority,
        "payload": {"kind": "inline", "value": payload}
    });
    if let Some(a) = agent {
        v["agentId"] = json!(a);
    }
    serde_json::from_value(v).unwrap()
}

fn view(events: Vec<Envelope>) -> RunView {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    s.view()
}

fn completed(n: u32, extra: Value) -> Envelope {
    let mut p =
        json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"});
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        p[k] = x;
    }
    ev(n, "run.completed", None, p)
}

fn red() -> Value {
    json!({"kind": "last_log_entry_red", "text": RED, "command": "pytest", "exit": "1", "failed": "2"})
}

// ---- the reducer -----------------------------------------------------------------------------------------------------------

#[test]
fn run_completed_observations_reach_the_view_as_written() {
    let v = view(vec![completed(1, json!({"observations": [red()]}))]);
    assert_eq!(v.observations.len(), 1);
    assert_eq!(v.observations[0].text, RED);
    assert!(
        v.observations[0].warns(),
        "a red last log entry wears the warning marker"
    );
    // The label is the one the event carries.
    assert_eq!(v.result.as_ref().unwrap().label, "Cline reported done");
}

#[test]
fn a_run_with_no_observation_has_none() {
    assert!(view(vec![completed(1, json!({}))]).observations.is_empty());
    assert!(
        view(vec![completed(1, json!({"observations": []}))])
            .observations
            .is_empty()
    );
}

#[test]
fn an_unknown_kind_is_kept_as_a_plain_line_and_an_entry_without_text_is_dropped() {
    let v = view(vec![completed(
        1,
        json!({"observations": [
            {"kind": "something_new", "text": "A newer writer's fact."},
            {"kind": "last_log_entry_red"},
            {"kind": "last_log_entry_red", "text": "   "},
            {"text": "No kind at all."}
        ]}),
    )]);
    let texts: Vec<&str> = v.observations.iter().map(|o| o.text.as_str()).collect();
    assert_eq!(texts, ["A newer writer's fact.", "No kind at all."]);
    assert!(v.observations.iter().all(|o| !o.warns()));
}

#[test]
fn at_most_four_observations_are_kept_in_order_and_their_text_is_cleaned() {
    let list: Vec<Value> = (0..6)
        .map(|i| json!({"kind": "last_log_entry_red", "text": format!("fact {i}\u{1b}[31m red\u{7}")}))
        .collect();
    let v = view(vec![completed(1, json!({ "observations": list }))]);
    assert_eq!(v.observations.len(), 4);
    assert_eq!(v.observations[0].text, "fact 0 red");
    assert_eq!(v.observations[3].text, "fact 3 red");
}

#[test]
fn a_later_run_completed_without_observations_does_not_erase_them() {
    let v = view(vec![
        completed(1, json!({"observations": [red()]})),
        completed(2, json!({})),
    ]);
    assert_eq!(v.observations.len(), 1);
}

// ---- the final card (`ham pull`, `ham watch`) ----------------------------------------------------------------------------------

#[test]
fn the_final_card_prints_each_observation_directly_under_the_outcome_line() {
    let v = view(vec![completed(
        1,
        json!({"detail": "hello.py printed 55", "observations": [red(), {"kind": "something_new", "text": "Another fact."}]}),
    )]);
    let text = final_card(&v, &FinalCard::default());
    let lines: Vec<&str> = text.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.starts_with("Outcome "))
        .unwrap();
    assert_eq!(lines[at], "Outcome   Cline reported done");
    assert_eq!(lines[at + 1], format!("Observed  {RED}"));
    assert_eq!(lines[at + 2], "Observed  Another fact.");
    assert!(lines[at + 3].starts_with("Detail "), "{text}");
}

#[test]
fn a_card_without_observations_is_the_card_it_was() {
    let v = view(vec![completed(1, json!({}))]);
    assert!(!final_card(&v, &FinalCard::default()).contains("Observed"));
}

#[test]
fn no_observation_line_names_the_work_as_checked() {
    let v = view(vec![completed(1, json!({"observations": [red()]}))]);
    let text = final_card(&v, &FinalCard::default()).to_lowercase();
    for w in ["verified", "tested", "gated", "evaluated"] {
        assert!(!text.contains(w), "{w} in {text}");
    }
}

// ---- the brake inside one round ----------------------------------------------------------------------------------------

fn brake(rule: &str, text: &str) -> Vec<Envelope> {
    vec![
        ev(
            1,
            "turn.progress",
            Some("direct"),
            json!({"turn": 1, "kind": "goal-loop", "phase": "round", "rounds": 1, "status": "completed"}),
        ),
        ev(
            2,
            "turn.progress",
            Some("direct"),
            json!({"turn": 1, "kind": "goal-loop", "phase": "brake", "rule": rule, "text": text}),
        ),
    ]
}

#[test]
fn the_round_no_progress_brake_has_plain_words_for_both_of_its_settings() {
    for text in [
        "The harness ended the run: Cline made no progress in 30 tool calls in a row: no file changed and no new text appeared.",
        "The harness ended the run: Cline made no progress in 12 tool calls over 20 minutes: no file changed.",
    ] {
        let card = from_events(brake("round-no-progress", text).iter()).unwrap();
        assert_eq!(card.rule, "round-no-progress");
        assert!(
            !card.explain().contains("no words for"),
            "a rule the client knows: {}",
            card.explain()
        );
        assert_eq!(
            card.explain(),
            "Cline kept calling tools inside one round, but no file changed and no new text appeared, so the harness ended the run."
        );
        let lines = card.lines(RUN, 100).join("\n");
        assert!(
            lines.contains("The harness ended the run: Cline made no progress in"),
            "{lines}"
        );
        assert!(lines.contains("kept calling tools"), "{lines}");
        assert!(lines.contains(&format!("ham continue {RUN} ")), "{lines}");
        let low = lines.to_lowercase();
        for w in ["verified", "tested", "gated", "evaluated"] {
            assert!(!low.contains(w), "{w} in {lines}");
        }
    }
}
