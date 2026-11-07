//! `ham clone <runId> [dir]`: the parser side. The delivered zip's `repo.bundle` becomes a local
//! git repository; the work is in `crates/ham/src/clone.rs`.

#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, Invocation, SUBCOMMANDS, parse};
use proptest::prelude::*;

fn cmd(args: &[&str]) -> Command {
    parse(args)
        .unwrap_or_else(|e| panic!("{args:?}: {e}"))
        .command
}

#[test]
fn clone_takes_a_run_and_an_optional_directory() {
    assert!(SUBCOMMANDS.contains(&"clone"));
    assert_eq!(
        cmd(&["clone", "R1"]),
        Command::Clone {
            run_id: "R1".into(),
            dir: None
        }
    );
    assert_eq!(
        cmd(&["clone", "R1", "work/calc"]),
        Command::Clone {
            run_id: "R1".into(),
            dir: Some("work/calc".into())
        }
    );
    let inv = parse(&["clone", "R1", "out", "--json"]).unwrap();
    assert!(inv.json);
    assert_eq!(inv.to_argv(), ["clone", "R1", "out", "--json"]);
    assert_eq!(parse(&inv.to_argv()).unwrap(), inv);
    // `clone` is a command name, so it is never a prompt; a prompt that says it is quoted or uses --prompt.
    assert!(matches!(
        cmd(&["--prompt", "clone", "--s3-only", "--direct"]),
        Command::StartDirect(_)
    ));
}

#[test]
fn clone_usage_errors() {
    for (args, needle) in [
        (vec!["clone"], "ham clone needs <runId>"),
        (vec!["clone", "R1", "a", "b"], "unexpected argument"),
        (vec!["clone", "R1", "--into", "x"], "unknown option"),
        (vec!["clone", "R1", ""], "directory"),
    ] {
        let err = parse(&args).expect_err("must fail").to_string();
        assert!(
            err.starts_with("usage:") && err.contains(needle),
            "{args:?}: {err}"
        );
    }
}

proptest! {
    #[test]
    fn clone_argv_round_trips(run in "[A-Z0-9]{1,26}", dir in proptest::option::of("[a-zA-Z0-9_./][a-zA-Z0-9_./-]{0,30}"), json in any::<bool>()) {
        let inv = Invocation { json, command: Command::Clone { run_id: run, dir } };
        let argv = inv.to_argv();
        let back = parse(&argv).map_err(|e| TestCaseError::fail(format!("{argv:?}: {e}")))?;
        prop_assert_eq!(back, inv);
    }
}
