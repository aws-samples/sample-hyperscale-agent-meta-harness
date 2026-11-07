//! The help of the command line (D6): `ham --help`, `ham -h`, `ham help`, `ham help <command>` and `ham <command> --help`.
//!
//! `help::request` decides from the argv alone whether the line asks for help and for which text; `help::top_text` and
//! `help::command_text` are the texts. A line is a help request only where the parser would otherwise refuse the flag: a help flag
//! that stands as the value of an option (`--prompt --help`) is the value, and `help` followed by anything but one command word is a
//! new run with the prompt `help`. The usage forms of `rules`, `lifecycle`, `config` and `engine` are the constants the parser's own
//! errors print, so the help and the errors cannot say different things. Every subcommand has a help text, and every option the parser
//! accepts after a command appears in that command's text (the parser is asked, option by option).

#![allow(clippy::unwrap_used)]

use ham_core::command::{
    CONFIG_FORMS, ENGINE_FORMS, LIFECYCLE_FORMS, RULES_FORMS, SUBCOMMANDS, parse,
};
use ham_core::exit::exit_code;
use ham_core::help::{self, HELP_FLAGS, HelpRequest};
use ham_core::registry;
use harness_protocol::goal::GoalStatus;
use std::collections::BTreeSet;

const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

fn req(args: &[&str]) -> Option<HelpRequest> {
    help::request(args)
}

/// The text with every run of white space as one space: the texts wrap, and a sentence may break at a line end.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_help_flags_are_the_two_a_person_types() {
    assert_eq!(HELP_FLAGS, ["--help", "-h"]);
}

#[test]
fn the_flag_or_the_word_alone_asks_for_the_top_help() {
    for args in [
        vec!["--help"],
        vec!["-h"],
        vec!["help"],
        vec!["--json", "help"],
        vec!["help", "--json"],
        vec!["help", "--help"],
        vec!["help", "-h"],
        vec!["help", "help"],
    ] {
        assert_eq!(req(&args), Some(HelpRequest::Top), "{args:?}");
    }
}

#[test]
fn a_flag_after_a_start_line_asks_for_the_top_help() {
    assert_eq!(
        req(&["do it", "--s3-only", "--direct", "--help"]),
        Some(HelpRequest::Top)
    );
    assert_eq!(req(&["--model", "opus", "-h"]), Some(HelpRequest::Top));
}

#[test]
fn a_command_word_with_a_flag_or_after_help_asks_for_that_commands_help() {
    for name in SUBCOMMANDS {
        let want = Some(HelpRequest::Command(name));
        assert_eq!(req(&[name, "--help"]), want, "{name} --help");
        assert_eq!(req(&[name, "-h"]), want, "{name} -h");
        assert_eq!(req(&["help", name]), want, "help {name}");
        assert_eq!(req(&[name, RUN, "--help"]), want, "{name} {RUN} --help");
    }
}

#[test]
fn a_nested_form_shows_the_help_of_its_command() {
    for (args, name) in [
        (vec!["config", "set", "--help"], "config"),
        (vec!["config", "set", "default_model", "-h"], "config"),
        (vec!["rules", "init", "--skills", "-h"], "rules"),
        (vec!["engine", "cline", "show", "--help"], "engine"),
        (vec!["lifecycle", "init", "--help"], "lifecycle"),
    ] {
        assert_eq!(req(&args), Some(HelpRequest::Command(name)), "{args:?}");
    }
}

#[test]
fn a_flag_that_is_the_value_of_an_option_is_no_request() {
    for args in [
        vec!["--prompt", "--help", "--s3-only", "--direct"],
        vec!["do it", "--model", "-h", "--s3-only", "--direct"],
        vec!["say", RUN, "--text", "-h"],
        vec!["say", RUN, "--request", "--help"],
        vec!["pull", RUN, "--artifact", "-h"],
        vec![
            "push",
            RUN,
            "https://example.test/r.git",
            "--branch",
            "--help",
        ],
    ] {
        assert_eq!(req(&args), None, "{args:?}");
    }
}

#[test]
fn help_followed_by_anything_but_one_command_word_is_a_new_run() {
    for args in [
        vec!["help", "--repo", "cc:x", "--direct"],
        vec!["help", "--s3-only", "--direct"],
        vec!["help", "a", "b"],
        vec!["--prompt", "help", "--s3-only", "--direct"],
    ] {
        assert_eq!(req(&args), None, "{args:?}");
    }
    // The parser still reads it as a new run with the prompt `help`.
    let inv = parse(&["help", "--s3-only", "--direct"]).unwrap();
    assert!(format!("{:?}", inv.command).contains("\"help\""));
}

#[test]
fn a_line_without_a_help_word_or_flag_is_no_request() {
    assert_eq!(req(&[]), None);
    for args in [
        vec!["runs"],
        vec!["runs", "--probes"],
        vec!["do it", "--s3-only", "--direct"],
        vec!["config", "set", "default_model", "opus"],
        vec!["--version"],
    ] {
        assert_eq!(req(&args), None, "{args:?}");
    }
}

#[test]
fn help_and_a_word_that_is_no_command_is_a_refusal_that_names_the_closest_commands() {
    assert_eq!(
        req(&["help", "nosuchword"]),
        Some(HelpRequest::Unknown("nosuchword".into()))
    );
    // A word of the terminal app is no command of the command line.
    assert_eq!(
        req(&["help", "settings"]),
        Some(HelpRequest::Unknown("settings".into()))
    );
    let text = help::unknown_text("pul");
    assert!(
        text.starts_with("no command \"pul\"; closest: pull"),
        "{text}"
    );
    assert!(text.contains("ham help"), "{text}");
    let plain = help::unknown_text("zzzzzz");
    assert!(plain.starts_with("no command \"zzzzzz\""), "{plain}");
    assert!(!plain.contains("closest"), "{plain}");
}

#[test]
fn the_top_text_is_a_usage_screen_with_every_command() {
    let t = help::top_text();
    assert!(t.starts_with("Usage:"), "{t}");
    for name in SUBCOMMANDS {
        let line = registry::subcommand(name).unwrap().help;
        assert!(
            t.contains(&format!("  {name}")),
            "{name} is not listed:\n{t}"
        );
        assert!(
            squash(&t).contains(line),
            "the help line of {name} is not shown:\n{t}"
        );
    }
    // How to start a run, the options of a run, the settings, the other ways to ask for help and the version.
    for needle in [
        "ham \"<prompt>\"",
        "--repo cc:<name>",
        "--new-repo cc:<name>",
        "--s3-only",
        "--direct",
        "ham config",
        "ham help <command>",
        "ham <command> --help",
        "ham --version",
    ] {
        assert!(t.contains(needle), "{needle:?} is missing:\n{t}");
    }
}

#[test]
fn the_top_text_lists_every_option_of_a_run() {
    let t = help::top_text();
    for opt in START_OPTIONS {
        assert!(t.contains(opt), "{opt} is missing from the top help:\n{t}");
    }
}

#[test]
fn the_exit_codes_in_the_top_text_are_the_ones_the_client_returns() {
    let t = help::top_text();
    let rows = [
        ("complete", GoalStatus::Complete),
        ("active", GoalStatus::Active),
        ("paused", GoalStatus::Paused),
        ("blocked", GoalStatus::Blocked),
        ("usage limit", GoalStatus::UsageLimited),
        ("budget limit", GoalStatus::BudgetLimited),
        ("cleared", GoalStatus::Cleared),
    ];
    for (word, status) in rows {
        let code = exit_code(Some(status));
        let row = t
            .lines()
            .find(|l| l.contains(word) && l.trim_start().starts_with(&format!("{code} ")))
            .unwrap_or_else(|| panic!("no exit code row `{code} ... {word}` in:\n{t}"));
        assert!(!row.is_empty());
    }
    assert!(t.contains("Exit codes"), "{t}");
    for code in [ham_core::exit::ERROR, ham_core::exit::USAGE] {
        assert!(
            t.lines()
                .any(|l| l.trim_start().starts_with(&format!("{code} "))),
            "no row for exit code {code}:\n{t}"
        );
    }
}

#[test]
fn the_words_taken_from_the_clients_tables_are_put_in() {
    // The engine, the model aliases and the default model come from the roadmap and the shipped model list, so a change there changes the help.
    let mut all: Vec<String> = SUBCOMMANDS
        .iter()
        .filter_map(|n| help::command_text(n))
        .collect();
    all.push(help::top_text());
    for t in &all {
        assert!(
            !t.contains('{') && !t.contains('}'),
            "a placeholder is left:\n{t}"
        );
    }
    let top = squash(&help::top_text());
    let default_name = ham_core::render::model_name(ham_core::models::DEFAULT_MODEL_ID);
    assert!(
        top.contains(&default_name),
        "{default_name} is not in:\n{top}"
    );
    let aliases: Vec<String> = ham_core::models::builtin()
        .models()
        .iter()
        .filter_map(|m| m.alias.clone())
        .collect();
    assert!(!aliases.is_empty());
    assert!(
        top.contains(&format!("({})", aliases.join(", "))),
        "the aliases are not in:\n{top}"
    );
    let engine = ham_core::engines::selected();
    assert!(
        top.contains(engine.name),
        "{} is not in:\n{top}",
        engine.name
    );
    let text = help::command_text("engine").unwrap();
    assert!(
        text.contains(&format!("ham engine {} init", engine.id)),
        "{text}"
    );
}

#[test]
fn every_subcommand_has_help_with_its_usage_and_its_registry_line() {
    for name in SUBCOMMANDS {
        let t = help::command_text(name).unwrap_or_else(|| panic!("no help for {name}"));
        assert!(t.starts_with("Usage:\n"), "{name}:\n{t}");
        assert!(t.contains(&format!("  ham {name}")), "{name}:\n{t}");
        assert!(
            squash(&t).contains(registry::subcommand(name).unwrap().help),
            "{name}:\n{t}"
        );
        assert!(t.contains("--json"), "{name} says nothing of --json:\n{t}");
    }
    assert_eq!(help::command_text("nosuchword"), None);
    assert_eq!(help::command_text("settings"), None);
}

#[test]
fn the_usage_forms_of_the_grouped_commands_are_the_ones_the_parser_prints() {
    // The parser's own error for a missing verb prints the forms constant; the help prints the same constant.
    for (name, forms) in [
        ("rules", RULES_FORMS),
        ("lifecycle", LIFECYCLE_FORMS),
        ("config", CONFIG_FORMS),
        ("engine", ENGINE_FORMS),
    ] {
        let shown = help::command_text(name).unwrap();
        for form in forms.split(" | ") {
            assert!(shown.contains(form), "{name}: `{form}` is not in:\n{shown}");
        }
    }
    for (args, forms) in [
        (vec!["rules"], RULES_FORMS),
        (vec!["lifecycle"], LIFECYCLE_FORMS),
        (vec!["engine"], ENGINE_FORMS),
    ] {
        assert_eq!(
            parse(&args).unwrap_err().to_string(),
            format!("usage: {forms}")
        );
    }
    assert!(
        parse(&["config", "bogus"])
            .unwrap_err()
            .to_string()
            .ends_with(CONFIG_FORMS)
    );
}

// ---- every option the parser accepts is in the help of its command --------------------------------------------------------

/// The options of a run, as the parser's arms name them.
const START_OPTIONS: [&str; 19] = [
    "--prompt",
    "--repo",
    "--new-repo",
    "--s3-only",
    "--base",
    "--direct",
    "--model",
    "--no-rules",
    "--no-skills",
    "--skills",
    "--continue",
    "--rules",
    "--input-wait",
    "--goal",
    "--research",
    "--no-research",
    "--allow-third-party",
    "--attach",
    "--compute",
];

/// Every option word of any form of any command: the candidates the parser is asked about.
const CANDIDATES: [&str; 29] = [
    "--probes",
    "--server",
    "--artifact",
    "--snapshot",
    "--branch",
    "--catalog-max-age",
    "--request",
    "--no-wait",
    "--text",
    "--strict",
    "--replace",
    "--kiro",
    "--pack",
    "--skills-as-rules",
    "--skills",
    "--no-skills",
    "--prompt",
    "--repo",
    "--new-repo",
    "--s3-only",
    "--base",
    "--direct",
    "--model",
    "--no-rules",
    "--continue",
    "--rules",
    "--input-wait",
    "--goal",
    "--attach",
];

/// The options the parser accepts after `prefix` (the words that make the line reach the option scan): an option is accepted when
/// neither the bare option nor the option with a value is refused as an unknown option.
fn accepted(prefix: &[&str]) -> BTreeSet<&'static str> {
    let mut all: Vec<&'static str> = CANDIDATES.to_vec();
    all.extend([
        "--research",
        "--no-research",
        "--allow-third-party",
        "--compute",
    ]);
    all.into_iter()
        .filter(|opt| {
            let mut bare: Vec<&str> = prefix.to_vec();
            bare.push(opt);
            let mut valued = bare.clone();
            valued.push("x");
            [bare, valued].iter().any(|argv| match parse(argv) {
                Ok(_) => true,
                Err(e) => !e.to_string().contains("unknown option"),
            })
        })
        .collect()
}

fn set(words: &[&'static str]) -> BTreeSet<&'static str> {
    words.iter().copied().collect()
}

#[test]
fn every_option_a_command_accepts_is_in_its_help_and_nothing_else_is_offered() {
    // (command, prefix that reaches its option scan, the options it takes)
    let table: Vec<(&str, Vec<&str>, BTreeSet<&str>)> = vec![
        ("runs", vec!["runs"], set(&["--probes"])),
        ("watch", vec!["watch"], set(&[])),
        ("cancel", vec!["cancel", RUN], set(&[])),
        ("models", vec!["models"], set(&["--server"])),
        ("pr", vec!["pr", RUN], set(&[])),
        (
            "pull",
            vec!["pull", RUN],
            set(&["--artifact", "--snapshot"]),
        ),
        ("clone", vec!["clone", RUN], set(&[])),
        ("push", vec!["push", RUN], set(&["--branch"])),
        ("link", vec!["link", RUN], set(&[])),
        ("doctor", vec!["doctor"], set(&["--catalog-max-age"])),
        (
            "say",
            vec!["say", RUN],
            set(&["--request", "--no-wait", "--text"]),
        ),
        (
            "new-session",
            vec!["new-session", RUN],
            set(&["--request", "--no-wait", "--text"]),
        ),
        (
            "try-model",
            vec!["try-model", RUN],
            set(&["--request", "--no-wait", "--text"]),
        ),
        (
            "finish",
            vec!["finish", RUN],
            set(&["--request", "--no-wait"]),
        ),
        (
            "rules",
            vec!["rules", "init"],
            set(&["--pack", "--skills-as-rules", "--skills", "--no-skills"]),
        ),
        (
            "rules",
            vec!["rules", "show"],
            set(&["--rules", "--continue"]),
        ),
        ("config", vec!["config", "check"], set(&["--strict"])),
        (
            "engine",
            vec!["engine", "cline", "import"],
            set(&["--replace"]),
        ),
        ("lifecycle", vec!["lifecycle", "init"], set(&["--kiro"])),
    ];
    for (name, prefix, want) in &table {
        let got = accepted(prefix);
        assert_eq!(&got, want, "the parser's options after {prefix:?}");
        let text = help::command_text(name).unwrap();
        for opt in want {
            assert!(text.contains(opt), "{name}: {opt} is not in:\n{text}");
        }
    }
}

#[test]
fn the_help_of_continue_lists_the_options_of_a_run_except_the_three_it_refuses() {
    let text = help::command_text("continue").unwrap();
    // The option rows: the lines of the Options section that start with an option.
    let section = text
        .split("\nOptions:\n")
        .nth(1)
        .expect("an Options section");
    let rows: Vec<&str> = section
        .lines()
        .map(str::trim_start)
        .filter(|l| l.starts_with("--"))
        .map(|l| l.split_whitespace().next().unwrap())
        .collect();
    let refused = ["--new-repo", "--base", "--direct"];
    for opt in START_OPTIONS {
        if refused.contains(&opt) {
            assert!(
                !rows.contains(&opt),
                "continue does not take {opt}:\n{text}"
            );
        } else {
            assert!(rows.contains(&opt), "continue takes {opt}:\n{text}");
        }
    }
    assert!(rows.contains(&"--text"), "{text}");
    // The text says which three it leaves out.
    for opt in refused {
        assert!(text.contains(opt), "{opt} is named as refused:\n{text}");
    }
    // The parser agrees: the three are refused, the others read.
    for opt in refused {
        assert!(parse(&["continue", RUN, opt, "cc:x"]).is_err(), "{opt}");
    }
    assert!(parse(&["continue", RUN, "--s3-only", "--goal", "off"]).is_ok());
}

#[test]
fn the_options_of_a_run_are_accepted_by_the_parser_as_the_top_help_lists_them() {
    // A sample line for each option: the top help may not offer a spelling the parser refuses.
    let samples: [(&str, Vec<&str>); 19] = [
        ("--prompt", vec!["--prompt", "x", "--s3-only", "--direct"]),
        ("--repo", vec!["x", "--repo", "cc:calc", "--direct"]),
        ("--new-repo", vec!["x", "--new-repo", "cc:calc", "--direct"]),
        ("--s3-only", vec!["x", "--s3-only", "--direct"]),
        (
            "--base",
            vec!["x", "--repo", "cc:calc", "--base", "main", "--direct"],
        ),
        ("--direct", vec!["x", "--s3-only", "--direct"]),
        (
            "--model",
            vec!["x", "--s3-only", "--direct", "--model", "opus"],
        ),
        (
            "--no-rules",
            vec!["x", "--s3-only", "--direct", "--no-rules"],
        ),
        (
            "--no-skills",
            vec!["x", "--s3-only", "--direct", "--no-skills"],
        ),
        (
            "--skills",
            vec!["x", "--s3-only", "--direct", "--skills", "d"],
        ),
        (
            "--continue",
            vec!["x", "--s3-only", "--direct", "--continue", "f"],
        ),
        (
            "--rules",
            vec!["x", "--s3-only", "--direct", "--rules", "d"],
        ),
        (
            "--input-wait",
            vec!["x", "--s3-only", "--direct", "--input-wait", "5"],
        ),
        (
            "--goal",
            vec!["x", "--s3-only", "--direct", "--goal", "off"],
        ),
        (
            "--research",
            vec!["x", "--s3-only", "--direct", "--research"],
        ),
        (
            "--no-research",
            vec!["x", "--s3-only", "--direct", "--no-research"],
        ),
        (
            "--allow-third-party",
            vec!["x", "--s3-only", "--direct", "--allow-third-party"],
        ),
        (
            "--attach",
            vec!["x", "--s3-only", "--direct", "--attach", "f"],
        ),
        (
            "--compute",
            vec!["x", "--s3-only", "--direct", "--compute", "auto"],
        ),
    ];
    let t = help::top_text();
    for (opt, argv) in samples {
        parse(&argv).unwrap_or_else(|e| panic!("{opt}: {e}"));
        assert!(t.contains(opt), "{opt} is not in the top help");
    }
}
