//! How long a new run waits for the user's answer: the flag, `HAMH_INPUT_WAIT`, `input_wait_minutes`,
//! then 30 minutes for an interactive client and none for any other; and the request body, which carries `inputWaitS` only when the
//! wait is above zero, so a script, the release gate and an old config send the bytes they sent before.
#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, DirectArgs, parse};
use ham_core::models::ModelPrefs;
use ham_core::request::{
    DEFAULT_INTERACTIVE_WAIT_MINUTES, DirectInputs, WaitError, WaitPrefs, WaitSource,
    direct_idempotency_key, direct_run_request, input_wait_minutes, with_input_wait,
};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::DEFAULT_CLIENT_INPUT_WAIT_S;

fn args(extra: &[&str]) -> DirectArgs {
    let mut full = vec!["build it", "--s3-only", "--direct"];
    full.extend_from_slice(extra);
    match parse(&full).unwrap().command {
        Command::StartDirect(d) => d,
        other => panic!("{other:?}"),
    }
}

fn request(a: &DirectArgs) -> StartDirectRunRequest {
    direct_run_request(a, DirectInputs::default(), &ModelPrefs::default()).unwrap()
}

fn prefs(env: Option<&str>, config: Option<u32>, interactive: bool) -> WaitPrefs {
    WaitPrefs {
        env: env.map(str::to_string),
        config,
        interactive,
    }
}

#[test]
fn the_interactive_default_is_thirty_minutes_and_the_server_constant_agrees() {
    assert_eq!(DEFAULT_INTERACTIVE_WAIT_MINUTES, 30);
    assert_eq!(
        DEFAULT_INTERACTIVE_WAIT_MINUTES * 60,
        DEFAULT_CLIENT_INPUT_WAIT_S
    );
}

#[test]
fn nothing_set_means_thirty_minutes_when_interactive_and_none_otherwise() {
    assert_eq!(
        input_wait_minutes(None, &prefs(None, None, true)).unwrap(),
        (30, WaitSource::Default)
    );
    assert_eq!(
        input_wait_minutes(None, &prefs(None, None, false)).unwrap(),
        (0, WaitSource::Default)
    );
}

#[test]
fn the_layers_run_flag_then_env_then_config() {
    let all = prefs(Some("20"), Some(10), true);
    assert_eq!(
        input_wait_minutes(Some(5), &all).unwrap(),
        (5, WaitSource::Flag)
    );
    assert_eq!(
        input_wait_minutes(None, &all).unwrap(),
        (20, WaitSource::Env)
    );
    assert_eq!(
        input_wait_minutes(None, &prefs(None, Some(10), true)).unwrap(),
        (10, WaitSource::Config)
    );
    // A zero from any layer is a choice (no wait) that beats the interactive default and the layers below it.
    assert_eq!(
        input_wait_minutes(Some(0), &all).unwrap(),
        (0, WaitSource::Flag)
    );
    assert_eq!(
        input_wait_minutes(None, &prefs(Some("0"), Some(10), true)).unwrap(),
        (0, WaitSource::Env)
    );
    assert_eq!(
        input_wait_minutes(None, &prefs(None, Some(0), true)).unwrap(),
        (0, WaitSource::Config)
    );
}

#[test]
fn a_blank_env_is_unset_and_a_wrong_one_is_an_error_that_names_it() {
    assert_eq!(
        input_wait_minutes(None, &prefs(Some("  "), Some(10), true)).unwrap(),
        (10, WaitSource::Config)
    );
    for bad in ["ten", "-1", "1.5", "43201"] {
        let e: WaitError = input_wait_minutes(None, &prefs(Some(bad), Some(10), true)).unwrap_err();
        assert_eq!(e.layer, WaitSource::Env, "{bad}");
        let text = e.to_string();
        assert!(
            text.starts_with("HAMH_INPUT_WAIT:") && text.contains(bad),
            "{text}"
        );
    }
    // The flag and the config are range-checked too, in case a caller bypasses the parser.
    assert_eq!(
        input_wait_minutes(Some(43_201), &prefs(None, None, true))
            .unwrap_err()
            .layer,
        WaitSource::Flag
    );
    assert_eq!(
        input_wait_minutes(None, &prefs(None, Some(43_201), true))
            .unwrap_err()
            .layer,
        WaitSource::Config
    );
}

#[test]
fn the_request_carries_the_wait_in_seconds_and_omits_zero() {
    let r = request(&args(&["--input-wait", "30"]));
    assert_eq!(r.input_wait_s, Some(1800));
    let json = serde_json::to_value(&r).unwrap();
    assert_eq!(json["inputWaitS"], 1800);
    let r = request(&args(&["--input-wait", "1"]));
    assert_eq!(r.input_wait_s, Some(60));
    for none in [args(&[]), args(&["--input-wait", "0"])] {
        let r = request(&none);
        assert_eq!(r.input_wait_s, None);
        assert!(
            serde_json::to_value(&r)
                .unwrap()
                .get("inputWaitS")
                .is_none()
        );
    }
}

#[test]
fn a_request_without_a_wait_is_the_same_bytes_and_the_same_key_as_before_the_feature() {
    let r = request(&args(&[]));
    let bytes = serde_json::to_string(&r).unwrap();
    assert_eq!(
        bytes,
        r#"{"prompt":"build it","target":{"mode":"s3-only"},"model":"global.anthropic.claude-sonnet-5-5","rules":[]}"#
    );
    // Setting a wait of zero through any layer changes nothing either.
    let zero = with_input_wait(&args(&[]), &prefs(None, Some(0), true)).unwrap();
    let r0 = request(&zero);
    assert_eq!(serde_json::to_string(&r0).unwrap(), bytes);
    assert_eq!(
        direct_idempotency_key(&r0, "n"),
        direct_idempotency_key(&r, "n")
    );
    // A wait is a different request, so a different launch key.
    let r30 = request(&args(&["--input-wait", "30"]));
    assert_ne!(
        direct_idempotency_key(&r30, "n"),
        direct_idempotency_key(&r, "n")
    );
}

#[test]
fn with_input_wait_applies_the_layers_to_the_arguments() {
    let a = with_input_wait(&args(&[]), &prefs(None, None, true)).unwrap();
    assert_eq!(a.input_wait, Some(30));
    assert_eq!(request(&a).input_wait_s, Some(1800));
    let a = with_input_wait(&args(&[]), &prefs(None, None, false)).unwrap();
    assert_eq!(a.input_wait, Some(0));
    assert_eq!(request(&a).input_wait_s, None);
    // The flag wins over everything below it.
    let a = with_input_wait(
        &args(&["--input-wait", "7"]),
        &prefs(Some("20"), Some(10), true),
    )
    .unwrap();
    assert_eq!(request(&a).input_wait_s, Some(420));
    // Nothing else about the arguments changes.
    let base = args(&["--model", "opus", "--base", "main"]);
    let a = with_input_wait(&base, &prefs(None, Some(5), true)).unwrap();
    assert_eq!(
        (a.model.as_deref(), a.base.as_deref()),
        (Some("opus"), Some("main"))
    );
    assert_eq!(a.prompt, base.prompt);
}

#[test]
fn every_layer_has_a_name_for_the_error_message() {
    for (layer, name) in [
        (WaitSource::Flag, "--input-wait"),
        (WaitSource::Env, "HAMH_INPUT_WAIT"),
        (WaitSource::Config, "input_wait_minutes in config.toml"),
        (WaitSource::Default, "the default"),
    ] {
        assert_eq!(layer.label(), name);
    }
}
