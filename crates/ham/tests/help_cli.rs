//! The help of the real binary (D6): `ham --help`, `ham -h`, `ham help`, `ham help <command>` and `ham <command> --help` print the usage on
//! standard output and exit 0, before any config file, credential or network is read; `ham help <word>` for a word that is no command
//! is a usage error. The environment is empty: a config file that cannot be parsed sits where `ham` would look for it, and no AWS
//! profile exists, so a help that read either would fail.
#![allow(clippy::unwrap_used)]

use ham_core::command::SUBCOMMANDS;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ham-help-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("config/ham")).unwrap();
    // A config file nobody can parse: a command that reads it fails with exit 1.
    std::fs::write(
        dir.join("config/ham/config.toml"),
        "this is = = not toml [[[\n",
    )
    .unwrap();
    dir
}

fn ham(args: &[&str]) -> Output {
    let root = scratch("run");
    Command::new(env!("CARGO_BIN_EXE_ham"))
        .args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TMPDIR", std::env::temp_dir())
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("AWS_CONFIG_FILE", root.join("no-aws-config"))
        .env(
            "AWS_SHARED_CREDENTIALS_FILE",
            root.join("no-aws-credentials"),
        )
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn the_flags_and_the_word_print_the_usage_and_exit_0() {
    for args in [vec!["--help"], vec!["-h"], vec!["help"]] {
        let o = ham(&args);
        assert_eq!(o.status.code(), Some(0), "{args:?}: {}", err(&o));
        assert_eq!(err(&o), "", "{args:?}");
        let t = out(&o);
        assert!(t.starts_with("Usage:"), "{args:?}:\n{t}");
        for name in SUBCOMMANDS {
            assert!(t.contains(&format!("  {name}")), "{args:?}: {name}:\n{t}");
        }
        assert!(t.contains("ham help <command>"), "{args:?}:\n{t}");
        assert!(t.contains("Exit codes"), "{args:?}:\n{t}");
    }
}

#[test]
fn the_three_ways_to_ask_for_the_top_help_print_the_same_text() {
    let a = out(&ham(&["--help"]));
    assert_eq!(a, out(&ham(&["-h"])));
    assert_eq!(a, out(&ham(&["help"])));
    assert_eq!(a, out(&ham(&["--json", "help"])));
    assert_eq!(a, ham_core::help::top_text());
}

#[test]
fn a_command_with_the_flag_prints_its_own_usage_and_exits_0() {
    let o = ham(&["runs", "--help"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(err(&o), "");
    let t = out(&o);
    assert!(t.starts_with("Usage:\n  ham runs [--probes]"), "{t}");
    assert!(t.contains("--probes"), "{t}");
    assert_eq!(t, ham_core::help::command_text("runs").unwrap());
    assert_eq!(t, out(&ham(&["help", "runs"])));
    assert_eq!(t, out(&ham(&["runs", "-h"])));
}

#[test]
fn every_command_answers_its_flag_with_its_name_and_exit_0() {
    for name in SUBCOMMANDS {
        let o = ham(&[name, "--help"]);
        assert_eq!(o.status.code(), Some(0), "{name}: {}", err(&o));
        assert_eq!(err(&o), "", "{name}");
        assert!(
            out(&o).contains(&format!("ham {name}")),
            "{name}:\n{}",
            out(&o)
        );
    }
}

#[test]
fn a_form_of_a_grouped_command_shows_the_help_of_the_command() {
    let whole = out(&ham(&["config", "--help"]));
    assert!(whole.contains("ham config set <key> <value>"), "{whole}");
    assert_eq!(whole, out(&ham(&["config", "set", "--help"])));
    assert_eq!(whole, out(&ham(&["config", "set", "default_model", "-h"])));
}

#[test]
fn a_word_that_is_no_command_is_a_usage_error() {
    let o = ham(&["help", "nosuchword"]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(out(&o), "");
    assert!(
        err(&o).starts_with("usage: no command \"nosuchword\""),
        "{}",
        err(&o)
    );
    let near = ham(&["help", "pul"]);
    assert_eq!(near.status.code(), Some(2));
    assert!(err(&near).contains("closest: pull"), "{}", err(&near));
}

#[test]
fn help_before_a_start_line_is_still_a_new_run_with_that_prompt() {
    // It reads the (broken) config like any command that needs the stage, so the help never reached it.
    let o = ham(&["help", "--s3-only", "--direct"]);
    assert_ne!(o.status.code(), Some(0));
    assert!(!out(&o).contains("Usage:"), "{}", out(&o));
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
}

#[test]
fn the_version_flag_and_the_bare_line_are_as_before() {
    let v = ham(&["--version"]);
    assert_eq!(v.status.code(), Some(0));
    assert!(out(&v).starts_with("ham v"), "{}", out(&v));
    // No arguments and no terminal: the usage error, not the help.
    let bare = ham(&[]);
    assert_eq!(bare.status.code(), Some(2));
    assert!(err(&bare).starts_with("usage: "), "{}", err(&bare));
    assert_eq!(out(&bare), "");
}

#[test]
fn the_help_flag_exists_in_the_list_the_documents_are_checked_against() {
    assert_eq!(ham::cli::HELP_FLAGS, ham_core::help::HELP_FLAGS);
    assert_eq!(ham::cli::HELP_FLAGS, ["--help", "-h"]);
}
