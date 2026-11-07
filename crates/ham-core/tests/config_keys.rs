//! `get_key`, `set_key` and `unset_key` over the registry's thirty-one keys (`ham config get|set|unset`). Before the registry, `slot()` ended in
//! `_ => &mut cfg.default_model`, so a key the match did not name would have read, written or cleared the model default.

#![allow(clippy::unwrap_used)]

use ham_core::config::{
    Config, ConfigError, KEYS, get_key, parse_config, render_config, set_key, unset_key,
};

const OPUS: &str = "us.anthropic.claude-opus-5-5";

/// A value that is not the default, for each key by its printed name, and what `get_key` then prints.
fn non_defaults() -> Vec<(&'static str, &'static str, String)> {
    vec![
        (
            "api_endpoint",
            "https://x.execute-api.us-east-1.amazonaws.com/direct",
            "https://x.execute-api.us-east-1.amazonaws.com/direct".into(),
        ),
        ("region", "us-west-2", "us-west-2".into()),
        ("aws_profile", "ham-test", "ham-test".into()),
        ("default_repo", "cc:my-service", "cc:my-service".into()),
        ("download_dir", "/downloads", "/downloads".into()),
        ("default_model", "opus", OPUS.into()),
        ("input_wait_minutes", "7", "7".into()),
        ("mouse", "auto", "auto".into()),
        ("report_tests", "off", "false".into()),
        ("run.goal", "off", "false".into()),
        ("run.rules", "no", "false".into()),
        ("run.skills", "0", "false".into()),
        ("research.enabled", "on", "true".into()),
        ("ui.notify", "yes", "true".into()),
        ("ui.bell", "false", "false".into()),
        ("ui.mascot", "ascii", "ascii".into()),
        ("run.report_command_minutes", "6", "6".into()),
        ("run.stall_calls", "45", "45".into()),
        ("run.stall_window_minutes", "30", "30".into()),
        ("run.stall_min_calls", "12", "12".into()),
        ("run.command_grace_minutes", "0", "0".into()),
        ("run.final_message", "off", "false".into()),
        ("run.lifecycle_flow", "full", "full".into()),
        ("run.review_round", "off", "off".into()),
        ("run.compute", "Instances", "instances".into()),
        ("run.stall_new_calls", "250", "250".into()),
        ("run.cargo_no_debug_info", "off", "false".into()),
        ("run.cargo_no_incremental", "no", "false".into()),
        ("run.exclude_target_dir", "0", "false".into()),
        ("run.outside_checkout_note", "off", "false".into()),
        ("ui.deprecation_notices", "off", "false".into()),
        ("ui.mascot_motion", "off", "false".into()),
        ("run.max_run_hours", "48", "48".into()),
        ("default_backend", "default", "default".into()),
        ("customer_content", "on", "true".into()),
    ]
}

fn all_set() -> Config {
    let mut cfg = Config::default();
    for (key, value, _) in non_defaults() {
        cfg = set_key(&cfg, key, value).unwrap();
    }
    cfg
}

#[test]
fn unset_of_each_new_key_leaves_the_other_sixteen_alone() {
    let full = all_set();
    assert_eq!(non_defaults().len(), 35);
    assert_eq!(KEYS.len(), 35);
    for (key, _, printed) in non_defaults() {
        assert_eq!(
            get_key(&full, key).unwrap().as_deref(),
            Some(printed.as_str()),
            "{key}"
        );
    }
    for (key, _, _) in non_defaults() {
        let next = unset_key(&full, key).unwrap();
        assert_eq!(get_key(&next, key).unwrap(), None, "{key} is cleared");
        for (other, _, printed) in non_defaults() {
            if other == key {
                continue;
            }
            assert_eq!(
                get_key(&next, other).unwrap().as_deref(),
                Some(printed.as_str()),
                "unsetting {key} changed {other}"
            );
        }
        // The A-1 defect: no key may fall through to the model default.
        assert_eq!(
            next.default_model.as_deref(),
            if key == "default_model" {
                None
            } else {
                Some(OPUS)
            },
            "unsetting {key} touched default_model"
        );
    }
    // Everything unset leaves the file the version 4.02 writes for an empty config.
    let mut cfg = full;
    for (key, _, _) in non_defaults() {
        cfg = unset_key(&cfg, key).unwrap();
    }
    assert_eq!(cfg, Config::default());
    assert_eq!(render_config(&cfg), "");
}

#[test]
fn set_get_unset_work_for_every_key_in_both_spellings() {
    // Every pair of KEYS: either spelling sets, either gets, either unsets.
    for (file, snake) in KEYS {
        let (_, value, printed) = non_defaults()
            .into_iter()
            .find(|(k, _, _)| *k == snake)
            .unwrap_or_else(|| panic!("no value for {snake}"));
        for by in [file, snake] {
            let cfg = set_key(&Config::default(), by, value).unwrap();
            for read in [file, snake] {
                assert_eq!(
                    get_key(&cfg, read).unwrap().as_deref(),
                    Some(printed.as_str()),
                    "set {by}, get {read}"
                );
            }
            let cleared = unset_key(&cfg, file).unwrap();
            assert_eq!(cleared, Config::default(), "{by}: nothing is left behind");
        }
    }
    // The file shows the table keys under their tables and the version while one is set; the legacy keys need neither.
    let goal = set_key(&Config::default(), "run.goal", "off").unwrap();
    let text = render_config(&goal);
    assert!(
        text.contains("configVersion = 1")
            && text.contains("[run]")
            && text.contains("goal = false"),
        "{text}"
    );
    assert_eq!(parse_config(&text).unwrap(), goal);
    let legacy = set_key(&Config::default(), "region", "us-east-1").unwrap();
    assert_eq!(render_config(&legacy), "region = \"us-east-1\"\n");
    // Two keys of one table share it; unsetting one keeps the other and the version; unsetting the last drops both.
    let two = set_key(&goal, "run.rules", "off").unwrap();
    let one = unset_key(&two, "run.goal").unwrap();
    assert_eq!(
        get_key(&one, "run.rules").unwrap().as_deref(),
        Some("false")
    );
    assert_eq!(one.config_version, Some(1));
    let none = unset_key(&one, "run.rules").unwrap();
    assert_eq!((none.config_version, none.run), (None, None));
    // A key that does not exist names the choices, dotted names included.
    for f in [
        set_key(&Config::default(), "run.nope", "on").map(|_| ()),
        unset_key(&Config::default(), "ui").map(|_| ()),
        get_key(&Config::default(), "run_goal").map(|_| ()),
    ] {
        match f.unwrap_err() {
            ConfigError::UnknownKey { known, .. } => {
                for k in ["run.goal", "research.enabled", "ui.bell", "default_model"] {
                    assert!(known.iter().any(|x| x == k), "{k} in {known:?}");
                }
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn a_bad_value_keeps_the_v4_02_wording_and_new_keys_say_on_or_off() {
    let reason_of =
        |key: &str, value: &str| match set_key(&Config::default(), key, value).unwrap_err() {
            ConfigError::Invalid { key: k, reason } => (k, reason),
            other => panic!("{key} {value:?}: {other:?}"),
        };
    // The words v4.02 prints for its own keys.
    let (k, r) = reason_of("mouse", "maybe");
    assert_eq!(k, "mouse");
    assert_eq!(r, "takes true, false or auto, got \"maybe\"");
    let (k, r) = reason_of("input_wait_minutes", "241");
    assert_eq!(k, "input_wait_minutes");
    assert_eq!(
        r,
        "takes whole minutes from 0 (do not wait) to 240, got \"241\""
    );
    let (k, r) = reason_of("report_tests", "maybe");
    assert_eq!(
        (k.as_str(), r.as_str()),
        ("report_tests", "takes on or off, got \"maybe\"")
    );
    let (_, r) = reason_of("region", "a\nb");
    assert_eq!(r, "takes one line of text with no control characters");
    // The new switches use the report switch's words.
    for key in [
        "run.goal",
        "run.rules",
        "run.skills",
        "research.enabled",
        "ui.notify",
        "ui.bell",
    ] {
        let (k, r) = reason_of(key, "maybe");
        assert_eq!(k, key);
        assert_eq!(r, "takes on or off, got \"maybe\"", "{key}");
        for good in ["true", "ON", "yes", "1", "false", "Off", "no", "0"] {
            assert!(
                set_key(&Config::default(), key, good).is_ok(),
                "{key} {good}"
            );
        }
    }
    // A refused value changes nothing.
    let before = all_set();
    assert!(set_key(&before, "run.goal", "perhaps").is_err());
    assert_eq!(before, all_set());
}
