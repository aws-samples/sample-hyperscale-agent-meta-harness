//! Command parser.

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::command::{Command, DirectArgs, Invocation, PullArtifact, TargetArg, parse};
use harness_protocol::target::RepoTarget;
use proptest::prelude::*;

fn cmd(args: &[&str]) -> Command {
    parse(args)
        .unwrap_or_else(|e| panic!("{args:?}: {e}"))
        .command
}

/// The start form: `args` and `--direct`.
fn start(args: &[&str]) -> DirectArgs {
    let mut full = args.to_vec();
    full.push("--direct");
    match cmd(&full) {
        Command::StartDirect(s) => s,
        other => panic!("expected start, got {other:?}"),
    }
}

#[test]
fn prompt_with_codecommit_repo_starts_a_run() {
    let s = start(&[
        "Add a percent_change(a, b) function with tests",
        "--repo",
        "cc:hamh-e2e-python-calc-20260925",
    ]);
    assert_eq!(s.prompt, "Add a percent_change(a, b) function with tests");
    assert_eq!(
        s.target,
        TargetArg::Repo("cc:hamh-e2e-python-calc-20260925".parse().unwrap())
    );
    assert!(s.model.is_none() && s.rules.is_none() && !s.no_rules && s.continue_file.is_none());
}

#[test]
fn a_start_without_direct_is_refused() {
    for args in [
        vec!["p", "--s3-only"],
        vec!["p", "--s3-only", "--model", "sonnet"],
        vec!["p", "--s3-only", "--no-rules"],
        vec!["p", "--s3-only", "--continue", "nudge.md"],
    ] {
        let err = parse(&args).expect_err("must fail").to_string();
        assert!(
            err.starts_with("usage:") && err.contains("--direct"),
            "{args:?}: {err}"
        );
    }
}

#[test]
fn new_repo_and_s3_only_targets() {
    assert_eq!(
        start(&["p", "--new-repo", "cc:hamh-e2e-new-20260925"]).target,
        TargetArg::NewRepo(RepoTarget::CodeCommit {
            name: "hamh-e2e-new-20260925".into()
        })
    );
    assert_eq!(
        start(&["p", "--new-repo", "cc:hamh-e2e-ntier-web-20261001"]).target,
        TargetArg::NewRepo(RepoTarget::CodeCommit {
            name: "hamh-e2e-ntier-web-20261001".into()
        })
    );
    assert_eq!(start(&["p", "--s3-only"]).target, TargetArg::S3Only);
    assert_eq!(
        start(&["p", "--repo", "gh:ex-sample1/app"]).target,
        TargetArg::Repo(RepoTarget::GitHub {
            owner: "ex-sample1".into(),
            name: "app".into()
        })
    );
    assert_eq!(
        start(&[
            "p",
            "--repo",
            "gl:testuser/hyperscale-agent-meta-harness-output"
        ])
        .target,
        TargetArg::Repo(RepoTarget::GitLab {
            path: "testuser/hyperscale-agent-meta-harness-output".into()
        })
    );
}

#[test]
fn exactly_one_target_is_required() {
    for args in [
        vec!["p"],
        vec!["p", "--repo", "cc:a", "--s3-only"],
        vec!["p", "--repo", "cc:a", "--new-repo", "cc:b"],
    ] {
        let err = parse(&args).expect_err("must fail");
        assert!(
            err.to_string()
                .contains("exactly one of --repo, --new-repo, --s3-only"),
            "{err}"
        );
    }
}

#[test]
fn bad_target_reports_the_protocol_reason() {
    let err = parse(&["p", "--repo", "svn:x"]).expect_err("must fail");
    assert!(err.to_string().contains("unknown target scheme"), "{err}");
}

#[test]
fn run_options_parse() {
    let s = start(&[
        "p",
        "--s3-only",
        "--base",
        "main",
        "--model",
        "sonnet",
        "--rules",
        "rules/",
        "--continue",
        "nudge.md",
    ]);
    assert_eq!(s.base.as_deref(), Some("main"));
    assert_eq!(s.model.as_deref(), Some("sonnet"));
    assert_eq!(s.rules.as_deref(), Some("rules/"));
    assert_eq!(s.continue_file.as_deref(), Some("nudge.md"));
    assert!(!s.no_rules);
    assert!(start(&["p", "--s3-only", "--no-rules"]).no_rules);
}

#[test]
fn bad_option_values_are_usage_errors() {
    for args in [
        vec!["p", "--s3-only", "--direct", "--bogus"],
        vec!["p", "--s3-only", "--direct", "--base"],
        // The options of the multi-agent line are gone.
        vec!["p", "--s3-only", "--direct", "--models", "m"],
        vec!["p", "--s3-only", "--direct", "--budget", "tokens=5"],
        vec!["p", "--s3-only", "--direct", "--approvals", "relay"],
        vec!["p", "--s3-only", "--direct", "--residency", "us"],
        vec!["p", "--s3-only", "--direct", "--context", "m=max"],
    ] {
        assert!(parse(&args).is_err(), "{args:?}");
    }
}

#[test]
fn explicit_prompt_flag_allows_prompts_that_look_like_commands() {
    assert_eq!(start(&["--prompt", "runs", "--s3-only"]).prompt, "runs");
    assert_eq!(cmd(&["runs"]), Command::Runs { probes: false });
}

#[test]
fn runs_leaves_out_probe_runs_unless_probes_asks_for_them() {
    assert_eq!(format!("{:?}", cmd(&["runs"])), "Runs { probes: false }");
    assert_eq!(
        format!("{:?}", cmd(&["runs", "--probes"])),
        "Runs { probes: true }"
    );
    assert!(parse(&["runs", "--probes", "R1"]).is_err());
    assert!(parse(&["runs", "--all"]).is_err());
}

#[test]
fn subcommands_parse() {
    assert_eq!(cmd(&["watch"]), Command::Watch { run_id: None });
    assert_eq!(
        cmd(&["watch", "R1"]),
        Command::Watch {
            run_id: Some("R1".into())
        }
    );
    assert_eq!(
        cmd(&["cancel", "R1"]),
        Command::Cancel {
            run_id: "R1".into()
        }
    );
    assert_eq!(cmd(&["models"]), Command::Models);
    assert_eq!(
        cmd(&["pr", "R1"]),
        Command::Pr {
            run_id: "R1".into()
        }
    );
    assert_eq!(
        cmd(&["pull", "R1"]),
        Command::Pull {
            run_id: "R1".into(),
            artifact: PullArtifact::Zip
        }
    );
    assert_eq!(
        cmd(&["pull", "R1", "--artifact", "evidence"]),
        Command::Pull {
            run_id: "R1".into(),
            artifact: PullArtifact::Evidence
        }
    );
    assert_eq!(
        cmd(&["pull", "R1", "--artifact", "partial"]),
        Command::Pull {
            run_id: "R1".into(),
            artifact: PullArtifact::Partial
        }
    );
    assert_eq!(
        cmd(&["link", "R1"]),
        Command::Link {
            run_id: "R1".into()
        }
    );
    assert_eq!(
        cmd(&["doctor"]),
        Command::Doctor {
            catalog_max_age_hours: None
        }
    );
    assert_eq!(
        cmd(&["doctor", "--catalog-max-age", "24"]),
        Command::Doctor {
            catalog_max_age_hours: Some(24)
        }
    );
}

/// The commands of the multi-agent line are no commands: a first word that is not a subcommand is a prompt, and a prompt needs a
/// target.
#[test]
fn the_commands_of_the_multi_agent_line_are_gone() {
    for word in [
        "agents", "steer", "approve", "pause", "shell", "capacity", "budget", "roles",
    ] {
        let err = parse(&[word]).expect_err("must fail").to_string();
        // A removed word that is one edit from a live subcommand (`roles` and `rules`) gets the message that names the
        // closest commands; every other removed word is read as a prompt and gets the old message about the missing target.
        let near_a_live_command = ham_core::registry::subcommand_names()
            .into_iter()
            .any(|n| ham_core::registry::edit_distance(word, n) <= 1);
        let expected = if near_a_live_command {
            "no command"
        } else {
            "exactly one of --repo, --new-repo, --s3-only"
        };
        assert!(err.contains(expected), "{word}: {err}");
    }
    assert!(parse(&["models", "--catalog"]).is_err());
    for artifact in ["bundle:T3", "log:A2"] {
        assert!(parse(&["pull", "R1", "--artifact", artifact]).is_err());
    }
}

/// The usage line names the targets the service serves today
/// and the clone command; it does not offer a target the first Step Functions state refuses.
#[test]
fn the_usage_line_names_only_served_targets_and_clone() {
    let line = parse::<&str>(&[]).expect_err("no arguments").to_string();
    for want in [
        "--repo cc:<name>",
        "--new-repo cc:<name>",
        "--s3-only",
        "pull, clone, push, link",
    ] {
        assert!(line.contains(want), "{want:?} missing from {line}");
    }
    for refused in ["gh:", "gl:", "<target>", "agents", "steer", "shell"] {
        assert!(!line.contains(refused), "{refused:?} in {line}");
    }
    let no_prompt = parse(&["--s3-only"]).expect_err("no prompt").to_string();
    assert!(!no_prompt.contains("<target>"), "{no_prompt}");
}

#[test]
fn an_unknown_scheme_names_only_the_served_target() {
    let err = parse(&["p", "--repo", "svn:x"])
        .expect_err("must fail")
        .to_string();
    assert!(err.contains("use cc:<name>"), "{err}");
    assert!(!err.contains("gh:<owner>"), "{err}");
    assert!(!err.contains("gl:<group>"), "{err}");
}

#[test]
fn json_flag_is_global() {
    assert!(parse(&["--json", "runs"]).unwrap().json);
    assert!(parse(&["runs", "--json"]).unwrap().json);
    assert!(
        parse(&["p", "--s3-only", "--direct", "--json"])
            .unwrap()
            .json
    );
    assert!(!parse(&["runs"]).unwrap().json);
}

#[test]
fn missing_arguments_are_usage_errors() {
    for args in [
        vec![],
        vec!["cancel"],
        vec!["pull", "R1", "--artifact", "tarball"],
        vec!["watch", "R1", "R2"],
    ] {
        let err = parse(&args).expect_err("must fail");
        assert!(err.to_string().starts_with("usage:"), "{args:?}: {err}");
    }
}

#[test]
fn every_command_variant_round_trips_through_to_argv() {
    let all = vec![
        Command::Runs { probes: false },
        Command::Runs { probes: true },
        Command::Watch {
            run_id: Some("R1".into()),
        },
        Command::Cancel {
            run_id: "R1".into(),
        },
        Command::Models,
        Command::ModelsServer,
        Command::Pr {
            run_id: "R1".into(),
        },
        Command::Pull {
            run_id: "R1".into(),
            artifact: PullArtifact::Partial,
        },
        Command::Pull {
            run_id: "R1".into(),
            artifact: PullArtifact::Evidence,
        },
        Command::Link {
            run_id: "R1".into(),
        },
        Command::Doctor {
            catalog_max_age_hours: None,
        },
        Command::Doctor {
            catalog_max_age_hours: Some(24),
        },
        Command::StartDirect(DirectArgs {
            prompt: "p".into(),
            target: TargetArg::S3Only,
            base: Some("main".into()),
            model: Some("sonnet".into()),
            rules: Some("r".into()),
            no_rules: false,
            continue_file: Some("c.md".into()),
            input_wait: Some(10),
            skills: None,
            no_skills: false,
            continue_from: None,
            goal: None,
            research: None,
            allow_third_party: false,
            attach: vec![],
            compute: None,
            max_run_hours: None,
            engine: None,
            provider_sharing: None,
        }),
        Command::Say {
            run_id: "R1".into(),
            text: "go on".into(),
            request_id: Some("Q1".into()),
            no_wait: true,
        },
        Command::NewSession {
            run_id: "R1".into(),
            text: None,
            request_id: None,
            no_wait: false,
        },
        Command::TryModel {
            run_id: "R1".into(),
            model: "opus".into(),
            text: Some("again".into()),
            request_id: None,
            no_wait: false,
        },
        Command::Finish {
            run_id: "R1".into(),
            request_id: None,
            no_wait: false,
        },
    ];
    for command in all {
        for json in [false, true] {
            let inv = Invocation {
                json,
                command: command.clone(),
            };
            assert_eq!(parse(&inv.to_argv()).unwrap(), inv, "{:?}", inv.to_argv());
        }
    }
}

#[test]
fn more_usage_errors() {
    for (args, needle) in [
        (
            vec!["--prompt", "a", "--prompt", "b", "--s3-only", "--direct"],
            "twice",
        ),
        (vec!["a", "b", "--s3-only", "--direct"], "one prompt"),
        (vec!["--s3-only", "--direct"], "a prompt is required"),
        (vec!["runs", "--bogus"], "unknown option"),
        (vec!["p", "--new-repo", "svn:x"], "--new-repo"),
    ] {
        let err = parse(&args).expect_err("must fail").to_string();
        assert!(
            err.starts_with("usage:") && err.contains(needle),
            "{args:?}: {err}"
        );
    }
}

fn id() -> impl Strategy<Value = String> {
    "[A-Z0-9]{1,26}"
}
fn text() -> impl Strategy<Value = String> {
    "[ -~]{1,40}"
}
fn target() -> impl Strategy<Value = TargetArg> {
    prop_oneof![
        "[a-z][a-z0-9-]{0,30}".prop_map(|n| TargetArg::Repo(RepoTarget::CodeCommit { name: n })),
        "[a-z][a-z0-9-]{0,30}".prop_map(|n| TargetArg::NewRepo(RepoTarget::CodeCommit { name: n })),
        ("[a-z][a-z0-9]{0,10}", "[a-z][a-z0-9-]{0,20}")
            .prop_map(|(o, n)| TargetArg::Repo(RepoTarget::GitHub { owner: o, name: n })),
        Just(TargetArg::S3Only),
    ]
}
fn direct_args() -> impl Strategy<Value = DirectArgs> {
    (
        text(),
        target(),
        proptest::option::of("[a-z]{1,10}"),
        proptest::option::of("[a-z][a-z0-9.-]{0,30}"),
        proptest::option::of("[a-z]{1,10}"),
        proptest::option::of("[a-z]{1,10}"),
        proptest::option::of(0u32..=240),
    )
        .prop_map(
            |(prompt, target, base, model, rules, continue_file, input_wait)| DirectArgs {
                prompt,
                target,
                base,
                model,
                no_rules: rules.is_none() && continue_file.is_some(),
                rules,
                continue_file,
                input_wait,
                skills: None,
                no_skills: false,
                continue_from: None,
                goal: None,
                research: None,
                allow_third_party: false,
                attach: vec![],
                compute: None,
                max_run_hours: None,
                engine: None,
                provider_sharing: None,
            },
        )
}
fn command() -> impl Strategy<Value = Command> {
    prop_oneof![
        direct_args().prop_map(Command::StartDirect),
        any::<bool>().prop_map(|probes| Command::Runs { probes }),
        proptest::option::of(id()).prop_map(|run_id| Command::Watch { run_id }),
        id().prop_map(|run_id| Command::Cancel { run_id }),
        (
            id(),
            prop_oneof![Just(PullArtifact::Zip), Just(PullArtifact::Evidence)]
        )
            .prop_map(|(run_id, artifact)| Command::Pull { run_id, artifact }),
        proptest::option::of(1u64..=u64::MAX).prop_map(|catalog_max_age_hours| Command::Doctor {
            catalog_max_age_hours
        }),
    ]
}

proptest! {
    #[test]
    fn argv_round_trips(command in command(), json in any::<bool>()) {
        let inv = Invocation { json, command };
        let argv = inv.to_argv();
        let back = parse(&argv).map_err(|e| TestCaseError::fail(format!("{argv:?}: {e}")))?;
        prop_assert_eq!(back, inv);
    }
}

// ---- ham doctor --catalog-max-age <hours> ------------------------------------------------------------------------
//
// The age of the newest model smoke record is a warning only when the caller names a limit in whole hours. The registry's usage text for
// `doctor` stays empty on purpose: a command with usage completes in the popup instead of running, and `/doctor` must keep running at once.

#[test]
fn doctor_takes_a_catalog_age_limit_in_whole_hours_and_writes_it_back() {
    for (args, argv) in [
        (
            vec!["doctor", "--catalog-max-age", "24"],
            vec!["doctor", "--catalog-max-age", "24"],
        ),
        (
            vec!["doctor", "--catalog-max-age", "168"],
            vec!["doctor", "--catalog-max-age", "168"],
        ),
        // The largest value does not overflow when it is turned into seconds later.
        (
            vec!["doctor", "--catalog-max-age", "18446744073709551615"],
            vec!["doctor", "--catalog-max-age", "18446744073709551615"],
        ),
    ] {
        let inv = parse(&args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        assert!(!inv.json);
        assert_eq!(inv.to_argv(), argv, "{args:?}");
        assert_eq!(parse(&inv.to_argv()).unwrap(), inv, "round trip");
    }
    let inv = parse(&["--json", "doctor", "--catalog-max-age", "24"]).unwrap();
    assert!(inv.json);
    // `to_argv` writes the global flag last.
    assert_eq!(
        inv.to_argv(),
        ["doctor", "--catalog-max-age", "24", "--json"]
    );
    assert_eq!(parse(&inv.to_argv()).unwrap(), inv);
    // With no flag the argv is what it always was.
    assert_eq!(parse(&["doctor"]).unwrap().to_argv(), ["doctor"]);
}

#[test]
fn a_bad_catalog_age_limit_is_a_usage_error_that_names_the_flag() {
    for bad in [
        "0",
        "abc",
        "-3",
        "1.5",
        "",
        "24h",
        "+24",
        "18446744073709551616",
    ] {
        let e = parse(&["doctor", "--catalog-max-age", bad])
            .expect_err(bad)
            .to_string();
        assert!(
            e.contains("--catalog-max-age takes whole hours, 1 or more"),
            "{bad:?}: {e}"
        );
    }
    let e = parse(&["doctor", "--catalog-max-age"])
        .unwrap_err()
        .to_string();
    assert!(e.contains("--catalog-max-age needs a value"), "{e}");
    // The flag belongs to `doctor` alone, and doctor takes no other option or word.
    assert!(parse(&["models", "--catalog-max-age", "24"]).is_err());
    assert!(parse(&["doctor", "--catalog", "24"]).is_err());
    assert!(parse(&["doctor", "24"]).is_err());
}
