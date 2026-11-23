//! `ham pull <runId> --before-refusal <notice id>` in the parser, the canonical argv, the help and the chat.
//!
//! When a session's storage refuses a write, the run's notice names the last copy the session had saved before `ham` first saw it,
//! and this option is the command that downloads that copy, by the id the notice printed. It stands alone: that copy is neither the
//! run's newest copy (`--snapshot`), the files read back from a session (`--read-back`) nor a delivered file (`--artifact`).
#![allow(clippy::unwrap_used)]

use ham_core::command::{BEFORE_REFUSAL_USAGE, parse};
use ham_core::registry::EffectClass;
use ham_core::router::{RouteContext, RouteOutcome, route};

const RUN: &str = "01JABCDEFGHJKMNPQRSTVWXYZ0";
/// The id of a notice: an event id, 26 characters of the id alphabet.
const NOTICE: &str = "01M5A2R7K30000000000000007";

fn argv(args: &[&str]) -> Result<Vec<String>, String> {
    parse(args)
        .map(|inv| inv.to_argv())
        .map_err(|e| e.to_string())
}

#[test]
fn b881_the_flag_stands_alone_and_takes_one_id() {
    assert_eq!(
        BEFORE_REFUSAL_USAGE,
        "ham pull takes --before-refusal <notice id> alone, as the run's notice printed it"
    );
    // It is parsed and written back as it was typed; it may stand before the run id, and --json is accepted as for every command.
    assert_eq!(
        argv(&["pull", RUN, "--before-refusal", NOTICE]),
        Ok(["pull", RUN, "--before-refusal", NOTICE]
            .map(str::to_string)
            .to_vec())
    );
    let inv = parse(&["pull", "--before-refusal", NOTICE, RUN, "--json"]).unwrap();
    assert!(inv.json);
    assert_eq!(
        inv.to_argv()[..4],
        ["pull", RUN, "--before-refusal", NOTICE].map(str::to_string)
    );
    // With any other selector it is the usage error.
    for other in [
        vec!["--snapshot"],
        vec!["--artifact", "zip"],
        vec!["--read-back", "1"],
    ] {
        let mut args = vec!["pull", RUN, "--before-refusal", NOTICE];
        args.extend(other.iter().copied());
        let e = argv(&args).unwrap_err();
        assert!(e.contains(BEFORE_REFUSAL_USAGE), "{args:?}: {e}");
    }
    // A value that is no id of a notice: too short, too long, a letter the id alphabet does not have, a path, a session number.
    for bad in [
        "",
        "2",
        "01M5A2R7K3000000000000000",
        "01M5A2R7K300000000000000077",
        "01M5A2R7K3000000000000000U",
        "../01M5A2R7K3000000000000",
        "01m5a2r7k30000000000000007",
    ] {
        let e = argv(&["pull", RUN, "--before-refusal", bad]).unwrap_err();
        assert!(e.contains(BEFORE_REFUSAL_USAGE), "{bad:?}: {e}");
    }
    // The option needs its value.
    assert!(argv(&["pull", RUN, "--before-refusal"]).is_err());
    // Trigger: the selectors that existed before read as they did.
    assert_eq!(
        argv(&["pull", RUN, "--snapshot"]).unwrap(),
        ["pull", RUN, "--snapshot"].map(str::to_string)
    );
    assert_eq!(
        argv(&["pull", RUN, "--read-back", "3"]).unwrap(),
        ["pull", RUN, "--read-back", "3"].map(str::to_string)
    );
    assert_eq!(
        argv(&["pull", RUN]).unwrap(),
        ["pull", RUN].map(str::to_string)
    );
}

#[test]
fn b881_the_help_of_pull_names_the_option_and_the_panels_line_keeps_its_width() {
    // The one-line usage of the help panel holds 100 characters with its indent and the command's name; with this option it would
    // hold 108. It stays as it was, and every line of the panel still fits.
    let usage = ham_core::registry::subcommand("pull").unwrap().usage;
    assert_eq!(
        usage,
        "<runId> [--artifact zip|partial|evidence | --snapshot | --read-back <n>]"
    );
    assert!(
        ham_core::registry::help_text()
            .lines()
            .all(|l| l.chars().count() <= 100)
    );
    // The help wraps its lines: the words are compared without the line breaks.
    let help = ham_core::help::command_text("pull").unwrap();
    let flat = help.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains(
            "ham pull <runId> [--artifact zip|partial|evidence | --snapshot | --read-back <n> | --before-refusal <notice id>]"
        ),
        "{help}"
    );
    assert!(
        flat.contains(
            "--before-refusal <notice id> Download the last copy a session had saved before ham first saw the session's storage \
             refuse a write; the id is the one the run's notice printed; this option stands alone."
        ),
        "{help}"
    );
}

/// Typed in the message box of a run, the command takes the run on screen: the id after `--before-refusal` is the option's value,
/// not a run id (it has the form of one), so the focused run still goes in. It is a local write and runs at once.
#[test]
fn b881_typed_in_the_chat_the_command_runs_at_once_on_the_focused_run() {
    let ctx = RouteContext {
        focused_run: Some(RUN.into()),
        tainted: false,
    };
    match route(&format!("/pull --before-refusal {NOTICE}"), &ctx) {
        RouteOutcome::Command {
            argv,
            invocation,
            class,
            preview,
        } => {
            assert_eq!(
                argv,
                ["pull", RUN, "--before-refusal", NOTICE].map(str::to_string)
            );
            assert_eq!(invocation.to_argv(), argv);
            assert_eq!(class, EffectClass::LocalWrite);
            assert!(preview.is_none(), "a local write runs at once");
        }
        other => panic!("the command was not routed to the focused run: {other:?}"),
    }
}

/// The command the worker's notice, the client's lines and the card print is the one the parser takes.
#[test]
fn b881_the_command_the_notice_prints_is_the_one_the_parser_takes() {
    use ham_core::storage_notice::{StorageFirst, StorageSeen, card_values};
    let seen = StorageSeen {
        count: 1,
        first: Some(StorageFirst {
            event_id: NOTICE.to_string(),
            ts: "2026-11-21T00:39:02Z".to_string(),
            session: 1,
            has_copy: true,
            started_from: None,
        }),
        recent: Vec::new(),
        test_switch: false,
    };
    let rows = card_values(&seen, RUN);
    let row = rows
        .iter()
        .find(|r| r.contains("--before-refusal"))
        .unwrap_or_else(|| panic!("{rows:?}"));
    let words: Vec<&str> = row.split_whitespace().take(5).collect();
    assert_eq!(words, ["ham", "pull", RUN, "--before-refusal", NOTICE]);
    assert_eq!(
        argv(&words[1..]).unwrap(),
        ["pull", RUN, "--before-refusal", NOTICE].map(str::to_string)
    );
}
