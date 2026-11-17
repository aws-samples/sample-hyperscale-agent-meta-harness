//! The shell command time limit, one setting for both engines. Cline's fork has ten minutes as its own default, so its
//! launch carries `CLINE_RUN_COMMANDS_TIMEOUT_MS` only for a limit the user set. OpenCode's shell tool stops a command that names no limit after
//! two minutes, and the pinned release reads no variable for it (the variable an earlier build set, `OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS`,
//! is a name the pinned binary does not hold). Its limit is placed in the session configuration, in every launch: the user's
//! `commands.timeout_seconds`, else the ten minutes Cline's runs end up with. The placement itself is tested in `b800_shell_limit.rs`; this
//! file keeps the setting, the key tables, the first engine's variable and the read-back of both engines.
#![allow(clippy::unwrap_used)]

mod b790_support;

use agentd::direct::request::engine_settings;
use agentd::emit::EmitContext;
use agentd::engine::direct::{TIMEOUT_ENV, launch_env, run_limit};
use agentd::engine::opencode::{Line, emit};
use agentd::engine::user_settings::{
    CommandSeconds, Placed, UserSettings, check_command_limit, read_back,
};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{CLINE_KEYS, EngineConfig, OPENCODE_KEYS, check_engine_config};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

/// The variable an earlier build set for the second engine. The pinned engine does not hold the name; no launch carries it any more.
const DEAD_OPENCODE_VAR: &str = "OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS";
const CLINE_VAR: &str = "CLINE_RUN_COMMANDS_TIMEOUT_MS";

/// The emitted launch of the second engine for `settings`: the context and the environment.
fn placed_for(settings: &UserSettings) -> (EmitContext, BTreeMap<String, String>) {
    let c = EmitContext {
        command_limit_ms: run_limit("opencode", settings).map(CommandSeconds::millis),
        ..b790_support::ctx(b790_support::MODEL)
    };
    let env = emit(&c, Line::V2).unwrap().env;
    (c, env)
}

fn timeout_ms(env: &BTreeMap<String, String>) -> Value {
    let doc: Value = serde_json::from_str(&env["OPENCODE_CONFIG_CONTENT"]).unwrap();
    doc["plugins"][0]["options"]["timeoutMs"].clone()
}

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
fn b800_the_first_engines_variable_is_its_own_and_the_second_engine_gets_none() {
    assert_eq!(TIMEOUT_ENV, CLINE_VAR);
    for settings in [UserSettings::default(), settings_of("opencode", limit(900))] {
        assert!(launch_env("opencode", &settings).is_empty());
        let (_, env) = placed_for(&settings);
        assert!(!env.contains_key(DEAD_OPENCODE_VAR), "{:?}", env.keys());
    }
}

#[test]
fn b800_a_launch_of_the_second_engine_with_no_setting_carries_ten_minutes() {
    let none = UserSettings::default();
    assert_eq!(
        run_limit("opencode", &none).map(CommandSeconds::get),
        Some(600)
    );
    let (_, env) = placed_for(&none);
    assert_eq!(timeout_ms(&env), json!(600_000));
}

#[test]
fn b800_a_launch_of_the_second_engine_carries_the_users_limit_in_milliseconds() {
    for (seconds, ms) in [
        (1, 1000u64),
        (30, 30_000),
        (600, 600_000),
        (900, 900_000),
        (1800, 1_800_000),
    ] {
        let s = settings_of("opencode", limit(seconds));
        assert_eq!(
            run_limit("opencode", &s).map(CommandSeconds::millis),
            Some(ms)
        );
        let (_, env) = placed_for(&s);
        assert_eq!(timeout_ms(&env), json!(ms), "{seconds} s");
        assert_eq!(s.command_timeout.unwrap().millis(), ms);
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
        run_limit("opencode", &UserSettings::default()),
        Some(CommandSeconds::default_limit())
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
    assert!(!env.contains_key(DEAD_OPENCODE_VAR));
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
    for engine in ["fake-acp", "pi", "", "opencode"] {
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
    // The text is the Debug form of the map `launch_env` returns, which is what the engine's environment gets. The second engine gets no variable.
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
        "{}",
        "{}",
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
    assert_eq!(run_limit("opencode", &s), CommandSeconds::new(900));
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

// ---- the check of the placement -----------------------------------------------------------------------------------------------

#[test]
fn b800_the_check_accepts_the_placement_the_launch_made() {
    for settings in [
        UserSettings::default(),
        settings_of("opencode", limit(1)),
        settings_of("opencode", limit(900)),
        settings_of("opencode", limit(1800)),
    ] {
        let (c, env) = placed_for(&settings);
        check_command_limit("opencode", &c.config_dir, &env, &settings).unwrap();
    }
    let gone = Path::new("/nonexistent-b800");
    for seconds in [1, 900, 1800] {
        let s = settings_of("cline", limit(seconds));
        check_command_limit("cline", gone, &launch_env("cline", &s), &s).unwrap();
    }
}

#[test]
fn b800_a_launch_of_the_second_engine_without_the_placement_is_refused_whatever_the_user_set() {
    // The check itself refuses in both cases. What a refusal means for the run differs: a user's limit ends it, the harness's own default is
    // told and the run goes on (`b800_shell_limit.rs`, `limit_step`).
    let gone = Path::new("/nonexistent-b800");
    for settings in [UserSettings::default(), settings_of("opencode", limit(900))] {
        let err = check_command_limit("opencode", gone, &BTreeMap::new(), &settings).unwrap_err();
        assert!(
            err.contains("engine settings") && err.contains("command time limit"),
            "{err}"
        );
        // The variable of an earlier build is no placement, and the other engine's variable is none either.
        for env in [
            env_of(&[(DEAD_OPENCODE_VAR, "900000")]),
            env_of(&[(CLINE_VAR, "900000")]),
        ] {
            assert!(check_command_limit("opencode", gone, &env, &settings).is_err());
        }
    }
}

#[test]
fn b800_a_wrong_number_is_refused_and_no_value_is_named() {
    let s = settings_of("opencode", limit(900));
    // The placement of another limit: the default's, and the engine's own number.
    for other in [UserSettings::default(), settings_of("opencode", limit(120))] {
        let (c, env) = placed_for(&other);
        let err = check_command_limit("opencode", &c.config_dir, &env, &s).unwrap_err();
        assert!(err.contains("command time limit"), "{err}");
        assert!(
            !err.contains("900") && !err.contains("120000") && !err.contains("600"),
            "{err}"
        );
    }
    // The default launch is checked against the default.
    let d = UserSettings::default();
    let (c, env) = placed_for(&s);
    assert!(check_command_limit("opencode", &c.config_dir, &env, &d).is_err());
    let (c, env) = placed_for(&d);
    check_command_limit("opencode", &c.config_dir, &env, &d).unwrap();
}

#[test]
fn b800_the_first_engine_is_checked_only_for_a_limit_the_user_set() {
    let gone = Path::new("/nonexistent-b800");
    // Nothing set: the stage may set its own default for the variable, or none, and the check does not look.
    let d = UserSettings::default();
    check_command_limit("cline", gone, &BTreeMap::new(), &d).unwrap();
    check_command_limit("cline", gone, &env_of(&[(CLINE_VAR, "1000")]), &d).unwrap();
    // A limit set: the variable must hold it.
    let s = settings_of("cline", limit(900));
    for env in [
        BTreeMap::new(),
        env_of(&[(CLINE_VAR, "1000")]),
        env_of(&[(DEAD_OPENCODE_VAR, "900000")]),
    ] {
        let err = check_command_limit("cline", gone, &env, &s).unwrap_err();
        assert!(err.contains("command time limit"), "{err}");
        assert!(err.contains("environment"), "{err}");
    }
    // An engine with no placement is not checked.
    check_command_limit("fake-acp", gone, &BTreeMap::new(), &s).unwrap();
}

fn placed<'a>(
    engine: &'a str,
    env: &'a BTreeMap<String, String>,
    config_dir: &'a Path,
) -> Placed<'a> {
    Placed {
        engine,
        config_dir,
        data_dir: Path::new("/nonexistent-b800"),
        argv: &[],
        env,
        stage_disabled: &[],
    }
}

#[test]
fn b800_the_read_back_of_the_second_engine_reads_its_placement_and_no_file_of_the_first() {
    let s = settings_of("opencode", limit(900));
    let (c, env) = placed_for(&s);
    read_back(&placed("opencode", &env, &c.config_dir), &s).unwrap();

    // The placement gone, or another number: the run is refused with the part and no value.
    let (other_c, other_env) = placed_for(&UserSettings::default());
    for (bad, dir) in [
        (BTreeMap::new(), c.config_dir.clone()),
        (
            env_of(&[(DEAD_OPENCODE_VAR, "900000")]),
            c.config_dir.clone(),
        ),
        (other_env, other_c.config_dir.clone()),
    ] {
        let err = read_back(&placed("opencode", &bad, &dir), &s).unwrap_err();
        assert!(
            err.contains("engine settings") && err.contains("configuration"),
            "{err}"
        );
        assert!(!err.contains("900") && !err.contains("600000"), "{err}");
    }
}

#[test]
fn b800_the_read_back_of_an_engine_this_build_cannot_place_settings_for_is_still_refused() {
    let s = settings_of("opencode", limit(900));
    let (c, env) = placed_for(&s);
    let err = read_back(&placed("fake-acp", &env, &c.config_dir), &s).unwrap_err();
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
