//! `ham watch` in line mode and the skills of a run: when the run's skills were placed, the card prints once in plain
//! lines (name, short digest, files and bytes), and the `--json` view of the run lists them. A run without skills prints and lists nothing
//! new, so every other view is unchanged.
#![allow(clippy::unwrap_used)]

use ham::watch::{InputMemo, view_json};
use ham_core::state::{RunState, RunView};
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";

fn progress(n: u32, value: Value) -> Envelope {
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": format!("01M5A2R7K3C9XJ4D8E6F{n:06}"), "runId": RUN, "agentId": "direct", "seq": n,
        "ts": "2026-10-17T08:15:00Z", "type": "turn.progress", "authority": "agent",
        "payload": {"kind": "inline", "value": value}
    }))
    .unwrap()
}

fn digest(lead: char) -> String {
    std::iter::repeat_n(lead, 12)
        .chain(std::iter::repeat_n('0', 52))
        .collect()
}

fn view(events: Vec<Envelope>) -> RunView {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    s.view()
}

fn placed() -> RunView {
    view(vec![progress(
        1,
        json!({"turn": 1, "kind": "skills", "preflight": {"ran": true, "ms": 61},
               "skills": [
                   {"name": "lifecycle-tdd", "digest": digest('a'), "files": 1, "bytes": 900},
                   {"name": "my-skill", "digest": digest('b'), "files": 3, "bytes": 2048}]}),
    )])
}

#[test]
fn the_card_prints_once_with_names_short_digests_and_sizes() {
    let mut memo = InputMemo::new();
    let v = placed();
    let first = memo.lines(&v).join("\n");
    assert!(first.contains("Skills placed for this run"), "{first}");
    assert!(
        first.contains("lifecycle-tdd") && first.contains("sha256 aaaaaaaaaaaa"),
        "{first}"
    );
    assert!(
        first.contains("my-skill") && first.contains("3 files, 2,048 bytes"),
        "{first}"
    );
    assert!(!first.contains(&digest('a')), "{first}");
    for w in ["verified", "tested", "gated", "evaluated"] {
        assert!(!first.to_lowercase().contains(w), "{first}");
    }
    assert!(memo.lines(&v).is_empty(), "the card prints once");
    println!("WATCH-SKILLS TRIGGER PRESENT: card printed once, 2 skills with short digests");
}

#[test]
fn a_run_without_skills_prints_nothing_new() {
    let mut memo = InputMemo::new();
    let v = view(vec![progress(
        1,
        json!({"turn": 1, "kind": "research", "searches": 1, "fetches": 0, "repos": 0, "placed": 0, "errors": 0}),
    )]);
    assert!(memo.lines(&v).is_empty());
}

#[test]
fn the_json_view_lists_the_skills_only_for_a_run_that_has_them() {
    let j = view_json(&placed());
    assert_eq!(
        j["skills"],
        json!([
            {"name": "lifecycle-tdd", "sha256": digest('a'), "files": 1, "bytes": 900},
            {"name": "my-skill", "sha256": digest('b'), "files": 3, "bytes": 2048}
        ])
    );
    assert_eq!(j["skillsPreflight"], json!({"ran": true, "ms": 61}));
    let none = view_json(&view(vec![]));
    assert!(none.get("skills").is_none(), "{none}");
    assert!(none.get("skillsPreflight").is_none(), "{none}");
}
