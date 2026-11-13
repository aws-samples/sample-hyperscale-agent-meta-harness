//! Wave 2, rows C9, C30 and C35 as client settings: `run.max_interrupts`, `run.max_questions`, `run.max_fresh_sessions` of `[run]` and
//! `research.max_file_mib`, `research.max_unpacked_mib`, `research.max_run_mib` of `[research]` ride in the request's `limits` member. A key that is
//! not set leaves the member out, so a request that keeps the defaults is byte for byte what it was; the defaults and the bounds are the protocol's own
//! constants, so the client, the control plane and agentd cannot drift apart.

#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, DirectArgs, parse};
use ham_core::config::{Config, KEYS, get_key, parse_config, set_key, unset_key};
use ham_core::request::{DirectInputs, Origin, resolve_run_settings};
use ham_core::settings::{DefaultValue, Group, Key, Kind};
use harness_protocol::direct::{
    LIMIT_FRESH_SESSIONS, LIMIT_INTERRUPTS, LIMIT_QUESTIONS, LIMIT_RESEARCH_FILE_BYTES,
    LIMIT_RESEARCH_RUN_BYTES, LIMIT_RESEARCH_UNPACKED_BYTES, LimitRange, RunLimits, check_limits,
};
use serde_json::{Value, json};

const PROMPT: &str = "Add a percent_change(a, b) function";
const MIB: u64 = 1024 * 1024;

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

fn request(cfg: &Config) -> Value {
    let run = resolve_run_settings(cfg, &|_| None, &args(&[]), false).unwrap();
    serde_json::to_value(run.request(DirectInputs::default()).unwrap()).unwrap()
}

/// The six keys with their file names, protocol ranges and the unit the user reads.
fn rows() -> [(Key, &'static str, LimitRange, u64); 6] {
    [
        (
            Key::RunMaxInterrupts,
            "run.max_interrupts",
            LIMIT_INTERRUPTS,
            1,
        ),
        (
            Key::RunMaxQuestions,
            "run.max_questions",
            LIMIT_QUESTIONS,
            1,
        ),
        (
            Key::RunMaxFreshSessions,
            "run.max_fresh_sessions",
            LIMIT_FRESH_SESSIONS,
            1,
        ),
        (
            Key::ResearchMaxFileMib,
            "research.max_file_mib",
            LIMIT_RESEARCH_FILE_BYTES,
            MIB,
        ),
        (
            Key::ResearchMaxUnpackedMib,
            "research.max_unpacked_mib",
            LIMIT_RESEARCH_UNPACKED_BYTES,
            MIB,
        ),
        (
            Key::ResearchMaxRunMib,
            "research.max_run_mib",
            LIMIT_RESEARCH_RUN_BYTES,
            MIB,
        ),
    ]
}

#[test]
fn b776w_c9_the_rows_are_in_the_registry_and_ride_in_the_limits_member() {
    for (key, name, _, _) in rows() {
        let r = key.row();
        assert_eq!(r.name, name);
        assert_eq!((r.wire, r.since), (Some("limits"), 1), "{name}: {r:?}");
        assert_eq!(r.group, Group::Limits, "{name}");
        assert!(
            r.flag.is_empty() && r.env.is_empty(),
            "{name}: a setting of the file only"
        );
        assert_eq!(Key::lookup(name), Some(key), "{name}");
        assert!(KEYS.contains(&(name, name)), "{name}");
        assert!(Key::ALL.contains(&key), "{name}");
    }
}

#[test]
fn b776w_c9_the_defaults_and_the_bounds_are_the_protocols_own_numbers() {
    for (key, name, range, unit) in rows() {
        let r = key.row();
        let Kind::Number { min, max, .. } = r.kind else {
            panic!("{name}: {:?}", r.kind);
        };
        // The byte budgets read in MiB, rounded up at the low end so the key never admits a number the protocol refuses.
        assert_eq!(u64::from(max) * unit, range.max, "{name} max");
        assert_eq!(
            u64::from(min) * unit,
            range.min.div_ceil(unit) * unit,
            "{name} min"
        );
        let DefaultValue::Number(d) = r.default else {
            panic!("{name}: {:?}", r.default);
        };
        assert_eq!(u64::from(d) * unit, range.default, "{name} default");
    }
}

#[test]
fn b776w_c9_ham_config_sets_prints_and_unsets_each_key_and_refuses_what_is_out_of_range() {
    for (_, name, _, _) in rows() {
        let cfg = set_key(&Config::default(), name, "7").unwrap();
        assert_eq!(get_key(&cfg, name).unwrap().as_deref(), Some("7"), "{name}");
        assert_eq!(
            cfg.config_version,
            Some(1),
            "{name}: a key of a table opens the one-way door"
        );
        let cfg = unset_key(&cfg, name).unwrap();
        assert_eq!(get_key(&cfg, name).unwrap(), None, "{name}");
        for bad in ["2049", "-1", "many", "1.5"] {
            let e = set_key(&Config::default(), name, bad)
                .unwrap_err()
                .to_string();
            assert!(
                e.contains(name) && e.contains("takes a whole number"),
                "{name} {bad}: {e}"
            );
        }
    }
    // 0 is a word of the three counts and below the minimum of the byte budgets.
    for name in [
        "run.max_interrupts",
        "run.max_questions",
        "run.max_fresh_sessions",
    ] {
        assert!(set_key(&Config::default(), name, "0").is_ok(), "{name}");
    }
    for name in [
        "research.max_file_mib",
        "research.max_unpacked_mib",
        "research.max_run_mib",
    ] {
        assert!(set_key(&Config::default(), name, "0").is_err(), "{name}");
    }
}

#[test]
fn b776w_c9_the_file_reader_takes_the_keys_and_names_the_key_of_a_wrong_one() {
    let cfg = cfg_of(
        "configVersion = 1\n[run]\nmax_interrupts = 20\nmax_questions = 6\nmax_fresh_sessions = 4\n[research]\nmax_file_mib = 4\nmax_unpacked_mib = 500\nmax_run_mib = 900\n",
    );
    let run = cfg.run.as_ref().unwrap();
    assert_eq!(
        (
            run.max_interrupts,
            run.max_questions,
            run.max_fresh_sessions
        ),
        (Some(20), Some(6), Some(4))
    );
    let research = cfg.research.as_ref().unwrap();
    assert_eq!(
        (
            research.max_file_mib,
            research.max_unpacked_mib,
            research.max_run_mib
        ),
        (Some(4), Some(500), Some(900))
    );
    for text in [
        "[run]\nmax_interrupts = 1001\n",
        "[run]\nmax_questions = \"3\"\n",
        "[research]\nmax_file_mib = 65\n",
        "[research]\nmax_run_mib = 0\n",
    ] {
        let e = parse_config(text).unwrap_err().to_string();
        assert!(e.contains("max_"), "{text}: {e}");
    }
}

#[test]
fn b776w_c9_a_run_with_no_key_set_sends_no_limits_member_as_it_always_did() {
    assert!(request(&Config::default()).get("limits").is_none());
    let wire = request(&cfg_of(
        "configVersion = 1\n[run]\nrules = true\n[research]\nenabled = true\n",
    ));
    assert!(wire.get("limits").is_none(), "{wire}");
}

#[test]
fn b776w_c9_the_keys_become_the_members_and_a_mib_is_a_mebibyte_of_bytes() {
    let cfg = cfg_of(
        "configVersion = 1\n[run]\nmax_interrupts = 20\nmax_questions = 0\nmax_fresh_sessions = 4\n[research]\nmax_file_mib = 4\nmax_unpacked_mib = 500\nmax_run_mib = 900\n",
    );
    assert_eq!(
        request(&cfg)["limits"],
        json!({
            "interrupts": 20, "questions": 0, "freshSessions": 4,
            "researchFileBytes": 4 * MIB, "researchUnpackedBytes": 500 * MIB, "researchRunBytes": 900 * MIB
        })
    );
    // One key sends one member.
    let one = cfg_of("configVersion = 1\n[run]\nmax_interrupts = 9\n");
    assert_eq!(request(&one)["limits"], json!({"interrupts": 9}));
}

#[test]
fn b776w_c9_every_value_the_client_accepts_passes_the_check_the_control_plane_and_agentd_run() {
    for (key, name, range, unit) in rows() {
        let Kind::Number { min, max, .. } = key.row().kind else {
            panic!("{name}");
        };
        for n in [min, max] {
            let cfg = set_key(&Config::default(), name, &n.to_string()).unwrap();
            let limits: RunLimits =
                serde_json::from_value(request(&cfg)["limits"].clone()).unwrap();
            check_limits(&limits).unwrap_or_else(|e| panic!("{name} {n}: {e}"));
            assert!(range.contains(u64::from(n) * unit), "{name} {n}");
        }
    }
}

#[test]
fn b776w_c9_where_the_value_came_from_is_the_file_when_it_is_set_and_the_default_when_it_is_not() {
    let run = resolve_run_settings(&Config::default(), &|_| None, &args(&[]), false).unwrap();
    let cfg =
        cfg_of("configVersion = 1\n[run]\nmax_questions = 5\n[research]\nmax_run_mib = 600\n");
    let set = resolve_run_settings(&cfg, &|_| None, &args(&[]), false).unwrap();
    for (key, name, _, _) in rows() {
        assert_eq!(run.origin(key), Some(Origin::Default), "{name}");
        let want = if matches!(key, Key::RunMaxQuestions | Key::ResearchMaxRunMib) {
            Origin::File
        } else {
            Origin::Default
        };
        assert_eq!(set.origin(key), Some(want), "{name}");
    }
}
