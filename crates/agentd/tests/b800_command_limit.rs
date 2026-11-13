//! The shell command time limit of the second engine. OpenCode's shell tool stops a command after two minutes unless
//! `OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS` says otherwise. Every launch of OpenCode now carries that variable, in milliseconds: the
//! user's `commands.timeout_seconds` when the engine file sets it, else the ten minutes Cline's launch ends up with. The launch of Cline is
//! the launch it was: its variable is added only for a setting the user made.
#![allow(clippy::unwrap_used)]

use agentd::direct::request::engine_settings;
use agentd::engine::direct::{OPENCODE_TIMEOUT_ENV, TIMEOUT_ENV, launch_env};
use agentd::engine::user_settings::{
    CommandSeconds, Placed, UserSettings, check_command_limit, read_back,
};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{CLINE_KEYS, EngineConfig, OPENCODE_KEYS, check_engine_config};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

const OPENCODE_VAR: &str = "OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS";
const CLINE_VAR: &str = "CLINE_RUN_COMMANDS_TIMEOUT_MS";

fn member(engine: &str, settings: Value) -> EngineConfig {
    EngineConfig {
        engine: engine.into(),
        schema: 1,
        settings: settings.as_object().unwrap().clone(),
    }
}

/// The typed settings of a member that passed the shared check, for `engine`.
fn settings_of(engine: &str, settings: Value) -> UserSettings {
    let c = member(engine, settings);
    check_engine_config(&c).unwrap();
    UserSettings::from_checked(&c).unwrap()
}

fn limit(seconds: u32) -> Value {
    json!({"commands": {"timeout_seconds": seconds}})
}

fn env_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

// ---- the launch -------------------------------------------------------------------------------------------------------------

#[test]
fn b800_the_names_of_the_two_variables_are_the_engines_own() {
    assert_eq!(OPENCODE_TIMEOUT_ENV, OPENCODE_VAR);
    assert_eq!(TIMEOUT_ENV, CLINE_VAR);
}

#[test]
fn b800_a_launch_of_the_second_engine_with_no_setting_carries_ten_minutes() {
    let env = launch_env("opencode", &UserSettings::default());
    assert_eq!(env, env_of(&[(OPENCODE_VAR, "600000")]), "{env:?}");
}

#[test]
fn b800_a_launch_of_the_second_engine_carries_the_users_limit_in_milliseconds() {
    for (seconds, ms) in [
        (1, "1000"),
        (30, "30000"),
        (600, "600000"),
        (900, "900000"),
        (1800, "1800000"),
    ] {
        let s = settings_of("opencode", limit(seconds));
        let env = launch_env("opencode", &s);
        assert_eq!(env, env_of(&[(OPENCODE_VAR, ms)]), "{seconds} s: {env:?}");
        assert_eq!(
            s.command_timeout.unwrap().millis(),
            ms.parse::<u64>().unwrap()
        );
    }
}

#[test]
fn b800_the_default_is_one_constant_and_it_is_the_default_of_the_first_engine() {
    assert_eq!(CommandSeconds::DEFAULT, 600);
    assert_eq!(CommandSeconds::default_limit().get(), 600);
    assert_eq!(CommandSeconds::default_limit().millis(), 600_000);
    assert!((CommandSeconds::MIN..=CommandSeconds::MAX).contains(&CommandSeconds::DEFAULT));
    // Both key tables name the same default as the constant.
    for rows in [CLINE_KEYS, OPENCODE_KEYS] {
        let row = rows
            .iter()
            .find(|r| r.path == "commands.timeout_seconds")
            .unwrap();
        assert_eq!(row.default, CommandSeconds::DEFAULT.to_string());
    }
    // Cline's own default is the fork's: patch 0015 gives the command tool 600000 ms.
    let patches = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../engine/cline/patches");
    let patch = std::fs::read_dir(&patches)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("0015-")
        })
        .expect("patch 0015");
    let text = std::fs::read_to_string(patch).unwrap();
    assert!(
        text.contains(&format!(
            "a {} ms default",
            CommandSeconds::default_limit().millis()
        )),
        "the default of the Cline fork is not the shared default"
    );
    // The default launch of the second engine is the default the first engine ends up with.
    assert_eq!(
        launch_env("opencode", &UserSettings::default())[OPENCODE_VAR],
        CommandSeconds::default_limit().millis().to_string()
    );
}

#[test]
fn b800_the_launch_of_the_first_engine_is_the_launch_it_was() {
    // No setting: nothing is added, so the environment is the empty map it was.
    assert_eq!(
        format!("{:?}", launch_env("cline", &UserSettings::default())),
        "{}"
    );
    // A member that sets other keys adds nothing either.
    let other = settings_of("cline", json!({"reasoning": {"effort": "low"}}));
    assert!(launch_env("cline", &other).is_empty());
    // The limit alone: one variable, in milliseconds, and not the other engine's.
    let s = settings_of("cline", limit(900));
    let env = launch_env("cline", &s);
    assert_eq!(
        format!("{env:?}"),
        r#"{"CLINE_RUN_COMMANDS_TIMEOUT_MS": "900000"}"#
    );
    assert!(!env.contains_key(OPENCODE_VAR));
    // All five keys: still the one variable.
    let all = settings_of(
        "cline",
        json!({
            "reasoning": {"effort": "high"},
            "commands": {"timeout_seconds": 1},
            "context": {"compaction": "basic", "compaction_trigger": 0.8},
            "tools": {"disable": ["spawn_agent"]}
        }),
    );
    assert_eq!(
        format!("{:?}", launch_env("cline", &all)),
        r#"{"CLINE_RUN_COMMANDS_TIMEOUT_MS": "1000"}"#
    );
}

#[test]
fn b800_an_engine_that_has_no_command_limit_variable_gets_none() {
    for engine in ["fake-acp", "pi", ""] {
        assert!(
            launch_env(engine, &UserSettings::default()).is_empty(),
            "{engine}"
        );
        let s = settings_of("opencode", limit(900));
        assert!(launch_env(engine, &s).is_empty(), "{engine}");
    }
}

#[test]
fn b800_the_launch_environments_as_a_run_prints_them() {
    // The four lines the report quotes. The text is the Debug form of the map `launch_env` returns, which is what the engine's environment gets.
    let none = UserSettings::default();
    let lines = [
        ("opencode, no setting", launch_env("opencode", &none)),
        (
            "opencode, commands.timeout_seconds = 900",
            launch_env("opencode", &settings_of("opencode", limit(900))),
        ),
        ("cline, no setting", launch_env("cline", &none)),
        (
            "cline, commands.timeout_seconds = 900",
            launch_env("cline", &settings_of("cline", limit(900))),
        ),
    ];
    let want = [
        r#"{"OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS": "600000"}"#,
        r#"{"OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS": "900000"}"#,
        "{}",
        r#"{"CLINE_RUN_COMMANDS_TIMEOUT_MS": "900000"}"#,
    ];
    for ((what, env), want) in lines.iter().zip(want) {
        println!("B800 LAUNCH-ENV {what}: {env:?}");
        assert_eq!(format!("{env:?}"), want, "{what}");
    }
}

// ---- the member -------------------------------------------------------------------------------------------------------------

fn request_with(m: Value) -> StartDirectRunRequest {
    let mut v = json!({"prompt": "p", "target": {"mode": "s3-only"}, "model": "m", "rules": []});
    v["engineConfig"] = m;
    serde_json::from_value(v).unwrap()
}

#[test]
fn b800_a_member_for_the_second_engine_with_the_limit_is_typed_for_that_engine() {
    let r = request_with(json!({"engine": "opencode", "schema": 1, "settings": limit(900)}));
    let s = engine_settings(&r, "opencode").unwrap();
    assert_eq!(s.command_timeout, CommandSeconds::new(900));
    assert_eq!(
        launch_env("opencode", &s),
        env_of(&[(OPENCODE_VAR, "900000")])
    );
    // The same member on a run of the other engine is a mismatch, not a setting that is set and never applied.
    let err = engine_settings(&r, "cline").unwrap_err();
    assert!(err.contains("another engine"), "{err}");
}

#[test]
fn b800_a_limit_outside_the_range_ends_the_run_before_it_starts_without_the_value() {
    for bad in [0, 1801, 99_999] {
        let r = request_with(json!({"engine": "opencode", "schema": 1, "settings": limit(bad)}));
        let err = engine_settings(&r, "opencode").unwrap_err();
        assert!(err.contains("commands.timeout_seconds"), "{err}");
        // The message names the range of the key, which holds neither 1801 nor 99999.
        assert!(!err.contains("1801") && !err.contains("99999"), "{err}");
    }
}

#[test]
fn b800_the_other_keys_of_the_first_engine_still_end_a_run_of_the_second() {
    for settings in [
        json!({"reasoning": {"effort": "high"}}),
        json!({"context": {"compaction": "off"}}),
        json!({"tools": {"disable": ["spawn_agent"]}}),
        json!({"commands": {"timeout_seconds": 900}, "reasoning": {"effort": "low"}}),
    ] {
        let r = request_with(json!({"engine": "opencode", "schema": 1, "settings": settings}));
        assert!(engine_settings(&r, "opencode").is_err(), "{settings}");
    }
}

// ---- the check of the placed environment ------------------------------------------------------------------------------------

#[test]
fn b800_the_check_accepts_the_environment_the_launch_made() {
    for settings in [
        UserSettings::default(),
        settings_of("opencode", limit(1)),
        settings_of("opencode", limit(900)),
        settings_of("opencode", limit(1800)),
    ] {
        let env = launch_env("opencode", &settings);
        check_command_limit("opencode", &env, &settings).unwrap();
    }
    for seconds in [1, 900, 1800] {
        let s = settings_of("cline", limit(seconds));
        check_command_limit("cline", &launch_env("cline", &s), &s).unwrap();
    }
}

#[test]
fn b800_a_launch_of_the_second_engine_without_the_variable_is_refused_whatever_the_user_set() {
    for settings in [UserSettings::default(), settings_of("opencode", limit(900))] {
        let err = check_command_limit("opencode", &BTreeMap::new(), &settings).unwrap_err();
        assert!(
            err.contains("engine settings") && err.contains("command time limit"),
            "{err}"
        );
    }
}

#[test]
fn b800_a_wrong_value_or_the_other_engines_variable_is_refused_and_no_value_is_named() {
    let s = settings_of("opencode", limit(900));
    for env in [
        env_of(&[(OPENCODE_VAR, "120000")]),
        env_of(&[(OPENCODE_VAR, "900")]),
        env_of(&[(OPENCODE_VAR, "")]),
        env_of(&[(OPENCODE_VAR, "900000 ")]),
        env_of(&[(CLINE_VAR, "900000")]),
    ] {
        let err = check_command_limit("opencode", &env, &s).unwrap_err();
        assert!(err.contains("command time limit"), "{env:?}: {err}");
        assert!(!err.contains("900") && !err.contains("120000"), "{err}");
    }
    // The default launch is checked against the default.
    let d = UserSettings::default();
    assert!(check_command_limit("opencode", &env_of(&[(OPENCODE_VAR, "120000")]), &d).is_err());
    check_command_limit("opencode", &env_of(&[(OPENCODE_VAR, "600000")]), &d).unwrap();
}

#[test]
fn b800_the_first_engine_is_checked_only_for_a_limit_the_user_set() {
    // Nothing set: the stage may set its own default for the variable, or none, and the check does not look.
    let d = UserSettings::default();
    check_command_limit("cline", &BTreeMap::new(), &d).unwrap();
    check_command_limit("cline", &env_of(&[(CLINE_VAR, "1000")]), &d).unwrap();
    // A limit set: the variable must hold it.
    let s = settings_of("cline", limit(900));
    for env in [
        BTreeMap::new(),
        env_of(&[(CLINE_VAR, "1000")]),
        env_of(&[(OPENCODE_VAR, "900000")]),
    ] {
        let err = check_command_limit("cline", &env, &s).unwrap_err();
        assert!(err.contains("command time limit"), "{err}");
    }
    // An engine without the variable is not checked.
    check_command_limit("fake-acp", &BTreeMap::new(), &s).unwrap();
}

fn placed<'a>(engine: &'a str, env: &'a BTreeMap<String, String>, gone: &'a Path) -> Placed<'a> {
    Placed {
        engine,
        config_dir: gone,
        data_dir: gone,
        argv: &[],
        env,
        stage_disabled: &[],
    }
}

#[test]
fn b800_the_read_back_of_the_second_engine_reads_the_variable_and_no_file_of_the_first() {
    let gone = Path::new("/nonexistent-b800");
    let s = settings_of("opencode", limit(900));
    let env = launch_env("opencode", &s);
    read_back(&placed("opencode", &env, gone), &s).unwrap();

    // The variable gone, or changed: the run is refused with the part and no value.
    for bad in [
        BTreeMap::new(),
        env_of(&[(OPENCODE_VAR, "120000")]),
        env_of(&[(CLINE_VAR, "900000")]),
    ] {
        let err = read_back(&placed("opencode", &bad, gone), &s).unwrap_err();
        assert!(
            err.contains("engine settings") && err.contains("environment"),
            "{err}"
        );
        assert!(!err.contains("900") && !err.contains("120000"), "{err}");
    }
}

#[test]
fn b800_the_read_back_of_an_engine_this_build_cannot_place_settings_for_is_still_refused() {
    let gone = Path::new("/nonexistent-b800");
    let s = settings_of("opencode", limit(900));
    let env = launch_env("opencode", &s);
    let err = read_back(&placed("fake-acp", &env, gone), &s).unwrap_err();
    assert!(err.contains("engine settings"), "{err}");
}

// ---- the event ---------------------------------------------------------------------------------------------------------------------

#[test]
fn b800_the_engine_config_event_of_a_second_engine_member_lists_the_limit_as_a_short_text() {
    use agentd::direct::engine_config::event;
    let c = member("opencode", limit(900));
    check_engine_config(&c).unwrap();
    let ev = event(&c, &c.to_ref());
    assert_eq!(ev["engine"], "opencode", "{ev}");
    assert_eq!(ev["keys"], 1, "{ev}");
    assert_eq!(ev["readBack"], "ok", "{ev}");
    assert_eq!(
        ev["settings"],
        json!([{"key": "commands.timeout_seconds", "value": "900"}]),
        "{ev}"
    );
}
