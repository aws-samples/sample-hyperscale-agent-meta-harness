//! The direct run command forms: `ham "<prompt>" --repo... --direct
//! --model <id>` starts a run that passes the prompt, one model and the user's rules to Cline, and `ham rules
//! init|edit|show` manages the rules files. `--direct` is the one start form; the options of the multi-agent line are unknown.

#![allow(clippy::unwrap_used)]

use ham_core::command::{
    Command, DirectArgs, Invocation, RulesCommand, SUBCOMMANDS, TargetArg, parse,
};
use ham_core::models::ModelPrefs;
use ham_core::request::{DirectInputs, direct_idempotency_key, direct_run_request};
use harness_protocol::api::v1::{ContinueSettings, RuleFile, RunTarget};

const MODEL: &str = "global.anthropic.claude-sonnet-5-5";

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

#[test]
fn direct_starts_with_one_model() {
    let d = direct(&[
        "Fix it",
        "--repo",
        "cc:hamh-e2e-python-calc",
        "--direct",
        "--model",
        MODEL,
    ]);
    assert_eq!(d.prompt, "Fix it");
    assert_eq!(d.model.as_deref(), Some(MODEL));
    assert!(matches!(d.target, TargetArg::Repo(_)));
    assert_eq!(
        (d.rules, d.no_rules, d.continue_file, d.base),
        (None, false, None, None)
    );
    // Without --direct the same words are refused: --model is an option of the direct start.
    let e = usage(&["Fix it", "--s3-only", "--model", MODEL]);
    assert!(e.contains("--model") && e.contains("--direct"), "{e}");
}

#[test]
fn direct_takes_rules_no_rules_continue_and_base_in_any_order() {
    let d = direct(&[
        "--model",
        MODEL,
        "--direct",
        "--continue",
        "c.md",
        "--rules",
        "rules",
        "--s3-only",
        "--base",
        "dev",
        "--prompt",
        "Do the thing",
    ]);
    assert_eq!(d.rules.as_deref(), Some("rules"));
    assert_eq!(d.continue_file.as_deref(), Some("c.md"));
    assert_eq!(d.base.as_deref(), Some("dev"));
    assert_eq!(d.prompt, "Do the thing");
    let d = direct(&["p", "--s3-only", "--direct", "--model", MODEL, "--no-rules"]);
    assert!(d.no_rules && d.rules.is_none());
}

#[test]
fn direct_needs_a_prompt_and_exactly_one_target_and_takes_the_model_from_the_default_when_not_given()
 {
    assert_eq!(direct(&["p", "--s3-only", "--direct"]).model, None);
    assert!(usage(&["--s3-only", "--direct", "--model", MODEL]).contains("prompt"));
    assert!(usage(&["p", "--direct", "--model", MODEL]).contains("--repo"));
    assert!(
        usage(&[
            "p",
            "--s3-only",
            "--repo",
            "cc:x",
            "--direct",
            "--model",
            MODEL
        ])
        .contains("exactly one")
    );
    assert!(
        usage(&[
            "p",
            "--s3-only",
            "--direct",
            "--model",
            MODEL,
            "--model",
            "other"
        ])
        .contains("--model")
    );
}

#[test]
fn direct_refuses_every_option_of_the_multi_agent_line() {
    for (flag, value) in [
        ("--models", Some("m")),
        ("--modes", Some("m.yaml")),
        ("--plan", Some("p.json")),
        ("--width", Some("m=2")),
        ("--budget", Some("tokens=5")),
        ("--approvals", Some("relay")),
        ("--residency", Some("us")),
        ("--context", Some("m=5")),
    ] {
        let mut a = vec!["p", "--s3-only", "--direct", "--model", MODEL, flag];
        a.extend(value);
        let e = usage(&a);
        assert!(
            e.contains(flag) && e.contains("unknown option"),
            "{flag}: {e}"
        );
    }
    // --no-rules and --rules together say two things.
    assert!(
        usage(&[
            "p",
            "--s3-only",
            "--direct",
            "--model",
            MODEL,
            "--no-rules",
            "--rules",
            "r"
        ])
        .contains("--no-rules")
    );
    // Without --direct the direct-only options are refused with the reason.
    for flag in [["--no-rules"].as_slice(), ["--continue", "c.md"].as_slice()] {
        let mut a = vec!["p", "--s3-only"];
        a.extend(flag);
        let e = usage(&a);
        assert!(e.contains("--direct"), "{flag:?}: {e}");
    }
}

#[test]
fn direct_round_trips_through_to_argv_and_json_is_a_flag() {
    for json in [false, true] {
        for args in [
            vec!["p", "--s3-only", "--direct", "--model", MODEL],
            vec![
                "p",
                "--repo",
                "cc:hamh-e2e-x",
                "--direct",
                "--model",
                MODEL,
                "--base",
                "b",
                "--rules",
                "r",
                "--continue",
                "c",
            ],
            vec![
                "p",
                "--new-repo",
                "cc:hamh-e2e-new-20261013",
                "--direct",
                "--model",
                MODEL,
                "--no-rules",
            ],
        ] {
            let command = parse(&args).unwrap().command;
            let inv = Invocation { json, command };
            assert_eq!(parse(&inv.to_argv()).unwrap(), inv, "{:?}", inv.to_argv());
        }
    }
    // A prompt that begins with a dash or equals a subcommand name travels through --prompt.
    let inv = parse(&[
        "--prompt",
        "rules",
        "--s3-only",
        "--direct",
        "--model",
        MODEL,
    ])
    .unwrap();
    assert_eq!(parse(&inv.to_argv()).unwrap(), inv);
    let inv = parse(&[
        "--prompt",
        "-x",
        "--s3-only",
        "--direct",
        "--model",
        MODEL,
        "--json",
    ])
    .unwrap();
    assert!(inv.json);
    assert_eq!(parse(&inv.to_argv()).unwrap().command, inv.command);
}

#[test]
fn rules_subcommands_parse_and_round_trip() {
    assert!(SUBCOMMANDS.contains(&"rules"));
    for (args, want) in [
        (vec!["rules", "init"], RulesCommand::Init),
        (vec!["rules", "edit"], RulesCommand::Edit { which: None }),
        (
            vec!["rules", "edit", "tdd.md"],
            RulesCommand::Edit {
                which: Some("tdd.md".into()),
            },
        ),
        (
            vec!["rules", "edit", "continue"],
            RulesCommand::Edit {
                which: Some("continue".into()),
            },
        ),
        (
            vec!["rules", "show"],
            RulesCommand::Show {
                rules: None,
                continue_file: None,
            },
        ),
        (
            vec!["rules", "show", "--rules", "d", "--continue", "c.md"],
            RulesCommand::Show {
                rules: Some("d".into()),
                continue_file: Some("c.md".into()),
            },
        ),
    ] {
        let inv = parse(&args).unwrap();
        assert_eq!(inv.command, Command::Rules(want), "{args:?}");
        assert_eq!(parse(&inv.to_argv()).unwrap(), inv);
    }
    assert!(usage(&["rules"]).contains("init"));
    assert!(usage(&["rules", "bogus"]).contains("bogus"));
    assert!(usage(&["rules", "init", "extra"]).contains("extra"));
    assert!(usage(&["rules", "show", "--model", "m"]).contains("--model"));
    // The prompt "rules" is still reachable through --prompt.
    assert!(matches!(
        parse(&["--prompt", "rules", "--s3-only", "--direct"])
            .unwrap()
            .command,
        Command::StartDirect(_)
    ));
}

#[test]
fn the_request_carries_the_prompt_unchanged_the_model_and_the_inputs() {
    let d = direct(&[
        "  Fix it.\n",
        "--repo",
        "cc:hamh-e2e-python-calc",
        "--direct",
        "--model",
        MODEL,
        "--base",
        "main",
    ]);
    let rules = vec![RuleFile {
        path: "a.md".into(),
        sha256: "a".repeat(64),
        content: "A\n".into(),
    }];
    let nudge = ContinueSettings {
        text: "Go.\n".into(),
        sha256: "b".repeat(64),
        max_nudges: 3,
    };
    let r = direct_run_request(
        &d,
        DirectInputs {
            rules: rules.clone(),
            nudge: Some(nudge.clone()),
            skills: vec![],
            attachments: vec![],
            review: None,
            engine_config: None,
        },
        &ModelPrefs::default(),
    )
    .unwrap();
    assert_eq!(r.prompt, "  Fix it.\n");
    assert_eq!(r.model, MODEL);
    assert_eq!(r.base_ref.as_deref(), Some("main"));
    assert_eq!(r.rules, rules);
    assert_eq!(r.nudge, Some(nudge));
    assert!(matches!(r.target, RunTarget::Repo { .. }));
}

#[test]
fn the_idempotency_key_repeats_for_a_retry_and_changes_with_the_rules_or_the_launch() {
    let d = direct(&["p", "--s3-only", "--direct", "--model", MODEL]);
    let rule = |c: &str| RuleFile {
        path: "a.md".into(),
        sha256: format!("{:0>64}", c.len()),
        content: c.into(),
    };
    let one = direct_run_request(
        &d,
        DirectInputs {
            rules: vec![rule("A\n")],
            nudge: None,
            skills: vec![],
            attachments: vec![],
            review: None,
            engine_config: None,
        },
        &ModelPrefs::default(),
    )
    .unwrap();
    let edited = direct_run_request(
        &d,
        DirectInputs {
            rules: vec![rule("AB\n")],
            nudge: None,
            skills: vec![],
            attachments: vec![],
            review: None,
            engine_config: None,
        },
        &ModelPrefs::default(),
    )
    .unwrap();
    let k = direct_idempotency_key(&one, "nonce-1");
    assert_eq!(
        k,
        direct_idempotency_key(&one, "nonce-1"),
        "a retry reuses the key"
    );
    assert_ne!(
        k,
        direct_idempotency_key(&one, "nonce-2"),
        "a new launch of the same prompt"
    );
    assert_ne!(
        k,
        direct_idempotency_key(&edited, "nonce-1"),
        "an edited rules file"
    );
    assert_eq!(k.len(), 64);
    assert!(k.bytes().all(|b| b.is_ascii_hexdigit()));
}
