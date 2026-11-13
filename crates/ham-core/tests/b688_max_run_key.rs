//! The total time of a run as a client setting: the key run.max_run_hours of [run] in config.toml and the flag --max-run-hours <hours>
//! of a start, which ride in the request as maxRunS in seconds. A run that sets neither sends no member, so the request is byte for byte what an older
//! client sends and the stage's own limit (24 hours) applies; the bounds are the protocol's own constants, so the client, the control plane and the
//! state machine cannot drift apart.

#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, DirectArgs, parse};
use ham_core::config::{Config, KEYS, get_key, parse_config, set_key, unset_key};
use ham_core::request::{DirectInputs, Origin, resolve_run_settings};
use ham_core::settings::{DefaultValue, Group, Key, Kind, Order};
use harness_protocol::direct::{DEFAULT_MAX_RUN_S, MAX_MAX_RUN_S, MIN_MAX_RUN_S, max_run_s_ok};
use serde_json::Value;

const PROMPT: &str = "Add a percent_change(a, b) function";

fn parsed(extra: &[&str]) -> Result<DirectArgs, String> {
    let mut words = vec![PROMPT, "--s3-only", "--direct"];
    words.extend_from_slice(extra);
    match parse(&words) {
        Ok(inv) => match inv.command {
            Command::StartDirect(a) => Ok(a),
            other => panic!("{words:?} is not a direct start: {other:?}"),
        },
        Err(e) => Err(e.to_string()),
    }
}

fn args(extra: &[&str]) -> DirectArgs {
    parsed(extra).unwrap()
}

fn cfg_of(text: &str) -> Config {
    parse_config(text).unwrap()
}

fn request(cfg: &Config, extra: &[&str]) -> Value {
    let run = resolve_run_settings(cfg, &|_| None, &args(extra), false).unwrap();
    serde_json::to_value(run.request(DirectInputs::default()).unwrap()).unwrap()
}

#[test]
fn b688_the_row_is_the_last_key_and_rides_in_the_max_run_member() {
    assert_eq!(Key::ALL.len(), 46);
    assert_eq!(Key::ALL[32], Key::RunMaxRunHours);
    let r = Key::RunMaxRunHours.row();
    assert_eq!(r.name, "run.max_run_hours");
    assert_eq!(r.file, ("run", "max_run_hours"));
    assert_eq!(
        (r.wire, r.group, r.since, r.order),
        (Some("maxRunS"), Group::Limits, 1, Order::FlagFile),
        "{r:?}"
    );
    assert_eq!(r.flag, "--max-run-hours <hours>");
    assert!(r.env.is_empty(), "no variable: the flag and the file only");
    assert_eq!(Key::lookup("run.max_run_hours"), Some(Key::RunMaxRunHours));
    assert!(KEYS.contains(&("run.max_run_hours", "run.max_run_hours")));
    assert_eq!(KEYS.len(), 46);
}

#[test]
fn b688_the_default_and_the_bounds_are_the_protocols_own_numbers() {
    let r = Key::RunMaxRunHours.row();
    assert_eq!(r.default, DefaultValue::Number(DEFAULT_MAX_RUN_S / 3600));
    assert_eq!(r.default_text, "24 hours");
    match r.kind {
        Kind::Number { min, max, unit } => {
            assert_eq!((min, max, unit), (1, MAX_MAX_RUN_S / 3600, "hours"));
            assert!(max_run_s_ok(min * 3600) && max_run_s_ok(max * 3600));
            assert!(
                min * 3600 >= MIN_MAX_RUN_S,
                "the least hour is above the protocol floor"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.kind.type_text(), "whole hours, 1 to 720");
    assert!(
        r.help.contains("session") && r.help.contains("new session"),
        "{}",
        r.help
    );
}

#[test]
fn b688_ham_config_sets_prints_and_unsets_the_key_and_refuses_what_is_out_of_range() {
    let cfg = set_key(&Config::default(), "run.max_run_hours", "48").unwrap();
    assert_eq!(
        get_key(&cfg, "run.max_run_hours").unwrap().as_deref(),
        Some("48")
    );
    assert_eq!(
        cfg.config_version,
        Some(1),
        "a key of a table opens the one-way door"
    );
    let cfg = unset_key(&cfg, "run.max_run_hours").unwrap();
    assert_eq!(get_key(&cfg, "run.max_run_hours").unwrap(), None);
    for bad in ["0", "721", "-1", "many", "1.5"] {
        let e = set_key(&Config::default(), "run.max_run_hours", bad)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("run.max_run_hours") && e.contains("takes a whole number"),
            "{bad}: {e}"
        );
    }
    let e = set_key(&Config::default(), "run.max_run_hours", "721")
        .unwrap_err()
        .to_string();
    assert!(e.contains("hours from 1 to 720"), "{e}");
}

#[test]
fn b688_the_file_reader_takes_the_key_and_names_the_key_of_a_wrong_one() {
    let cfg = cfg_of("configVersion = 1\n[run]\nmax_run_hours = 12\n");
    assert_eq!(cfg.run.unwrap().max_run_hours, Some(12));
    for text in [
        "[run]\nmax_run_hours = 0\n",
        "[run]\nmax_run_hours = 721\n",
        "[run]\nmax_run_hours = \"12\"\n",
        "[run]\nmax_run_hours = -3\n",
    ] {
        let e = parse_config(text).unwrap_err().to_string();
        assert!(e.contains("max_run_hours"), "{text}: {e}");
    }
}

#[test]
fn b688_a_run_with_no_setting_and_no_flag_sends_no_member() {
    assert!(request(&Config::default(), &[]).get("maxRunS").is_none());
    let wire = request(&cfg_of("[run]\nrules = true\n"), &[]);
    assert!(wire.get("maxRunS").is_none(), "{wire}");
}

#[test]
fn b688_the_file_becomes_seconds_and_the_flag_wins_over_the_file() {
    let cfg = cfg_of("configVersion = 1\n[run]\nmax_run_hours = 12\n");
    assert_eq!(request(&cfg, &[])["maxRunS"], 43_200);
    assert_eq!(
        request(&cfg, &["--max-run-hours", "30"])["maxRunS"],
        108_000
    );
    assert_eq!(
        request(&Config::default(), &["--max-run-hours", "9"])["maxRunS"],
        32_400,
        "9 hours is over the 8-hour session limit: the first run that renews its session"
    );
}

#[test]
fn b688_every_hour_the_client_accepts_passes_the_check_the_control_plane_runs() {
    for h in [1u32, 8, 9, 24, 719, 720] {
        let wire = request(&Config::default(), &["--max-run-hours", &h.to_string()]);
        let s = u32::try_from(wire["maxRunS"].as_u64().unwrap()).unwrap();
        assert_eq!(s, h * 3600);
        assert!(max_run_s_ok(s), "{h} h = {s} s");
    }
}

#[test]
fn b688_the_flag_takes_whole_hours_once_and_only_on_a_direct_start() {
    assert_eq!(args(&["--max-run-hours", "9"]).max_run_hours, Some(9));
    assert_eq!(args(&[]).max_run_hours, None);
    for (bad, want) in [
        (vec!["--max-run-hours", "0"], "1 to 720"),
        (vec!["--max-run-hours", "721"], "1 to 720"),
        (vec!["--max-run-hours", "1.5"], "1 to 720"),
        (vec!["--max-run-hours", "soon"], "1 to 720"),
        (vec!["--max-run-hours"], "--max-run-hours"),
        (
            vec!["--max-run-hours", "9", "--max-run-hours", "10"],
            "twice",
        ),
    ] {
        let e = parsed(&bad).unwrap_err();
        assert!(e.contains(want), "{bad:?}: {e}");
    }
    let e = parse(&[PROMPT, "--s3-only", "--max-run-hours", "9"])
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("--max-run-hours") && e.contains("--direct"),
        "{e}"
    );
}

#[test]
fn b688_the_flag_is_written_back_into_the_command_line() {
    let inv = parse(&[PROMPT, "--s3-only", "--direct", "--max-run-hours", "9"]).unwrap();
    let argv = inv.to_argv();
    let at = argv.iter().position(|w| w == "--max-run-hours").unwrap();
    assert_eq!(argv[at + 1], "9", "{argv:?}");
    let again = parse(&argv.iter().map(String::as_str).collect::<Vec<_>>()).unwrap();
    assert_eq!(again, inv);
}

#[test]
fn b688_where_the_value_came_from_is_the_flag_then_the_file_then_the_default() {
    let run = resolve_run_settings(&Config::default(), &|_| None, &args(&[]), false).unwrap();
    assert_eq!(run.origin(Key::RunMaxRunHours), Some(Origin::Default));
    let cfg = cfg_of("configVersion = 1\n[run]\nmax_run_hours = 12\n");
    let run = resolve_run_settings(&cfg, &|_| None, &args(&[]), false).unwrap();
    assert_eq!(run.origin(Key::RunMaxRunHours), Some(Origin::File));
    let run =
        resolve_run_settings(&cfg, &|_| None, &args(&["--max-run-hours", "9"]), false).unwrap();
    assert_eq!(run.origin(Key::RunMaxRunHours), Some(Origin::Flag));
}
