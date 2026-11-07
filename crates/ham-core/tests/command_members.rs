//! The start options of package REQ1: `--goal on|off`, `--research` and `--no-research`, `--allow-third-party`
//! and `--attach <file>`. Every form parses, renders back to the same argv, is an option of a direct
//! run (and of `ham continue`), and becomes a member of the start request; a request that uses none of them is the request it always was.

#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, DirectArgs, parse};
use ham_core::request::{DirectInputs, direct_run_request};
use harness_protocol::direct::{Attachment, GoalOptions};
use harness_protocol::research::{ResearchOptions, ThirdPartyMode};

const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

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

fn start(extra: &[&str]) -> DirectArgs {
    let mut args = vec!["Fix it", "--s3-only", "--direct"];
    args.extend_from_slice(extra);
    match round_trip(&args) {
        Command::StartDirect(d) => d,
        other => panic!("{other:?}"),
    }
}

fn follow_up(extra: &[&str]) -> DirectArgs {
    let mut args = vec!["continue", RUN, "go"];
    args.extend_from_slice(extra);
    match round_trip(&args) {
        Command::Continue(d, _) => d,
        other => panic!("{other:?}"),
    }
}

fn request_of(
    d: &DirectArgs,
    inputs: DirectInputs,
) -> harness_protocol::api::v1::StartDirectRunRequest {
    direct_run_request(d, inputs, &Default::default()).unwrap()
}

// ---- parsing -----------------------------------------------------------------------------------------------------------------

#[test]
fn a_plain_start_sets_none_of_them() {
    let d = start(&[]);
    assert_eq!(d.goal, None);
    assert_eq!(d.research, None);
    assert!(!d.allow_third_party);
    assert!(d.attach.is_empty());
}

#[test]
fn goal_takes_on_or_off() {
    assert_eq!(start(&["--goal", "off"]).goal, Some(false));
    assert_eq!(start(&["--goal", "on"]).goal, Some(true));
    for bad in ["maybe", "OFF", "0", "false", ""] {
        let e = usage(&["Fix it", "--s3-only", "--direct", "--goal", bad]);
        assert!(
            e.contains("--goal") && e.contains("on or off"),
            "{bad:?}: {e}"
        );
    }
    usage(&["Fix it", "--s3-only", "--direct", "--goal"]);
    let e = usage(&[
        "Fix it",
        "--s3-only",
        "--direct",
        "--goal",
        "off",
        "--goal",
        "on",
    ]);
    assert!(e.contains("--goal"), "{e}");
}

#[test]
fn research_has_one_switch_for_each_way() {
    assert_eq!(start(&["--research"]).research, Some(true));
    assert_eq!(start(&["--no-research"]).research, Some(false));
    let e = usage(&[
        "Fix it",
        "--s3-only",
        "--direct",
        "--research",
        "--no-research",
    ]);
    assert!(
        e.contains("--research") && e.contains("--no-research"),
        "{e}"
    );
}

#[test]
fn third_party_is_a_switch_that_can_only_allow() {
    assert!(start(&["--allow-third-party"]).allow_third_party);
}

#[test]
fn attach_is_repeatable_and_keeps_the_order_given() {
    let d = start(&[
        "--attach",
        "notes.md",
        "--attach",
        "dir/data.csv",
        "--attach",
        "notes.md",
    ]);
    assert_eq!(d.attach, ["notes.md", "dir/data.csv", "notes.md"]);
    usage(&["Fix it", "--s3-only", "--direct", "--attach"]);
}

#[test]
fn all_of_them_together_render_back_to_the_same_argv() {
    let d = start(&[
        "--goal",
        "off",
        "--no-research",
        "--allow-third-party",
        "--attach",
        "a.md",
        "--attach",
        "b.md",
    ]);
    assert_eq!(d.goal, Some(false));
    assert_eq!(d.research, Some(false));
    assert!(d.allow_third_party);
    assert_eq!(d.attach, ["a.md", "b.md"]);
}

#[test]
fn they_are_options_of_a_direct_run_and_are_refused_without_direct() {
    for flag in [
        vec!["--goal", "off"],
        vec!["--research"],
        vec!["--no-research"],
        vec!["--allow-third-party"],
        vec!["--attach", "a.md"],
    ] {
        let mut args = vec!["Fix it", "--s3-only"];
        args.extend(flag.iter().copied());
        let e = usage(&args);
        assert!(
            e.contains(flag[0]) && e.contains("--direct"),
            "{flag:?}: {e}"
        );
    }
}

#[test]
fn a_follow_up_takes_them_too() {
    let d = follow_up(&[
        "--goal",
        "off",
        "--research",
        "--allow-third-party",
        "--attach",
        "x.md",
    ]);
    assert_eq!(d.goal, Some(false));
    assert_eq!(d.research, Some(true));
    assert!(d.allow_third_party);
    assert_eq!(d.attach, ["x.md"]);
}

// ---- the request ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_start_that_uses_none_of_them_is_the_request_it_always_was() {
    let req = request_of(&start(&[]), DirectInputs::default());
    assert_eq!(req.goal, None);
    assert_eq!(req.research, None);
    assert_eq!(req.third_party, None);
    assert!(req.attachments.is_empty());
    let wire = serde_json::to_string(&req).unwrap();
    for member in ["goal", "research", "thirdParty", "attachments"] {
        assert!(!wire.contains(member), "{member} in {wire}");
    }
}

#[test]
fn the_options_become_members_of_the_request() {
    let d = start(&["--goal", "off", "--no-research", "--allow-third-party"]);
    let req = request_of(&d, DirectInputs::default());
    assert_eq!(req.goal, Some(GoalOptions::OFF));
    assert_eq!(req.research, Some(ResearchOptions { enabled: false }));
    assert_eq!(req.third_party, Some(ThirdPartyMode::Allow));
    let wire = serde_json::to_value(&req).unwrap();
    assert_eq!(wire["goal"], serde_json::json!({"enabled": false}));
    assert_eq!(wire["research"], serde_json::json!({"enabled": false}));
    assert_eq!(wire["thirdParty"], "allow");
}

#[test]
fn on_says_the_stage_default_out_loud() {
    let d = start(&["--goal", "on", "--research"]);
    let req = request_of(&d, DirectInputs::default());
    assert_eq!(req.goal, Some(GoalOptions::switch(true)));
    assert_eq!(req.research, Some(ResearchOptions { enabled: true }));
}

#[test]
fn the_request_carries_the_attachments_the_client_read() {
    let items = vec![Attachment::new("notes.md", "the notes\n")];
    let inputs = DirectInputs {
        attachments: items.clone(),
        ..DirectInputs::default()
    };
    let req = request_of(&start(&["--attach", "notes.md"]), inputs);
    assert_eq!(req.attachments, items);
}
