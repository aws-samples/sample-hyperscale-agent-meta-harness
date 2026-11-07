//! The `report_tests` config key: `report_tests = false` in `config.toml` keeps a run from making the clean-checkout test
//! report. It is on by default, so the request carries the member only to turn it off (`reportTests: false`); any other value leaves it out
//! and a request is byte for byte what it was before the key existed.

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::command::{Command, parse};
use ham_core::config::{
    Config, ConfigError, KEYS, get_key, parse_config, render_config, set_key, unset_key,
};
use ham_core::models::ModelPrefs;
use ham_core::request::{DirectInputs, direct_run_request, with_report_setting};

fn request() -> harness_protocol::api::v1::StartDirectRunRequest {
    let Command::StartDirect(args) = parse(&["p", "--s3-only", "--direct"]).unwrap().command else {
        panic!("a direct start was expected");
    };
    direct_run_request(&args, DirectInputs::default(), &ModelPrefs::default()).unwrap()
}

#[test]
fn a_boolean_in_the_file_is_read_in_either_spelling_of_the_key() {
    assert_eq!(
        parse_config("report_tests = false\n").unwrap().report_tests,
        Some(false)
    );
    assert_eq!(
        parse_config("reportTests = true\n").unwrap().report_tests,
        Some(true)
    );
    assert_eq!(
        parse_config("region = \"us-west-2\"\n")
            .unwrap()
            .report_tests,
        None
    );
    assert_eq!(Config::default().report_tests, None);
}

#[test]
fn a_value_that_is_not_a_boolean_is_refused_and_the_error_names_the_key() {
    for bad in [
        "report_tests = \"off\"\n",
        "report_tests = 1\n",
        "report_tests = [true]\n",
    ] {
        let err = parse_config(bad).unwrap_err().to_string();
        assert!(err.contains("report_tests"), "{bad:?} gave {err}");
    }
}

#[test]
fn the_setting_is_written_as_a_boolean_and_reads_back() {
    let cfg = Config {
        report_tests: Some(false),
        ..Config::default()
    };
    let text = render_config(&cfg);
    assert!(text.contains("reportTests = false"), "{text:?}");
    assert_eq!(parse_config(&text).unwrap(), cfg);
    assert!(!render_config(&Config::default()).contains("reportTests"));
}

#[test]
fn ham_config_sets_gets_and_unsets_the_key() {
    let none = Config::default();
    for (given, want, text) in [
        ("false", false, "false"),
        ("off", false, "false"),
        ("NO", false, "false"),
        ("0", false, "false"),
        ("true", true, "true"),
        ("On", true, "true"),
        ("yes", true, "true"),
        ("1", true, "true"),
    ] {
        let set = set_key(&none, "report_tests", given).unwrap();
        assert_eq!(set.report_tests, Some(want), "set {given}");
        assert_eq!(
            get_key(&set, "reportTests").unwrap().as_deref(),
            Some(text),
            "get after {given}"
        );
    }
    assert_eq!(get_key(&none, "report_tests").unwrap(), None);
    let off = set_key(&none, "report_tests", "off").unwrap();
    assert_eq!(unset_key(&off, "report_tests").unwrap().report_tests, None);
}

#[test]
fn a_bad_value_is_refused_with_the_key_and_the_choices() {
    let err = set_key(&Config::default(), "report_tests", "maybe").unwrap_err();
    let ConfigError::Invalid { key, reason } = &err else {
        panic!("expected Invalid, got {err:?}");
    };
    assert_eq!(key, "report_tests");
    for word in ["on", "off", "maybe"] {
        assert!(reason.contains(word), "{reason:?} lacks {word:?}");
    }
}

#[test]
fn the_key_list_names_report_tests() {
    assert!(KEYS.contains(&("reportTests", "report_tests")), "{KEYS:?}");
}

#[test]
fn only_off_reaches_the_request_and_the_default_leaves_the_request_unchanged() {
    let base = request();
    assert_eq!(base.report_tests, None);
    assert_eq!(with_report_setting(base.clone(), None), base);
    assert_eq!(
        with_report_setting(base.clone(), Some(true)),
        base,
        "on is the default and is not sent"
    );
    let off = with_report_setting(base.clone(), Some(false));
    assert_eq!(off.report_tests, Some(false));
    let wire = serde_json::to_value(&off).unwrap();
    assert_eq!(wire["reportTests"], serde_json::json!(false));
    assert!(
        serde_json::to_value(&base)
            .unwrap()
            .get("reportTests")
            .is_none()
    );
}
