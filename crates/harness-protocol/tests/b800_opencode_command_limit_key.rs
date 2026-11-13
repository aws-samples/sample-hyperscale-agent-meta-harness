//! The key table of the second engine holds one key, the shell command time limit, with the range and the default of the
//! first engine's key. Every other key of the first engine stays unknown to the second.
#![allow(clippy::unwrap_used)]

use harness_protocol::direct::{
    CLINE_KEYS, EngineConfig, KeyKind, KeyRow, OPENCODE_KEYS, check_engine_config,
    key_table_sha256, keys_for, table_problems,
};
use serde_json::{Value, json};

const KEY: &str = "commands.timeout_seconds";

fn cfg(settings: Value) -> EngineConfig {
    EngineConfig {
        engine: "opencode".into(),
        schema: 1,
        settings: settings.as_object().unwrap().clone(),
    }
}

fn limit(value: Value) -> EngineConfig {
    cfg(json!({"commands": {"timeout_seconds": value}}))
}

fn row_of(rows: &'static [KeyRow], path: &str) -> KeyRow {
    *rows.iter().find(|r| r.path == path).unwrap()
}

#[test]
fn b800_the_table_of_the_second_engine_holds_the_command_time_limit_and_nothing_else() {
    assert_eq!(OPENCODE_KEYS.len(), 1, "{OPENCODE_KEYS:?}");
    assert_eq!(OPENCODE_KEYS[0].path, KEY);
    assert_eq!(table_problems(OPENCODE_KEYS), Vec::<String>::new());
    assert_eq!(keys_for("opencode").unwrap().len(), 1);
}

#[test]
fn b800_the_row_has_the_kind_the_default_and_the_dependents_of_the_first_engines_row() {
    let first = row_of(CLINE_KEYS, KEY);
    let second = row_of(OPENCODE_KEYS, KEY);
    assert_eq!(second, first);
    assert_eq!(second.kind, KeyKind::Int { min: 1, max: 1800 });
    assert_eq!(second.default, "600");
    assert!(second.required_by.is_empty());
}

#[test]
fn b800_the_table_hash_is_pinned_so_that_an_edit_of_the_row_is_a_conscious_one() {
    // The hash of the one line `commands.timeout_seconds|int|1..=1800|600|` and its newline.
    assert_eq!(
        key_table_sha256(OPENCODE_KEYS),
        "3a832ebec8f6c9c78aac1f01c59da6546f1bd7946777aaaae0087c8a0f4d2f2b"
    );
    // The first engine's table does not move: its own pin is in engine_config_check.rs.
    assert_eq!(CLINE_KEYS.len(), 5);
}

#[test]
fn b800_both_ends_of_the_range_are_accepted() {
    for seconds in [1, 2, 600, 900, 1799, 1800] {
        check_engine_config(&limit(json!(seconds))).unwrap_or_else(|e| panic!("{seconds}: {e}"));
    }
}

#[test]
fn b800_a_value_outside_the_range_is_refused_and_never_clamped() {
    for bad in [0_i64, -1, 1801, 86_400, i64::MIN, i64::MAX] {
        let err = check_engine_config(&limit(json!(bad))).unwrap_err();
        assert_eq!(err.code(), "out_of_range", "{bad}");
        assert_eq!(err.key(), Some(KEY), "{bad}");
    }
}

#[test]
fn b800_a_value_of_the_wrong_kind_is_refused() {
    for bad in [
        json!("900"),
        json!(9.5),
        json!(600.0),
        json!(true),
        json!(null),
        json!([600]),
        json!({"a": 1}),
    ] {
        let err = check_engine_config(&limit(bad.clone())).unwrap_err();
        assert_eq!(err.code(), "bad_type", "{bad}");
    }
}

#[test]
fn b800_the_other_keys_of_the_first_engine_stay_unknown_to_the_second() {
    for (settings, path) in [
        (json!({"reasoning": {"effort": "high"}}), "reasoning.effort"),
        (
            json!({"context": {"compaction": "off"}}),
            "context.compaction",
        ),
        (
            json!({"context": {"compaction_trigger": 0.5}}),
            "context.compaction_trigger",
        ),
        (
            json!({"tools": {"disable": ["spawn_agent"]}}),
            "tools.disable",
        ),
    ] {
        let err = check_engine_config(&cfg(settings.clone())).unwrap_err();
        assert_eq!(err.code(), "unknown_key", "{settings}");
        assert_eq!(err.key(), Some(path), "{settings}");
    }
}

#[test]
fn b800_a_member_with_the_limit_next_to_an_unknown_key_is_refused_for_the_unknown_key() {
    let err = check_engine_config(&cfg(
        json!({"commands": {"timeout_seconds": 900}, "reasoning": {"effort": "low"}}),
    ))
    .unwrap_err();
    assert_eq!(err.code(), "unknown_key");
    assert_eq!(err.key(), Some("reasoning.effort"));
}

#[test]
fn b800_the_limit_is_read_back_from_the_member_and_counted_as_one_key() {
    let c = limit(json!(900));
    check_engine_config(&c).unwrap();
    assert_eq!(c.get(KEY), Some(&json!(900)));
    assert_eq!(c.key_count(), 1);
    // The summary reference counts it; a member with no key counts none.
    assert_eq!(c.to_ref().keys, 1);
    assert_eq!(cfg(json!({})).key_count(), 0);
    // The hash of the settings differs from the same value on the first engine's member only by the engine name.
    let mut first = c.clone();
    first.engine = "cline".into();
    assert_ne!(c.canonical_sha256(), first.canonical_sha256());
}
