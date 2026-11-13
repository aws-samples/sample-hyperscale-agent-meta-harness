//! The compute setting on the client: the key `run.compute`, the flag `--compute` and the request member they make.
//! `ham` reads the file and the flag and sends the member; the stage decides what `auto` means.
//!
//! - `run.compute` is `auto` (the default), `microvm` or `instances`, any case in the file and in `ham config set`.
//! - `--compute <word>` sets it for one run and wins over the file.
//! - `auto` and no setting send no member, so the request is byte for byte the request of an older client; `microvm` and `instances` send
//!   `{"mode": <word>}`.
#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, DirectArgs, parse};
use ham_core::config::{
    Config, KEYS, compute_mode, get_key, parse_config, read_config, render_config, set_key,
    unset_key,
};
use ham_core::config_write::plan_set;
use ham_core::request::{DirectInputs, Origin, resolve_run_settings};
use ham_core::settings::{Applies, DefaultValue, Group, Key, Kind, Order, ROWS};
use harness_protocol::direct::{COMPUTE_WORDS, ComputeMode, ComputeOptions};
use serde_json::Value;

const PROMPT: &str = "Add a percent_change(a, b) function";

fn args_with(extra: &[&str]) -> DirectArgs {
    let mut argv = vec![PROMPT, "--s3-only", "--direct"];
    argv.extend_from_slice(extra);
    let Command::StartDirect(a) = parse(&argv).unwrap().command else {
        panic!("not a direct start");
    };
    a
}

fn env(_: &str) -> Option<String> {
    None
}

fn with(word: &str) -> Config {
    set_key(&Config::default(), "run.compute", word).unwrap()
}

fn request_json(cfg: &Config, extra: &[&str]) -> Value {
    let resolved = resolve_run_settings(cfg, &env, &args_with(extra), false).unwrap();
    serde_json::to_value(resolved.request(DirectInputs::default()).unwrap()).unwrap()
}

// ---- the registry row ---------------------------------------------------------------------------------------------------------

#[test]
fn the_registry_has_one_row_for_it_after_the_review_round_row() {
    assert_eq!(Key::ALL.len(), 46);
    assert_eq!(ROWS.len(), 46);
    assert_eq!(KEYS.len(), 46);
    assert_eq!(Key::ALL[23], Key::RunReviewRound);
    assert_eq!(Key::ALL[24], Key::RunCompute);
    let row = Key::RunCompute.row();
    assert_eq!(row.name, "run.compute");
    assert_eq!(row.file, ("run", "compute"));
    assert_eq!(row.kind, Kind::Compute);
    assert_eq!(row.default_text, "auto");
    assert_eq!(row.default, DefaultValue::Absent);
    assert_eq!(row.env, "");
    assert_eq!(row.flag, "--compute auto|microvm|instances");
    assert_eq!(row.order, Order::FlagFile);
    assert_eq!(
        row.group,
        Group::Limits,
        "an advanced setting of a run, beside the review round"
    );
    assert_eq!(row.since, 1);
    assert_eq!(row.applies, Applies::NextRun);
    assert_eq!(row.wire, Some("compute"));
    assert!(!row.security);
    assert!(KEYS.contains(&("run.compute", "run.compute")));
    assert_eq!(Key::lookup("run.compute"), Some(Key::RunCompute));
    for word in ["auto", "microvm", "instances"] {
        assert!(
            row.help.contains(word),
            "the help names {word}: {}",
            row.help
        );
    }
    assert_eq!(Kind::Compute.type_text(), "auto, microvm or instances");
    assert_eq!(Kind::Compute.word(), "compute");
}

#[test]
fn the_reference_table_lists_the_key_with_its_default_and_its_type() {
    let table = ham_core::settings::reference_markdown();
    let line = table
        .lines()
        .find(|l| l.starts_with("| `run.compute` |"))
        .unwrap_or_else(|| panic!("no row in {table}"));
    assert!(line.contains("auto, microvm or instances"), "{line}");
    assert!(line.contains("| auto |"), "{line}");
}

// ---- the three words ----------------------------------------------------------------------------------------------------------

#[test]
fn the_three_words_are_the_values_and_nothing_else_is() {
    for word in COMPUTE_WORDS {
        assert_eq!(
            get_key(&with(word), "run.compute").unwrap().as_deref(),
            Some(word)
        );
    }
    for (given, stored) in [
        ("AUTO", "auto"),
        (" MicroVM ", "microvm"),
        ("iNsTaNcEs", "instances"),
    ] {
        assert_eq!(
            get_key(&with(given), "run.compute").unwrap().as_deref(),
            Some(stored)
        );
    }
    for bad in [
        "",
        "gpu",
        "ec2",
        "vm",
        "true",
        "1",
        "auto microvm",
        "instance",
    ] {
        let e = set_key(&Config::default(), "run.compute", bad)
            .expect_err(&format!("{bad:?} is not a mode"))
            .to_string();
        assert!(
            e.contains("auto, microvm or instances"),
            "the refusal names the three values: {e}"
        );
    }
}

#[test]
fn the_default_is_auto_and_unset_goes_back_to_it() {
    assert_eq!(compute_mode(&Config::default()), ComputeMode::Auto);
    assert_eq!(compute_mode(&with("instances")), ComputeMode::Instances);
    assert_eq!(compute_mode(&with("microvm")), ComputeMode::Microvm);
    let back = unset_key(&with("instances"), "run.compute").unwrap();
    assert_eq!(get_key(&back, "run.compute").unwrap(), None);
    assert_eq!(compute_mode(&back), ComputeMode::Auto);
}

#[test]
fn the_file_reads_it_from_the_run_table_and_refuses_another_value() {
    let ok = read_config("configVersion = 1\n\n[run]\ncompute = \"instances\"\n");
    assert!(!ok.findings.has_errors(), "{:?}", ok.findings);
    assert_eq!(compute_mode(&ok.config), ComputeMode::Instances);
    for bad in [
        "\"gpu\"",
        "\"ec2\"",
        "\"Instances\"",
        "true",
        "1",
        "[\"auto\"]",
    ] {
        let r = read_config(&format!("configVersion = 1\n\n[run]\ncompute = {bad}\n"));
        assert!(r.findings.has_errors(), "{bad}");
        let text = r.render_errors();
        assert!(
            text.contains("auto") && text.contains("microvm") && text.contains("instances"),
            "{bad}: {text}"
        );
        assert_eq!(compute_mode(&r.config), ComputeMode::Auto, "{bad}");
        assert!(r.run_blocked(), "a file with an error starts no run");
    }
    // The key lives in [run]; at the top level of the file it is an unknown key.
    let top = read_config("configVersion = 1\ncompute = \"instances\"\n");
    assert!(!top.findings.is_empty());
    assert_eq!(compute_mode(&top.config), ComputeMode::Auto);
}

#[test]
fn a_set_writes_the_word_under_run_and_changes_only_its_line() {
    let written = render_config(&with("instances"));
    assert!(written.contains("[run]"), "{written}");
    assert!(written.contains("compute = \"instances\""), "{written}");
    assert_eq!(parse_config(&written).unwrap(), with("instances"));
    let before = "# my settings\nconfigVersion = 1\n\n[run]\ngoal = true\n";
    let plan = plan_set(before, "run.compute", "microvm").unwrap();
    let after = plan.text.unwrap();
    assert!(after.starts_with("# my settings\n"), "{after}");
    assert!(after.contains("goal = true"), "{after}");
    assert!(after.contains("compute = \"microvm\""), "{after}");
    assert_eq!(
        compute_mode(&parse_config(&after).unwrap()),
        ComputeMode::Microvm
    );
}

// ---- the flag -----------------------------------------------------------------------------------------------------------------

#[test]
fn the_flag_takes_the_three_words_and_nothing_else() {
    assert_eq!(args_with(&[]).compute, None);
    for word in COMPUTE_WORDS {
        assert_eq!(
            args_with(&["--compute", word]).compute,
            ComputeMode::parse(word),
            "{word}"
        );
    }
    for bad in ["gpu", "Instances", ""] {
        let e = parse(&[PROMPT, "--s3-only", "--direct", "--compute", bad])
            .map(|_| ())
            .expect_err(bad)
            .to_string();
        assert!(
            e.contains("--compute") && e.contains("auto, microvm or instances"),
            "{bad}: {e}"
        );
    }
    let twice = parse(&[
        PROMPT,
        "--s3-only",
        "--direct",
        "--compute",
        "microvm",
        "--compute",
        "instances",
    ])
    .map(|_| ())
    .unwrap_err()
    .to_string();
    assert!(twice.contains("--compute is given twice"), "{twice}");
    let no_direct = parse(&[PROMPT, "--s3-only", "--compute", "instances"])
        .map(|_| ())
        .unwrap_err()
        .to_string();
    assert!(
        no_direct.contains("--compute is an option of a direct run"),
        "{no_direct}"
    );
}

#[test]
fn the_flag_overrides_the_file_and_the_origin_says_so() {
    let none = resolve_run_settings(&Config::default(), &env, &args_with(&[]), false).unwrap();
    assert_eq!(none.origin(Key::RunCompute), Some(Origin::Default));
    assert_eq!(none.compute, None);
    let file = resolve_run_settings(&with("microvm"), &env, &args_with(&[]), false).unwrap();
    assert_eq!(file.origin(Key::RunCompute), Some(Origin::File));
    assert_eq!(file.compute, Some(ComputeMode::Microvm));
    let flag = resolve_run_settings(
        &with("microvm"),
        &env,
        &args_with(&["--compute", "instances"]),
        false,
    )
    .unwrap();
    assert_eq!(flag.origin(Key::RunCompute), Some(Origin::Flag));
    assert_eq!(flag.compute, Some(ComputeMode::Instances));
}

// ---- the request member -------------------------------------------------------------------------------------------------------

#[test]
fn unset_and_auto_send_no_member_so_the_request_is_the_older_one() {
    assert!(
        request_json(&Config::default(), &[])
            .get("compute")
            .is_none()
    );
    assert!(request_json(&with("auto"), &[]).get("compute").is_none());
    assert!(
        request_json(&Config::default(), &["--compute", "auto"])
            .get("compute")
            .is_none()
    );
    // The flag `auto` beats a file that says `instances`: the user asked for the stage's choice for this run.
    assert!(
        request_json(&with("instances"), &["--compute", "auto"])
            .get("compute")
            .is_none()
    );
}

#[test]
fn microvm_and_instances_reach_the_request() {
    for word in ["microvm", "instances"] {
        let v = request_json(&with(word), &[]);
        assert_eq!(v["compute"], serde_json::json!({ "mode": word }), "{v}");
        let sent: ComputeOptions = serde_json::from_value(v["compute"].clone()).unwrap();
        assert_eq!(
            Some(sent),
            ComputeOptions::for_mode(ComputeMode::parse(word).unwrap())
        );
    }
    let v = request_json(&with("microvm"), &["--compute", "instances"]);
    assert_eq!(
        v["compute"],
        serde_json::json!({ "mode": "instances" }),
        "{v}"
    );
}
