//! The `engineConfig` member of a direct run's request. A default user sends no member and
//! the request is byte for byte the request of protocol 1.38.0 (a guard, green before and after); a user with an engine file sends the typed
//! settings, and an edit of the file starts a new run because the member is part of the request the idempotency key hashes.
#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, parse};
use ham_core::engine_file;
use ham_core::models::ModelPrefs;
use ham_core::request::{DirectInputs, direct_idempotency_key, direct_run_request};
use harness_protocol::api::v1::StartDirectRunRequest;

fn args(words: &[&str]) -> ham_core::command::DirectArgs {
    match parse(words).unwrap().command {
        Command::StartDirect(d) => d,
        other => panic!("{other:?}"),
    }
}

fn request(inputs: DirectInputs) -> StartDirectRunRequest {
    direct_run_request(
        &args(&["Add a function", "--s3-only", "--direct"]),
        inputs,
        &ModelPrefs::default(),
    )
    .unwrap()
}

#[test]
fn a_user_with_an_engine_file_sends_the_typed_settings() {
    let text =
        "# note\nschema = 1\n[reasoning]\neffort = \"low\"\n[commands]\ntimeout_seconds = 30\n";
    let config = engine_file::read_text(text, "cline").config.unwrap();
    let r = request(DirectInputs {
        engine_config: Some(config.clone()),
        ..DirectInputs::default()
    });
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(
        v["engineConfig"],
        serde_json::json!({
            "engine": "cline", "schema": 1,
            "settings": {"commands": {"timeout_seconds": 30}, "reasoning": {"effort": "low"}}
        })
    );
    // Comments do not travel.
    assert!(!v.to_string().contains("note"));
    // The member is part of what the idempotency key hashes: an edit of the file is a new run.
    let plain = request(DirectInputs::default());
    assert_ne!(
        direct_idempotency_key(&r, "nonce"),
        direct_idempotency_key(&plain, "nonce")
    );
    let other = engine_file::read_text(&text.replace("low", "high"), "cline")
        .config
        .unwrap();
    let r2 = request(DirectInputs {
        engine_config: Some(other),
        ..DirectInputs::default()
    });
    assert_ne!(
        direct_idempotency_key(&r, "nonce"),
        direct_idempotency_key(&r2, "nonce")
    );
    // The same file read twice is the same request.
    let again = engine_file::read_text(text, "cline").config.unwrap();
    let r3 = request(DirectInputs {
        engine_config: Some(again),
        ..DirectInputs::default()
    });
    assert_eq!(
        direct_idempotency_key(&r, "nonce"),
        direct_idempotency_key(&r3, "nonce")
    );
}
