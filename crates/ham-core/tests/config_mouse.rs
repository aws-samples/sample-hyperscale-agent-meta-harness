//! The `mouse` config key: `mouse = false` in `config.toml` keeps the TUI from asking the terminal for
//! mouse reports. The file takes a boolean or the word `auto`; `ham config set mouse <true|false|auto>` stores the same.

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::config::{
    Config, ConfigError, KEYS, MouseMode, get_key, parse_config, render_config, set_key, unset_key,
};

#[test]
fn a_boolean_in_the_file_is_on_or_off() {
    assert_eq!(
        parse_config("mouse = false\n").unwrap().mouse,
        Some(MouseMode::Off)
    );
    assert_eq!(
        parse_config("mouse = true\n").unwrap().mouse,
        Some(MouseMode::On)
    );
}

#[test]
fn the_word_auto_is_the_third_setting_and_nothing_else_is_taken() {
    assert_eq!(
        parse_config("mouse = \"auto\"\n").unwrap().mouse,
        Some(MouseMode::Auto)
    );
    for bad in [
        "mouse = \"sometimes\"\n",
        "mouse = 1\n",
        "mouse = \"\"\n",
        "mouse = [true]\n",
    ] {
        let err = parse_config(bad).unwrap_err();
        assert!(
            err.to_string().contains("mouse"),
            "{bad:?} gave {err} which does not name the key"
        );
    }
}

#[test]
fn a_file_without_the_key_has_no_setting() {
    assert_eq!(
        parse_config("region = \"us-west-2\"\n").unwrap().mouse,
        None
    );
    assert_eq!(Config::default().mouse, None);
}

#[test]
fn the_setting_is_written_as_a_boolean_or_auto_and_reads_back() {
    for (mode, line) in [
        (MouseMode::Off, "mouse = false"),
        (MouseMode::On, "mouse = true"),
        (MouseMode::Auto, "mouse = \"auto\""),
    ] {
        let cfg = Config {
            mouse: Some(mode),
            ..Config::default()
        };
        let text = render_config(&cfg);
        assert!(text.contains(line), "{line:?} missing from {text:?}");
        assert_eq!(parse_config(&text).unwrap(), cfg);
    }
    assert!(!render_config(&Config::default()).contains("mouse"));
}

#[test]
fn ham_config_sets_gets_and_unsets_the_key_in_either_spelling_of_the_value() {
    let none = Config::default();
    for (given, want_mode, want_text) in [
        ("false", MouseMode::Off, "false"),
        ("off", MouseMode::Off, "false"),
        ("no", MouseMode::Off, "false"),
        ("true", MouseMode::On, "true"),
        ("on", MouseMode::On, "true"),
        ("AUTO", MouseMode::Auto, "auto"),
    ] {
        let set = set_key(&none, "mouse", given).unwrap();
        assert_eq!(set.mouse, Some(want_mode), "set {given}");
        assert_eq!(
            get_key(&set, "mouse").unwrap().as_deref(),
            Some(want_text),
            "get after {given}"
        );
    }
    assert_eq!(get_key(&none, "mouse").unwrap(), None);
    let on = set_key(&none, "mouse", "true").unwrap();
    assert_eq!(unset_key(&on, "mouse").unwrap().mouse, None);
}

#[test]
fn a_bad_value_is_refused_with_the_key_and_the_choices() {
    let err = set_key(&Config::default(), "mouse", "maybe").unwrap_err();
    let ConfigError::Invalid { key, reason } = &err else {
        panic!("expected Invalid, got {err:?}");
    };
    assert_eq!(key, "mouse");
    for word in ["true", "false", "auto", "maybe"] {
        assert!(reason.contains(word), "{reason:?} lacks {word:?}");
    }
}

#[test]
fn the_key_list_names_mouse() {
    assert!(KEYS.contains(&("mouse", "mouse")), "{KEYS:?}");
}
