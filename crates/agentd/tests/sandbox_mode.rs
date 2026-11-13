//! The mode of the engine's confinement is the deployment's choice: `uid` or `landlock`, and nothing else starts an engine.
#![allow(clippy::unwrap_used)]

use agentd::config::Config;
use agentd::sandbox::{ENV_VAR, Mode, Setting};

#[test]
fn the_variable_is_named_for_the_deployment() {
    assert_eq!(ENV_VAR, "AGENTD_ENGINE_SANDBOX");
}

#[test]
fn unset_is_a_refusal_that_names_the_variable_and_the_two_modes() {
    match Setting::from_value(None) {
        Setting::Refused(why) => {
            assert!(why.contains("AGENTD_ENGINE_SANDBOX"), "{why}");
            assert!(why.contains("uid") && why.contains("landlock"), "{why}");
        }
        other => panic!("unset must be refused, got {other:?}"),
    }
}

#[test]
fn the_two_modes_parse_and_surrounding_space_is_ignored() {
    assert_eq!(Setting::from_value(Some("uid")), Setting::Mode(Mode::Uid));
    assert_eq!(
        Setting::from_value(Some("landlock")),
        Setting::Mode(Mode::Landlock)
    );
    assert_eq!(
        Setting::from_value(Some(" landlock\n")),
        Setting::Mode(Mode::Landlock)
    );
}

#[test]
fn every_other_value_is_refused_with_the_value_in_the_reason() {
    for v in [
        "",
        "auto",
        "Landlock",
        "LANDLOCK",
        "none",
        "off",
        "uid,landlock",
        "0",
    ] {
        match Setting::from_value(Some(v)) {
            Setting::Refused(why) => assert!(
                why.contains("AGENTD_ENGINE_SANDBOX"),
                "{v:?}: the reason must name the variable: {why}"
            ),
            other => panic!("{v:?} must be refused, got {other:?}"),
        }
    }
    let Setting::Refused(why) = Setting::from_value(Some("auto")) else {
        panic!("auto is refused");
    };
    assert!(why.contains("auto"), "the value is in the reason: {why}");
}

#[test]
fn landlock_needs_no_engine_user_and_uid_takes_any() {
    let landlock = Setting::Mode(Mode::Landlock);
    assert_eq!(landlock.resolve(None), Ok(Mode::Landlock));
    assert_eq!(landlock.resolve(Some("")), Ok(Mode::Landlock));
    let conflict = landlock.resolve(Some("agent")).unwrap_err();
    assert!(conflict.contains("AGENTD_ENGINE_USER"), "{conflict}");
    let uid = Setting::Mode(Mode::Uid);
    assert_eq!(uid.resolve(None), Ok(Mode::Uid));
    assert_eq!(uid.resolve(Some("agent")), Ok(Mode::Uid));
}

#[test]
fn a_refused_setting_resolves_to_its_reason_whatever_the_engine_user() {
    let refused = Setting::from_value(Some("auto"));
    let a = refused.resolve(None).unwrap_err();
    let b = refused.resolve(Some("agent")).unwrap_err();
    assert!(a.contains("auto") && b.contains("auto"));
}

#[test]
fn config_reads_the_variable() {
    let landlock = Config::from_vars(|k| (k == ENV_VAR).then(|| "landlock".to_string()));
    assert_eq!(landlock.engine_sandbox, Setting::Mode(Mode::Landlock));
    let unset = Config::from_vars(|_| None);
    assert!(matches!(unset.engine_sandbox, Setting::Refused(_)));
}
