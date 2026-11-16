//! The copy interval of a run as a client setting: the key run.copy_every_minutes of [run] in config.toml, which rides in the request as
//! snapshotEveryS in seconds. While a run works, the worker looks at the work tree on a timer and saves a copy of the work to the stage's bucket when
//! files changed; release 5.19 set that timer to one minute and gave it no setting. A run that does not set the key sends no member, so the request is
//! byte for byte what an older client sends and the stage's own minute applies. A key set to the default sends none either: the default is what the
//! stage does by itself, and a stage of an older release refuses a request with a member it does not know. The bounds are the protocol's own constants, so the client, the control
//! plane and the worker cannot drift apart. The key has no value that switches the copies off: a run always has a copy of its work while it works.

#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, DirectArgs, parse};
use ham_core::config::{Config, KEYS, get_key, parse_config, set_key, unset_key};
use ham_core::request::{DirectInputs, Origin, resolve_run_settings};
use ham_core::settings::{DefaultValue, Group, Key, Kind, Order, Row};
use harness_protocol::direct::{
    DEFAULT_SNAPSHOT_EVERY_S, MAX_SNAPSHOT_EVERY_S, MIN_SNAPSHOT_EVERY_S,
};
use serde_json::Value;

const PROMPT: &str = "Add a percent_change(a, b) function";
const NAME: &str = "run.copy_every_minutes";

fn args() -> DirectArgs {
    match parse(&[PROMPT, "--s3-only", "--direct"]).unwrap().command {
        Command::StartDirect(a) => a,
        other => panic!("not a direct start: {other:?}"),
    }
}

fn cfg_of(text: &str) -> Config {
    parse_config(text).unwrap_or_else(|e| panic!("{text:?}: {e}"))
}

fn request(cfg: &Config) -> Value {
    let run = resolve_run_settings(cfg, &|_| None, &args(), false).unwrap();
    serde_json::to_value(run.request(DirectInputs::default()).unwrap()).unwrap()
}

fn key() -> Key {
    Key::lookup(NAME).unwrap_or_else(|| panic!("{NAME} is no key of the registry"))
}

fn the_row() -> &'static Row {
    key().row()
}

#[test]
fn b841_copy_the_row_rides_in_the_snapshot_member() {
    // Added one key after the two of the copy interval.
    assert_eq!(Key::ALL.len(), 49);
    assert_eq!(
        Key::ALL[Key::ALL.len() - 3],
        key(),
        "the key before run.copy_every_seconds"
    );
    let r = the_row();
    assert_eq!(r.name, NAME);
    assert_eq!(r.file, ("run", "copy_every_minutes"));
    assert_eq!(
        (r.wire, r.group, r.since, r.order),
        (Some("snapshotEveryS"), Group::Limits, 1, Order::FileOnly),
        "{r:?}"
    );
    assert!(
        r.flag.is_empty() && r.env.is_empty(),
        "the file only: no flag and no variable"
    );
    assert!(KEYS.contains(&(NAME, NAME)));
    assert_eq!(KEYS.len(), 49);
}

#[test]
fn b841_copy_the_default_and_the_bounds_are_the_protocols_own_numbers() {
    let r = the_row();
    // The default of the timer is 15 seconds, no whole minute. The key has no default of its own: unset, `run.copy_every_seconds`
    // decides.
    assert_eq!(DEFAULT_SNAPSHOT_EVERY_S, 15);
    assert_eq!(r.default, DefaultValue::Absent);
    assert_eq!(r.default_text, "not set");
    match r.kind {
        Kind::Number { min, max, unit } => {
            assert_eq!(unit, "minutes");
            assert_eq!(
                (min, max * 60),
                (1, MAX_SNAPSHOT_EVERY_S),
                "one minute to the most seconds an invocation may name"
            );
            assert!(min * 60 >= MIN_SNAPSHOT_EVERY_S);
            assert!(
                min >= 1,
                "no value switches the copies off: 0 is no minute of the key"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.kind.type_text(), "whole minutes, 1 to 60");
    for word in ["copy", "tool call", "when the engine stops", "ham pull"] {
        assert!(r.help.contains(word), "{word:?} is not in: {}", r.help);
    }
}

#[test]
fn b841_copy_the_help_says_which_interval_a_resumed_or_retried_run_uses() {
    // `ham resume` and `/retry` start a run with the stored request of the run they come from, so the interval of that run rides along and
    // the key as it is now is not read. The help says so where the user sets the key.
    let help = the_row().help;
    assert!(
        help.contains(
            "A run that ham resume or /retry starts keeps the interval of the run it comes from."
        ),
        "{help}"
    );
}

#[test]
fn b841_copy_ham_config_sets_prints_and_unsets_the_key_and_refuses_what_is_out_of_range() {
    let cfg = set_key(&Config::default(), NAME, "10").unwrap();
    assert_eq!(get_key(&cfg, NAME).unwrap().as_deref(), Some("10"));
    assert_eq!(
        cfg.config_version,
        Some(1),
        "a key of a table opens the one-way door"
    );
    let cfg = unset_key(&cfg, NAME).unwrap();
    assert_eq!(get_key(&cfg, NAME).unwrap(), None);
    for bad in ["0", "61", "-1", "often", "1.5", "off"] {
        let e = set_key(&Config::default(), NAME, bad)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains(NAME) && e.contains("takes a whole number"),
            "{bad}: {e}"
        );
        assert!(e.contains("minutes from 1 to 60"), "{bad}: {e}");
    }
}

#[test]
fn b841_copy_the_file_reader_takes_the_key_and_names_the_key_of_a_wrong_one() {
    let cfg = cfg_of("configVersion = 1\n[run]\ncopy_every_minutes = 10\n");
    assert_eq!(get_key(&cfg, NAME).unwrap().as_deref(), Some("10"));
    for text in [
        "[run]\ncopy_every_minutes = 0\n",
        "[run]\ncopy_every_minutes = 61\n",
        "[run]\ncopy_every_minutes = \"10\"\n",
        "[run]\ncopy_every_minutes = -3\n",
        "[run]\ncopy_every_minutes = false\n",
    ] {
        let e = parse_config(text).unwrap_err().to_string();
        assert!(e.contains("copy_every_minutes"), "{text}: {e}");
    }
}

#[test]
fn b841_copy_a_run_with_no_setting_sends_no_member() {
    assert!(request(&Config::default()).get("snapshotEveryS").is_none());
    let wire = request(&cfg_of("[run]\nrules = true\n"));
    assert!(wire.get("snapshotEveryS").is_none(), "{wire}");
}

#[test]
fn b841_copy_one_minute_is_sent_as_sixty_seconds_since_the_default_is_fifteen() {
    // While the default of the timer was one minute, a key set to 1 sent no member. The default is 15 seconds now, so one minute is
    // an interval the user chose and it rides in the request like every other.
    let cfg = cfg_of("configVersion = 1\n[run]\ncopy_every_minutes = 1\n");
    assert_eq!(get_key(&cfg, NAME).unwrap().as_deref(), Some("1"));
    let wire = request(&cfg);
    assert_eq!(wire["snapshotEveryS"], serde_json::json!(60), "{wire}");
    let run = resolve_run_settings(&cfg, &|_| None, &args(), false).unwrap();
    assert_eq!(run.origin(key()), Some(Origin::File));
    assert_eq!(run.snapshot_every_s, Some(60));
}

#[test]
fn b841_copy_the_file_becomes_seconds_and_every_minute_of_the_key_is_one_an_invocation_may_name() {
    // The default of the timer is one an invocation may name too; it is the one value the request leaves out.
    assert!((MIN_SNAPSHOT_EVERY_S..=MAX_SNAPSHOT_EVERY_S).contains(&DEFAULT_SNAPSHOT_EVERY_S));
    for minutes in [1u32, 2, 10, 59, 60] {
        let cfg = cfg_of(&format!(
            "configVersion = 1\n[run]\ncopy_every_minutes = {minutes}\n"
        ));
        let wire = request(&cfg);
        let s = u32::try_from(
            wire["snapshotEveryS"]
                .as_u64()
                .unwrap_or_else(|| panic!("{minutes} min: no snapshotEveryS in {wire}")),
        )
        .unwrap();
        assert_eq!(s, minutes * 60);
        assert!(
            (MIN_SNAPSHOT_EVERY_S..=MAX_SNAPSHOT_EVERY_S).contains(&s),
            "{minutes} min = {s} s"
        );
    }
}

#[test]
fn b841_copy_where_the_value_came_from_is_the_file_then_the_default() {
    let run = resolve_run_settings(&Config::default(), &|_| None, &args(), false).unwrap();
    assert_eq!(run.origin(key()), Some(Origin::Default));
    let cfg = cfg_of("configVersion = 1\n[run]\ncopy_every_minutes = 10\n");
    let run = resolve_run_settings(&cfg, &|_| None, &args(), false).unwrap();
    assert_eq!(run.origin(key()), Some(Origin::File));
}
