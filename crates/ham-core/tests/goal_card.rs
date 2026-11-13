//! The card of a goal-loop brake:
//! when agentd's goal module ends a run with one of its stopping rules, the
//! run view carries a card that says which rule, how far the loop got, Cline's own `BLOCKED:` line when there is one, and the command that
//! starts a follow-up. The card is built from the `goal-loop` events alone (`rounds`, `status`, `rule`, `text`); it never calls the work
//! verified, tested, gated or evaluated, and the label of the run stays the one of `run.completed`.

#![allow(clippy::unwrap_used)]

use ham_core::state::{RunState, RunView};
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";

fn id(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn event(n: u32, ty: &str, value: Value, authority: &str) -> Envelope {
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": id(n), "runId": RUN, "agentId": "direct", "seq": n, "ts": "2026-10-16T08:15:00Z",
        "type": ty, "authority": authority, "payload": {"kind": "inline", "value": value}
    }))
    .unwrap()
}

fn goal(n: u32, phase: &str, extra: Value) -> Envelope {
    let mut v = json!({"turn": 1, "kind": "goal-loop", "phase": phase});
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    event(n, "turn.progress", v, "agent")
}

fn round(n: u32, rounds: u64, status: &str) -> Envelope {
    goal(n, "round", json!({"rounds": rounds, "status": status}))
}

fn brake(n: u32, rule: &str, text: &str) -> Envelope {
    goal(n, "brake", json!({"rule": rule, "text": text}))
}

fn view_of(events: Vec<Envelope>) -> RunView {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    s.view()
}

const BLOCKED_TEXT: &str = "The harness ended the run: Cline ended 2 rounds in a row with the same last line. Its words: \"BLOCKED: the API key is missing\"";
const RULES: [(&str, &str); 7] = [
    ("blocked-twice", BLOCKED_TEXT),
    (
        "text-only-stall",
        "The harness ended the run: 3 rounds in a row had no tool call and changed no file.",
    ),
    (
        "no-progress",
        "The harness ended the run: 5 rounds in a row changed no file.",
    ),
    (
        "dead-loop",
        "The harness ended the run: the goal loop stopped 2 times in a row with no change to the files in between (the last round ended failed).",
    ),
    (
        "unfinished-claim",
        "The harness ended the run: Cline called the goal-complete tool, but its round ended aborted.",
    ),
    (
        "goal-cleared",
        "The harness ended the run: the goal is no longer in the state file and Cline never completed it.",
    ),
    (
        "round-no-progress",
        "The harness ended the run: Cline made no progress in 30 tool calls in a row: no file changed and no new text appeared.",
    ),
];

const BANNED: [&str; 4] = ["verified", "tested", "gated", "evaluated"];

fn no_claim(text: &str) {
    let lower = text.to_lowercase();
    for w in BANNED {
        assert!(!lower.contains(w), "{text:?} holds {w:?}");
    }
}

#[test]
fn a_run_without_a_brake_has_no_card() {
    let v = view_of(vec![
        goal(1, "on", json!({})),
        round(2, 1, "completed"),
        goal(3, "done", json!({"summary": "Added the function."})),
    ]);
    assert!(v.goal_brake.is_none());
}

#[test]
fn a_blocked_twice_brake_gives_the_rule_the_rounds_and_the_engines_own_line() {
    let v = view_of(vec![
        goal(1, "on", json!({})),
        round(2, 1, "completed"),
        round(3, 3, "completed"),
        brake(4, "blocked-twice", BLOCKED_TEXT),
    ]);
    let c = v.goal_brake.expect("a card");
    assert_eq!(c.request_id, id(4));
    assert_eq!(c.rule, "blocked-twice");
    assert_eq!(
        (c.rounds, c.status.as_deref()),
        (Some(3), Some("completed"))
    );
    assert_eq!(
        c.headline,
        "The harness ended the run: Cline ended 2 rounds in a row with the same last line."
    );
    assert_eq!(
        c.blocked_line.as_deref(),
        Some("BLOCKED: the API key is missing")
    );
    let lines = c.lines(&v.run_id, 76).join("\n");
    assert!(lines.contains("3 rounds"), "{lines}");
    assert!(lines.contains("BLOCKED: the API key is missing"), "{lines}");
    assert!(lines.contains(&format!("ham continue {RUN}")), "{lines}");
    assert!(lines.contains("Cline decides"), "{lines}");
}

#[test]
fn every_rule_has_a_card_with_its_plain_words_and_no_claim_about_the_work() {
    for (rule, text) in RULES {
        let v = view_of(vec![round(1, 4, "failed"), brake(2, rule, text)]);
        let c = v.goal_brake.unwrap_or_else(|| panic!("no card for {rule}"));
        assert_eq!(c.rule, rule);
        assert!(
            c.headline.starts_with("The harness ended the run:"),
            "{rule}: {}",
            c.headline
        );
        assert_eq!(c.blocked_line.is_some(), rule == "blocked-twice", "{rule}");
        let words = c.explain();
        assert!(
            !words.is_empty() && !words.contains('\n'),
            "{rule}: {words:?}"
        );
        let all = format!("{}\n{}", c.title(), c.lines(RUN, 76).join("\n"));
        no_claim(&all);
        println!(
            "GOAL-CARD TRIGGER PRESENT: rule={rule} rounds=4 status=failed card-lines={}",
            c.lines(RUN, 76).len()
        );
    }
}

/// The words `Rule::word` returns in `crates/agentd/src/direct/goal/machine.rs`, in the order of its arms (read from the source; nothing is
/// built from agentd).
fn agentd_rule_words() -> Vec<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../agentd/src/direct/goal/machine.rs");
    let text = std::fs::read_to_string(&path).unwrap();
    let body = text
        .split("pub fn word(self)")
        .nth(1)
        .and_then(|rest| rest.split("\n    }\n").next())
        .expect("the function `Rule::word` is in machine.rs");
    body.split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// The table lists seven rules, and the client has words for every one of them. A rule the goal module can write that is missing
/// here, or whose sentence is the fallback, fails this test.
#[test]
fn the_table_lists_all_seven_rules_and_the_client_has_its_own_words_for_each() {
    let rules: Vec<&str> = RULES.iter().map(|(r, _)| *r).collect();
    assert_eq!(
        rules,
        [
            "blocked-twice",
            "text-only-stall",
            "no-progress",
            "dead-loop",
            "unfinished-claim",
            "goal-cleared",
            "round-no-progress",
        ],
        "the seven stopping rules of the goal module, in the order of its `Rule` enum"
    );
    // The module itself is the source of the list: a rule it learns later and this table lacks fails here, not in a user's terminal.
    assert_eq!(
        rules,
        agentd_rule_words(),
        "agentd's `Rule::word` and this table name the same rules"
    );
    let mut sentences = std::collections::BTreeSet::new();
    for (rule, text) in RULES {
        let v = view_of(vec![brake(1, rule, text)]);
        let words = v.goal_brake.unwrap().explain();
        assert!(
            !words.contains("no words for"),
            "{rule} falls back to the sentence for an unknown rule: {words:?}"
        );
        assert!(
            sentences.insert(words.clone()),
            "{rule} repeats a sentence: {words:?}"
        );
    }
    assert_eq!(sentences.len(), 7);
    // An eighth word the client does not know still gets a plain card, and its sentence says so.
    let v = view_of(vec![brake(1, "some-future-rule", "")]);
    assert!(v.goal_brake.unwrap().explain().contains("no words for"));
}

#[test]
fn the_card_fits_any_width_it_is_given() {
    let v = view_of(vec![
        round(1, 12, "completed"),
        brake(2, "blocked-twice", BLOCKED_TEXT),
    ]);
    let c = v.goal_brake.unwrap();
    for width in [20usize, 40, 76, 120] {
        for l in c.lines(RUN, width) {
            assert!(l.chars().count() <= width, "{width}: {l:?}");
        }
    }
}

#[test]
fn without_round_events_the_card_says_nothing_about_rounds() {
    let v = view_of(vec![brake(1, "dead-loop", RULES[3].1)]);
    let c = v.goal_brake.unwrap();
    assert_eq!((c.rounds, c.status.as_deref()), (None, None));
    let lines = c.lines(RUN, 76).join("\n");
    assert!(!lines.contains("rounds ended"), "{lines}");
}

#[test]
fn a_brake_without_words_or_with_an_unknown_rule_still_makes_a_plain_card() {
    let v = view_of(vec![goal(1, "brake", json!({}))]);
    let c = v.goal_brake.unwrap();
    assert_eq!(c.headline, "The harness ended the run.");
    let v = view_of(vec![brake(1, "something-new", "")]);
    let c = v.goal_brake.unwrap();
    assert!(c.headline.contains("something-new"), "{}", c.headline);
}

#[test]
fn text_from_the_agent_side_is_made_safe_before_it_reaches_a_screen() {
    let hostile = "The harness ended the run: \u{1b}[31mred\u{7} rounds. Its words: \"BLOCKED: \u{1b}]0;title\u{7}stop\"";
    let v = view_of(vec![brake(1, "blocked-twice", hostile)]);
    let c = v.goal_brake.unwrap();
    for l in c.lines(RUN, 76).iter().chain([&c.headline, &c.title()]) {
        assert!(l.chars().all(|ch| !ch.is_control()), "{l:?}");
    }
}

#[test]
fn the_card_stands_beside_a_real_question_and_does_not_replace_it() {
    let q = event(
        1,
        "turn.progress",
        json!({"turn": 1, "kind": "needs-input", "title": "Cline stopped its own prompt", "blocking": true,
               "requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT1N", "n": 1, "reason": "engine_stop", "options": ["finish"],
               "models": [], "recommended": null, "canContinue": true, "waitS": 1800,
               "expiresAt": "2026-10-16T08:45:00Z", "files": [], "filesTotal": 0}),
        "agent",
    );
    let v = view_of(vec![q, brake(2, "no-progress", RULES[2].1)]);
    assert!(
        v.input.open.is_some(),
        "the recovery ladder's question is untouched"
    );
    assert!(v.goal_brake.is_some());
}

#[test]
fn the_label_of_the_run_is_still_the_one_of_run_completed() {
    let done = event(
        3,
        "run.completed",
        json!({"outcome": "stopped_other", "label": "Cline stopped: the harness ended the run (5 rounds in a row changed no file)",
               "detail": "The harness ended the run: 5 rounds in a row changed no file."}),
        "harness",
    );
    let v = view_of(vec![
        round(1, 5, "completed"),
        brake(2, "no-progress", RULES[2].1),
        done,
    ]);
    assert!(v.goal_brake.is_some());
    assert_eq!(
        v.result.unwrap().label,
        "Cline stopped: the harness ended the run (5 rounds in a row changed no file)"
    );
}

#[test]
fn only_the_last_brake_counts() {
    let v = view_of(vec![
        brake(1, "no-progress", RULES[2].1),
        brake(2, "dead-loop", RULES[3].1),
    ]);
    assert_eq!(v.goal_brake.unwrap().rule, "dead-loop");
}
