//! `ham engine <name> init|show|check|edit|set|unset|import`: the parser, the registry
//! entry and the effects of each form. The engine name is an argument: the parser names no engine, and a word that could leave the engines
//! folder is a usage error.
#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, EngineAction, EngineCommand, Invocation, SUBCOMMANDS, parse};
use ham_core::registry::{self, Effects, Kind};

fn engine(args: &[&str]) -> EngineCommand {
    match parse(args).unwrap().command {
        Command::Engine(e) => e,
        other => panic!("{other:?}"),
    }
}

fn usage(args: &[&str]) -> String {
    let e = parse(args).unwrap_err().to_string();
    assert!(e.starts_with("usage: "), "{e}");
    e
}

fn cmd(name: &str, action: EngineAction) -> EngineCommand {
    EngineCommand {
        engine: name.to_string(),
        action,
    }
}

#[test]
fn every_form_parses_to_its_action() {
    assert_eq!(
        engine(&["engine", "cline", "init"]),
        cmd("cline", EngineAction::Init)
    );
    assert_eq!(
        engine(&["engine", "cline", "show"]),
        cmd("cline", EngineAction::Show)
    );
    assert_eq!(
        engine(&["engine", "cline", "check"]),
        cmd("cline", EngineAction::Check)
    );
    assert_eq!(
        engine(&["engine", "cline", "edit"]),
        cmd("cline", EngineAction::Edit)
    );
    assert_eq!(
        engine(&["engine", "cline", "set", "reasoning.effort", "high"]),
        cmd(
            "cline",
            EngineAction::Set {
                key: "reasoning.effort".into(),
                value: "high".into()
            }
        )
    );
    assert_eq!(
        engine(&["engine", "cline", "unset", "tools.disable"]),
        cmd(
            "cline",
            EngineAction::Unset {
                key: "tools.disable".into()
            }
        )
    );
    assert_eq!(
        engine(&["engine", "cline", "import", "my.toml"]),
        cmd(
            "cline",
            EngineAction::Import {
                file: "my.toml".into(),
                replace: false
            }
        )
    );
    assert_eq!(
        engine(&["engine", "cline", "import", "my.toml", "--replace"]),
        cmd(
            "cline",
            EngineAction::Import {
                file: "my.toml".into(),
                replace: true
            }
        )
    );
    // Another engine of the roadmap parses too; the run time answers that it has no schema.
    assert_eq!(engine(&["engine", "pi", "show"]).engine, "pi");
}

#[test]
fn the_forms_round_trip_through_the_canonical_argv_and_json_is_accepted_anywhere() {
    for args in [
        vec!["engine", "cline", "init"],
        vec!["engine", "cline", "show"],
        vec!["engine", "cline", "check"],
        vec!["engine", "cline", "edit"],
        vec!["engine", "cline", "set", "commands.timeout_seconds", "900"],
        vec![
            "engine",
            "cline",
            "set",
            "tools.disable",
            "spawn_agent,fetch_web_content",
        ],
        vec!["engine", "cline", "unset", "reasoning.effort"],
        vec!["engine", "cline", "import", "/a b/my.toml"],
        vec!["engine", "cline", "import", "my.toml", "--replace"],
    ] {
        let inv = parse(&args).unwrap();
        assert_eq!(inv.to_argv(), args, "{args:?}");
        let again: Invocation = parse(&inv.to_argv()).unwrap();
        assert_eq!(again, inv);
    }
    let with_json = parse(&["--json", "engine", "cline", "show"]).unwrap();
    assert!(with_json.json);
    assert_eq!(
        parse(&["engine", "cline", "show", "--json"]).unwrap(),
        with_json
    );
}

#[test]
fn b787_an_engine_name_alone_is_the_choose_form() {
    assert_eq!(
        engine(&["engine", "opencode"]),
        cmd("opencode", EngineAction::Choose)
    );
    let argv = parse(&["engine", "opencode"]).unwrap().to_argv();
    assert_eq!(argv, ["engine", "opencode"]);
    assert!(parse(&["engine", "bad word"]).is_err());
}

#[test]
fn a_form_that_is_not_complete_is_a_usage_error_that_lists_the_forms() {
    for args in [
        vec!["engine"],
        vec!["engine", "cline", "frobnicate"],
        vec!["engine", "cline", "set", "reasoning.effort"],
        vec!["engine", "cline", "set"],
        vec!["engine", "cline", "unset"],
        vec!["engine", "cline", "import"],
        vec!["engine", "cline", "init", "extra"],
        vec!["engine", "cline", "show", "--replace"],
        vec!["engine", "cline", "set", "a", "b", "c"],
        vec!["engine", "cline", "import", "a", "b"],
    ] {
        let e = usage(&args);
        assert!(e.contains("engine"), "{args:?}: {e}");
    }
    assert!(usage(&["engine"]).contains("init"));
    assert!(usage(&["engine", "cline", "frobnicate"]).contains("unknown engine command"));
}

#[test]
fn an_engine_word_that_could_leave_the_engines_folder_is_refused() {
    let long = "a".repeat(33);
    for bad in [
        "../x",
        "a/b",
        "A",
        "x y",
        ".",
        "..",
        "",
        "-x",
        long.as_str(),
        "cline.toml",
    ] {
        let e = usage(&["engine", bad, "show"]);
        assert!(e.contains("engine"), "{bad:?}: {e}");
    }
}

#[test]
fn the_word_is_a_subcommand_with_one_registry_entry_and_the_effects_of_its_forms() {
    assert!(SUBCOMMANDS.contains(&"engine"));
    let e = registry::subcommand("engine").expect("an entry");
    assert_eq!(e.kind, Kind::Subcommand);
    assert!(
        e.usage.contains("init") && e.usage.contains("import"),
        "{}",
        e.usage
    );
    assert!(e.help.len() <= 80, "{}", e.help);
    // The strongest form of the command: a write and a hand-off of the terminal.
    assert!(e.effects.contains(Effects::LOCAL_WRITE));
    assert!(e.effects.contains(Effects::TERMINAL_HANDOFF));
    let fx = |args: &[&str]| registry::effects_of(&parse(args).unwrap().command);
    assert_eq!(fx(&["engine", "cline", "show"]), Effects::NONE);
    assert_eq!(fx(&["engine", "cline", "check"]), Effects::NONE);
    for write in [
        vec!["engine", "cline", "init"],
        vec!["engine", "cline", "set", "reasoning.effort", "low"],
        vec!["engine", "cline", "unset", "reasoning.effort"],
        vec!["engine", "cline", "import", "f.toml"],
    ] {
        assert_eq!(fx(&write), Effects::LOCAL_WRITE, "{write:?}");
    }
    assert_eq!(
        fx(&["engine", "cline", "edit"]),
        Effects::LOCAL_WRITE.union(Effects::TERMINAL_HANDOFF)
    );
    // None of the forms changes where requests go: no second Enter with a typed name.
    assert!(
        !fx(&["engine", "cline", "set", "reasoning.effort", "low"])
            .contains(Effects::CONFIG_SECURITY)
    );
}

#[test]
fn the_word_is_in_the_popup_and_the_help_and_names_no_engine() {
    let popup: Vec<&str> = registry::popup("eng").iter().map(|e| e.name).collect();
    assert!(popup.contains(&"engine"), "{popup:?}");
    assert!(registry::help_text().contains("/engine "));
    let e = registry::subcommand("engine").unwrap();
    for text in [e.usage, e.help] {
        let lower = text.to_lowercase();
        for word in ["cline", "opencode", "codex", "kiro"] {
            assert!(!lower.contains(word), "{text:?} names {word}");
        }
    }
}
