//! The stall limits of the goal loop as client settings: five keys of `[run]` in `config.toml` -- `stall_calls`,
//! `stall_window_minutes`, `stall_min_calls`, `command_grace_minutes` and `final_message` -- that ride in the request's `goal` member. A key that is
//! not set leaves its member out, so a request that keeps the defaults is byte for byte what it was; the defaults and the bounds are the
//! protocol's own constants, so the client, the control plane and agentd cannot drift apart.

#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, DirectArgs, parse};
use ham_core::config::{Config, get_key, parse_config, set_key};
use ham_core::request::{DirectInputs, Origin, resolve_run_settings};
use ham_core::settings::{DefaultValue, Group, Key, Kind, Row};
use harness_protocol::direct::{
    GOAL_COMMAND_GRACE_S, GOAL_FINAL_MESSAGE, GOAL_ROUND_CALLS, GOAL_ROUND_WINDOW_S, check_goal,
};
use serde_json::{Value, json};

const PROMPT: &str = "Add a percent_change(a, b) function";

const KEYS: [Key; 5] = [
    Key::RunStallCalls,
    Key::RunStallWindowMinutes,
    Key::RunStallMinCalls,
    Key::RunCommandGraceMinutes,
    Key::RunFinalMessage,
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

fn row(k: Key) -> &'static Row {
    k.row()
}

#[test]
fn the_five_rows_are_keys_of_the_run_table_that_ride_in_the_goal_member() {
    let names: Vec<_> = KEYS.iter().map(|k| row(*k).name).collect();
    assert_eq!(
        names,
        [
            "run.stall_calls",
            "run.stall_window_minutes",
            "run.stall_min_calls",
            "run.command_grace_minutes",
            "run.final_message"
        ]
    );
    for k in KEYS {
        let r = row(k);
        assert_eq!(
            (r.file.0, r.wire, r.group, r.since),
            ("run", Some("goal"), Group::Limits, 1),
            "{k:?}"
        );
        assert!(
            r.flag.is_empty() && r.env.is_empty(),
            "{k:?}: a setting of the file only"
        );
        assert_eq!(Key::lookup(r.name), Some(k));
        assert_eq!(Key::lookup(&format!("run.{}", r.file.1)), Some(k));
    }
}

#[test]
fn the_defaults_and_bounds_are_the_protocols_own_numbers() {
    let number = |k: Key| match (row(k).kind, row(k).default) {
        (Kind::Number { min, max, .. }, DefaultValue::Number(d)) => (min, max, d),
        other => panic!("{k:?}: {other:?}"),
    };
    assert_eq!(
        number(Key::RunStallCalls),
        (
            GOAL_ROUND_CALLS.min,
            GOAL_ROUND_CALLS.max,
            GOAL_ROUND_CALLS.default
        )
    );
    assert_eq!(number(Key::RunStallMinCalls), (1, 100, 10));
    assert_eq!(number(Key::RunStallWindowMinutes), (1, 240, 20));
    assert_eq!(GOAL_ROUND_WINDOW_S.default, 20 * 60);
    assert_eq!(number(Key::RunCommandGraceMinutes), (0, 1440, 120));
    assert_eq!(GOAL_COMMAND_GRACE_S.default, 120 * 60);
    assert_eq!(
        row(Key::RunFinalMessage).default,
        DefaultValue::Bool(GOAL_FINAL_MESSAGE)
    );
    assert_eq!(
        row(Key::RunStallCalls).kind.type_text(),
        "whole calls, 5 to 1000"
    );
    assert_eq!(
        row(Key::RunStallWindowMinutes).kind.type_text(),
        "whole minutes, 1 to 240"
    );
}

#[test]
fn ham_config_sets_and_prints_the_keys_and_refuses_what_is_out_of_range() {
    let cfg = set_key(&Config::default(), "run.stall_calls", "45").unwrap();
    assert_eq!(
        get_key(&cfg, "run.stall_calls").unwrap().as_deref(),
        Some("45")
    );
    assert_eq!(
        cfg.config_version,
        Some(1),
        "a key of a table opens the one-way door"
    );
    let cfg = set_key(&cfg, "run.command_grace_minutes", "0").unwrap();
    assert_eq!(
        get_key(&cfg, "run.command_grace_minutes")
            .unwrap()
            .as_deref(),
        Some("0")
    );
    let cfg = set_key(&cfg, "run.final_message", "off").unwrap();
    assert_eq!(
        get_key(&cfg, "run.final_message").unwrap().as_deref(),
        Some("false")
    );
    for (key, bad) in [
        ("run.stall_calls", "4"),
        ("run.stall_calls", "1001"),
        ("run.stall_calls", "many"),
        ("run.stall_window_minutes", "0"),
        ("run.stall_window_minutes", "241"),
        ("run.stall_min_calls", "0"),
        ("run.command_grace_minutes", "1441"),
        ("run.command_grace_minutes", "-1"),
    ] {
        let e = set_key(&Config::default(), key, bad)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains(key) && e.contains("takes a whole number"),
            "{key}={bad}: {e}"
        );
    }
    let e = set_key(&Config::default(), "run.stall_calls", "4")
        .unwrap_err()
        .to_string();
    assert!(e.contains("calls from 5 to 1000"), "{e}");
}

#[test]
fn the_file_reader_takes_the_keys_and_names_the_key_and_the_range_of_a_wrong_one() {
    let cfg = cfg_of(
        "configVersion = 1\n[run]\nstall_calls = 45\nstall_window_minutes = 30\nstall_min_calls = 12\ncommand_grace_minutes = 90\nfinal_message = false\n",
    );
    let run = cfg.run.unwrap();
    assert_eq!(
        (
            run.stall_calls,
            run.stall_window_minutes,
            run.stall_min_calls,
            run.command_grace_minutes,
            run.final_message
        ),
        (Some(45), Some(30), Some(12), Some(90), Some(false))
    );
    for (text, key) in [
        ("[run]\nstall_calls = 3\n", "stall_calls"),
        (
            "[run]\nstall_window_minutes = 500\n",
            "stall_window_minutes",
        ),
        ("[run]\nstall_calls = \"45\"\n", "stall_calls"),
        ("[run]\nfinal_message = 3\n", "final_message"),
    ] {
        let e = parse_config(text).unwrap_err().to_string();
        assert!(e.contains(key), "{text}: {e}");
    }
}

#[test]
fn a_run_with_none_of_the_keys_sends_no_goal_member_as_it_always_did() {
    let wire = request(&Config::default(), &[]);
    assert!(wire.get("goal").is_none(), "{wire}");
    let wire = request(&cfg_of("[run]\nrules = true\n"), &[]);
    assert!(wire.get("goal").is_none(), "{wire}");
}

#[test]
fn the_keys_that_are_set_become_seconds_in_the_goal_member_and_the_others_stay_out() {
    let cfg = cfg_of(
        "configVersion = 1\n[run]\nstall_window_minutes = 30\ncommand_grace_minutes = 90\nfinal_message = false\n",
    );
    let wire = request(&cfg, &[]);
    assert_eq!(
        wire["goal"],
        json!({"enabled": true, "roundWindowS": 1800, "commandGraceS": 5400, "finalMessage": false})
    );
    let cfg = cfg_of(
        "configVersion = 1\n[run]\nstall_calls = 45\nstall_min_calls = 12\ncommand_grace_minutes = 0\n",
    );
    assert_eq!(
        request(&cfg, &[])["goal"],
        json!({"enabled": true, "roundCalls": 45, "roundMinCalls": 12, "commandGraceS": 0})
    );
}

#[test]
fn a_loop_that_is_switched_off_carries_no_limit() {
    let cfg = cfg_of("configVersion = 1\n[run]\ngoal = false\nstall_calls = 45\n");
    assert_eq!(request(&cfg, &[])["goal"], json!({"enabled": false}));
    let cfg = cfg_of("configVersion = 1\n[run]\nstall_calls = 45\n");
    assert_eq!(
        request(&cfg, &["--goal", "off"])["goal"],
        json!({"enabled": false})
    );
    assert_eq!(
        request(&cfg, &["--goal", "on"])["goal"],
        json!({"enabled": true, "roundCalls": 45})
    );
}

#[test]
fn every_value_the_client_accepts_passes_the_check_the_control_plane_and_agentd_run() {
    // the lowest and the highest value of each number, set through `ham config`, as a request
    for key in [
        "run.stall_calls",
        "run.stall_window_minutes",
        "run.stall_min_calls",
        "run.command_grace_minutes",
    ] {
        let Kind::Number { min, max, .. } = Key::lookup(key).unwrap().row().kind else {
            panic!("{key} is a number");
        };
        for n in [min, max] {
            let cfg = set_key(&Config::default(), key, &n.to_string()).unwrap();
            let wire = request(&cfg, &[]);
            let goal: harness_protocol::direct::GoalOptions =
                serde_json::from_value(wire["goal"].clone()).unwrap();
            check_goal(&goal).unwrap_or_else(|e| panic!("{key}={n}: {e}"));
        }
    }
}

#[test]
fn where_a_value_came_from_is_the_file_when_it_is_set_and_the_default_when_it_is_not() {
    let run = resolve_run_settings(&Config::default(), &|_| None, &args(&[]), false).unwrap();
    for k in KEYS {
        assert_eq!(run.origin(k), Some(Origin::Default), "{k:?}");
    }
    let cfg = cfg_of("configVersion = 1\n[run]\nstall_calls = 45\nfinal_message = true\n");
    let run = resolve_run_settings(&cfg, &|_| None, &args(&[]), false).unwrap();
    assert_eq!(run.origin(Key::RunStallCalls), Some(Origin::File));
    assert_eq!(run.origin(Key::RunFinalMessage), Some(Origin::File));
    assert_eq!(run.origin(Key::RunStallMinCalls), Some(Origin::Default));
}
