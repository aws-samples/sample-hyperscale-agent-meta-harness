//! One function turns the flags, the environment and `config.toml` into the settings of a run (`request::resolve_run_settings`). The three
//! places that start a run (`ham "<prompt>" --direct`, the composer, the follow-up) call it, so a key of the file reaches all three.
//! Order, strongest first: the flag, the environment, the file, the default. A value that is set and wrong is an error that names its layer
//! and never falls through to a weaker one.

#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, DirectArgs, parse};
use ham_core::config::{Config, ResearchTable, RunTable};
use ham_core::models::{DEFAULT_MODEL_ID, ModelSource};
use ham_core::request::{
    DirectInputs, Origin, ResolvedRun, SettingsError, WaitSource, resolve_run_settings,
};
use ham_core::settings::Key;
use serde_json::{Value, json};

const SOL: &str = "us.openai.gpt-6.1-sol";
const OPUS: &str = "us.anthropic.claude-opus-5-5";
const ASTRA: &str = "us.openai.gpt-6-astra";
const PROMPT: &str = "Add a percent_change(a, b) function";

/// The requests v4.02 built for these inputs, captured from `main` (35a904e91) before the resolver existed, one rule file in each.
const GOLDEN_CLI: &str = r#"{"prompt":"Add a percent_change(a, b) function","target":{"mode":"s3-only"},"model":"global.anthropic.claude-sonnet-5-5","rules":[{"path":"00-my-rules.md","sha256":"1992241e1e7086648352a7c734cc204df6a94d845599ad770d245c214a95d3f6","content":"Write the test first.\n"}]}"#;
const GOLDEN_TUI: &str = r#"{"prompt":"Add a percent_change(a, b) function","target":{"mode":"s3-only"},"model":"global.anthropic.claude-sonnet-5-5","rules":[{"path":"00-my-rules.md","sha256":"1992241e1e7086648352a7c734cc204df6a94d845599ad770d245c214a95d3f6","content":"Write the test first.\n"}],"inputWaitS":1800}"#;
const GOLDEN_FLAGS: &str = r#"{"prompt":"Add a percent_change(a, b) function","target":{"mode":"repo","repo":"cc:my-service"},"baseRef":"main","model":"us.anthropic.claude-opus-5-5","rules":[{"path":"00-my-rules.md","sha256":"1992241e1e7086648352a7c734cc204df6a94d845599ad770d245c214a95d3f6","content":"Write the test first.\n"}],"inputWaitS":420,"research":{"enabled":true},"reportTests":false,"goal":{"enabled":false},"thirdParty":"allow"}"#;
const GOLDEN_LAYERS: &str = r#"{"prompt":"Add a percent_change(a, b) function","target":{"mode":"s3-only"},"model":"us.openai.gpt-6.1-sol","rules":[{"path":"00-my-rules.md","sha256":"1992241e1e7086648352a7c734cc204df6a94d845599ad770d245c214a95d3f6","content":"Write the test first.\n"}],"inputWaitS":720,"reportTests":false}"#;

fn args(extra: &[&str]) -> DirectArgs {
    let mut words = vec![PROMPT, "--s3-only", "--direct"];
    words.extend_from_slice(extra);
    let Command::StartDirect(a) = parse(&words).unwrap().command else {
        panic!("{words:?} is not a direct start");
    };
    a
}

fn env_of(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let owned: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    move |k: &str| owned.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
}

fn resolve(
    cfg: &Config,
    env: &[(&'static str, &'static str)],
    extra: &[&str],
    interactive: bool,
) -> Result<ResolvedRun, SettingsError> {
    resolve_run_settings(cfg, &env_of(env), &args(extra), interactive)
}

fn inputs() -> DirectInputs {
    DirectInputs {
        rules: vec![ham_core::rules::rule_file(
            "00-my-rules.md",
            "Write the test first.\n",
        )],
        ..DirectInputs::default()
    }
}

fn request_json(r: &ResolvedRun) -> Value {
    serde_json::to_value(r.request(inputs()).unwrap()).unwrap()
}

fn with_run(table: RunTable) -> Config {
    Config {
        run: Some(table),
        config_version: Some(1),
        ..Config::default()
    }
}

#[test]
fn precedence_flag_over_env_over_file_over_default_for_every_key_with_a_flag_or_env() {
    // The model: --model, HAMH_MODEL, default_model, Claude Sonnet 5.5.
    let cfg = Config {
        default_model: Some(SOL.into()),
        ..Config::default()
    };
    let r = resolve(
        &cfg,
        &[("HAMH_MODEL", "opus")],
        &["--model", "astra"],
        false,
    )
    .unwrap();
    assert_eq!(
        (r.model.id.as_str(), r.origin(Key::DefaultModel)),
        (ASTRA, Some(Origin::Flag))
    );
    assert_eq!(r.model.source, ModelSource::Flag);
    let r = resolve(&cfg, &[("HAMH_MODEL", "opus")], &[], false).unwrap();
    assert_eq!(
        (r.model.id.as_str(), r.origin(Key::DefaultModel)),
        (OPUS, Some(Origin::Env("HAMH_MODEL")))
    );
    let r = resolve(&cfg, &[], &[], false).unwrap();
    assert_eq!(
        (r.model.id.as_str(), r.origin(Key::DefaultModel)),
        (SOL, Some(Origin::File))
    );
    let r = resolve(&Config::default(), &[], &[], false).unwrap();
    assert_eq!(
        (r.model.id.as_str(), r.origin(Key::DefaultModel)),
        (DEFAULT_MODEL_ID, Some(Origin::Default))
    );

    // The wait: --input-wait, HAMH_INPUT_WAIT, input_wait_minutes, then 30 (a terminal) or 0.
    let cfg = Config {
        input_wait_minutes: Some(5),
        ..Config::default()
    };
    let env = [("HAMH_INPUT_WAIT", "9")];
    let r = resolve(&cfg, &env, &["--input-wait", "3"], false).unwrap();
    assert_eq!(
        (r.args.input_wait, r.origin(Key::InputWaitMinutes)),
        (Some(3), Some(Origin::Flag))
    );
    let r = resolve(&cfg, &env, &[], false).unwrap();
    assert_eq!(
        (r.args.input_wait, r.origin(Key::InputWaitMinutes)),
        (Some(9), Some(Origin::Env("HAMH_INPUT_WAIT")))
    );
    let r = resolve(&cfg, &[], &[], false).unwrap();
    assert_eq!(
        (r.args.input_wait, r.origin(Key::InputWaitMinutes)),
        (Some(5), Some(Origin::File))
    );
    let r = resolve(&Config::default(), &[], &[], true).unwrap();
    assert_eq!(
        (r.args.input_wait, r.origin(Key::InputWaitMinutes)),
        (Some(30), Some(Origin::Default))
    );
    let r = resolve(&Config::default(), &[], &[], false).unwrap();
    assert_eq!(
        (r.args.input_wait, r.origin(Key::InputWaitMinutes)),
        (Some(0), Some(Origin::Default))
    );

    // The goal loop: --goal on|off, run.goal, then the stage's default (no member).
    let off = with_run(RunTable {
        goal: Some(false),
        ..RunTable::default()
    });
    let on = with_run(RunTable {
        goal: Some(true),
        ..RunTable::default()
    });
    let r = resolve(&off, &[], &["--goal", "on"], false).unwrap();
    assert_eq!(
        (r.args.goal, r.origin(Key::RunGoal)),
        (Some(true), Some(Origin::Flag))
    );
    let r = resolve(&on, &[], &["--goal", "off"], false).unwrap();
    assert_eq!(
        (r.args.goal, r.origin(Key::RunGoal)),
        (Some(false), Some(Origin::Flag))
    );
    let r = resolve(&off, &[], &[], false).unwrap();
    assert_eq!(
        (r.args.goal, r.origin(Key::RunGoal)),
        (Some(false), Some(Origin::File))
    );
    let r = resolve(&Config::default(), &[], &[], false).unwrap();
    assert_eq!(
        (r.args.goal, r.origin(Key::RunGoal)),
        (None, Some(Origin::Default))
    );

    // Web research: --research, --no-research, research.enabled, then the stage's default.
    let off = Config {
        research: Some(ResearchTable {
            enabled: Some(false),
            ..ResearchTable::default()
        }),
        config_version: Some(1),
        ..Config::default()
    };
    let on = Config {
        research: Some(ResearchTable {
            enabled: Some(true),
            ..ResearchTable::default()
        }),
        config_version: Some(1),
        ..Config::default()
    };
    let r = resolve(&off, &[], &["--research"], false).unwrap();
    assert_eq!(
        (r.args.research, r.origin(Key::ResearchEnabled)),
        (Some(true), Some(Origin::Flag))
    );
    let r = resolve(&on, &[], &["--no-research"], false).unwrap();
    assert_eq!(
        (r.args.research, r.origin(Key::ResearchEnabled)),
        (Some(false), Some(Origin::Flag))
    );
    let r = resolve(&on, &[], &[], false).unwrap();
    assert_eq!(
        (r.args.research, r.origin(Key::ResearchEnabled)),
        (Some(true), Some(Origin::File))
    );
    let r = resolve(&Config::default(), &[], &[], false).unwrap();
    assert_eq!(
        (r.args.research, r.origin(Key::ResearchEnabled)),
        (None, Some(Origin::Default))
    );

    // Rules and skills: --no-rules / --rules <dir> and --no-skills / --skills <dir> over run.rules and run.skills.
    let no_rules = with_run(RunTable {
        rules: Some(false),
        ..RunTable::default()
    });
    let r = resolve(&no_rules, &[], &[], false).unwrap();
    assert_eq!(
        (r.args.no_rules, r.origin(Key::RunRules)),
        (true, Some(Origin::File))
    );
    let r = resolve(&no_rules, &[], &["--rules", "./my-rules"], false).unwrap();
    assert_eq!(
        (
            r.args.no_rules,
            r.args.rules.as_deref(),
            r.origin(Key::RunRules)
        ),
        (false, Some("./my-rules"), Some(Origin::Flag))
    );
    let rules_on = with_run(RunTable {
        rules: Some(true),
        ..RunTable::default()
    });
    let r = resolve(&rules_on, &[], &["--no-rules"], false).unwrap();
    assert_eq!(
        (r.args.no_rules, r.origin(Key::RunRules)),
        (true, Some(Origin::Flag))
    );
    let r = resolve(&Config::default(), &[], &[], false).unwrap();
    assert_eq!(
        (r.args.no_rules, r.origin(Key::RunRules)),
        (false, Some(Origin::Default))
    );
    let no_skills = with_run(RunTable {
        skills: Some(false),
        ..RunTable::default()
    });
    let r = resolve(&no_skills, &[], &[], false).unwrap();
    assert_eq!(
        (r.args.no_skills, r.args.no_rules, r.origin(Key::RunSkills)),
        (true, false, Some(Origin::File))
    );
    let r = resolve(&no_skills, &[], &["--skills", "./my-skills"], false).unwrap();
    assert_eq!(
        (
            r.args.no_skills,
            r.args.skills.as_deref(),
            r.origin(Key::RunSkills)
        ),
        (false, Some("./my-skills"), Some(Origin::Flag))
    );
    let r = resolve(&Config::default(), &[], &["--no-skills"], false).unwrap();
    assert_eq!(
        (r.args.no_skills, r.origin(Key::RunSkills)),
        (true, Some(Origin::Flag))
    );

    // The clean-checkout report has the file only.
    let cfg = Config {
        report_tests: Some(false),
        ..Config::default()
    };
    let r = resolve(&cfg, &[], &[], false).unwrap();
    assert_eq!(
        (r.report_tests, r.origin(Key::ReportTests)),
        (Some(false), Some(Origin::File))
    );
    let r = resolve(&Config::default(), &[], &[], false).unwrap();
    assert_eq!(
        (r.report_tests, r.origin(Key::ReportTests)),
        (None, Some(Origin::Default))
    );

    // The labels the Settings screen and `ham config show` print.
    assert_eq!(Origin::Flag.label(), "flag");
    assert_eq!(Origin::Env("HAMH_MODEL").label(), "env HAMH_MODEL");
    assert_eq!(Origin::File.label(), "file");
    assert_eq!(Origin::Default.label(), "default");
    // Download folder: the environment wins over the file (an exception that stays, with the Region and the profile reading the file first).
    let cfg = Config {
        download_dir: Some("/from/file".into()),
        ..Config::default()
    };
    assert_eq!(
        ham_core::config::download_dir(&env_of(&[("HAMH_DOWNLOAD_DIR", "/from/env")]), &cfg),
        Some("/from/env".into())
    );
}

#[test]
fn a_set_but_wrong_value_names_its_layer_and_never_falls_through() {
    // A bad HAMH_INPUT_WAIT is an error although the file holds a good wait.
    let cfg = Config {
        input_wait_minutes: Some(5),
        ..Config::default()
    };
    let e = resolve(&cfg, &[("HAMH_INPUT_WAIT", "abc")], &[], false).unwrap_err();
    match &e {
        SettingsError::Wait(w) => assert_eq!(w.layer, WaitSource::Env),
        other => panic!("{other:?}"),
    }
    assert!(
        e.to_string().contains("HAMH_INPUT_WAIT") && e.to_string().contains("abc"),
        "{e}"
    );
    // A bad HAMH_MODEL is an error although the file holds a good model.
    let cfg = Config {
        default_model: Some(SOL.into()),
        ..Config::default()
    };
    let e = resolve(&cfg, &[("HAMH_MODEL", "gpt-9")], &[], false).unwrap_err();
    assert!(matches!(e, SettingsError::Model(_)), "{e:?}");
    assert!(
        e.to_string().contains("HAMH_MODEL") && e.to_string().contains("gpt-9"),
        "{e}"
    );
    // A wait over the longest run in the file (a Config built by hand; the reader refuses the text) names the file's layer.
    let cfg = Config {
        input_wait_minutes: Some(43_201),
        ..Config::default()
    };
    let e = resolve(&cfg, &[], &[], false).unwrap_err();
    match &e {
        SettingsError::Wait(w) => assert_eq!(w.layer, WaitSource::Config),
        other => panic!("{other:?}"),
    }
    // A blank variable counts as unset.
    let cfg = Config {
        input_wait_minutes: Some(5),
        default_model: Some(SOL.into()),
        ..Config::default()
    };
    let r = resolve(
        &cfg,
        &[("HAMH_INPUT_WAIT", "  "), ("HAMH_MODEL", "")],
        &[],
        false,
    )
    .unwrap();
    assert_eq!((r.args.input_wait, r.model.id.as_str()), (Some(5), SOL));
}

#[test]
fn run_goal_false_in_the_file_sends_goal_enabled_false_and_unset_sends_no_member() {
    let off = with_run(RunTable {
        goal: Some(false),
        ..RunTable::default()
    });
    let json_off = request_json(&resolve(&off, &[], &[], false).unwrap());
    assert_eq!(json_off["goal"], json!({"enabled": false}));
    let on = with_run(RunTable {
        goal: Some(true),
        ..RunTable::default()
    });
    let json_on = request_json(&resolve(&on, &[], &[], false).unwrap());
    assert_eq!(json_on["goal"], json!({"enabled": true}));
    let unset = request_json(&resolve(&Config::default(), &[], &[], false).unwrap());
    assert!(unset.get("goal").is_none(), "{unset}");
    // A flag still wins over the file.
    let flag = request_json(&resolve(&off, &[], &["--goal", "on"], false).unwrap());
    assert_eq!(flag["goal"], json!({"enabled": true}));
    // The other members of the three requests are equal.
    for v in [&json_off, &json_on, &flag] {
        for member in [
            "research",
            "reportTests",
            "thirdParty",
            "skills",
            "attachments",
        ] {
            assert!(v.get(member).is_none(), "{member} in {v}");
        }
    }
}

#[test]
fn research_enabled_in_the_file_sends_research_enabled() {
    let on = Config {
        research: Some(ResearchTable {
            enabled: Some(true),
            ..ResearchTable::default()
        }),
        config_version: Some(1),
        ..Config::default()
    };
    let v = request_json(&resolve(&on, &[], &[], false).unwrap());
    assert_eq!(v["research"], json!({"enabled": true}));
    let off = Config {
        research: Some(ResearchTable {
            enabled: Some(false),
            ..ResearchTable::default()
        }),
        config_version: Some(1),
        ..Config::default()
    };
    let v = request_json(&resolve(&off, &[], &[], false).unwrap());
    assert_eq!(v["research"], json!({"enabled": false}));
    // A --research flag beats a file that says false.
    let v = request_json(&resolve(&off, &[], &["--research"], false).unwrap());
    assert_eq!(v["research"], json!({"enabled": true}));
    let v = request_json(&resolve(&Config::default(), &[], &[], false).unwrap());
    assert!(v.get("research").is_none(), "{v}");
}

#[test]
fn run_rules_false_is_no_rules_and_sends_no_skills_and_run_skills_false_sends_no_skills() {
    // run.rules = false is --no-rules, which also sends no skills (rules_cmd::direct_inputs reads both switches).
    let r = resolve(
        &with_run(RunTable {
            rules: Some(false),
            ..RunTable::default()
        }),
        &[],
        &[],
        false,
    )
    .unwrap();
    assert!(r.args.no_rules && !r.args.no_skills);
    // run.skills = false is --no-skills and leaves the rules alone.
    let r = resolve(
        &with_run(RunTable {
            skills: Some(false),
            ..RunTable::default()
        }),
        &[],
        &[],
        false,
    )
    .unwrap();
    assert!(!r.args.no_rules && r.args.no_skills);
    // True is the default and changes nothing.
    let r = resolve(
        &with_run(RunTable {
            rules: Some(true),
            skills: Some(true),
            goal: None,
            ..RunTable::default()
        }),
        &[],
        &[],
        false,
    )
    .unwrap();
    assert!(!r.args.no_rules && !r.args.no_skills);
    // A --rules <dir> flag beats run.rules = false.
    let r = resolve(
        &with_run(RunTable {
            rules: Some(false),
            ..RunTable::default()
        }),
        &[],
        &["--rules", "./my-rules"],
        false,
    )
    .unwrap();
    assert!(!r.args.no_rules);
}

#[test]
fn a_default_user_request_is_byte_identical_to_the_v4_02_golden() {
    // The command line of a script: no terminal, no config.
    let r = resolve(&Config::default(), &[], &[], false).unwrap();
    assert_eq!(
        serde_json::to_string(&r.request(inputs()).unwrap()).unwrap(),
        GOLDEN_CLI
    );
    // The composer and the follow-up are interactive: the same request with the 30 minutes of the dashboard.
    let r = resolve(&Config::default(), &[], &[], true).unwrap();
    assert_eq!(
        serde_json::to_string(&r.request(inputs()).unwrap()).unwrap(),
        GOLDEN_TUI
    );
    // Flags only.
    let words = [
        "--repo",
        "cc:my-service",
        "--base",
        "main",
        "--goal",
        "off",
        "--research",
        "--allow-third-party",
        "--input-wait",
        "7",
        "--model",
        "opus",
    ];
    let a = {
        let mut w = vec![PROMPT, "--direct"];
        w.extend_from_slice(&words);
        let Command::StartDirect(a) = parse(&w).unwrap().command else {
            panic!()
        };
        a
    };
    let cfg = Config {
        report_tests: Some(false),
        ..Config::default()
    };
    let r = resolve_run_settings(&cfg, &env_of(&[]), &a, false).unwrap();
    assert_eq!(
        serde_json::to_string(&r.request(inputs()).unwrap()).unwrap(),
        GOLDEN_FLAGS
    );
    // Layers: the wait from the variable over the file, the model from the file, the report switched off in the file.
    let cfg = Config {
        default_model: Some(SOL.into()),
        input_wait_minutes: Some(5),
        report_tests: Some(false),
        ..Config::default()
    };
    let r = resolve(&cfg, &[("HAMH_INPUT_WAIT", "12")], &[], true).unwrap();
    assert_eq!(
        serde_json::to_string(&r.request(inputs()).unwrap()).unwrap(),
        GOLDEN_LAYERS
    );
}

#[test]
fn report_tests_still_sends_only_false() {
    for (setting, want) in [
        (None, None),
        (Some(true), None),
        (Some(false), Some(json!(false))),
    ] {
        let cfg = Config {
            report_tests: setting,
            ..Config::default()
        };
        let v = request_json(&resolve(&cfg, &[], &[], false).unwrap());
        assert_eq!(v.get("reportTests").cloned(), want, "{setting:?}");
    }
}
