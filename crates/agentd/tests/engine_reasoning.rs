//! The reasoning level of a launch, in pieces.
//! `direct_reasoning_default.rs` runs the whole agent; these tests take the typed choice, the argv word and the read-back one at a time.
//!
//! The rule: the user's `reasoning.effort` decides, and `default` (or no engine settings at all) means the level the model's catalog row gave the
//! model spec (`DirectModelSpec.defaultReasoningEffort`). `off` and a spec with no level launch the engine with no reasoning argument.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::engine::direct::direct_launch_argv;
use agentd::engine::user_settings::{
    Effort, ReasoningLaunch, ReasoningSource, UserSettings, check_reasoning_launch,
};
use common::repo_root;
use harness_core::engine_profile::load_profile;
use harness_protocol::direct::{EngineConfig, check_engine_config};
use serde_json::{Value, json};

fn words(a: &[&str]) -> Vec<String> {
    a.iter().map(ToString::to_string).collect()
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

fn effort_of(word: &str) -> Effort {
    settings_of(json!({"reasoning": {"effort": word}})).effort
}

#[test]
fn the_key_has_six_values_and_the_two_new_ones_are_typed() {
    assert_eq!(effort_of("default"), Effort::Default);
    assert_eq!(effort_of("off"), Effort::Off);
    assert_eq!(effort_of("low"), Effort::Low);
    assert_eq!(effort_of("medium"), Effort::Medium);
    assert_eq!(effort_of("high"), Effort::High);
    assert_eq!(effort_of("xhigh"), Effort::XHigh);
    for (e, flag) in [
        (Effort::Default, None),
        (Effort::Off, None),
        (Effort::Low, Some("low")),
        (Effort::Medium, Some("medium")),
        (Effort::High, Some("high")),
        (Effort::XHigh, Some("xhigh")),
    ] {
        assert_eq!(e.flag(), flag, "{e:?}");
    }
}

#[test]
fn off_and_xhigh_count_as_settings_but_default_still_places_nothing() {
    assert!(!settings_of(json!({"reasoning": {"effort": "off"}})).is_empty());
    assert!(!settings_of(json!({"reasoning": {"effort": "xhigh"}})).is_empty());
    assert!(settings_of(json!({"reasoning": {"effort": "default"}})).is_empty());
    // A value the check refuses is still refused by the typed reader, naming the key and never the value.
    let c: EngineConfig = serde_json::from_value(
        json!({"engine": "cline", "schema": 1, "settings": {"reasoning": {"effort": "max"}}}),
    )
    .unwrap();
    let e = UserSettings::from_checked(&c).unwrap_err();
    assert!(e.contains("reasoning.effort") && !e.contains("max"), "{e}");
}

#[test]
fn the_choice_follows_the_user_first_and_the_catalog_second() {
    let launch = |effort: Effort, catalog: Option<&str>| {
        let s = UserSettings {
            effort,
            ..UserSettings::default()
        };
        s.reasoning_launch(catalog)
    };
    // default (or no settings) takes the catalog level.
    for level in ["low", "medium", "high", "xhigh"] {
        let l = launch(Effort::Default, Some(level));
        assert_eq!(l.effort.flag(), Some(level));
        assert_eq!(l.source, Some(ReasoningSource::Default));
        let l = UserSettings::default().reasoning_launch(Some(level));
        assert_eq!(l.effort.flag(), Some(level));
        assert_eq!(l.source, Some(ReasoningSource::Default));
    }
    // default with no catalog level is the launch of 1.45.0: no argument, nothing recorded.
    assert_eq!(
        launch(Effort::Default, None),
        ReasoningLaunch {
            effort: Effort::Default,
            source: None
        }
    );
    // A word that is not a launch level in the spec is no level.
    assert_eq!(launch(Effort::Default, Some("max")).source, None);
    assert_eq!(launch(Effort::Default, Some("")).effort.flag(), None);
    // The user's own level wins over the catalog.
    let l = launch(Effort::High, Some("xhigh"));
    assert_eq!(
        (l.effort.flag(), l.source),
        (Some("high"), Some(ReasoningSource::Explicit))
    );
    let l = launch(Effort::XHigh, Some("high"));
    assert_eq!(
        (l.effort.flag(), l.source),
        (Some("xhigh"), Some(ReasoningSource::Explicit))
    );
    let l = launch(Effort::Low, None);
    assert_eq!(
        (l.effort.flag(), l.source),
        (Some("low"), Some(ReasoningSource::Explicit))
    );
    // off launches with no argument whatever the catalog says, and says so.
    for catalog in [Some("xhigh"), Some("low"), None] {
        let l = launch(Effort::Off, catalog);
        assert_eq!(
            (l.effort.flag(), l.source),
            (None, Some(ReasoningSource::Off))
        );
    }
    assert_eq!(ReasoningSource::Default.word(), "default");
    assert_eq!(ReasoningSource::Explicit.word(), "explicit");
    assert_eq!(ReasoningSource::Off.word(), "off");
}

#[test]
fn the_argv_carries_xhigh_once_and_off_removes_any_level_the_profile_names() {
    let base = direct_launch_argv("cline", profile_argv(), Effort::Default);
    let mut want = base.clone();
    want.extend(words(&["--thinking", "xhigh"]));
    let argv = direct_launch_argv("cline", profile_argv(), Effort::XHigh);
    assert_eq!(argv, want);
    assert_eq!(
        direct_launch_argv("cline", argv.clone(), Effort::XHigh),
        argv
    );
    // off adds nothing, and takes a level the profile names out, so that off means off.
    assert_eq!(
        direct_launch_argv("cline", profile_argv(), Effort::Off),
        base
    );
    let with_level = words(&[
        "/opt/harness/bin/cline",
        "--acp",
        "--thinking",
        "high",
        "--config",
        "c",
    ]);
    assert_eq!(
        direct_launch_argv("cline", with_level.clone(), Effort::Off),
        words(&[
            "/opt/harness/bin/cline",
            "--acp",
            "--auto-approve",
            "true",
            "--config",
            "c"
        ])
    );
    // default leaves what the profile names, as before.
    assert!(
        direct_launch_argv("cline", with_level, Effort::Default)
            .windows(2)
            .any(|w| w == ["--thinking", "high"])
    );
    // Only Cline has the flag.
    assert_eq!(
        direct_launch_argv("fake-acp", words(&["x", "--acp"]), Effort::XHigh),
        words(&["x", "--acp"])
    );
}

#[test]
fn the_read_back_finds_the_level_in_the_argv_or_names_the_part_that_differs() {
    let launch = |effort, source| ReasoningLaunch { effort, source };
    let ok = words(&["cline", "--acp", "--thinking", "xhigh"]);
    check_reasoning_launch(&ok, &launch(Effort::XHigh, Some(ReasoningSource::Default))).unwrap();
    // Nothing asked, nothing checked: the launch of 1.45.0.
    check_reasoning_launch(&words(&["cline", "--acp"]), &launch(Effort::Default, None)).unwrap();
    check_reasoning_launch(
        &words(&["cline", "--acp"]),
        &launch(Effort::Off, Some(ReasoningSource::Off)),
    )
    .unwrap();
    for (argv, want) in [
        (
            words(&["cline", "--acp"]),
            launch(Effort::XHigh, Some(ReasoningSource::Default)),
        ),
        (
            words(&["cline", "--acp", "--thinking", "high"]),
            launch(Effort::XHigh, Some(ReasoningSource::Default)),
        ),
        (
            words(&["cline", "--acp", "--thinking", "high"]),
            launch(Effort::Off, Some(ReasoningSource::Off)),
        ),
        (
            words(&["cline", "--acp", "--thinking"]),
            launch(Effort::Low, Some(ReasoningSource::Explicit)),
        ),
    ] {
        let e = check_reasoning_launch(&argv, &want).unwrap_err();
        assert!(e.contains("reasoning level"), "{e}");
        assert!(
            !e.contains("xhigh") && !e.contains("high"),
            "no value in the words: {e}"
        );
    }
}

/// The six supported models, read from the shipped catalog (a test-only read: agentd never reads `engine/catalog`): each launches with its own level
/// when the user keeps `default`, and the user's words still win or switch it off. Claude Fable 5.2 is the sixth row and defaults to high.
#[test]
fn each_model_of_the_shipped_catalog_launches_with_its_own_level() {
    let catalog: Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("engine/catalog/models.json")).unwrap(),
    )
    .unwrap();
    // The rows of the default engine carry the reasoning levels; a row that names another engine is the same model smoked with it and has none.
    let rows: Vec<&Value> = catalog["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r.get("engine").is_none())
        .collect();
    assert_eq!(rows.len(), 6);
    let mut seen = Vec::new();
    for row in rows {
        let id = row["id"].as_str().unwrap();
        let level = row["reasoning"]["default"].as_str().unwrap();
        let accepted: Vec<&str> = row["reasoning"]["accepted"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(accepted.contains(&level), "{id}");
        // keep default (or no settings): the model's level reaches the argv.
        for settings in [
            UserSettings::default(),
            settings_of(json!({"reasoning": {"effort": "default"}})),
        ] {
            let launch = settings.reasoning_launch(Some(level));
            assert_eq!(launch.effort.flag(), Some(level), "{id}");
            let argv = direct_launch_argv("cline", profile_argv(), launch.effort);
            assert_eq!(
                argv.iter().filter(|a| *a == "--thinking").count(),
                1,
                "{id}"
            );
            assert_eq!(argv.last().map(String::as_str), Some(level), "{id}");
            check_reasoning_launch(&argv, &launch).unwrap();
        }
        // every accepted level can be asked for explicitly, and off removes it.
        for word in &accepted {
            let launch =
                settings_of(json!({"reasoning": {"effort": word}})).reasoning_launch(Some(level));
            assert_eq!(launch.effort.flag(), Some(*word), "{id} {word}");
            assert_eq!(launch.source, Some(ReasoningSource::Explicit));
        }
        let off =
            settings_of(json!({"reasoning": {"effort": "off"}})).reasoning_launch(Some(level));
        let argv = direct_launch_argv("cline", profile_argv(), off.effort);
        assert!(!argv.iter().any(|a| a == "--thinking"), "{id}");
        seen.push((id.to_string(), level.to_string()));
    }
    assert_eq!(
        seen.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>(),
        ["high", "high", "xhigh", "xhigh", "high", "high"],
        "{seen:?}"
    );
}
