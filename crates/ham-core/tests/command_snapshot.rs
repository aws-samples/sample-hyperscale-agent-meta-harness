//! `ham pull <runId> --snapshot` and `/pull --snapshot`: the parser, the registry and the router side. The
//! download is `ham::chat_routes::fetch_snapshot` (tests in `crates/ham/tests/pull_snapshot.rs`). The flag chooses the last snapshot bundle of a
//! live run instead of an artifact, so it cannot be combined with `--artifact`.

#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, PullArtifact, parse};
use ham_core::registry::{EffectClass, Effects, effects_of, find};
use ham_core::router::{RouteContext, RouteOutcome, route};

const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

fn cmd(args: &[&str]) -> Command {
    parse(args)
        .unwrap_or_else(|e| panic!("{args:?}: {e}"))
        .command
}

#[test]
fn pull_takes_a_snapshot_flag() {
    assert_eq!(
        cmd(&["pull", RUN, "--snapshot"]),
        Command::Pull {
            run_id: RUN.into(),
            artifact: PullArtifact::Snapshot,
        }
    );
    // The flag may come first, and `--json` goes with it as with every command.
    assert_eq!(
        cmd(&["pull", "--snapshot", RUN, "--json"]),
        Command::Pull {
            run_id: RUN.into(),
            artifact: PullArtifact::Snapshot,
        }
    );
}

#[test]
fn the_snapshot_flag_and_an_artifact_choice_exclude_each_other() {
    let e = parse(&["pull", RUN, "--snapshot", "--artifact", "zip"])
        .expect_err("a snapshot is not an artifact")
        .to_string();
    assert!(e.starts_with("usage:"), "{e}");
    assert!(e.contains("--snapshot") && e.contains("--artifact"), "{e}");
}

#[test]
fn the_snapshot_form_round_trips_through_to_argv() {
    let inv = parse(&["pull", RUN, "--snapshot"]).unwrap();
    assert_eq!(inv.to_argv(), vec!["pull", RUN, "--snapshot"]);
    assert_eq!(parse(&inv.to_argv()), Ok(inv));
}

#[test]
fn the_usage_and_the_help_name_the_flag() {
    let e = find("pull").expect("pull is a command");
    assert!(e.usage.contains("--snapshot"), "{}", e.usage);
    assert!(
        ham_core::registry::help_text().contains("--snapshot")
            || ham_core::registry::command_help("pull")
                .unwrap()
                .contains("--snapshot")
    );
}

#[test]
fn a_snapshot_pull_is_a_local_write_like_any_pull() {
    let c = cmd(&["pull", RUN, "--snapshot"]);
    assert_eq!(effects_of(&c), Effects::LOCAL_WRITE);
    assert_eq!(effects_of(&c).class(), EffectClass::LocalWrite);
}

#[test]
fn typed_in_the_chat_the_command_runs_at_once_on_the_focused_run() {
    let ctx = RouteContext {
        focused_run: Some(RUN.into()),
        tainted: false,
    };
    match route("/pull --snapshot", &ctx) {
        RouteOutcome::Command {
            argv,
            invocation,
            class,
            preview,
        } => {
            assert_eq!(argv, vec!["pull", RUN, "--snapshot"]);
            assert_eq!(
                invocation.command,
                Command::Pull {
                    run_id: RUN.into(),
                    artifact: PullArtifact::Snapshot
                }
            );
            assert_eq!(class, EffectClass::LocalWrite);
            assert!(preview.is_none(), "a local write runs at once");
        }
        other => panic!("{other:?}"),
    }
}
