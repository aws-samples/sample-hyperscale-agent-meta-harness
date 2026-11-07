//! The `input_wait_minutes` key of `config.toml`: how long an interactive `ham` asks the run to wait
//! for the user's answer when Cline needs input. It is a whole number of minutes from 0 (no wait) to 240.
#![allow(clippy::unwrap_used)]

use ham_core::config::{
    Config, ConfigError, KEYS, get_key, parse_config, render_config, set_key, unset_key,
};

#[test]
fn the_key_is_listed_in_both_spellings() {
    assert!(
        KEYS.contains(&("inputWaitMinutes", "input_wait_minutes")),
        "{KEYS:?}"
    );
    assert_eq!(KEYS.len(), 35);
}

#[test]
fn set_get_and_unset_round_trip_a_number() {
    let cfg = set_key(&Config::default(), "input_wait_minutes", "10").unwrap();
    assert_eq!(cfg.input_wait_minutes, Some(10));
    assert_eq!(
        get_key(&cfg, "input_wait_minutes").unwrap().as_deref(),
        Some("10")
    );
    assert_eq!(
        get_key(&cfg, "inputWaitMinutes").unwrap().as_deref(),
        Some("10")
    );
    let text = render_config(&cfg);
    assert!(text.contains("inputWaitMinutes = 10"), "{text}");
    assert_eq!(parse_config(&text).unwrap(), cfg);
    let cfg = unset_key(&cfg, "inputWaitMinutes").unwrap();
    assert_eq!(cfg.input_wait_minutes, None);
    assert_eq!(get_key(&cfg, "input_wait_minutes").unwrap(), None);
    assert!(!render_config(&cfg).contains("inputWait"));
}

#[test]
fn zero_is_a_value_that_means_no_wait() {
    let cfg = set_key(&Config::default(), "input_wait_minutes", "0").unwrap();
    assert_eq!(cfg.input_wait_minutes, Some(0));
    assert_eq!(
        get_key(&cfg, "input_wait_minutes").unwrap().as_deref(),
        Some("0")
    );
    assert!(render_config(&cfg).contains("inputWaitMinutes = 0"));
}

#[test]
fn the_owners_snake_case_spelling_is_read_from_the_file() {
    let cfg = parse_config("input_wait_minutes = 15\n").unwrap();
    assert_eq!(cfg.input_wait_minutes, Some(15));
}

#[test]
fn values_outside_zero_to_240_or_not_whole_numbers_are_refused_with_the_reason() {
    for bad in ["241", "-1", "ten", "1.5", "", " ", "1 0"] {
        match set_key(&Config::default(), "input_wait_minutes", bad) {
            Err(ConfigError::Invalid { key, reason }) => {
                assert_eq!(key, "input_wait_minutes", "{bad}");
                assert!(
                    reason.contains("0") && reason.contains("240"),
                    "{bad}: {reason}"
                );
            }
            other => panic!("{bad:?}: {other:?}"),
        }
    }
    assert!(set_key(&Config::default(), "input_wait_minutes", " 240 ").is_ok());
    // A file with a text value or a number past the range is refused when it is read.
    assert!(parse_config("inputWaitMinutes = \"ten\"\n").is_err());
    assert!(parse_config("inputWaitMinutes = -3\n").is_err());
    assert!(parse_config("inputWaitMinutes = 241\n").is_err());
    assert!(parse_config("inputWaitMinutes = 240\n").is_ok());
}

#[test]
fn an_unset_key_is_not_written_and_old_files_still_parse() {
    assert!(!render_config(&Config::default()).contains("inputWait"));
    let old = "region = \"us-east-1\"\ndefaultModel = \"global.anthropic.claude-sonnet-5-5\"\n";
    let cfg = parse_config(old).unwrap();
    assert_eq!(cfg.input_wait_minutes, None);
    assert_eq!(render_config(&cfg), old);
}
