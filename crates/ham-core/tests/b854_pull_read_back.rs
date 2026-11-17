//! `ham pull <runId> --read-back <n>` in the parser, the canonical argv and the help.
//!
//! The service uses the files it read back from a stopped session's storage only when its checks leave no doubt. Files it did not use
//! stay in the stage's bucket, and this option is the one command that downloads them, by the number of the session. It stands alone:
//! the files read back from one session are neither the run's newest copy (`--snapshot`) nor a delivered file (`--artifact`).
#![allow(clippy::unwrap_used)]

use ham_core::command::parse;
use ham_core::registry::EffectClass;
use ham_core::router::{RouteContext, RouteOutcome, route};

const RUN: &str = "01JABCDEFGHJKMNPQRSTVWXYZ0";

fn argv(args: &[&str]) -> Result<Vec<String>, String> {
    parse(args)
        .map(|inv| inv.to_argv())
        .map_err(|e| e.to_string())
}

#[test]
fn b854_pull_read_back_is_parsed_and_written_back_as_it_was_typed() {
    for n in ["1", "2", "17", "256"] {
        assert_eq!(
            argv(&["pull", RUN, "--read-back", n]),
            Ok(vec![
                "pull".to_string(),
                RUN.to_string(),
                "--read-back".to_string(),
                n.to_string()
            ]),
            "session {n}"
        );
    }
    // The option may stand before the run id, and --json is accepted anywhere, as for every command.
    let inv = parse(&["pull", "--read-back", "3", RUN, "--json"]).unwrap();
    assert!(inv.json);
    assert_eq!(
        inv.to_argv()[..4],
        ["pull", RUN, "--read-back", "3"].map(str::to_string)
    );
    // Trigger: the two selectors that existed before read as they did.
    assert_eq!(
        argv(&["pull", RUN, "--snapshot"]).unwrap(),
        ["pull", RUN, "--snapshot"].map(str::to_string)
    );
    assert_eq!(
        argv(&["pull", RUN]).unwrap(),
        ["pull", RUN].map(str::to_string)
    );
}

#[test]
fn b854_pull_read_back_stands_alone_and_takes_a_session_number() {
    for other in [vec!["--snapshot"], vec!["--artifact", "zip"]] {
        let mut args = vec!["pull", RUN, "--read-back", "1"];
        args.extend(other.iter().copied());
        let e = argv(&args).unwrap_err();
        assert!(
            e.contains("ham pull takes --read-back <n> alone, without --snapshot or --artifact"),
            "{args:?}: {e}"
        );
    }
    for bad in ["0", "257", "-1", "01", "1.0", "one", ""] {
        let e = argv(&["pull", RUN, "--read-back", bad]).unwrap_err();
        assert!(
            e.contains("--read-back takes the number of a session of the run, 1 to 256"),
            "{bad:?}: {e}"
        );
    }
    // The option needs its value.
    assert!(argv(&["pull", RUN, "--read-back"]).is_err());
    // Trigger: a good number passes.
    assert!(argv(&["pull", RUN, "--read-back", "1"]).is_ok());
}

#[test]
fn b854_the_usage_and_the_help_of_pull_name_the_option() {
    let usage = ham_core::registry::subcommand("pull").unwrap().usage;
    assert_eq!(
        usage,
        "<runId> [--artifact zip|partial|evidence | --snapshot | --read-back <n>]"
    );
    // The help wraps its lines: the words are compared without the line breaks.
    let help = ham_core::help::command_text("pull").unwrap();
    let flat = help.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains(
            "ham pull <runId> [--artifact zip|partial|evidence | --snapshot | --read-back <n>]"
        ),
        "{help}"
    );
    assert!(
        flat.contains(
            "--read-back <n> Download the files the service read back from the storage of session n after that session ended, \
             whether the run went on from them or not; this option stands alone."
        ),
        "{help}"
    );
}

/// Typed in the message box of a run, the command takes the run on screen: the number after `--read-back` is the option's value, not
/// a run id, so the focused run still goes in. It is a local write and runs at once, like `/pull --snapshot`.
#[test]
fn b854_typed_in_the_chat_the_command_runs_at_once_on_the_focused_run() {
    let ctx = RouteContext {
        focused_run: Some(RUN.into()),
        tainted: false,
    };
    match route("/pull --read-back 2", &ctx) {
        RouteOutcome::Command {
            argv,
            invocation,
            class,
            preview,
        } => {
            assert_eq!(argv, ["pull", RUN, "--read-back", "2"].map(str::to_string));
            assert_eq!(invocation.to_argv(), argv);
            assert_eq!(class, EffectClass::LocalWrite);
            assert!(preview.is_none(), "a local write runs at once");
        }
        other => panic!("the command was not routed to the focused run: {other:?}"),
    }
    // Trigger: the selector that existed before is routed the same way.
    assert!(matches!(
        route("/pull --snapshot", &ctx),
        RouteOutcome::Command { .. }
    ));
}
