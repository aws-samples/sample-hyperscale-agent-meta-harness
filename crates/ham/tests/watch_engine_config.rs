//! `ham watch` in line mode and the engine settings of a run: when the run's settings were placed, the card
//! prints once in plain lines (the hash, then one line per setting), and the `--json` view of the run lists them. A run without engine settings
//! prints and lists nothing new, so every other view is unchanged.
#![allow(clippy::unwrap_used)]

use ham::watch::{InputMemo, view_json};
use ham_core::state::{RunState, RunView};
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";

fn progress(n: u32, value: Value) -> Envelope {
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": format!("01M5A2R7K3C9XJ4D8E6F{n:06}"), "runId": RUN, "agentId": "direct", "seq": n,
        "ts": "2026-10-21T08:15:00Z", "type": "turn.progress", "authority": "agent",
        "payload": {"kind": "inline", "value": value}
    }))
    .unwrap()
}

fn hash() -> String {
    "a1b2c3d4e5f6".to_string() + &"0".repeat(52)
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
        json!({"turn": 1, "kind": "engine-config", "engine": "cline", "sha256": hash(), "keys": 2,
               "settings": [{"key": "reasoning.effort", "value": "low"}, {"key": "tools.disable", "value": "spawn_agent"}],
               "readBack": "ok"}),
    )])
}

#[test]
fn the_card_prints_once_with_the_hash_and_each_setting() {
    let mut memo = InputMemo::new();
    let v = placed();
    let first = memo.lines(&v).join("\n");
    assert!(
        first.contains("Engine settings placed for this run"),
        "{first}"
    );
    assert!(first.contains("settings hash a1b2c3d4e5f6"), "{first}");
    assert!(first.contains("reasoning.effort = low"), "{first}");
    assert!(first.contains("tools.disable = spawn_agent"), "{first}");
    assert!(!first.contains(&hash()), "{first}");
    for w in ["verified", "tested", "gated", "evaluated", "applied"] {
        assert!(!first.to_lowercase().contains(w), "{first}");
    }
    assert!(memo.lines(&v).is_empty(), "the card prints once");
}

#[test]
fn a_run_without_engine_settings_prints_nothing_new() {
    let mut memo = InputMemo::new();
    let v = view(vec![progress(
        1,
        json!({"turn": 1, "kind": "research", "searches": 1, "fetches": 0, "repos": 0, "placed": 0, "errors": 0}),
    )]);
    assert!(memo.lines(&v).is_empty());
}

#[test]
fn the_json_view_lists_the_settings_only_for_a_run_that_has_them() {
    let j = view_json(&placed());
    assert_eq!(
        j["engineConfig"],
        json!({"engine": "cline", "sha256": hash(), "keys": 2,
               "settings": [{"key": "reasoning.effort", "value": "low"}, {"key": "tools.disable", "value": "spawn_agent"}]})
    );
    let none = view_json(&view(vec![]));
    assert!(none.get("engineConfig").is_none(), "{none}");
}
