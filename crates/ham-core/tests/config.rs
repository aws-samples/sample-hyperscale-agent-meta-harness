//! Local state: the client file `config.toml` beside the user's own rules, skills and engine files, plus downloaded artifacts.

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::config::{
    Config, MouseMode, ResearchTable, RunTable, UiTable, config_path, download_dir, parse_config,
    render_config,
};
use std::collections::HashMap;
use std::path::PathBuf;

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    move |k| map.get(k).cloned()
}

#[test]
fn config_path_prefers_xdg_config_home() {
    assert_eq!(
        config_path(&env(&[("XDG_CONFIG_HOME", "/x/cfg"), ("HOME", "/h")])),
        Some(PathBuf::from("/x/cfg/ham/config.toml"))
    );
    assert_eq!(
        config_path(&env(&[("HOME", "/h")])),
        Some(PathBuf::from("/h/.config/ham/config.toml"))
    );
    assert_eq!(config_path(&env(&[])), None);
}

#[test]
fn download_dir_prefers_the_environment() {
    let cfg = Config {
        download_dir: Some("/cfg/dl".into()),
        ..Config::default()
    };
    assert_eq!(
        download_dir(&env(&[("HAMH_DOWNLOAD_DIR", "/env/dl")]), &cfg),
        Some(PathBuf::from("/env/dl"))
    );
    assert_eq!(
        download_dir(&env(&[]), &cfg),
        Some(PathBuf::from("/cfg/dl"))
    );
    assert_eq!(download_dir(&env(&[]), &Config::default()), None);
}

#[test]
fn config_round_trips_and_holds_only_the_documented_keys() {
    let cfg = Config {
        api_endpoint: Some("https://abc.execute-api.us-west-2.amazonaws.com/live".into()),
        region: Some("us-west-2".into()),
        aws_profile: Some("ham-acct-b02".into()),
        default_repo: Some("cc:hamh-e2e-python-calc-20260925".into()),
        download_dir: Some("/downloads".into()),
        default_model: Some("global.anthropic.claude-sonnet-5-5".into()),
        default_engine: None,
        update_source: None,
        input_wait_minutes: Some(5),
        mouse: Some(MouseMode::Off),
        report_tests: Some(false),
        config_version: Some(1),
        default_backend: None,
        customer_content: None,
        backend: None,
        run: Some(RunTable {
            goal: Some(false),
            rules: Some(true),
            skills: Some(false),
            ..RunTable::default()
        }),
        research: Some(ResearchTable {
            enabled: Some(true),
            ..ResearchTable::default()
        }),
        ui: Some(UiTable {
            notify: Some(true),
            bell: Some(false),
            mascot: None,
            mascot_motion: None,
            copy_limit_kib: None,
            deprecation_notices: None,
        }),
    };
    let text = render_config(&cfg);
    assert_eq!(parse_config(&text).unwrap(), cfg);
    for key in [
        "apiEndpoint",
        "region",
        "awsProfile",
        "defaultRepo",
        "downloadDir",
        "defaultModel",
        "inputWaitMinutes",
        "mouse",
        "reportTests",
        "configVersion",
        "[run]",
        "goal",
        "rules",
        "skills",
        "[research]",
        "enabled",
        "[ui]",
        "notify",
        "bell",
    ] {
        assert!(text.contains(key), "{key} missing from {text}");
    }
    assert!(
        parse_config("region = \"us-west-2\"\nrunCache = []\n").is_err(),
        "unknown keys are refused"
    );
    assert_eq!(parse_config("").unwrap(), Config::default());
}
