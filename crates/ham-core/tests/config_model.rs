//! The config file's sixth key, `default_model`: the model a direct run uses when the user
//! passes no `--model` and sets no `HAMH_MODEL`. It is validated against `engine/catalog/models.json` when it is set
//! (`ham config set default_model <id|alias>` stores the full id), and `get`, `set` and `unset` accept the snake_case spelling
//! the owner uses and the camelCase spelling the other keys have in the file.

#![allow(clippy::unwrap_used)]

use ham_core::config::{
    Config, ConfigError, KEYS, get_key, parse_config, render_config, set_key, unset_key,
};

const SONNET: &str = "global.anthropic.claude-sonnet-5-5";
const OPUS: &str = "us.anthropic.claude-opus-5-5";

#[test]
fn the_config_holds_fifteen_documented_keys_and_default_model_round_trips() {
    let cfg = Config {
        api_endpoint: Some("https://abc.execute-api.us-east-1.amazonaws.com/direct".into()),
        region: Some("us-east-1".into()),
        aws_profile: Some("ham-acct-a01".into()),
        default_repo: Some("cc:hamh-e2e-python-calc".into()),
        download_dir: Some("/downloads".into()),
        default_model: Some(OPUS.into()),
        input_wait_minutes: Some(10),
        mouse: None,
        report_tests: None,
        ..Config::default()
    };
    let text = render_config(&cfg);
    assert_eq!(parse_config(&text).unwrap(), cfg);
    assert!(text.contains("defaultModel"), "{text}");
    assert_eq!(KEYS.len(), 46);
    // The owner's spelling is read too.
    let owner = parse_config("default_model = \"us.openai.gpt-6.1-sol\"\n").unwrap();
    assert_eq!(
        owner.default_model.as_deref(),
        Some("us.openai.gpt-6.1-sol")
    );
    // An unset default is not written at all.
    assert!(!render_config(&Config::default()).contains("defaultModel"));
    assert!(
        parse_config("surprise = 1\n").is_err(),
        "unknown keys are still refused"
    );
}

#[test]
fn set_stores_the_full_id_for_an_alias_and_for_an_id() {
    for (value, want) in [
        ("opus", OPUS),
        ("Sonnet", SONNET),
        (" astra ", "us.openai.gpt-6-astra"),
        (OPUS, OPUS),
        ("sol", "us.openai.gpt-6.1-sol"),
    ] {
        for key in ["default_model", "defaultModel"] {
            let cfg = set_key(&Config::default(), key, value).unwrap();
            assert_eq!(cfg.default_model.as_deref(), Some(want), "{key} {value:?}");
        }
    }
}

#[test]
fn set_refuses_an_unknown_model_and_lists_the_choices_and_changes_nothing() {
    let before = Config {
        default_model: Some(OPUS.into()),
        ..Config::default()
    };
    for bad in ["", "   ", "gpt-9", "zai.glm-5", "glm-5", "OPUS-5-5"] {
        let e = set_key(&before, "default_model", bad).unwrap_err();
        match &e {
            ConfigError::Invalid { key, reason } => {
                assert_eq!(key, "default_model");
                for want in ["sonnet", "opus", "sol", "astra"] {
                    assert!(reason.contains(want), "{bad:?}: {reason}");
                }
            }
            other => panic!("{bad:?}: {other:?}"),
        }
    }
    assert_eq!(before.default_model.as_deref(), Some(OPUS));
}

#[test]
fn set_get_and_unset_work_for_every_key_in_both_spellings() {
    let mut cfg = Config::default();
    for (key, snake, value) in [
        (
            "apiEndpoint",
            "api_endpoint",
            "https://x.execute-api.us-east-1.amazonaws.com/direct",
        ),
        ("region", "region", "us-east-1"),
        ("awsProfile", "aws_profile", "ham-acct-a01"),
        ("defaultRepo", "default_repo", "cc:hamh-e2e-python-calc"),
        ("downloadDir", "download_dir", "/tmp/dl"),
    ] {
        cfg = set_key(&cfg, snake, value).unwrap();
        assert_eq!(get_key(&cfg, key).unwrap().as_deref(), Some(value), "{key}");
        assert_eq!(
            get_key(&cfg, snake).unwrap().as_deref(),
            Some(value),
            "{snake}"
        );
        cfg = unset_key(&cfg, key).unwrap();
        assert_eq!(get_key(&cfg, key).unwrap(), None, "{key} unset");
    }
    assert_eq!(cfg, Config::default());
    // A value with a newline or a control character would corrupt the file.
    for bad in ["a\nb", "a\u{1b}b", ""] {
        assert!(
            set_key(&Config::default(), "region", bad).is_err(),
            "{bad:?}"
        );
    }
}

#[test]
fn an_unknown_key_is_refused_with_the_known_ones() {
    for f in [
        set_key(&Config::default(), "default-model", "opus").map(|_| ()),
        unset_key(&Config::default(), "bogus").map(|_| ()),
        get_key(&Config::default(), "bogus").map(|_| ()),
    ] {
        match f.unwrap_err() {
            ConfigError::UnknownKey { key, known } => {
                assert!(key == "default-model" || key == "bogus");
                for k in ["default_model", "api_endpoint", "download_dir"] {
                    assert!(known.iter().any(|x| x == k), "{k} in {known:?}");
                }
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn unsetting_the_default_returns_to_the_built_in_one() {
    let cfg = set_key(&Config::default(), "default_model", "opus").unwrap();
    let cfg = unset_key(&cfg, "default_model").unwrap();
    assert_eq!(cfg.default_model, None);
    assert!(!render_config(&cfg).contains("defaultModel"));
}

#[test]
fn the_same_key_in_both_spellings_is_refused() {
    let text = "defaultModel = \"us.anthropic.claude-opus-5-5\"\ndefault_model = \"us.openai.gpt-6.1-sol\"\n";
    assert!(
        parse_config(text).is_err(),
        "two spellings of one key are ambiguous"
    );
}
