//! Provider data sharing as a client setting: the key run.provider_sharing of [run] in config.toml and the flag --provider-sharing on|off of a
//! start. Off is the default. On rides in the request as providerSharing: true; off sends no member, so the request is byte for byte what an older client
//! sends. A model that needs sharing is refused by the client, before any call, while the setting is off.

#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, DirectArgs, parse};
use ham_core::config::{Config, KEYS, get_key, parse_config, set_key, unset_key};
use ham_core::models::{ModelError, Models};
use ham_core::request::{
    DirectInputs, Origin, SettingsError, resolve_run_settings, resolve_run_settings_in,
};
use ham_core::settings::{Applies, DefaultValue, Group, Key, Kind, Order};
use serde_json::{Value, json};

const PROMPT: &str = "Add a percent_change(a, b) function";
const FABLE: &str = "global.anthropic.claude-fable-5-2";

fn parsed(extra: &[&str]) -> Result<DirectArgs, String> {
    let mut words = vec![PROMPT, "--s3-only", "--direct"];
    words.extend_from_slice(extra);
    match parse(&words) {
        Ok(inv) => match inv.command {
            Command::StartDirect(a) => Ok(a),
            other => panic!("{words:?} is not a direct start: {other:?}"),
        },
        Err(e) => Err(e.to_string()),
    }
}

fn args(extra: &[&str]) -> DirectArgs {
    parsed(extra).unwrap()
}

fn cfg_of(text: &str) -> Config {
    parse_config(text).unwrap()
}

fn request(cfg: &Config, extra: &[&str]) -> Value {
    let run = resolve_run_settings(cfg, &|_| None, &args(extra), false).unwrap();
    serde_json::to_value(run.request(DirectInputs::default()).unwrap()).unwrap()
}

/// The committed list with the Fable row given a passing record, as it will read after the live call.
fn smoked() -> Models {
    let text = include_str!("../../../engine/catalog/models.json");
    let mut v: Value = serde_json::from_str(text).unwrap();
    let fable = v["models"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|r| r["id"] == FABLE)
        .unwrap();
    fable["smokeRecordId"] = json!("s6:test-record");
    fable["smokedAt"] = json!("2026-11-09T00:00:00Z");
    Models::parse(&v.to_string()).unwrap()
}

#[test]
fn b803_the_row_follows_the_total_run_time_and_rides_in_the_provider_sharing_member() {
    assert_eq!(Key::ALL.len(), 46);
    assert_eq!(Key::ALL[33], Key::RunProviderSharing);
    let r = Key::RunProviderSharing.row();
    assert_eq!(r.name, "run.provider_sharing");
    assert_eq!(r.file, ("run", "provider_sharing"));
    assert_eq!(
        (r.wire, r.group, r.since, r.order, r.applies),
        (
            Some("providerSharing"),
            Group::Run,
            1,
            Order::FlagFile,
            Applies::NextRun
        ),
        "{r:?}"
    );
    assert_eq!(r.flag, "--provider-sharing on|off");
    assert!(r.env.is_empty(), "no variable: the flag and the file only");
    assert_eq!(r.kind, Kind::Bool);
    assert_eq!(r.default, DefaultValue::Bool(false));
    assert_eq!(r.default_text, "off");
    assert!(
        !r.security,
        "the six security rows of today stay six: the label says what the setting does"
    );
    assert_eq!(
        Key::lookup("run.provider_sharing"),
        Some(Key::RunProviderSharing)
    );
    assert!(KEYS.contains(&("run.provider_sharing", "run.provider_sharing")));
    assert_eq!(KEYS.len(), 46);
    assert_eq!(KEYS[33], ("run.provider_sharing", "run.provider_sharing"));
    assert_eq!(Key::ALL[34], Key::DefaultBackend);
    assert_eq!(Key::ALL[35], Key::CustomerContent);
}

#[test]
fn b803_the_label_and_the_help_say_what_sharing_means() {
    let r = Key::RunProviderSharing.row();
    // The Settings screen draws a label in a column of 20 characters (the registry allows 24); the full phrase opens the help, which the screen shows in full.
    assert_eq!(r.label, "Data sharing (3P)");
    assert!(r.label.len() <= 20);
    assert!(
        r.help
            .starts_with("Share data with the model provider (3P sharing)."),
        "{}",
        r.help
    );
    for want in [
        "prompts and answers",
        "model's provider",
        "30 days",
        "safety monitoring",
        "needs",
    ] {
        assert!(r.help.contains(want), "{want:?} missing: {}", r.help);
    }
    assert!(!r.help.contains('\n'));
}

#[test]
fn b803_ham_config_sets_prints_and_unsets_the_key() {
    for (typed, printed) in [
        ("on", "true"),
        ("true", "true"),
        ("off", "false"),
        ("no", "false"),
    ] {
        let cfg = set_key(&Config::default(), "run.provider_sharing", typed).unwrap();
        assert_eq!(
            get_key(&cfg, "run.provider_sharing").unwrap().as_deref(),
            Some(printed),
            "{typed}"
        );
        assert_eq!(cfg.config_version, Some(1));
    }
    let cfg = set_key(&Config::default(), "run.provider_sharing", "on").unwrap();
    let cfg = unset_key(&cfg, "run.provider_sharing").unwrap();
    assert_eq!(get_key(&cfg, "run.provider_sharing").unwrap(), None);
    let e = set_key(&Config::default(), "run.provider_sharing", "maybe")
        .unwrap_err()
        .to_string();
    assert!(e.contains("run.provider_sharing"), "{e}");
}

#[test]
fn b803_the_file_reader_takes_the_key_and_names_the_key_of_a_wrong_one() {
    let cfg = cfg_of("configVersion = 1\n[run]\nprovider_sharing = true\n");
    assert_eq!(cfg.run.unwrap().provider_sharing, Some(true));
    for text in [
        "[run]\nprovider_sharing = \"on\"\n",
        "[run]\nprovider_sharing = 1\n",
    ] {
        let e = parse_config(text).unwrap_err().to_string();
        assert!(e.contains("provider_sharing"), "{text}: {e}");
    }
}

#[test]
fn b803_a_run_with_no_setting_and_no_flag_sends_no_member() {
    assert!(
        request(&Config::default(), &[])
            .get("providerSharing")
            .is_none()
    );
    let wire = request(&cfg_of("configVersion = 1\n[run]\nrules = true\n"), &[]);
    assert!(wire.get("providerSharing").is_none(), "{wire}");
    let off = request(
        &cfg_of("configVersion = 1\n[run]\nprovider_sharing = false\n"),
        &[],
    );
    assert!(
        off.get("providerSharing").is_none(),
        "off sends nothing: {off}"
    );
}

#[test]
fn b803_on_sends_true_and_the_flag_wins_over_the_file() {
    let on = cfg_of("configVersion = 1\n[run]\nprovider_sharing = true\n");
    assert_eq!(request(&on, &[])["providerSharing"], json!(true));
    assert_eq!(
        request(&Config::default(), &["--provider-sharing", "on"])["providerSharing"],
        json!(true)
    );
    let wire = request(&on, &["--provider-sharing", "off"]);
    assert!(wire.get("providerSharing").is_none(), "{wire}");
    let off = cfg_of("configVersion = 1\n[run]\nprovider_sharing = false\n");
    assert_eq!(
        request(&off, &["--provider-sharing", "on"])["providerSharing"],
        json!(true)
    );
}

#[test]
fn b803_the_flag_takes_on_or_off_once_and_only_on_a_direct_start() {
    assert_eq!(
        args(&["--provider-sharing", "on"]).provider_sharing,
        Some(true)
    );
    assert_eq!(
        args(&["--provider-sharing", "off"]).provider_sharing,
        Some(false)
    );
    assert_eq!(args(&[]).provider_sharing, None);
    for (bad, want) in [
        (vec!["--provider-sharing", "maybe"], "on or off"),
        (vec!["--provider-sharing", "true"], "on or off"),
        (vec!["--provider-sharing"], "--provider-sharing"),
        (
            vec!["--provider-sharing", "on", "--provider-sharing", "off"],
            "twice",
        ),
    ] {
        let e = parsed(&bad).unwrap_err();
        assert!(e.contains(want), "{bad:?}: {e}");
    }
    let e = parse(&[PROMPT, "--s3-only", "--provider-sharing", "on"])
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("--provider-sharing") && e.contains("--direct"),
        "{e}"
    );
}

#[test]
fn b803_the_flag_is_written_back_into_the_command_line() {
    let inv = parse(&[PROMPT, "--s3-only", "--direct", "--provider-sharing", "on"]).unwrap();
    let argv = inv.to_argv();
    let at = argv.iter().position(|w| w == "--provider-sharing").unwrap();
    assert_eq!(argv[at + 1], "on", "{argv:?}");
    let again = parse(&argv.iter().map(String::as_str).collect::<Vec<_>>()).unwrap();
    assert_eq!(again, inv);
}

#[test]
fn b803_where_the_value_came_from_is_the_flag_then_the_file_then_the_default() {
    let run = resolve_run_settings(&Config::default(), &|_| None, &args(&[]), false).unwrap();
    assert_eq!(run.origin(Key::RunProviderSharing), Some(Origin::Default));
    let cfg = cfg_of("configVersion = 1\n[run]\nprovider_sharing = true\n");
    let run = resolve_run_settings(&cfg, &|_| None, &args(&[]), false).unwrap();
    assert_eq!(run.origin(Key::RunProviderSharing), Some(Origin::File));
    let run = resolve_run_settings(
        &cfg,
        &|_| None,
        &args(&["--provider-sharing", "off"]),
        false,
    )
    .unwrap();
    assert_eq!(run.origin(Key::RunProviderSharing), Some(Origin::Flag));
    assert!(!run.provider_sharing);
}

#[test]
fn b803_a_model_that_needs_sharing_is_refused_by_the_client_while_the_setting_is_off() {
    let m = smoked();
    let e = resolve_run_settings_in(
        &m,
        &Config::default(),
        &|_| None,
        &args(&["--model", "fable"]),
        false,
    )
    .unwrap_err();
    assert!(
        matches!(e, SettingsError::Model(ModelError::NeedsSharing(_))),
        "{e:?}"
    );
    let text = e.to_string();
    for want in [
        "Claude Fable 5.2",
        FABLE,
        "needs provider data sharing",
        "ham config set run.provider_sharing on",
        "--provider-sharing on",
        "Settings screen",
        "30 days",
    ] {
        assert!(text.contains(want), "{want:?} missing: {text}");
    }
    // The flag set to off over a file that says on is the same refusal.
    let on = cfg_of("configVersion = 1\n[run]\nprovider_sharing = true\n");
    let e = resolve_run_settings_in(
        &m,
        &on,
        &|_| None,
        &args(&["--model", "fable", "--provider-sharing", "off"]),
        false,
    )
    .unwrap_err();
    assert!(matches!(
        e,
        SettingsError::Model(ModelError::NeedsSharing(_))
    ));
}

#[test]
fn b803_the_default_model_in_the_file_or_in_the_environment_is_refused_the_same_way() {
    let m = smoked();
    let cfg = Config {
        default_model: Some(FABLE.into()),
        ..Config::default()
    };
    let e = resolve_run_settings_in(&m, &cfg, &|_| None, &args(&[]), false).unwrap_err();
    assert!(
        matches!(e, SettingsError::Model(ModelError::NeedsSharing(_))),
        "{e:?}"
    );
    let env = |k: &str| (k == "HAMH_MODEL").then(|| "fable".to_string());
    let e = resolve_run_settings_in(&m, &Config::default(), &env, &args(&[]), false).unwrap_err();
    assert!(
        matches!(e, SettingsError::Model(ModelError::NeedsSharing(_))),
        "{e:?}"
    );
}

#[test]
fn b803_with_the_setting_on_the_sharing_model_is_chosen_and_nothing_else_changes() {
    let m = smoked();
    for extra in [
        vec!["--model", "fable", "--provider-sharing", "on"],
        vec!["--model", FABLE, "--provider-sharing", "on"],
    ] {
        let run = resolve_run_settings_in(&m, &Config::default(), &|_| None, &args(&extra), false)
            .unwrap();
        assert_eq!(run.model.id, FABLE);
        assert!(run.provider_sharing);
    }
    let on = cfg_of("configVersion = 1\n[run]\nprovider_sharing = true\n");
    let run =
        resolve_run_settings_in(&m, &on, &|_| None, &args(&["--model", "fable"]), false).unwrap();
    assert_eq!(run.model.id, FABLE);
    // Models that need nothing are never refused, whatever the setting says.
    let run = resolve_run_settings_in(
        &m,
        &Config::default(),
        &|_| None,
        &args(&["--model", "opus"]),
        false,
    )
    .unwrap();
    assert!(!run.provider_sharing);
}

#[test]
fn b803_the_committed_catalog_refuses_fable_while_the_setting_is_off_and_accepts_it_with_the_setting_on()
 {
    // The row has its live smoke record, so it is not pending: the setting alone decides.
    let e = resolve_run_settings(
        &Config::default(),
        &|_| None,
        &args(&["--model", "fable"]),
        false,
    )
    .unwrap_err();
    assert!(
        matches!(e, SettingsError::Model(ModelError::NeedsSharing(_))),
        "{e:?}"
    );
    let run = resolve_run_settings(
        &Config::default(),
        &|_| None,
        &args(&["--model", "fable", "--provider-sharing", "on"]),
        false,
    )
    .unwrap();
    assert_eq!(run.model.id, FABLE);
    assert!(run.provider_sharing);
}
