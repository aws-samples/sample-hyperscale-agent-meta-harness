//! The user's settings file of the second engine: `engines/opencode.toml` is accepted and checked by the one shared check. Its
//! table holds one key (the shell command time limit, `b800_opencode_command_limit_key.rs`), so a file with no key passes and every
//! other key is refused, the locked ones with their class.
#![allow(clippy::unwrap_used)]

use harness_protocol::direct::{
    EngineConfig, OPENCODE_KEYS, check_engine_config, key_table_sha256, keys_for, table_problems,
};
use serde_json::{Value, json};

fn cfg(settings: Value) -> EngineConfig {
    EngineConfig {
        engine: "opencode".into(),
        schema: 1,
        settings: settings.as_object().unwrap().clone(),
    }
}

#[test]
fn b790_the_second_engine_has_a_table_and_it_has_no_problems() {
    let table = keys_for("opencode").expect("a table for opencode");
    assert_eq!(table.len(), OPENCODE_KEYS.len());
    assert_eq!(table_problems(OPENCODE_KEYS), Vec::<String>::new());
    // The hash of the table: a key added later moves it, and the pin of this test is edited with it. Moved it from the hash of nothing
    // (e3b0c442...) to the hash of the one row `commands.timeout_seconds|int|1..=1800|600|`.
    assert_eq!(
        key_table_sha256(OPENCODE_KEYS),
        "3a832ebec8f6c9c78aac1f01c59da6546f1bd7946777aaaae0087c8a0f4d2f2b"
    );
}

#[test]
fn b790_a_file_with_no_key_is_accepted() {
    check_engine_config(&cfg(json!({}))).unwrap();
}

#[test]
fn b790_a_key_of_the_first_engine_is_unknown_to_the_second() {
    let err = check_engine_config(&cfg(json!({"reasoning": {"effort": "high"}}))).unwrap_err();
    assert_eq!(err.code(), "unknown_key");
    assert_eq!(err.key(), Some("reasoning.effort"));
}

#[test]
fn b790_a_locked_key_is_refused_with_its_class() {
    for (settings, class) in [
        (json!({"aws": {"profile": "p"}}), "credentials"),
        (json!({"autoupdate": true}), "telemetry"),
        (json!({"permissions": {"bash": "allow"}}), "permissions"),
        (
            json!({"provider": {"options": {"baseurl": "x"}}}),
            "provider",
        ),
    ] {
        let err = check_engine_config(&cfg(settings.clone())).unwrap_err();
        assert_eq!(err.denied_class().map(|c| c.id), Some(class), "{settings}");
    }
}

#[test]
fn b790_the_engines_own_setting_names_are_unknown_keys_and_not_a_way_around_the_locks() {
    for settings in [
        json!({"permission": {"bash": "ask"}}),
        json!({"share": "auto"}),
        json!({"mcp": {"x": {"type": "remote"}}}),
        json!({"instructions": ["/etc/passwd"]}),
    ] {
        let err = check_engine_config(&cfg(settings.clone())).unwrap_err();
        assert_eq!(err.code(), "unknown_key", "{settings}");
    }
}

#[test]
fn b790_an_engine_without_a_table_is_still_refused_as_the_wrong_engine() {
    for other in ["pi", "kiro", "codex", "", "OpenCode", "opencode "] {
        assert!(keys_for(other).is_none(), "{other:?}");
        let mut c = cfg(json!({}));
        c.engine = other.into();
        assert_eq!(
            check_engine_config(&c).unwrap_err().code(),
            "wrong_engine",
            "{other:?}"
        );
    }
}
