//! The second engine's user settings as agentd reads them.
#![allow(clippy::unwrap_used)]

use agentd::direct::request::engine_settings;
use agentd::engine::user_settings::UserSettings;
use harness_protocol::api::v1::StartDirectRunRequest;
use serde_json::{Value, json};

fn request_with(member: Option<Value>) -> StartDirectRunRequest {
    let mut v = json!({"prompt": "p", "target": {"mode": "s3-only"}, "model": "m", "rules": []});
    if let Some(m) = member {
        v["engineConfig"] = m;
    }
    serde_json::from_value(v).unwrap()
}

#[test]
fn b790_a_run_of_the_second_engine_with_no_member_has_no_settings() {
    assert_eq!(
        engine_settings(&request_with(None), "opencode").unwrap(),
        UserSettings::default()
    );
}

#[test]
fn b790_an_empty_member_for_the_second_engine_is_accepted_for_that_engine_only() {
    let r = request_with(Some(
        json!({"engine": "opencode", "schema": 1, "settings": {}}),
    ));
    assert_eq!(
        engine_settings(&r, "opencode").unwrap(),
        UserSettings::default()
    );
    let err = engine_settings(&r, "cline").unwrap_err();
    assert!(err.contains("another engine"), "{err}");
}

#[test]
fn b790_a_locked_or_unknown_key_in_the_second_engines_member_ends_the_run_before_it_starts() {
    for settings in [
        json!({"aws": {"profile": "marker-profile-9x"}}),
        json!({"reasoning": {"effort": "high"}}),
    ] {
        let r = request_with(Some(
            json!({"engine": "opencode", "schema": 1, "settings": settings}),
        ));
        let err = engine_settings(&r, "opencode").unwrap_err();
        assert!(
            !err.contains("marker-profile-9x"),
            "a value is never echoed: {err}"
        );
        assert!(
            err.starts_with("engine config") || err.contains("key"),
            "{err}"
        );
    }
}
