//! Where the harness's files for the engine go, how that place is protected, which settings are locked and which switches are off.
//! One layout per engine behind one API; Cline's place and protection are the ones it always had.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::engine::direct::{
    config_is_read_only, lock_global_settings, locked_settings_path, protect_config,
    user_rules_dir, user_skills_dir,
};
use agentd::engine::layout::{
    CONFIG_ROOT_ENV, KillSwitches, Protection, check_kill_switches, config_layout, config_root_env,
    kill_switches, lock_settings, protect, protection_of,
};
use harness_core::engine_profile::{bundled_names, bundled_profile, known_engine};
use serde_json::Value;
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

fn read_json(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

// ---- the layout -------------------------------------------------------------------------------------------------------------

#[test]
fn b789_the_layout_of_the_qualified_engine_is_its_config_directory_and_the_older_functions_agree() {
    let config = common::scratch("b789-layout-cline");
    let l = config_layout("cline", &config);
    assert_eq!(l.config_dir, config);
    assert_eq!(
        l.root, config,
        "the harness's files are in the engine's own config directory"
    );
    assert_eq!(l.rules, config.join("rules"));
    assert_eq!(l.skills, config.join("skills"));
    assert_eq!(l.locked, config.join("locked"));
    assert_eq!(
        l.locked_settings(),
        config.join("locked/global-settings.json")
    );
    assert_eq!(l.rules, user_rules_dir(&config));
    assert_eq!(l.skills, user_skills_dir(&config));
    assert_eq!(l.locked_settings(), locked_settings_path(&config));
    assert_eq!(l.protection, Protection::RootOwnsConfigDir);
}

#[test]
fn b789_an_engine_that_keeps_state_in_its_config_directory_gets_a_root_of_its_own() {
    let config = common::scratch("b789-layout-state").join("config");
    let l = config_layout("opencode", &config);
    assert_eq!(l.protection, Protection::SeparateRoot);
    assert_eq!(l.config_dir, config);
    // The root is a sibling: neither inside the directory the engine writes nor around it.
    assert!(!l.root.starts_with(&config), "{:?}", l.root);
    assert!(!config.starts_with(&l.root), "{:?}", l.root);
    assert_eq!(l.root.parent(), config.parent());
    for p in [&l.rules, &l.skills, &l.locked] {
        assert!(p.starts_with(&l.root), "{p:?}");
        assert!(!p.starts_with(&config), "{p:?}");
    }
    assert_eq!(l.rules, l.root.join("rules"));
    assert_eq!(l.skills, l.root.join("skills"));
    // The engine's state directory stays the engine's.
    assert!(!config_is_read_only("opencode"));
}

#[test]
fn b789_an_engine_with_no_arm_keeps_the_old_layout_and_is_not_protected() {
    for engine in ["fake-acp", "an-engine-with-no-arm"] {
        let config = common::scratch("b789-layout-none");
        let l = config_layout(engine, &config);
        assert_eq!(l.protection, Protection::InPlace, "{engine}");
        assert_eq!(
            (l.root.clone(), l.rules.clone(), l.skills.clone()),
            (config.clone(), config.join("rules"), config.join("skills")),
            "{engine}"
        );
        assert_eq!(protection_of(engine), Protection::InPlace);
        assert!(!config_is_read_only(engine));
    }
}

#[test]
fn b789_preparing_a_layout_makes_the_separate_root_and_refuses_a_link_in_its_place() {
    let base = common::scratch("b789-prepare");
    let config = base.join("config");
    std::fs::create_dir_all(&config).unwrap();
    // A layout inside the config directory needs no preparing.
    config_layout("cline", &config).prepare().unwrap();
    let l = config_layout("opencode", &config);
    assert!(!l.root.exists());
    l.prepare().unwrap();
    assert!(l.root.is_dir());
    l.prepare().unwrap();
    // A link where the root belongs is refused and what it points at is left alone.
    let other = common::scratch("b789-prepare-target");
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o700)).unwrap();
    let base2 = common::scratch("b789-prepare-two");
    let l2 = config_layout("opencode", &base2.join("config2"));
    std::fs::create_dir_all(base2.join("config2")).unwrap();
    std::os::unix::fs::symlink(&other, &l2.root).unwrap();
    let err = l2.prepare().unwrap_err();
    assert!(err.to_string().contains("symbolic link"), "{err}");
    assert_eq!(mode(&other), 0o700);
    assert!(std::fs::read_dir(&other).unwrap().next().is_none());
}

// ---- protection --------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn b789_the_qualified_engines_config_directory_still_goes_to_root_and_without_an_owner_nothing_happens()
 {
    let config = common::scratch("b789-protect-cline");
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o700)).unwrap();
    let l = config_layout("cline", &config);
    assert!(
        !protect(&l, None).await,
        "no engine user: nothing to take from"
    );
    assert_eq!(mode(&config), 0o700);
    assert!(protect(&l, Some((1234, 1234))).await);
    assert_eq!(
        mode(&config),
        0o755,
        "the config directory is readable and no longer the engine's to write"
    );
    // The older entry point is the same call.
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(protect_config("cline", &config, Some((1234, 1234))).await);
    assert_eq!(mode(&config), 0o755);
    assert!(!protect_config("cline", &config, None).await);
}

#[tokio::test]
async fn b789_a_separate_root_is_protected_and_the_engines_config_directory_is_left_to_the_engine()
{
    let base = common::scratch("b789-protect-sep");
    let config = base.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o700)).unwrap();
    let l = config_layout("opencode", &config);
    l.prepare().unwrap();
    std::fs::set_permissions(&l.root, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!protect(&l, None).await);
    assert!(protect(&l, Some((1234, 1234))).await);
    assert_eq!(
        mode(&l.root),
        0o755,
        "the root can be read by the engine, written by nobody but root"
    );
    assert_eq!(
        mode(&config),
        0o700,
        "the engine's own directory is not touched"
    );
}

// ---- locked settings ---------------------------------------------------------------------------------------------------------

#[test]
fn b789_the_hook_locks_the_qualified_engines_settings_exactly_as_the_older_function_does() {
    let seed = r#"{"telemetryOptOut": false, "autoUpdateEnabled": true, "other": 7}"#;
    let (a, b) = (
        common::scratch("b789-lock-a"),
        common::scratch("b789-lock-b"),
    );
    for d in [&a, &b] {
        std::fs::write(d.join("global-settings.json"), seed).unwrap();
    }
    let (mut env_a, mut env_b) = (BTreeMap::new(), BTreeMap::new());
    let disabled = ["spawn_agent".to_string()];
    let via_old = lock_global_settings("cline", &a, &disabled, &mut env_a)
        .unwrap()
        .unwrap();
    let via_hook = lock_settings("cline", &config_layout("cline", &b), &disabled, &mut env_b)
        .unwrap()
        .unwrap();
    assert_eq!(
        std::fs::read(&via_old).unwrap(),
        std::fs::read(&via_hook).unwrap(),
        "the same bytes"
    );
    let doc = read_json(&via_hook);
    assert_eq!(doc["telemetryOptOut"], true);
    assert_eq!(doc["autoUpdateEnabled"], false);
    assert_eq!(doc["disabledTools"], serde_json::json!(["spawn_agent"]));
    assert_eq!(doc["other"], 7);
    assert_eq!(mode(&via_hook), 0o444);
    assert_eq!(mode(via_hook.parent().unwrap()), 0o555);
    assert_eq!(
        env_a["CLINE_GLOBAL_SETTINGS_PATH"].replace(a.to_str().unwrap(), "<dir>"),
        env_b["CLINE_GLOBAL_SETTINGS_PATH"].replace(b.to_str().unwrap(), "<dir>")
    );
}

#[test]
fn b789_an_engine_with_no_locked_settings_file_gets_none_and_keeps_its_environment() {
    for engine in ["opencode", "fake-acp"] {
        let config = common::scratch("b789-lock-none");
        let mut env = BTreeMap::new();
        env.insert("KEEP".to_string(), "1".to_string());
        let got = lock_settings(engine, &config_layout(engine, &config), &[], &mut env).unwrap();
        assert_eq!(got, None, "{engine}");
        assert_eq!(env.len(), 1, "{engine}: nothing is added");
        assert!(!config.join("locked").exists());
    }
}

// ---- the switches that are off for every engine ------------------------------------------------------------------------------

#[test]
fn b789_every_engine_the_build_has_a_profile_for_declares_its_kill_switches() {
    for name in bundled_names() {
        let k: &KillSwitches = kill_switches(name)
            .unwrap_or_else(|| panic!("engine {name} has a profile and no kill-switch table"));
        if matches!(name, "cline" | "opencode") {
            assert!(
                !k.env.is_empty() || !k.locked.is_empty(),
                "{name}: telemetry and self-update are off by something"
            );
        }
    }
    assert!(kill_switches("an-engine-with-no-table").is_none());
}

#[test]
fn b789_a_launch_that_lacks_a_kill_switch_is_refused_and_the_message_names_it() {
    let config = common::scratch("b789-kill");
    let l = config_layout("cline", &config);
    std::fs::write(
        l.config_dir.join("global-settings.json"),
        r#"{"telemetryOptOut": true, "autoUpdateEnabled": false}"#,
    )
    .unwrap();
    let mut env = BTreeMap::new();
    let locked = lock_settings("cline", &l, &[], &mut env).unwrap();
    // The environment of the launch carries no switch yet.
    let err = check_kill_switches("cline", &env, locked.as_deref()).unwrap_err();
    assert!(err.contains("CLINE_NO_AUTO_UPDATE"), "{err}");
    env.insert("CLINE_NO_AUTO_UPDATE".into(), "1".into());
    check_kill_switches("cline", &env, locked.as_deref()).unwrap();
    // A switch with the wrong value is as good as none.
    env.insert("CLINE_NO_AUTO_UPDATE".into(), "0".into());
    assert!(check_kill_switches("cline", &env, locked.as_deref()).is_err());
    env.insert("CLINE_NO_AUTO_UPDATE".into(), "1".into());
    // The locked file must say it too, and must exist.
    let err = check_kill_switches("cline", &env, None).unwrap_err();
    assert!(err.contains("locked settings"), "{err}");
    let weak = config.join("weak.json");
    std::fs::write(
        &weak,
        r#"{"telemetryOptOut": false, "autoUpdateEnabled": false}"#,
    )
    .unwrap();
    let err = check_kill_switches("cline", &env, Some(&weak)).unwrap_err();
    assert!(err.contains("telemetryOptOut"), "{err}");
    // An engine the build has no table for is refused, not waved through.
    let err = check_kill_switches("an-engine-with-no-table", &env, None).unwrap_err();
    assert!(err.contains("an-engine-with-no-table"), "{err}");
}

#[test]
fn b789_the_kill_switches_an_engine_declares_are_in_its_profile_or_its_locked_file() {
    for name in ["cline", "opencode"] {
        let profile = bundled_profile(known_engine(name).unwrap())
            .unwrap()
            .unwrap();
        let k = kill_switches(name).unwrap();
        for (var, value) in k.env {
            assert_eq!(
                profile.env.get(*var).map(String::as_str),
                Some(*value),
                "{name}: the profile's [env] carries {var}"
            );
        }
    }
    // Cline's locked keys are the ones the lock writes.
    let config = common::scratch("b789-kill-profile");
    std::fs::write(config.join("global-settings.json"), "{}").unwrap();
    let mut env = BTreeMap::new();
    let locked = lock_settings("cline", &config_layout("cline", &config), &[], &mut env)
        .unwrap()
        .unwrap();
    let doc = read_json(&locked);
    for (key, want) in kill_switches("cline").unwrap().locked {
        assert_eq!(doc[*key], Value::Bool(*want), "{key}");
    }
}

// ---- the neutral variable ----------------------------------------------------------------------------------------------------

#[test]
fn b789_the_config_root_variable_names_the_directory_the_older_variable_pointed_into() {
    assert_eq!(CONFIG_ROOT_ENV, "HAMH_ENGINE_CONFIG_ROOT");
    let config = common::scratch("b789-root-var");
    std::fs::write(config.join("global-settings.json"), "{}").unwrap();
    let l = config_layout("cline", &config);
    let mut env = BTreeMap::new();
    lock_settings("cline", &l, &[], &mut env).unwrap();
    let (name, value) = config_root_env(&l);
    assert_eq!(name, CONFIG_ROOT_ENV);
    // `${CLINE_GLOBAL_SETTINGS_PATH%/locked/*}` of the shipped rule is this directory.
    let old = &env["CLINE_GLOBAL_SETTINGS_PATH"];
    assert_eq!(old.rsplit_once("/locked/").unwrap().0, value);
    // The lifecycle pack's scanner kit sits in the skills directory of every engine's root.
    let o = config_layout("opencode", &config.join("c"));
    assert_eq!(
        config_root_env(&o).1,
        o.root.display().to_string(),
        "the variable follows the root of the layout, not the engine's config directory"
    );
    assert_eq!(o.skills, o.root.join("skills"));
}
