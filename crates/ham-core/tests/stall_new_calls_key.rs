//! The allowance of calls that bring something new as a client setting: the key `run.stall_new_calls` of `[run]` in
//! `config.toml`, which rides in the request's `goal` member as `newCalls`. A key that is not set leaves the member out, so a request that keeps the
//! default is byte for byte what it was; 0 counts every call, as the brake did before the allowance; the default and the bounds are the protocol's own
//! constants, so the client, the control plane and agentd cannot drift apart.

#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, DirectArgs, parse};
use ham_core::config::{Config, KEYS, get_key, parse_config, set_key, unset_key};
use ham_core::request::{DirectInputs, Origin, resolve_run_settings};
use ham_core::settings::{DefaultValue, Group, Key, Kind};
use harness_protocol::direct::{GOAL_NEW_CALLS, GoalOptions, check_goal};
use serde_json::{Value, json};

const PROMPT: &str = "Add a percent_change(a, b) function";

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
fn the_row_follows_the_compute_row_and_rides_in_the_goal_member() {
    assert_eq!(Key::ALL.len(), 35);
    assert_eq!(Key::ALL[24], Key::RunCompute);
    assert_eq!(Key::ALL[25], Key::RunStallNewCalls);
    let r = Key::RunStallNewCalls.row();
    assert_eq!(r.name, "run.stall_new_calls");
    assert_eq!(r.file, ("run", "stall_new_calls"));
    assert_eq!(
        (r.wire, r.group, r.since),
        (Some("goal"), Group::Limits, 1),
        "{r:?}"
    );
    assert!(
        r.flag.is_empty() && r.env.is_empty(),
        "a setting of the file only"
    );
    assert_eq!(
        Key::lookup("run.stall_new_calls"),
        Some(Key::RunStallNewCalls)
    );
    assert!(KEYS.contains(&("run.stall_new_calls", "run.stall_new_calls")));
}

#[test]
fn the_default_and_the_bounds_are_the_protocols_own_numbers() {
    let r = Key::RunStallNewCalls.row();
    assert_eq!(r.default, DefaultValue::Number(GOAL_NEW_CALLS.default));
    assert_eq!(GOAL_NEW_CALLS.default, 100);
    assert_eq!(r.default_text, "100 calls");
    // 0 is the user's word for "count every call"; the protocol range is that of a value other than 0
    match r.kind {
        Kind::Number { min, max, unit } => {
            assert_eq!((min, max, unit), (0, GOAL_NEW_CALLS.max, "calls"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.kind.type_text(), "whole calls, 0 to 1000");
    assert!(
        r.help.contains("count every call") || r.help.contains("counts every call"),
        "{}",
        r.help
    );
}

#[test]
fn ham_config_sets_prints_and_unsets_the_key_and_refuses_what_is_out_of_range() {
    let cfg = set_key(&Config::default(), "run.stall_new_calls", "250").unwrap();
    assert_eq!(
        get_key(&cfg, "run.stall_new_calls").unwrap().as_deref(),
        Some("250")
    );
    assert_eq!(
        cfg.config_version,
        Some(1),
        "a key of a table opens the one-way door"
    );
    let cfg = set_key(&cfg, "run.stall_new_calls", "0").unwrap();
    assert_eq!(
        get_key(&cfg, "run.stall_new_calls").unwrap().as_deref(),
        Some("0")
    );
    let cfg = unset_key(&cfg, "run.stall_new_calls").unwrap();
    assert_eq!(get_key(&cfg, "run.stall_new_calls").unwrap(), None);
    for bad in ["1001", "-1", "many", "1.5"] {
        let e = set_key(&Config::default(), "run.stall_new_calls", bad)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("run.stall_new_calls") && e.contains("takes a whole number"),
            "{bad}: {e}"
        );
    }
    let e = set_key(&Config::default(), "run.stall_new_calls", "1001")
        .unwrap_err()
        .to_string();
    assert!(e.contains("calls from 0 to 1000"), "{e}");
}

#[test]
fn the_file_reader_takes_the_key_and_names_the_key_of_a_wrong_one() {
    let cfg = cfg_of("configVersion = 1\n[run]\nstall_new_calls = 20\n");
    assert_eq!(cfg.run.unwrap().stall_new_calls, Some(20));
    for text in [
        "[run]\nstall_new_calls = 1001\n",
        "[run]\nstall_new_calls = \"20\"\n",
        "[run]\nstall_new_calls = -3\n",
    ] {
        let e = parse_config(text).unwrap_err().to_string();
        assert!(e.contains("stall_new_calls"), "{text}: {e}");
    }
}

#[test]
fn a_run_with_the_key_unset_sends_no_goal_member_as_it_always_did() {
    assert!(request(&Config::default(), &[]).get("goal").is_none());
    let wire = request(&cfg_of("[run]\nrules = true\n"), &[]);
    assert!(wire.get("goal").is_none(), "{wire}");
}

#[test]
fn the_key_becomes_the_new_calls_member_and_zero_is_sent_as_zero() {
    let cfg = cfg_of("configVersion = 1\n[run]\nstall_new_calls = 20\n");
    assert_eq!(
        request(&cfg, &[])["goal"],
        json!({"enabled": true, "newCalls": 20})
    );
    let cfg = cfg_of("configVersion = 1\n[run]\nstall_new_calls = 0\n");
    assert_eq!(
        request(&cfg, &[])["goal"],
        json!({"enabled": true, "newCalls": 0})
    );
    let cfg = cfg_of(
        "configVersion = 1\n[run]\nstall_calls = 45\nstall_window_minutes = 30\nstall_new_calls = 300\n",
    );
    assert_eq!(
        request(&cfg, &[])["goal"],
        json!({"enabled": true, "roundCalls": 45, "roundWindowS": 1800, "newCalls": 300})
    );
}

#[test]
fn a_loop_that_is_switched_off_carries_no_allowance() {
    let cfg = cfg_of("configVersion = 1\n[run]\ngoal = false\nstall_new_calls = 20\n");
    assert_eq!(request(&cfg, &[])["goal"], json!({"enabled": false}));
    let cfg = cfg_of("configVersion = 1\n[run]\nstall_new_calls = 20\n");
    assert_eq!(
        request(&cfg, &["--goal", "off"])["goal"],
        json!({"enabled": false})
    );
}

#[test]
fn every_value_the_client_accepts_passes_the_check_the_control_plane_and_agentd_run() {
    for n in [0u32, 1, 100, 1000] {
        let cfg = set_key(&Config::default(), "run.stall_new_calls", &n.to_string()).unwrap();
        let wire = request(&cfg, &[]);
        let goal: GoalOptions = serde_json::from_value(wire["goal"].clone()).unwrap();
        assert_eq!(goal.new_calls, Some(n));
        check_goal(&goal).unwrap_or_else(|e| panic!("{n}: {e}"));
    }
}

#[test]
fn where_the_value_came_from_is_the_file_when_it_is_set_and_the_default_when_it_is_not() {
    let run = resolve_run_settings(&Config::default(), &|_| None, &args(&[]), false).unwrap();
    assert_eq!(run.origin(Key::RunStallNewCalls), Some(Origin::Default));
    let cfg = cfg_of("configVersion = 1\n[run]\nstall_new_calls = 20\n");
    let run = resolve_run_settings(&cfg, &|_| None, &args(&[]), false).unwrap();
    assert_eq!(run.origin(Key::RunStallNewCalls), Some(Origin::File));
}
