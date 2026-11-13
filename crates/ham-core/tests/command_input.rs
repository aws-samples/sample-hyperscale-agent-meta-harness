//! The four commands that answer a run's "Cline needs you" question and the `--input-wait` option of a
//! direct run: `ham say`, `ham new-session`, `ham try-model` and `ham finish`. Parse and `to_argv` round trip; the text `-` is kept
//! as typed (the caller reads standard input for it).
#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, Invocation, SUBCOMMANDS, parse};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";

fn cmd(args: &[&str]) -> Command {
    parse(args).unwrap().command
}

fn usage(args: &[&str]) -> String {
    let e = parse(args).unwrap_err().to_string();
    assert!(e.starts_with("usage:"), "{e}");
    e
}

fn round(args: &[&str]) -> Invocation {
    let inv = parse(args).unwrap();
    assert_eq!(parse(&inv.to_argv()).unwrap(), inv, "{args:?}");
    inv
}

#[test]
fn the_four_verbs_are_subcommands() {
    for v in ["say", "new-session", "try-model", "finish"] {
        assert!(SUBCOMMANDS.contains(&v), "{v}");
    }
}

#[test]
fn say_takes_the_run_and_the_text() {
    assert_eq!(
        cmd(&["say", RUN, "Run the sleeps in the background."]),
        Command::Say {
            run_id: RUN.into(),
            text: "Run the sleeps in the background.".into(),
            request_id: None,
            no_wait: false,
        }
    );
}

#[test]
fn say_keeps_a_lone_dash_for_the_caller_to_read_standard_input() {
    match cmd(&["say", RUN, "-"]) {
        Command::Say { text, .. } => assert_eq!(text, "-"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn text_that_starts_with_a_dash_goes_through_the_text_option() {
    match cmd(&["say", RUN, "--text", "-1 is not a valid index"]) {
        Command::Say { text, .. } => assert_eq!(text, "-1 is not a valid index"),
        other => panic!("{other:?}"),
    }
    // A value that looks like an option is read as an unknown option, never as the text.
    assert!(usage(&["say", RUN, "-1 is wrong"]).contains("unknown option"));
}

#[test]
fn say_needs_a_run_and_a_text_that_is_not_blank() {
    assert!(usage(&["say"]).contains("runId"));
    assert!(usage(&["say", RUN]).contains("text"));
    assert!(usage(&["say", RUN, "   "]).contains("text"));
    assert!(usage(&["say", RUN, "a", "b"]).contains("unexpected argument"));
}

#[test]
fn new_session_text_is_optional() {
    assert_eq!(
        cmd(&["new-session", RUN]),
        Command::NewSession {
            run_id: RUN.into(),
            text: None,
            request_id: None,
            no_wait: false,
        }
    );
    match cmd(&["new-session", RUN, "Start with the failing test."]) {
        Command::NewSession { text, .. } => {
            assert_eq!(text.as_deref(), Some("Start with the failing test."));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn try_model_takes_a_model_and_an_optional_text() {
    assert_eq!(
        cmd(&["try-model", RUN, "opus"]),
        Command::TryModel {
            run_id: RUN.into(),
            model: "opus".into(),
            text: None,
            request_id: None,
            no_wait: false,
        }
    );
    match cmd(&[
        "try-model",
        RUN,
        "us.anthropic.claude-opus-5-5",
        "Carry on.",
    ]) {
        Command::TryModel { model, text, .. } => {
            assert_eq!(model, "us.anthropic.claude-opus-5-5");
            assert_eq!(text.as_deref(), Some("Carry on."));
        }
        other => panic!("{other:?}"),
    }
    assert!(usage(&["try-model", RUN]).contains("model"));
}

#[test]
fn finish_takes_only_the_run() {
    assert_eq!(
        cmd(&["finish", RUN]),
        Command::Finish {
            run_id: RUN.into(),
            request_id: None,
            no_wait: false,
        }
    );
    assert!(usage(&["finish", RUN, "now"]).contains("unexpected argument"));
    assert!(usage(&["finish"]).contains("runId"));
}

#[test]
fn request_no_wait_and_json_are_options_of_every_verb() {
    match cmd(&["say", RUN, "hi", "--request", REQ, "--no-wait", "--json"]) {
        Command::Say {
            request_id,
            no_wait,
            ..
        } => {
            assert_eq!(request_id.as_deref(), Some(REQ));
            assert!(no_wait);
        }
        other => panic!("{other:?}"),
    }
    assert!(parse(&["finish", RUN, "--json"]).unwrap().json);
    // `--json` as the value of `--request` is the value, not the flag.
    match cmd(&["finish", RUN, "--request", "--json"]) {
        Command::Finish { request_id, .. } => assert_eq!(request_id.as_deref(), Some("--json")),
        other => panic!("{other:?}"),
    }
    assert!(usage(&["finish", RUN, "--request"]).contains("needs a value"));
}

#[test]
fn every_form_round_trips_through_to_argv() {
    for args in [
        vec!["say", RUN, "hello"],
        vec!["say", RUN, "-"],
        vec!["say", RUN, "--text", "-1 is wrong"],
        vec!["say", RUN, "hello", "--request", REQ, "--no-wait", "--json"],
        vec!["new-session", RUN],
        vec!["new-session", RUN, "Start with the failing test."],
        vec!["new-session", RUN, "--request", REQ],
        vec!["try-model", RUN, "sonnet"],
        vec!["try-model", RUN, "sol", "Carry on.", "--no-wait"],
        vec!["finish", RUN],
        vec!["finish", RUN, "--request", REQ, "--json"],
    ] {
        round(&args);
    }
}

fn direct(extra: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = ["build it", "--repo", "cc:demo", "--direct"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    v.extend(extra.iter().map(|s| (*s).to_string()));
    v
}

#[test]
fn input_wait_is_a_direct_run_option_in_minutes() {
    let inv = parse(&direct(&["--input-wait", "10"])).unwrap();
    match &inv.command {
        Command::StartDirect(d) => assert_eq!(d.input_wait, Some(10)),
        other => panic!("{other:?}"),
    }
    assert_eq!(parse(&inv.to_argv()).unwrap(), inv);
    // Zero is the value that keeps today's behaviour, so it is a value and not "absent".
    match parse(&direct(&["--input-wait", "0"])).unwrap().command {
        Command::StartDirect(d) => assert_eq!(d.input_wait, Some(0)),
        other => panic!("{other:?}"),
    }
    match parse(&direct(&[])).unwrap().command {
        Command::StartDirect(d) => assert_eq!(d.input_wait, None),
        other => panic!("{other:?}"),
    }
}

#[test]
fn input_wait_is_zero_to_the_longest_run_and_a_whole_number() {
    for bad in ["43201", "-1", "ten", "1.5", ""] {
        let e = parse(&direct(&["--input-wait", bad]))
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("usage:") && e.contains("--input-wait"),
            "{bad}: {e}"
        );
    }
    assert!(parse(&direct(&["--input-wait", "43200"])).is_ok());
    let e = parse(&direct(&["--input-wait"])).unwrap_err().to_string();
    assert!(e.contains("needs a value"), "{e}");
    // `--json` after the option is its value (a bad one), not the flag.
    assert!(parse(&direct(&["--input-wait", "--json"])).is_err());
}

#[test]
fn input_wait_without_direct_is_refused_like_the_other_direct_options() {
    let e = parse(&["build it", "--repo", "cc:demo", "--input-wait", "5"])
        .unwrap_err()
        .to_string();
    assert!(e.contains("--input-wait") && e.contains("--direct"), "{e}");
}

#[test]
fn the_usage_line_lists_the_four_answer_commands() {
    let e = parse::<&str>(&[]).unwrap_err().to_string();
    for v in ["say", "new-session", "try-model", "finish"] {
        assert!(e.contains(v), "{v} missing from {e}");
    }
}
