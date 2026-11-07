// COMPAT-CAH: this file names the earlier variable prefix on purpose.
//! The earlier variable names of the client (`CAH_MODEL` and nine more) are read after their `HAMH_` names until VERSION 5.50: the new name
//! wins, the earlier name answers when the new one is unset or empty, a name with no earlier name is read as it is, one notice is made for each
//! earlier name that is set, and one switch (`HAMH_NO_DEPRECATION_NOTICES`) silences every notice of the window.
#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, DirectArgs, parse};
use ham_core::config::Config;
use ham_core::envnames::{
    NO_NOTICES, PAIRS, answered_by, earlier_name, notices, silenced_by_env, wrap,
};
use ham_core::request::{Origin, resolve_run_settings};
use ham_core::settings::Key;
use std::collections::HashMap;

fn raw(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    move |k| map.get(k).cloned()
}

/// The raw closure of `pairs` seen through the fallback.
fn wrapped(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let r = raw(pairs);
    move |k| wrap(&r)(k)
}

#[test]
fn the_client_has_nine_variables_with_an_earlier_name() {
    let mut current: Vec<&str> = PAIRS.iter().map(|(c, _)| *c).collect();
    current.sort_unstable();
    assert_eq!(
        current,
        [
            "HAMH_AWS_CLI",
            "HAMH_DOWNLOAD_DIR",
            "HAMH_INPUT_WAIT",
            "HAMH_MODEL",
            "HAMH_NOTIFY",
            "HAMH_NO_MASCOT",
            "HAMH_NO_MOUSE",
            "HAMH_OPEN_COMMAND",
            "HAMH_RANGE_BYTES",
        ]
    );
    for (current, earlier) in PAIRS {
        assert_eq!(
            earlier.strip_prefix("CAH_"),
            current.strip_prefix("HAMH_"),
            "{current} and {earlier} differ only in the prefix"
        );
        assert_eq!(earlier_name(current), Some(earlier));
    }
    assert_eq!(earlier_name("HAMH_SHELL_ENDPOINT"), None);
    // The launch nonce is a test and development name: renamed with no fallback.
    assert_eq!(earlier_name("HAMH_IDEMPOTENCY_NONCE"), None);
    assert_eq!(earlier_name("HOME"), None);
}

#[test]
fn the_new_name_answers() {
    let env = wrapped(&[("HAMH_MODEL", "sonnet")]);
    assert_eq!(env("HAMH_MODEL").as_deref(), Some("sonnet"));
}

#[test]
fn the_earlier_name_answers_when_the_new_one_is_unset() {
    let env = wrapped(&[("CAH_MODEL", "opus")]);
    assert_eq!(env("HAMH_MODEL").as_deref(), Some("opus"));
}

#[test]
fn the_earlier_name_answers_when_the_new_one_is_empty() {
    let env = wrapped(&[("HAMH_MODEL", ""), ("CAH_MODEL", "opus")]);
    assert_eq!(env("HAMH_MODEL").as_deref(), Some("opus"));
}

#[test]
fn the_new_name_wins_when_both_are_set() {
    let env = wrapped(&[("HAMH_MODEL", "sonnet"), ("CAH_MODEL", "opus")]);
    assert_eq!(env("HAMH_MODEL").as_deref(), Some("sonnet"));
}

#[test]
fn an_empty_new_name_with_no_earlier_name_stays_empty() {
    let env = wrapped(&[("HAMH_NO_MOUSE", "")]);
    assert_eq!(env("HAMH_NO_MOUSE").as_deref(), Some(""));
    let env = wrapped(&[]);
    assert_eq!(env("HAMH_NO_MOUSE"), None);
}

#[test]
fn a_name_without_an_earlier_name_is_read_as_it_is() {
    let env = wrapped(&[("CAH_SHELL_ENDPOINT", "x"), ("HOME", "/h")]);
    assert_eq!(env("HAMH_SHELL_ENDPOINT"), None);
    assert_eq!(env("HOME").as_deref(), Some("/h"));
}

#[test]
fn every_variable_of_the_client_is_answered_by_its_earlier_name() {
    for (current, earlier) in PAIRS {
        let env = wrapped(&[(earlier, "v")]);
        assert_eq!(env(current).as_deref(), Some("v"), "{earlier}");
        let env = wrapped(&[(current, "n"), (earlier, "v")]);
        assert_eq!(env(current).as_deref(), Some("n"), "{current}");
    }
}

#[test]
fn the_earlier_name_is_seen_only_while_it_answers() {
    // Only the earlier name is set: it answers, and asking for it gives the value.
    let env = wrapped(&[("CAH_NOTIFY", "1")]);
    assert_eq!(env("CAH_NOTIFY").as_deref(), Some("1"));
    // Both are set: the new name answers and the earlier name is hidden.
    let env = wrapped(&[("HAMH_NOTIFY", "0"), ("CAH_NOTIFY", "1")]);
    assert_eq!(env("CAH_NOTIFY"), None);
    // The earlier name of a variable the client does not read is not answered at all.
    let env = wrapped(&[("CAH_SHELL_ENDPOINT", "x")]);
    assert_eq!(env("CAH_SHELL_ENDPOINT").as_deref(), Some("x"));
}

#[test]
fn the_variable_that_answered_is_named() {
    let env = wrapped(&[("CAH_NOTIFY", "1")]);
    assert_eq!(answered_by(&env, "HAMH_NOTIFY"), "CAH_NOTIFY");
    let env = wrapped(&[("HAMH_NOTIFY", "1")]);
    assert_eq!(answered_by(&env, "HAMH_NOTIFY"), "HAMH_NOTIFY");
    let env = wrapped(&[("HAMH_NOTIFY", "0"), ("CAH_NOTIFY", "1")]);
    assert_eq!(answered_by(&env, "HAMH_NOTIFY"), "HAMH_NOTIFY");
    // Nothing set: the current name, as before the rename.
    let env = wrapped(&[]);
    assert_eq!(answered_by(&env, "HAMH_NOTIFY"), "HAMH_NOTIFY");
    // A name that has no earlier name is its own answer.
    assert_eq!(
        answered_by(&env, "HAMH_SHELL_ENDPOINT"),
        "HAMH_SHELL_ENDPOINT"
    );
}

#[test]
fn one_notice_is_made_for_each_earlier_name_that_is_set() {
    let got = notices(&raw(&[
        ("CAH_MODEL", "opus"),
        ("CAH_NOTIFY", "1"),
        ("CAH_SHELL_ENDPOINT", "x"),
        ("HAMH_NO_MOUSE", "1"),
    ]));
    assert_eq!(
        got,
        [
            "CAH_MODEL is the old name of HAMH_MODEL. It still works and will stop working in a later release.",
            "CAH_NOTIFY is the old name of HAMH_NOTIFY. It still works and will stop working in a later release.",
        ]
    );
}

#[test]
fn a_notice_is_in_the_order_of_the_table_and_never_repeats() {
    let got = notices(&raw(&[("CAH_OPEN_COMMAND", "o"), ("CAH_AWS_CLI", "a")]));
    assert_eq!(got.len(), 2);
    assert!(got[0].starts_with("CAH_AWS_CLI "), "{got:?}");
    assert!(got[1].starts_with("CAH_OPEN_COMMAND "), "{got:?}");
}

#[test]
fn both_names_set_make_the_notice_that_says_which_one_is_used() {
    let got = notices(&raw(&[("CAH_MODEL", "opus"), ("HAMH_MODEL", "sonnet")]));
    assert_eq!(
        got,
        ["Both CAH_MODEL and HAMH_MODEL are set. HAMH_MODEL is used."]
    );
}

#[test]
fn an_empty_earlier_name_makes_no_notice() {
    assert!(notices(&raw(&[("CAH_MODEL", "")])).is_empty());
    assert!(notices(&raw(&[])).is_empty());
}

#[test]
fn the_switch_silences_the_notices_of_the_window() {
    assert_eq!(NO_NOTICES, "HAMH_NO_DEPRECATION_NOTICES");
    for on in ["1", "true", "TRUE", "yes", "on", " On "] {
        assert!(silenced_by_env(&raw(&[(NO_NOTICES, on)])), "{on:?}");
    }
    for off in ["", "0", "false", "no", "off", "2", "maybe"] {
        assert!(!silenced_by_env(&raw(&[(NO_NOTICES, off)])), "{off:?}");
    }
    assert!(!silenced_by_env(&raw(&[])));
    // The switch is new in this release: it has no earlier name.
    assert!(!silenced_by_env(&raw(&[(
        "CAH_NO_DEPRECATION_NOTICES",
        "1"
    )])));
}

/// When it is on the branch, the client's nine names must be in it as Tier 1, and no Tier 1 name owned by the client may be missing from the
/// table, except the deploy setting the infra resolver reads.
#[test]
fn the_table_agrees_with_the_manifest_of_names_when_the_manifest_is_present() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../compat/env-names.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        eprintln!("compat/env-names.json is not on this branch; the comparison runs once it is");
        return;
    };
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    let names = doc["names"].as_array().unwrap();
    let tier1: Vec<(&str, &str)> = names
        .iter()
        .filter(|n| n["tier"] == 1)
        .map(|n| {
            (
                n["suffix"].as_str().unwrap(),
                n["owner"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    for (current, _) in PAIRS {
        let suffix = current.strip_prefix("HAMH_").unwrap();
        assert!(
            tier1.iter().any(|(s, _)| *s == suffix),
            "{suffix} is not Tier 1 in compat/env-names.json"
        );
    }
    for (suffix, owner) in &tier1 {
        // `UTILITY_MODEL` is the model of the run names at deploy time: infra/lib/env.ts reads it, the client never does.
        if matches!(*owner, "ham" | "ham-core" | "client") && *suffix != "UTILITY_MODEL" {
            assert!(
                PAIRS
                    .iter()
                    .any(|(c, _)| c.strip_prefix("HAMH_") == Some(*suffix)),
                "{suffix} is owned by the client in compat/env-names.json but has no entry in envnames::PAIRS"
            );
        }
    }
}

fn run_args() -> DirectArgs {
    let words = [
        "Add a percent_change(a, b) function",
        "--s3-only",
        "--direct",
    ];
    let Command::StartDirect(a) = parse(&words).unwrap().command else {
        panic!("{words:?} is not a direct start");
    };
    a
}

#[test]
fn the_origin_of_a_run_setting_names_the_variable_that_answered() {
    let cfg = Config::default();
    let resolve = |pairs: &[(&str, &str)]| {
        let env = wrapped(pairs);
        resolve_run_settings(&cfg, &env, &run_args(), false).unwrap()
    };
    let r = resolve(&[("CAH_MODEL", "opus"), ("CAH_INPUT_WAIT", "9")]);
    assert_eq!(r.origin(Key::DefaultModel), Some(Origin::Env("CAH_MODEL")));
    assert_eq!(r.model.id, "us.anthropic.claude-opus-5-5");
    assert_eq!(
        r.origin(Key::InputWaitMinutes),
        Some(Origin::Env("CAH_INPUT_WAIT"))
    );
    assert_eq!(r.args.input_wait, Some(9));
    let r = resolve(&[
        ("HAMH_MODEL", "opus"),
        ("CAH_MODEL", "sonnet"),
        ("HAMH_INPUT_WAIT", "9"),
    ]);
    assert_eq!(r.origin(Key::DefaultModel), Some(Origin::Env("HAMH_MODEL")));
    assert_eq!(
        r.origin(Key::InputWaitMinutes),
        Some(Origin::Env("HAMH_INPUT_WAIT"))
    );
    assert_eq!(Origin::Env("CAH_MODEL").label(), "env CAH_MODEL");
}

/// The fallback works only when a variable is read through the wrapped environment. A source line that reads one of the nine names straight from
/// the process (`std::env::var("HAMH_MODEL")`, `var_os(...)`) would ignore the earlier name, so no such line may exist outside this module.
#[test]
fn no_source_line_reads_one_of_the_nine_names_around_the_wrapper() {
    fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                rust_files(&p, out);
            } else if p.extension().is_some_and(|e| e == "rs") {
                out.push(p);
            }
        }
    }
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for root in ["src", "../ham/src", "../ham-admin/src"] {
        rust_files(&base.join(root), &mut files);
    }
    assert!(
        files.len() > 50,
        "the source of the client crates was found: {}",
        files.len()
    );
    let mut problems = Vec::new();
    for file in files {
        if file.ends_with("src/envnames.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        for (n, line) in text.lines().enumerate() {
            let direct = line.contains("env::var(") || line.contains("env::var_os(");
            if direct && PAIRS.iter().any(|(current, _)| line.contains(current)) {
                problems.push(format!("{}:{}: {}", file.display(), n + 1, line.trim()));
            }
        }
    }
    assert_eq!(problems, Vec::<String>::new());
}
