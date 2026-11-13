//! The command forms of the lifecycle pack and the follow-up run: `ham rules init --pack lifecycle`, `ham lifecycle
//! init <dir>`, the `--skills` and `--no-skills` options of a direct run, and `ham continue <runId>`. Every form parses, renders back to the
//! same argv and has a registry entry with the effects a person is told about before it runs.

#![allow(clippy::unwrap_used)]

use ham_core::command::{
    Command, DirectArgs, LifecycleCommand, Pack, RulesCommand, SUBCOMMANDS, TargetArg, parse,
};
use ham_core::registry::{
    self, Availability, EffectClass, Effects, RunArg, TextArg, class_of, effects_of, find,
};
use ham_core::request::{DirectInputs, direct_run_request};

const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const MODEL: &str = "global.anthropic.claude-sonnet-5-5";

fn usage(args: &[&str]) -> String {
    let e = parse(args).unwrap_err().to_string();
    assert!(e.starts_with("usage: "), "{e}");
    e
}

fn round_trip(args: &[&str]) -> Command {
    let inv = parse(args).unwrap();
    assert_eq!(
        parse(&inv.to_argv()).unwrap().command,
        inv.command,
        "{args:?}"
    );
    inv.command
}

fn continue_args(args: &[&str]) -> DirectArgs {
    match round_trip(args) {
        Command::Continue(d, _) => d,
        other => panic!("{other:?}"),
    }
}

// ---- rules init --pack -------------------------------------------------------------------------------------------------------

#[test]
fn rules_init_without_a_pack_is_the_default_install() {
    // The default of `ham rules init` is the lifecycle pack's contract beside the default rules; the command
    // line is the old one.
    assert_eq!(
        round_trip(&["rules", "init"]),
        Command::Rules(RulesCommand::Init)
    );
}

#[test]
fn rules_init_with_pack_none_keeps_the_pointer_only_defaults() {
    assert_eq!(
        round_trip(&["rules", "init", "--pack", "none"]),
        Command::Rules(RulesCommand::InitPack {
            pack: Pack::None,
            skills_as_rules: false,
            skills: false,
        })
    );
    assert_eq!(Pack::None.name(), "none");
    assert_eq!(Pack::Lifecycle.name(), "lifecycle");
}

#[test]
fn rules_init_installs_the_lifecycle_pack() {
    for (args, skills_as_rules, skills) in [
        (vec!["rules", "init", "--pack", "lifecycle"], false, false),
        (
            vec!["rules", "init", "--pack", "lifecycle", "--skills-as-rules"],
            true,
            false,
        ),
        (
            vec!["rules", "init", "--pack", "lifecycle", "--skills"],
            false,
            true,
        ),
        (
            vec![
                "rules",
                "init",
                "--skills",
                "--pack",
                "lifecycle",
                "--skills-as-rules",
            ],
            true,
            true,
        ),
        // The pack is the default, so its options need no `--pack`.
        (vec!["rules", "init", "--skills"], false, true),
        (vec!["rules", "init", "--skills-as-rules"], true, false),
    ] {
        assert_eq!(
            round_trip(&args),
            Command::Rules(RulesCommand::InitPack {
                pack: Pack::Lifecycle,
                skills_as_rules,
                skills,
            }),
            "{args:?}"
        );
    }
}

#[test]
fn rules_init_refuses_an_unknown_pack_and_pack_options_with_none() {
    let e = usage(&["rules", "init", "--pack", "tdd"]);
    assert!(
        e.contains("lifecycle") && e.contains("none") && e.contains("tdd"),
        "{e}"
    );
    for flag in ["--skills", "--skills-as-rules"] {
        let e = usage(&["rules", "init", "--pack", "none", flag]);
        assert!(e.contains(flag) && e.contains("none"), "{flag}: {e}");
    }
    usage(&["rules", "init", "--pack"]);
}

#[test]
fn rules_init_no_skills_is_the_contract_alone_and_round_trips() {
    // It is a form of its own (not `--pack lifecycle`, which is the pack's default install).
    let alone = round_trip(&["rules", "init", "--no-skills"]);
    assert_eq!(
        parse(&["rules", "init", "--no-skills"]).unwrap().to_argv(),
        ["rules", "init", "--no-skills"]
    );
    assert_eq!(
        round_trip(&["rules", "init", "--pack", "lifecycle", "--no-skills"]),
        alone,
        "naming the pack changes nothing"
    );
    assert_ne!(alone, round_trip(&["rules", "init"]));
    assert_ne!(
        alone,
        round_trip(&["rules", "init", "--pack", "lifecycle"]),
        "`--pack lifecycle` is the pack's default install, skills included"
    );
    assert_ne!(alone, round_trip(&["rules", "init", "--skills"]));
    // The skill bodies as rules install no skills folder, so `--no-skills` beside them says what they already mean.
    assert_eq!(
        round_trip(&["rules", "init", "--skills-as-rules", "--no-skills"]),
        round_trip(&["rules", "init", "--skills-as-rules"])
    );
    // `--pack none` installs no skills either; the flag beside it is accepted and changes nothing.
    assert_eq!(
        round_trip(&["rules", "init", "--pack", "none", "--no-skills"]),
        round_trip(&["rules", "init", "--pack", "none"])
    );
}

#[test]
fn rules_init_refuses_skills_and_no_skills_together_and_says_so() {
    for args in [
        vec!["rules", "init", "--skills", "--no-skills"],
        vec!["rules", "init", "--no-skills", "--skills"],
        vec![
            "rules",
            "init",
            "--pack",
            "lifecycle",
            "--skills",
            "--no-skills",
        ],
    ] {
        let e = usage(&args);
        assert!(
            e.contains("--skills") && e.contains("--no-skills") && e.contains("opposite"),
            "{args:?}: {e}"
        );
    }
}

#[test]
fn the_usage_line_of_rules_init_names_no_skills() {
    let e = usage(&["rules"]);
    assert!(e.contains("--no-skills"), "{e}");
    let e = usage(&["rules", "bogus"]);
    assert!(e.contains("--no-skills"), "{e}");
}

// ---- lifecycle init ----------------------------------------------------------------------------------------------------------

#[test]
fn lifecycle_init_takes_a_directory_and_optionally_kiro() {
    assert_eq!(
        round_trip(&["lifecycle", "init", "my-repo"]),
        Command::Lifecycle(LifecycleCommand::Init {
            dir: "my-repo".into(),
            kiro: false
        })
    );
    assert_eq!(
        round_trip(&["lifecycle", "init", "--kiro", "."]),
        Command::Lifecycle(LifecycleCommand::Init {
            dir: ".".into(),
            kiro: true
        })
    );
}

#[test]
fn lifecycle_init_needs_a_directory() {
    let e = usage(&["lifecycle", "init"]);
    assert!(e.contains("dir"), "{e}");
    let e = usage(&["lifecycle"]);
    assert!(e.contains("lifecycle init"), "{e}");
    let e = usage(&["lifecycle", "update", "x"]);
    assert!(e.contains("update") && e.contains("lifecycle init"), "{e}");
    usage(&["lifecycle", "init", "a", "b"]);
}

// ---- skills options of a direct run ------------------------------------------------------------------------------------------

fn start(extra: &[&str]) -> DirectArgs {
    let mut args = vec!["Fix it", "--s3-only", "--direct"];
    args.extend_from_slice(extra);
    match round_trip(&args) {
        Command::StartDirect(d) => d,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_direct_run_takes_a_skills_directory_or_none() {
    let d = start(&[]);
    assert_eq!(
        (d.skills, d.no_skills, d.continue_from),
        (None, false, None)
    );
    let d = start(&["--skills", "my-skills"]);
    assert_eq!(d.skills.as_deref(), Some("my-skills"));
    assert!(!d.no_skills);
    assert!(start(&["--no-skills"]).no_skills);
}

#[test]
fn skills_options_are_direct_options_and_do_not_contradict() {
    let e = usage(&["Fix it", "--s3-only", "--skills", "x"]);
    assert!(e.contains("--skills") && e.contains("--direct"), "{e}");
    let e = usage(&["Fix it", "--s3-only", "--no-skills"]);
    assert!(e.contains("--no-skills") && e.contains("--direct"), "{e}");
    let e = usage(&[
        "Fix it",
        "--s3-only",
        "--direct",
        "--skills",
        "x",
        "--no-skills",
    ]);
    assert!(e.contains("--skills") && e.contains("--no-skills"), "{e}");
    usage(&["Fix it", "--s3-only", "--direct", "--skills"]);
}

// ---- continue ----------------------------------------------------------------------------------------------------------------

#[test]
fn continue_names_the_run_and_starts_an_s3_only_direct_run() {
    let d = continue_args(&["continue", RUN, "add a second file"]);
    assert_eq!(d.continue_from.as_deref(), Some(RUN));
    assert_eq!(d.prompt, "add a second file");
    assert_eq!(d.target, TargetArg::S3Only);
    assert_eq!((d.base, d.rules, d.no_rules), (None, None, false));
}

#[test]
fn the_prompt_is_a_word_or_the_prompt_option_or_left_for_the_editor() {
    assert_eq!(
        continue_args(&["continue", RUN, "--prompt", "-x first"]).prompt,
        "-x first"
    );
    assert_eq!(
        continue_args(&["continue", RUN]).prompt,
        "",
        "empty: the client opens the editor"
    );
    let e = usage(&["continue", RUN, "a", "--prompt", "b"]);
    assert!(e.contains("prompt"), "{e}");
    let e = usage(&["continue", RUN, "one", "two"]);
    assert!(e.contains("quote"), "{e}");
}

#[test]
fn continue_takes_the_options_of_a_direct_run() {
    let d = continue_args(&[
        "continue",
        RUN,
        "go",
        "--model",
        MODEL,
        "--input-wait",
        "5",
        "--no-rules",
        "--no-skills",
        "--continue",
        "nudge.md",
    ]);
    assert_eq!(d.model.as_deref(), Some(MODEL));
    assert_eq!(d.input_wait, Some(5));
    assert!(d.no_rules && d.no_skills);
    assert_eq!(d.continue_file.as_deref(), Some("nudge.md"));
    let d = continue_args(&["continue", RUN, "go", "--rules", "r", "--skills", "s"]);
    assert_eq!(
        (d.rules.as_deref(), d.skills.as_deref()),
        (Some("r"), Some("s"))
    );
}

#[test]
fn continue_needs_a_run_and_takes_no_new_repository_or_base() {
    let e = usage(&["continue"]);
    assert!(e.contains("runId"), "{e}");
    // `--s3-only` and `--repo` are the explicit targets of a follow-up (command_continue_target.rs); the rest stay refused.
    for flag in [
        vec!["--new-repo", "cc:hamh-e2e-new-1"],
        vec!["--base", "main"],
        vec!["--direct"],
    ] {
        let mut args = vec!["continue", RUN, "go"];
        args.extend(flag.iter().copied());
        let e = usage(&args);
        assert!(e.contains("continue") && e.contains(flag[0]), "{e}");
    }
    let e = usage(&["continue", "../etc/passwd"]);
    assert!(e.contains("run id") || e.contains("runId"), "{e}");
}

#[test]
fn the_request_of_a_follow_up_names_the_source_and_leaves_the_head_to_control_api() {
    let d = continue_args(&["continue", RUN, "go"]);
    let req = direct_run_request(&d, DirectInputs::default(), &Default::default()).unwrap();
    let c = req.continue_from.expect("continueFrom");
    assert_eq!((c.run_id.as_str(), c.head_sha), (RUN, None));
    assert_eq!(req.target, harness_protocol::api::v1::RunTarget::S3Only);
    assert!(req.skills.is_empty());
    // A new run names no source.
    let d = start(&[]);
    assert!(
        direct_run_request(&d, DirectInputs::default(), &Default::default())
            .unwrap()
            .continue_from
            .is_none()
    );
}

#[test]
fn the_request_carries_the_skills_the_client_read() {
    let skill = harness_protocol::direct::Skill {
        name: "demo".into(),
        files: vec![harness_protocol::direct::SkillFile::new(
            "SKILL.md",
            "---\nname: demo\ndescription: d\n---\nx\n",
        )],
    };
    let inputs = DirectInputs {
        skills: vec![skill.clone()],
        ..DirectInputs::default()
    };
    let req = direct_run_request(&start(&[]), inputs, &Default::default()).unwrap();
    assert_eq!(req.skills, vec![skill]);
}

// ---- the registry ------------------------------------------------------------------------------------------------------------

#[test]
fn both_words_are_registered_subcommands() {
    assert!(SUBCOMMANDS.contains(&"continue") && SUBCOMMANDS.contains(&"lifecycle"));
    for word in ["continue", "lifecycle"] {
        let e = find(word).unwrap_or_else(|| panic!("no entry for {word}"));
        assert_eq!(e.status, Availability::Built, "{word}");
        assert!(registry::subcommand(word).is_some());
    }
    let c = find("continue").unwrap();
    assert_eq!(
        (c.run_arg, c.min_args, c.text),
        (RunArg::Required, 1, TextArg::Optional)
    );
    assert!(
        c.effects.contains(Effects::BILLABLE),
        "a follow-up starts a billable run"
    );
    let l = find("lifecycle").unwrap();
    assert_eq!(l.effects, Effects::LOCAL_WRITE);
    assert_eq!(l.class(), EffectClass::LocalWrite);
    assert!(l.help.len() <= 80 && c.help.len() <= 80);
}

#[test]
fn a_parsed_command_declares_the_effects_of_its_form() {
    let fx = |args: &[&str]| effects_of(&parse(args).unwrap().command);
    assert_eq!(
        fx(&["rules", "init", "--pack", "lifecycle"]),
        Effects::LOCAL_WRITE
    );
    assert_eq!(fx(&["rules", "init", "--no-skills"]), Effects::LOCAL_WRITE);
    assert_eq!(fx(&["lifecycle", "init", "d"]), Effects::LOCAL_WRITE);
    assert_eq!(
        class_of(&parse(&["lifecycle", "init", "d"]).unwrap().command),
        EffectClass::LocalWrite
    );
    // A follow-up costs money and runs as a child `ham continue` that has the terminal: the editor needs it when there is no text, and the
    // in-process runner of the chat box does not follow the new run when there is.
    let billable_child = Effects::BILLABLE.union(Effects::TERMINAL_HANDOFF);
    assert_eq!(fx(&["continue", RUN, "go"]), billable_child);
    assert_eq!(fx(&["continue", RUN]), billable_child);
    assert_eq!(
        class_of(&parse(&["continue", RUN, "go"]).unwrap().command),
        EffectClass::ChangesRun
    );
}
