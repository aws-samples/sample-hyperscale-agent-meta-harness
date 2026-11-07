//! The seven verbs of `ham config`: `get`, `set` and `unset` of v4.02 and the four that read and repair the file (`show`, `check`, `edit`,
//! `docs`). The parser, the canonical argv, the effects the chat box judges them by and the preview of the one that opens the editor.
#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, ConfigCommand, parse};
use ham_core::registry::{EffectClass, Effects, class_of, effects_of};
use ham_core::router::{RouteContext, RouteOutcome, route};

fn config(args: &[&str]) -> ConfigCommand {
    let mut argv = vec!["config"];
    argv.extend_from_slice(args);
    match parse(&argv).unwrap().command {
        Command::Config(c) => c,
        other => panic!("{argv:?}: {other:?}"),
    }
}

#[test]
fn the_four_new_verbs_parse_and_the_old_three_still_do() {
    assert_eq!(config(&["show"]), ConfigCommand::Show);
    assert_eq!(config(&["check"]), ConfigCommand::Check { strict: false });
    assert_eq!(
        config(&["check", "--strict"]),
        ConfigCommand::Check { strict: true }
    );
    assert_eq!(config(&["edit"]), ConfigCommand::Edit);
    assert_eq!(config(&["docs"]), ConfigCommand::Docs);
    assert_eq!(config(&[]), ConfigCommand::List);
    assert_eq!(
        config(&["get", "run.goal"]),
        ConfigCommand::Get {
            key: "run.goal".into()
        }
    );
    assert_eq!(
        config(&["set", "run.goal", "off"]),
        ConfigCommand::Set {
            key: "run.goal".into(),
            value: "off".into()
        }
    );
    assert_eq!(
        config(&["unset", "run.goal"]),
        ConfigCommand::Unset {
            key: "run.goal".into()
        }
    );
    assert!(parse(&["config", "show", "--json"]).unwrap().json);
    assert!(parse(&["config", "--json", "check"]).unwrap().json);
}

#[test]
fn a_verb_with_a_word_or_an_option_it_does_not_take_is_a_usage_error() {
    for args in [
        vec!["config", "show", "run.goal"],
        vec!["config", "check", "extra"],
        vec!["config", "check", "--loose"],
        vec!["config", "edit", "now"],
        vec!["config", "docs", "--strict"],
        vec!["config", "show", "--strict"],
    ] {
        let e = parse(&args).unwrap_err().to_string();
        assert!(e.starts_with("usage:"), "{args:?}: {e}");
    }
    let e = parse(&["config", "frobnicate"]).unwrap_err().to_string();
    for verb in ["get", "set", "unset", "show", "check", "edit", "docs"] {
        assert!(e.contains(verb), "the error lists {verb}: {e}");
    }
}

#[test]
fn every_verb_has_one_canonical_argv_that_parses_back() {
    for args in [
        vec!["config"],
        vec!["config", "get", "region"],
        vec!["config", "set", "region", "us-east-1"],
        vec!["config", "unset", "region"],
        vec!["config", "show"],
        vec!["config", "check"],
        vec!["config", "check", "--strict"],
        vec!["config", "edit"],
        vec!["config", "docs"],
    ] {
        let inv = parse(&args).unwrap();
        assert_eq!(inv.to_argv(), args, "{args:?}");
        assert_eq!(parse(&inv.to_argv()).unwrap(), inv);
    }
    let json = parse(&["config", "show", "--json"]).unwrap();
    assert_eq!(json.to_argv(), ["config", "show", "--json"]);
}

#[test]
fn show_check_and_docs_only_read_and_edit_writes_through_the_terminal() {
    let fx = |args: &[&str]| effects_of(&parse(args).unwrap().command);
    for args in [
        vec!["config", "show"],
        vec!["config", "check"],
        vec!["config", "check", "--strict"],
        vec!["config", "docs"],
    ] {
        assert_eq!(fx(&args), Effects::NONE, "{args:?}");
        assert_eq!(
            class_of(&parse(&args).unwrap().command),
            EffectClass::ReadOnly
        );
    }
    assert_eq!(
        fx(&["config", "edit"]),
        Effects::LOCAL_WRITE.union(Effects::TERMINAL_HANDOFF)
    );
}

#[test]
fn slash_config_edit_shows_a_preview_that_says_where_the_terminal_goes_and_the_readers_run_at_once()
{
    let ctx = RouteContext::default();
    for line in ["/config show", "/config check --strict", "/config docs"] {
        match route(line, &ctx) {
            RouteOutcome::Command { preview, class, .. } => {
                assert!(preview.is_none(), "{line}");
                assert_eq!(class, EffectClass::ReadOnly);
            }
            other => panic!("{line}: {other:?}"),
        }
    }
    match route("/config edit", &ctx) {
        RouteOutcome::Command {
            argv,
            preview: Some(preview),
            class,
            ..
        } => {
            assert_eq!(argv, ["config", "edit"]);
            assert_eq!(class, EffectClass::ChangesRun);
            assert!(
                preview.text.contains("editor") && preview.text.contains("config.toml"),
                "{}",
                preview.text
            );
            assert!(preview.confirm_with.is_none());
        }
        other => panic!("{other:?}"),
    }
}
