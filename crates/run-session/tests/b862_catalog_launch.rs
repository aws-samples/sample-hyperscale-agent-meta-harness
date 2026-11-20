//! The reasoning level of every Cline row of the shipped model list reaches the engine's launch. The loop over the list came from
//! `crates/agentd/tests/engine_reasoning.rs`.
//!
//! Why it lives here: every file of `crates/agentd` is an input of the worker image key, and a test there that reads `engine/catalog/models.json`
//! has to change with every model update, which moves the key and costs every stage a worker image for the same worker code. run-session depends
//! on agentd and is outside the key, so this file follows the list and the worker image does not.
//!
//! The test names no number of rows and no list of levels: a row is checked the day it is added. The default level of each model is pinned,
//! with its reason, in `crates/ham-core/tests/model_reasoning.rs`.
#![allow(clippy::unwrap_used)]

use agentd::engine::direct::direct_launch_argv;
use agentd::engine::user_settings::{ReasoningSource, UserSettings, check_reasoning_launch};
use ham_core::models::builtin;
use harness_core::engine_profile::load_profile;
use harness_protocol::direct::{EngineConfig, REASONING_LEVELS, check_engine_config};
use run_session::prepare::model_spec;
use serde_json::{Value, json};
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn profile_argv() -> Vec<String> {
    load_profile(&repo_root().join("engine/profiles/cline.toml"))
        .unwrap()
        .transports
        .acp
        .argv
}

fn settings_of(doc: Value) -> UserSettings {
    let c: EngineConfig =
        serde_json::from_value(json!({"engine": "cline", "schema": 1, "settings": doc})).unwrap();
    check_engine_config(&c).unwrap();
    UserSettings::from_checked(&c).unwrap()
}

/// What the reasoning object of one row must do at the launch of the engine, or the first thing that is wrong, in words that name the model.
///
/// `level` is the row's `reasoning.default`, the level the model spec of a run carries; `accepted` is the row's `reasoning.accepted`.
fn row_launches(id: &str, level: &str, accepted: &[&str]) -> Result<(), String> {
    if !REASONING_LEVELS.contains(&level) {
        return Err(format!("{id}: {level:?} is not a launch level"));
    }
    if !accepted.contains(&level) {
        return Err(format!(
            "{id}: the default {level:?} is not an accepted level"
        ));
    }
    // The user keeps `default` (or sends no settings at all): the model's level reaches the argv, once and last.
    for settings in [
        UserSettings::default(),
        settings_of(json!({"reasoning": {"effort": "default"}})),
    ] {
        let launch = settings.reasoning_launch(Some(level));
        if launch.effort.flag() != Some(level) || launch.source != Some(ReasoningSource::Default) {
            return Err(format!(
                "{id}: default gives {:?} from {:?}, not {level:?}",
                launch.effort.flag(),
                launch.source
            ));
        }
        let argv = direct_launch_argv("cline", profile_argv(), launch.effort);
        if argv.iter().filter(|a| *a == "--thinking").count() != 1 {
            return Err(format!("{id}: the argv does not hold --thinking once"));
        }
        if argv.last().map(String::as_str) != Some(level) {
            return Err(format!("{id}: the argv does not end with {level:?}"));
        }
        check_reasoning_launch(&argv, &launch).map_err(|e| format!("{id}: {e}"))?;
    }
    // Every accepted level can be asked for by name, and it wins over the model's own.
    for word in accepted {
        let launch =
            settings_of(json!({"reasoning": {"effort": word}})).reasoning_launch(Some(level));
        if launch.effort.flag() != Some(*word) || launch.source != Some(ReasoningSource::Explicit) {
            return Err(format!(
                "{id}: asking for {word:?} gives {:?} from {:?}",
                launch.effort.flag(),
                launch.source
            ));
        }
        let argv = direct_launch_argv("cline", profile_argv(), launch.effort);
        if argv.last().map(String::as_str) != Some(*word) {
            return Err(format!("{id}: the argv does not end with {word:?}"));
        }
        check_reasoning_launch(&argv, &launch).map_err(|e| format!("{id}: {e}"))?;
    }
    // off launches with no reasoning argument, whatever the row says.
    let off = settings_of(json!({"reasoning": {"effort": "off"}})).reasoning_launch(Some(level));
    let argv = direct_launch_argv("cline", profile_argv(), off.effort);
    if off.source != Some(ReasoningSource::Off) || argv.iter().any(|a| a == "--thinking") {
        return Err(format!("{id}: off still launches with a reasoning level"));
    }
    check_reasoning_launch(&argv, &off).map_err(|e| format!("{id}: {e}"))
}

#[test]
fn b862_each_cline_row_of_the_shipped_catalog_launches_with_its_own_level() {
    let mut checked = 0;
    for m in builtin().models() {
        let Some(reasoning) = &m.reasoning else {
            continue;
        };
        let accepted: Vec<&str> = reasoning.accepted.iter().map(String::as_str).collect();
        row_launches(&m.id, &reasoning.default, &accepted).unwrap();
        // The level travels in the model spec run-session builds from the row; agentd launches from the spec, never from the list.
        if m.selectable() {
            let spec = model_spec(&m.id).unwrap();
            assert_eq!(
                spec.default_reasoning_effort.as_deref(),
                Some(reasoning.default.as_str()),
                "{}",
                m.id
            );
        }
        checked += 1;
    }
    assert!(
        checked > 0,
        "the shipped list has no row with a reasoning object"
    );
}

/// The check itself, on rows the shipped list does not hold: a level no shipped row uses as its default passes, and each way a row can be
/// wrong is named.
#[test]
fn b862_the_row_check_takes_any_launch_level_and_names_a_row_that_is_wrong() {
    // The shipped rows default to high or xhigh; a row may give any of the four.
    row_launches("fixture.low", "low", &["low"]).unwrap();
    row_launches("fixture.medium", "medium", &["low", "medium", "high"]).unwrap();
    let e = row_launches("fixture.max", "max", &["max"]).unwrap_err();
    assert!(
        e.contains("fixture.max") && e.contains("not a launch level"),
        "{e}"
    );
    let e = row_launches("fixture.out", "xhigh", &["low", "high"]).unwrap_err();
    assert!(
        e.contains("fixture.out") && e.contains("not an accepted level"),
        "{e}"
    );
}
