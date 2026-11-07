//! The switches for four defaults of the session: keys of `[run]` in `config.toml` that let a user turn a default off.
//!
//! - `run.cargo_no_debug_info`, `run.cargo_no_incremental` and `run.exclude_target_dir` ride in the request's `build` member;
//! - `run.outside_checkout_note` rides in the request's `goal` member beside the stall limits.
//!
//! All four are `true` by default. A key that is not set, or is set to `true`, leaves its member out, so a request that keeps the defaults is
//! byte for byte what it was; only `false` reaches the request.

#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, DirectArgs, parse};
use ham_core::config::{Config, get_key, parse_config, set_key, unset_key};
use ham_core::request::{DirectInputs, Origin, resolve_run_settings};
use ham_core::settings::{DefaultValue, Group, Key, Kind};
use serde_json::{Value, json};

const PROMPT: &str = "Add a percent_change(a, b) function";

const BUILD_KEYS: [Key; 3] = [
    Key::RunCargoNoDebugInfo,
    Key::RunCargoNoIncremental,
    Key::RunExcludeTargetDir,
];

fn args(extra: &[&str]) -> DirectArgs {
    let mut words = vec![PROMPT, "--s3-only", "--direct"];
    words.extend_from_slice(extra);
    let Command::StartDirect(a) = parse(&words).unwrap().command else {
        panic!("{words:?} is not a direct start");
    };
    a
}

fn cfg_of(text: &str) -> Config {
    parse_config(text).unwrap()
}

fn request(cfg: &Config, extra: &[&str]) -> Value {
    let run = resolve_run_settings(cfg, &|_| None, &args(extra), false).unwrap();
    serde_json::to_value(run.request(DirectInputs::default()).unwrap()).unwrap()
}

#[test]
fn the_four_rows_are_keys_of_the_run_table_that_default_to_on() {
    let want = [
        (Key::RunCargoNoDebugInfo, "run.cargo_no_debug_info", "build"),
        (
            Key::RunCargoNoIncremental,
            "run.cargo_no_incremental",
            "build",
        ),
        (Key::RunExcludeTargetDir, "run.exclude_target_dir", "build"),
        (
            Key::RunOutsideCheckoutNote,
            "run.outside_checkout_note",
            "goal",
        ),
    ];
    for (k, name, wire) in want {
        let r = k.row();
        assert_eq!(r.name, name);
        assert_eq!(r.kind, Kind::Bool, "{name}");
        assert_eq!(r.default, DefaultValue::Bool(true), "{name}");
        assert_eq!(r.default_text, "on", "{name}");
        assert_eq!(
            (r.file.0, r.wire, r.group, r.since),
            ("run", Some(wire), Group::Limits, 1),
            "{name}"
        );
        assert!(
            r.flag.is_empty() && r.env.is_empty(),
            "{name}: a setting of the file only"
        );
        assert!(!r.security, "{name}");
        assert_eq!(Key::lookup(name), Some(k));
        assert_eq!(Key::lookup(&format!("run.{}", r.file.1)), Some(k));
        assert!(!r.help.is_empty() && !r.label.is_empty(), "{name}");
    }
}

#[test]
fn the_help_of_each_row_says_what_the_default_does_and_what_off_restores() {
    let help = |k: Key| k.row().help;
    assert!(help(Key::RunCargoNoDebugInfo).contains("debug information"));
    assert!(help(Key::RunCargoNoDebugInfo).contains("CARGO_PROFILE_DEV_DEBUG"));
    assert!(help(Key::RunCargoNoIncremental).contains("CARGO_INCREMENTAL"));
    assert!(help(Key::RunExcludeTargetDir).contains("target/"));
    assert!(help(Key::RunOutsideCheckoutNote).contains("outside"));
    for k in [
        Key::RunCargoNoDebugInfo,
        Key::RunCargoNoIncremental,
        Key::RunExcludeTargetDir,
        Key::RunOutsideCheckoutNote,
    ] {
        assert!(help(k).contains("Off"), "{k:?}: {}", help(k));
    }
}

#[test]
fn ham_config_sets_prints_and_unsets_each_key() {
    for name in [
        "run.cargo_no_debug_info",
        "run.cargo_no_incremental",
        "run.exclude_target_dir",
        "run.outside_checkout_note",
    ] {
        let cfg = set_key(&Config::default(), name, "off").unwrap();
        assert_eq!(
            get_key(&cfg, name).unwrap().as_deref(),
            Some("false"),
            "{name}"
        );
        assert_eq!(
            cfg.config_version,
            Some(1),
            "{name}: a key of a table opens the one-way door"
        );
        let cfg = set_key(&cfg, name, "yes").unwrap();
        assert_eq!(
            get_key(&cfg, name).unwrap().as_deref(),
            Some("true"),
            "{name}"
        );
        let back = unset_key(&cfg, name).unwrap();
        assert_eq!(get_key(&back, name).unwrap(), None, "{name}");
        assert!(
            set_key(&Config::default(), name, "sometimes").is_err(),
            "{name}"
        );
    }
}

#[test]
fn a_user_who_sets_nothing_sends_no_build_member_and_no_note_switch() {
    let v = request(&Config::default(), &[]);
    assert!(v.get("build").is_none(), "{v}");
    assert!(v.get("goal").is_none(), "{v}");
    // Said out loud, `true` is the default and is not sent either.
    let cfg = cfg_of(
        "configVersion = 1\n\n[run]\ncargo_no_debug_info = true\ncargo_no_incremental = true\nexclude_target_dir = true\n",
    );
    assert!(request(&cfg, &[]).get("build").is_none());
}

#[test]
fn each_key_set_to_false_reaches_the_build_member_alone() {
    for (key, member) in [
        ("cargo_no_debug_info", "cargoNoDebugInfo"),
        ("cargo_no_incremental", "cargoNoIncremental"),
        ("exclude_target_dir", "excludeTargetDir"),
    ] {
        let cfg = cfg_of(&format!("configVersion = 1\n\n[run]\n{key} = false\n"));
        let v = request(&cfg, &[]);
        assert_eq!(v["build"], json!({ member: false }), "{key}");
        assert!(v.get("goal").is_none(), "{key}");
    }
    let all = cfg_of(
        "configVersion = 1\n\n[run]\ncargo_no_debug_info = false\ncargo_no_incremental = false\nexclude_target_dir = false\n",
    );
    assert_eq!(
        request(&all, &[])["build"],
        json!({"cargoNoDebugInfo": false, "cargoNoIncremental": false, "excludeTargetDir": false})
    );
}

#[test]
fn the_note_switch_rides_in_the_goal_member_and_a_goal_that_is_off_carries_none() {
    let cfg = cfg_of("configVersion = 1\n\n[run]\noutside_checkout_note = false\n");
    assert_eq!(
        request(&cfg, &[])["goal"],
        json!({"enabled": true, "outsideCheckoutNote": false})
    );
    // With the stall limits in the same member.
    let cfg =
        cfg_of("configVersion = 1\n\n[run]\noutside_checkout_note = false\nstall_calls = 45\n");
    assert_eq!(
        request(&cfg, &[])["goal"],
        json!({"enabled": true, "roundCalls": 45, "outsideCheckoutNote": false})
    );
    // The goal loop is off: nothing would read the switch.
    assert_eq!(
        request(&cfg, &["--goal", "off"])["goal"],
        json!({"enabled": false})
    );
}

#[test]
fn the_origin_of_each_key_is_the_file_or_the_default() {
    let none = resolve_run_settings(&Config::default(), &|_| None, &args(&[]), false).unwrap();
    let set = resolve_run_settings(
        &cfg_of(
            "configVersion = 1\n\n[run]\ncargo_no_debug_info = false\noutside_checkout_note = false\n",
        ),
        &|_| None,
        &args(&[]),
        false,
    )
    .unwrap();
    for k in BUILD_KEYS.into_iter().chain([Key::RunOutsideCheckoutNote]) {
        assert_eq!(none.origin(k), Some(Origin::Default), "{k:?}");
    }
    assert_eq!(set.origin(Key::RunCargoNoDebugInfo), Some(Origin::File));
    assert_eq!(set.origin(Key::RunOutsideCheckoutNote), Some(Origin::File));
    assert_eq!(
        set.origin(Key::RunCargoNoIncremental),
        Some(Origin::Default)
    );
}
