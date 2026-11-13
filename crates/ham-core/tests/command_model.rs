//! `ham config get|set|unset`, `ham models` and the direct start form's `--model` as an optional flag.

#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, ConfigCommand, DirectArgs, Invocation, SUBCOMMANDS, parse};
use ham_core::models::{ModelError, ModelPrefs, ModelSource};
use ham_core::request::{DirectInputs, direct_run_request};

const SONNET: &str = "global.anthropic.claude-sonnet-5-5";
const OPUS: &str = "us.anthropic.claude-opus-5-5";
const SOL: &str = "us.openai.gpt-6.1-sol";

fn direct(args: &[&str]) -> DirectArgs {
    match parse(args).unwrap().command {
        Command::StartDirect(d) => d,
        other => panic!("{other:?}"),
    }
}

fn usage(args: &[&str]) -> String {
    let e = parse(args).unwrap_err().to_string();
    assert!(e.starts_with("usage: "), "{e}");
    e
}

fn prefs(env: Option<&str>, config: Option<&str>) -> ModelPrefs {
    ModelPrefs {
        env: env.map(str::to_string),
        config: config.map(str::to_string),
    }
}

#[test]
fn a_direct_start_without_a_model_parses_and_keeps_the_flag_as_typed() {
    assert_eq!(direct(&["p", "--s3-only", "--direct"]).model, None);
    assert_eq!(
        direct(&["p", "--s3-only", "--direct", "--model", "opus"])
            .model
            .as_deref(),
        Some("opus"),
        "an alias is resolved by the caller, which also reads HAMH_MODEL and the config file"
    );
    assert_eq!(
        direct(&["p", "--s3-only", "--direct", "--model", OPUS])
            .model
            .as_deref(),
        Some(OPUS)
    );
    // Without --direct the start is refused with the reason.
    assert!(usage(&["p", "--s3-only", "--model", "opus"]).contains("--direct"));
}

#[test]
fn direct_args_round_trip_with_and_without_a_model() {
    for args in [
        vec!["p", "--s3-only", "--direct"],
        vec!["p", "--s3-only", "--direct", "--model", "glm"],
        vec![
            "p",
            "--repo",
            "cc:hamh-e2e-x",
            "--direct",
            "--model",
            SOL,
            "--no-rules",
        ],
    ] {
        let inv = parse(&args).unwrap();
        let argv = inv.to_argv();
        assert_eq!(parse(&argv).unwrap(), inv, "{argv:?}");
        assert_eq!(
            argv.contains(&"--model".to_string()),
            args.contains(&"--model")
        );
    }
}

#[test]
fn the_request_names_the_resolved_model_and_the_flag_wins() {
    let none = direct(&["p", "--s3-only", "--direct"]);
    let r = direct_run_request(&none, DirectInputs::default(), &ModelPrefs::default()).unwrap();
    assert_eq!(r.model, SONNET, "nothing set: Claude Sonnet 5.5");
    let r = direct_run_request(
        &none,
        DirectInputs::default(),
        &prefs(Some("sol"), Some("opus")),
    )
    .unwrap();
    assert_eq!(r.model, SOL, "HAMH_MODEL beats the config file");
    let r = direct_run_request(&none, DirectInputs::default(), &prefs(None, Some("opus"))).unwrap();
    assert_eq!(r.model, OPUS, "the config file beats the built-in default");
    let flag = direct(&["p", "--s3-only", "--direct", "--model", "astra"]);
    let r = direct_run_request(
        &flag,
        DirectInputs::default(),
        &prefs(Some("sol"), Some("opus")),
    )
    .unwrap();
    assert_eq!(
        r.model, "us.openai.gpt-6-astra",
        "the flag beats everything"
    );
    let r = direct_run_request(&flag, DirectInputs::default(), &ModelPrefs::default()).unwrap();
    assert_eq!(r.model, "us.openai.gpt-6-astra");
}

#[test]
fn a_wrong_value_stops_the_request_and_names_where_it_came_from() {
    let none = direct(&["p", "--s3-only", "--direct"]);
    let e =
        direct_run_request(&none, DirectInputs::default(), &prefs(Some("nope"), None)).unwrap_err();
    assert!(
        matches!(&e, ModelError::Unknown { from, .. } if *from == ModelSource::Env),
        "{e:?}"
    );
    let bad_flag = direct(&["p", "--s3-only", "--direct", "--model", "nope"]);
    let e = direct_run_request(
        &bad_flag,
        DirectInputs::default(),
        &prefs(None, Some("opus")),
    )
    .unwrap_err();
    assert!(
        matches!(&e, ModelError::Unknown { from, .. } if *from == ModelSource::Flag),
        "{e:?}"
    );
}

#[test]
fn config_subcommands_parse_and_round_trip() {
    assert!(SUBCOMMANDS.contains(&"config"));
    for (args, want) in [
        (vec!["config"], ConfigCommand::List),
        (vec!["config", "get"], ConfigCommand::List),
        (
            vec!["config", "get", "default_model"],
            ConfigCommand::Get {
                key: "default_model".into(),
            },
        ),
        (
            vec!["config", "set", "default_model", "opus"],
            ConfigCommand::Set {
                key: "default_model".into(),
                value: "opus".into(),
            },
        ),
        (
            vec!["config", "unset", "default_model"],
            ConfigCommand::Unset {
                key: "default_model".into(),
            },
        ),
    ] {
        let inv = parse(&args).unwrap();
        assert_eq!(inv.command, Command::Config(want), "{args:?}");
        assert_eq!(parse(&inv.to_argv()).unwrap(), inv, "{args:?}");
    }
    for json in [false, true] {
        let inv = Invocation {
            json,
            command: Command::Config(ConfigCommand::Get {
                key: "region".into(),
            }),
        };
        assert_eq!(parse(&inv.to_argv()).unwrap(), inv);
    }
    assert!(usage(&["config", "set", "default_model"]).contains("value"));
    assert!(usage(&["config", "set"]).contains("key"));
    assert!(usage(&["config", "unset"]).contains("key"));
    assert!(usage(&["config", "bogus"]).contains("bogus"));
    assert!(usage(&["config", "get", "a", "b"]).contains('b'));
    assert!(usage(&["config", "set", "default_model", "opus", "extra"]).contains("extra"));
    // The prompt "config" is still reachable through --prompt.
    assert!(matches!(
        parse(&["--prompt", "config", "--s3-only", "--direct"])
            .unwrap()
            .command,
        Command::StartDirect(_)
    ));
}

#[test]
fn models_is_the_local_list_and_the_server_list_has_a_flag() {
    assert_eq!(parse(&["models"]).unwrap().command, Command::Models);
    assert_eq!(
        parse(&["models", "--server"]).unwrap().command,
        Command::ModelsServer
    );
    for args in [vec!["models"], vec!["models", "--server", "--json"]] {
        let inv = parse(&args).unwrap();
        assert_eq!(parse(&inv.to_argv()).unwrap(), inv, "{args:?}");
    }
    assert!(usage(&["models", "--bogus"]).contains("--bogus"));
    // The multi-agent catalog view is gone.
    assert!(usage(&["models", "--catalog"]).contains("--catalog"));
    assert!(usage(&["models", "extra"]).contains("extra"));
}
