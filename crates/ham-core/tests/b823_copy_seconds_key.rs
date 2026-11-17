//! The copy interval of a run in seconds, the client key run.copy_every_seconds of [run] in config.toml:
//! whole seconds from 15 to 3,600, default 15, the file only. It rides in the request as snapshotEveryS and is left out when it is the default, so
//! the request of a user who sets nothing is byte for byte what an older client sends. run.copy_every_minutes keeps working with its meaning (m
//! minutes are 60 m seconds) for a config written before this key; it has no default of its own any more. A config that sets both keys is refused
//! with one sentence that names both, and `ham config set` of one key while the other is set says the same.

#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, DirectArgs, parse};
use ham_core::config::{Config, KEYS, get_key, parse_config, set_key, unset_key};
use ham_core::request::{DirectInputs, Origin, resolve_run_settings};
use ham_core::settings::{DefaultValue, Group, Key, Kind, Order, Row};
use harness_protocol::direct::{
    DEFAULT_SNAPSHOT_EVERY_S, MAX_SNAPSHOT_EVERY_S, MIN_SNAPSHOT_EVERY_S,
};
use serde_json::{Value, json};

const PROMPT: &str = "Add a percent_change(a, b) function";
const SECONDS: &str = "run.copy_every_seconds";
const MINUTES: &str = "run.copy_every_minutes";

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

fn key(name: &str) -> Key {
    Key::lookup(name).unwrap_or_else(|| panic!("{name} is no key of the registry"))
}

fn row(name: &str) -> &'static Row {
    key(name).row()
}

/// The words of the one sentence about the two keys.
fn says_to_unset_one(e: &str) {
    for words in [SECONDS, MINUTES, "cannot both be set", "unset one of them"] {
        assert!(e.contains(words), "{words:?} is not in: {e}");
    }
}

#[test]
fn b823_seconds_the_row_follows_the_minutes_key_and_rides_in_the_snapshot_member() {
    assert_eq!(Key::ALL.len(), 50);
    // Added `run.delivery_note` after it, and `run.recover_after_stop` after that.
    assert_eq!(
        Key::ALL[Key::ALL.len() - 3],
        key(SECONDS),
        "the key after run.copy_every_minutes and before run.delivery_note"
    );
    let r = row(SECONDS);
    assert_eq!(r.name, SECONDS);
    assert_eq!(r.file, ("run", "copy_every_seconds"));
    assert_eq!(
        (r.wire, r.group, r.since, r.order),
        (Some("snapshotEveryS"), Group::Limits, 1, Order::FileOnly),
        "{r:?}"
    );
    assert!(
        r.flag.is_empty() && r.env.is_empty(),
        "the file only: no flag and no variable"
    );
    assert!(KEYS.contains(&(SECONDS, SECONDS)));
    assert_eq!(KEYS.len(), 50);
}

#[test]
fn b823_seconds_the_default_and_the_bounds_are_the_protocols_own_numbers() {
    let r = row(SECONDS);
    assert_eq!(r.default, DefaultValue::Number(DEFAULT_SNAPSHOT_EVERY_S));
    assert_eq!(r.default_text, "15 seconds");
    match r.kind {
        Kind::Number { min, max, unit } => {
            assert_eq!(unit, "seconds");
            assert_eq!((min, max), (MIN_SNAPSHOT_EVERY_S, MAX_SNAPSHOT_EVERY_S));
            assert_eq!((min, max), (15, 3_600));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.kind.type_text(), "whole seconds, 15 to 3600");
    for word in [
        "copy",
        "tool call",
        "when the engine stops",
        "ham pull",
        "does not save every write",
        MINUTES,
    ] {
        assert!(r.help.contains(word), "{word:?} is not in: {}", r.help);
    }
    assert!(
        r.help.contains(
            "A run that ham resume or /retry starts keeps the interval of the run it comes from."
        ),
        "{}",
        r.help
    );
}

#[test]
fn b823_seconds_the_minutes_key_keeps_its_meaning_and_names_the_seconds_key() {
    let r = row(MINUTES);
    assert_eq!(r.default, DefaultValue::Absent);
    assert_eq!(r.default_text, "not set");
    assert!(r.help.contains(SECONDS), "{}", r.help);
    for minutes in [1u32, 2, 10, 60] {
        let wire = request(&cfg_of(&format!(
            "configVersion = 1\n[run]\ncopy_every_minutes = {minutes}\n"
        )));
        assert_eq!(wire["snapshotEveryS"], json!(minutes * 60), "{minutes} min");
    }
}

#[test]
fn b823_seconds_ham_config_sets_prints_and_unsets_the_key_and_refuses_what_is_out_of_range() {
    let cfg = set_key(&Config::default(), SECONDS, "30").unwrap();
    assert_eq!(get_key(&cfg, SECONDS).unwrap().as_deref(), Some("30"));
    assert_eq!(
        cfg.config_version,
        Some(1),
        "a key of a table opens the one-way door"
    );
    let cfg = unset_key(&cfg, SECONDS).unwrap();
    assert_eq!(get_key(&cfg, SECONDS).unwrap(), None);
    for bad in ["0", "14", "3601", "-1", "often", "1.5", "off"] {
        let e = set_key(&Config::default(), SECONDS, bad)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains(SECONDS) && e.contains("takes a whole number"),
            "{bad}: {e}"
        );
        assert!(e.contains("seconds from 15 to 3600"), "{bad}: {e}");
    }
}

#[test]
fn b823_seconds_the_file_reader_takes_the_key_and_names_the_key_of_a_wrong_one() {
    let cfg = cfg_of("configVersion = 1\n[run]\ncopy_every_seconds = 45\n");
    assert_eq!(get_key(&cfg, SECONDS).unwrap().as_deref(), Some("45"));
    for text in [
        "[run]\ncopy_every_seconds = 0\n",
        "[run]\ncopy_every_seconds = 14\n",
        "[run]\ncopy_every_seconds = 3601\n",
        "[run]\ncopy_every_seconds = \"30\"\n",
        "[run]\ncopy_every_seconds = -3\n",
        "[run]\ncopy_every_seconds = false\n",
    ] {
        let e = parse_config(text).unwrap_err().to_string();
        assert!(e.contains("copy_every_seconds"), "{text}: {e}");
    }
}

#[test]
fn b823_seconds_the_request_carries_the_seconds_and_leaves_the_default_out() {
    assert!(request(&Config::default()).get("snapshotEveryS").is_none());
    for s in [16u32, 30, 59, 60, 600, 3_600] {
        let wire = request(&cfg_of(&format!(
            "configVersion = 1\n[run]\ncopy_every_seconds = {s}\n"
        )));
        assert_eq!(wire["snapshotEveryS"], json!(s), "{wire}");
    }
    // The default is what the stage does by itself: a key set to it sends no member, as an unset key does.
    let cfg = cfg_of("configVersion = 1\n[run]\ncopy_every_seconds = 15\n");
    let wire = request(&cfg);
    assert!(wire.get("snapshotEveryS").is_none(), "{wire}");
    assert_eq!(wire, request(&cfg_of("configVersion = 1\n")));
    let run = resolve_run_settings(&cfg, &|_| None, &args(), false).unwrap();
    assert_eq!(run.origin(key(SECONDS)), Some(Origin::File));
    assert_eq!(run.snapshot_every_s, None);
    let run = resolve_run_settings(&Config::default(), &|_| None, &args(), false).unwrap();
    assert_eq!(run.origin(key(SECONDS)), Some(Origin::Default));
}

#[test]
fn b823_seconds_a_config_that_sets_both_keys_is_refused_with_one_sentence_that_names_both() {
    let e = parse_config(
        "configVersion = 1\n[run]\ncopy_every_seconds = 30\ncopy_every_minutes = 10\n",
    )
    .unwrap_err()
    .to_string();
    says_to_unset_one(&e);
    assert_eq!(
        e.matches("cannot both be set").count(),
        1,
        "one sentence: {e}"
    );
    // Each key alone is read.
    cfg_of("configVersion = 1\n[run]\ncopy_every_seconds = 30\n");
    cfg_of("configVersion = 1\n[run]\ncopy_every_minutes = 10\n");
}

#[test]
fn b823_seconds_ham_config_set_of_one_key_while_the_other_is_set_says_the_same() {
    let with_minutes = set_key(&Config::default(), MINUTES, "10").unwrap();
    says_to_unset_one(
        &set_key(&with_minutes, SECONDS, "30")
            .unwrap_err()
            .to_string(),
    );
    let with_seconds = set_key(&Config::default(), SECONDS, "30").unwrap();
    says_to_unset_one(
        &set_key(&with_seconds, MINUTES, "10")
            .unwrap_err()
            .to_string(),
    );
    // Setting the key that is set already is no clash, and after the unset the other key is taken.
    set_key(&with_seconds, SECONDS, "45").unwrap();
    let cfg = unset_key(&with_minutes, MINUTES).unwrap();
    let cfg = set_key(&cfg, SECONDS, "30").unwrap();
    assert_eq!(get_key(&cfg, SECONDS).unwrap().as_deref(), Some("30"));
    assert_eq!(get_key(&cfg, MINUTES).unwrap(), None);
}

/// The request carries seconds and does not say which key of the file they came from. A sentence that names the key (the one for a stage of an
/// older release) asks the resolved run.
#[test]
fn b823_seconds_the_resolved_run_names_the_key_that_set_the_interval() {
    let key_of = |text: &str| {
        resolve_run_settings(&cfg_of(text), &|_| None, &args(), false)
            .unwrap()
            .copy_interval_key()
    };
    assert_eq!(
        key_of("configVersion = 1\n[run]\ncopy_every_seconds = 120\n"),
        Some(key(SECONDS))
    );
    assert_eq!(
        key_of("configVersion = 1\n[run]\ncopy_every_minutes = 2\n"),
        Some(key(MINUTES))
    );
    // A key set to the default sends no member; it is still the key the file sets.
    assert_eq!(
        key_of("configVersion = 1\n[run]\ncopy_every_seconds = 15\n"),
        Some(key(SECONDS))
    );
    assert_eq!(key_of("configVersion = 1\n"), None);
}
