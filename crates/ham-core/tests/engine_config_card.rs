//! The engine settings of a run on the card.
//!
//! agentd's engine-config module writes one `turn.progress` event of kind `engine-config` after it placed the user's settings where the engine reads
//! them and read them back (`crates/agentd/src/direct/engine_config/mod.rs`, `event`): the engine, the settings hash, the number of keys, the keys
//! that were placed with a short value each, and `readBack: "ok"`. The run view carries it as `RunView::engine_config`; the run screen draws it as a
//! folded card, `ham watch` prints it once, and the transcript gets one plain notice. The card says "placed": agentd wrote the value where the
//! engine reads it, which is not a statement that the engine used it, and nothing here says the work was verified, tested, gated or evaluated.
//! Every string from the event is cleaned and redacted.
#![allow(clippy::unwrap_used)]

use ham_core::notice::{NoticeKind, notice_of};
use ham_core::state::{RunState, RunView};
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const BANNED: [&str; 5] = ["verified", "tested", "gated", "evaluated", "applied"];

fn id(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn event(n: u32, value: Value) -> Envelope {
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": id(n), "runId": RUN, "agentId": "direct", "seq": n, "ts": "2026-10-21T08:15:00Z",
        "type": "turn.progress", "authority": "agent", "payload": {"kind": "inline", "value": value}
    }))
    .unwrap()
}

fn hash() -> String {
    "a1b2c3d4e5f6".to_string() + &"0".repeat(52)
}

/// The event as agentd writes it.
fn placed(n: u32, settings: &[(&str, &str)]) -> Envelope {
    event(
        n,
        json!({
            "turn": 1, "kind": "engine-config", "engine": "cline", "sha256": hash(), "keys": settings.len(),
            "settings": settings.iter().map(|(k, v)| json!({"key": k, "value": v})).collect::<Vec<_>>(),
            "readBack": "ok",
        }),
    )
}

fn view_of(events: Vec<Envelope>) -> RunView {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    s.view()
}

fn no_claim(text: &str) {
    let lower = text.to_lowercase();
    for w in BANNED {
        assert!(!lower.contains(w), "{text:?} holds {w:?}");
    }
}

#[test]
fn a_run_with_placed_settings_has_a_card_with_the_hash_and_each_key() {
    let v = view_of(vec![placed(
        4,
        &[
            ("reasoning.effort", "high"),
            ("commands.timeout_seconds", "900"),
            ("tools.disable", "fetch_web_content"),
        ],
    )]);
    let c = v.engine_config.expect("a card");
    assert_eq!(c.request_id, id(4));
    assert_eq!((c.engine.as_str(), c.keys), ("cline", 3));
    assert_eq!(c.sha256, hash());
    assert_eq!(c.short_hash(), "a1b2c3d4e5f6");
    assert_eq!(c.rows.len(), 3);
    assert_eq!(
        c.title(),
        "engine settings placed · cline · 3 set · settings hash a1b2c3d4e5f6"
    );
    let lines = c.lines(100);
    assert_eq!(
        lines,
        [
            "reasoning.effort = high",
            "commands.timeout_seconds = 900",
            "tools.disable = fetch_web_content"
        ]
    );
    no_claim(&c.title());
    no_claim(&lines.join("\n"));
}

#[test]
fn a_run_without_the_event_has_no_card() {
    assert!(view_of(vec![]).engine_config.is_none());
    // Another kind of event, and a placed event that the read-back did not clear, make no card.
    let other = event(2, json!({"turn": 1, "kind": "skills", "skills": []}));
    assert!(view_of(vec![other]).engine_config.is_none());
    let unconfirmed = event(
        3,
        json!({"turn": 1, "kind": "engine-config", "engine": "cline", "sha256": hash(), "keys": 1,
               "settings": [{"key": "reasoning.effort", "value": "low"}], "readBack": "mismatch"}),
    );
    assert!(view_of(vec![unconfirmed]).engine_config.is_none());
    let empty = event(
        4,
        json!({"turn": 1, "kind": "engine-config", "engine": "cline", "sha256": hash(), "keys": 0, "settings": [], "readBack": "ok"}),
    );
    assert!(view_of(vec![empty]).engine_config.is_none());
}

#[test]
fn the_card_reads_only_the_engine_config_event_cleans_and_redacts_every_string() {
    let secret = ["gh", "p_"].concat() + &"Z".repeat(36);
    let hostile = event(
        9,
        json!({
            "turn": 1, "kind": "engine-config", "engine": "cl\u{1b}[31mine",
            "sha256": "not a hash \u{7}", "keys": 3,
            "settings": [
                {"key": "reasoning.effort", "value": format!("\u{1b}]0;title\u{7}high {secret}")},
                {"key": "evil\u{1b}[2Jkey", "value": "x".repeat(500)},
                {"key": "tools.disable", "value": "spawn_agent\u{202e}"},
            ],
            "readBack": "ok",
        }),
    );
    let c = view_of(vec![hostile]).engine_config.expect("a card");
    // A hash that is not 64 lowercase hex digits shows as ?, as the skills card does.
    assert_eq!(c.sha256, "");
    assert_eq!(c.short_hash(), "?");
    let text = format!("{}\n{}", c.title(), c.lines(80).join("\n"));
    for ch in text.chars() {
        assert!(!ch.is_control() || ch == '\n', "{ch:?} in {text:?}");
    }
    assert!(!text.contains(&secret), "{text}");
    assert!(!text.contains('\u{202e}'), "{text}");
    assert!(
        !text.contains("title"),
        "the title of an OSC string leaked: {text}"
    );
    for line in c.lines(80) {
        assert!(line.chars().count() <= 80, "{line:?}");
    }
    // The newest usable event wins.
    let v = view_of(vec![
        placed(5, &[("reasoning.effort", "low")]),
        placed(6, &[("reasoning.effort", "high")]),
    ]);
    assert_eq!(v.engine_config.unwrap().rows[0].value, "high");
}

#[test]
fn a_long_list_is_cut_and_counted() {
    let many: Vec<(String, String)> = (0..40)
        .map(|i| (format!("k{i}.x"), "1".to_string()))
        .collect();
    let refs: Vec<(&str, &str)> = many.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let c = view_of(vec![placed(7, &refs)]).engine_config.unwrap();
    assert!(c.rows.len() <= ham_core::engine_config_card::MAX_ROWS);
    assert_eq!(c.keys, 40);
    let lines = c.lines(80);
    assert!(lines.last().unwrap().starts_with("and "), "{lines:?}");
}

#[test]
fn the_transcript_has_one_plain_notice_instead_of_an_event_word() {
    let e = placed(
        8,
        &[
            ("reasoning.effort", "high"),
            ("commands.timeout_seconds", "900"),
        ],
    );
    let n = notice_of(&e).expect("a notice");
    assert_eq!(n.kind, NoticeKind::Notify);
    assert_eq!(
        n.text,
        "Engine settings: 2 placed (settings hash a1b2c3d4e5f6)."
    );
    assert!(!n.text.contains("Event "), "{}", n.text);
    no_claim(&n.text);
}
