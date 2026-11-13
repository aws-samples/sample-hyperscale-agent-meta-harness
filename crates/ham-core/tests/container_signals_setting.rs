//! The setting `run.container_signals` on the client: the user's list of container signals, one line of entries separated by
//! semicolons (a signal or the word `default`), or the word `none`. It rides in the `compute` member of the request, also for `auto`, and a
//! user who never sets it sends the request of an older client.
#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, DirectArgs, parse};
use ham_core::config::{Config, KEYS, get_key, read_config, set_key, unset_key};
use ham_core::config_write::plan_set;
use ham_core::request::{DirectInputs, Origin, resolve_run_settings};
use ham_core::settings::{Applies, DefaultValue, Group, Key, Kind, Order, ROWS};
use serde_json::{Value, json};

const PROMPT: &str = "Add a percent_change(a, b) function";
const KEY: &str = "run.container_signals";

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

fn with(text: &str) -> Config {
    set_key(&Config::default(), KEY, text).unwrap()
}

fn request_json(cfg: &Config, extra: &[&str]) -> Value {
    let resolved = resolve_run_settings(cfg, &env, &args_with(extra), false).unwrap();
    serde_json::to_value(resolved.request(DirectInputs::default()).unwrap()).unwrap()
}

#[test]
fn b632_the_registry_has_the_row_as_its_last_key_with_the_default_list_and_the_compute_member() {
    // The default_engine row and the update_source row follow it now; the container signals row is the last of the run keys.
    let last = Key::ALL.len() - 3;
    assert_eq!(Key::ALL[last], Key::RunContainerSignals);
    assert_eq!(ROWS.len(), Key::ALL.len());
    assert_eq!(KEYS.len(), Key::ALL.len());
    let row = Key::RunContainerSignals.row();
    assert_eq!(row.name, KEY);
    assert_eq!(row.file, ("run", "container_signals"));
    assert_eq!(row.kind, Kind::Signals);
    assert_eq!(row.default_text, "default");
    assert_eq!(row.default, DefaultValue::Absent);
    assert_eq!(row.flag, "");
    assert_eq!(row.env, "");
    assert_eq!(row.order, Order::FileOnly);
    assert_eq!(row.group, Group::Limits);
    assert_eq!(row.since, 1);
    assert_eq!(row.applies, Applies::NextRun);
    assert_eq!(row.wire, Some("compute"));
    assert!(!row.security);
    assert_eq!(Key::lookup(KEY), Some(Key::RunContainerSignals));
    for word in ["microVM", "none", "default", "Instances"] {
        assert!(
            row.help.contains(word),
            "the help names {word}: {}",
            row.help
        );
    }
    assert_eq!(Kind::Signals.word(), "signals");
    assert!(Kind::Signals.type_text().contains("semicolons"));
}

#[test]
fn b632_the_reference_table_lists_the_key_with_its_default() {
    let table = ham_core::settings::reference_markdown();
    let line = table
        .lines()
        .find(|l| l.starts_with("| `run.container_signals` |"))
        .unwrap_or_else(|| panic!("no row in {table}"));
    assert!(line.contains("| default |"), "{line}");
}

#[test]
fn b632_set_stores_the_normal_form_and_refuses_what_is_not_a_list() {
    assert_eq!(
        get_key(&with("default;  Gemfile  contains   testcontainers "), KEY)
            .unwrap()
            .as_deref(),
        Some("default; Gemfile contains testcontainers")
    );
    assert_eq!(
        get_key(&with(" NONE "), KEY).unwrap().as_deref(),
        Some("none")
    );
    for bad in ["", " ; ", "pom.xml contains", "a b", "none; compose.yml"] {
        let e = set_key(&Config::default(), KEY, bad)
            .expect_err(&format!("{bad:?} is not a list"))
            .to_string();
        assert!(e.contains(KEY), "the refusal names the key: {e}");
    }
    let back = unset_key(&with("none"), KEY).unwrap();
    assert_eq!(get_key(&back, KEY).unwrap(), None);
}

#[test]
fn b632_the_file_reads_the_text_and_refuses_a_bad_one() {
    let ok = read_config(
        "configVersion = 1\n\n[run]\ncontainer_signals = \"Taskfile.yml contains docker\"\n",
    );
    assert!(!ok.findings.has_errors(), "{:?}", ok.findings);
    assert_eq!(
        get_key(&ok.config, KEY).unwrap().as_deref(),
        Some("Taskfile.yml contains docker")
    );
    for bad in [
        "\"pom.xml contains\"",
        "\"\"",
        "true",
        "1",
        "[\"compose.yml\"]",
    ] {
        let r = read_config(&format!(
            "configVersion = 1\n\n[run]\ncontainer_signals = {bad}\n"
        ));
        assert!(r.findings.has_errors(), "{bad}");
        assert_eq!(get_key(&r.config, KEY).unwrap(), None, "{bad}");
        assert!(r.run_blocked(), "a file with an error starts no run");
    }
}

#[test]
fn b632_a_set_writes_the_key_under_run_and_changes_only_its_line() {
    let before = "# my settings\nconfigVersion = 1\n\n[run]\ngoal = true\n";
    let plan = plan_set(before, KEY, "default; Gemfile contains testcontainers").unwrap();
    let after = plan.text.unwrap();
    assert!(after.starts_with("# my settings\n"), "{after}");
    assert!(after.contains("goal = true"), "{after}");
    assert!(
        after.contains("container_signals = \"default; Gemfile contains testcontainers\""),
        "{after}"
    );
}

#[test]
fn b632_auto_with_a_list_sends_the_compute_member_with_the_list() {
    let v = request_json(&with("default; Gemfile contains testcontainers"), &[]);
    assert_eq!(
        v["compute"],
        json!({"mode": "auto", "containerSignals": ["default", "Gemfile contains testcontainers"]}),
        "{v}"
    );
    let none = request_json(&with("none"), &[]);
    assert_eq!(
        none["compute"],
        json!({"mode": "auto", "containerSignals": []}),
        "{none}"
    );
}

#[test]
fn b632_a_run_with_no_list_sends_no_compute_member_as_before() {
    assert!(
        request_json(&Config::default(), &[])
            .get("compute")
            .is_none()
    );
    let instances = set_key(&Config::default(), "run.compute", "instances").unwrap();
    assert_eq!(
        request_json(&instances, &[])["compute"],
        json!({"mode": "instances"})
    );
}

#[test]
fn b632_the_mode_and_the_list_combine_whichever_layer_sets_the_mode() {
    let file = set_key(&with("compose.yml"), "run.compute", "instances").unwrap();
    assert_eq!(
        request_json(&file, &[])["compute"],
        json!({"mode": "instances", "containerSignals": ["compose.yml"]})
    );
    assert_eq!(
        request_json(&with("compose.yml"), &["--compute", "microvm"])["compute"],
        json!({"mode": "microvm", "containerSignals": ["compose.yml"]})
    );
    assert_eq!(
        request_json(&with("compose.yml"), &["--compute", "auto"])["compute"],
        json!({"mode": "auto", "containerSignals": ["compose.yml"]}),
        "the flag auto still carries the user's list"
    );
}

#[test]
fn b632_the_origin_says_file_when_the_list_is_set_and_default_when_it_is_not() {
    let none = resolve_run_settings(&Config::default(), &env, &args_with(&[]), false).unwrap();
    assert_eq!(none.origin(Key::RunContainerSignals), Some(Origin::Default));
    let set = resolve_run_settings(&with("none"), &env, &args_with(&[]), false).unwrap();
    assert_eq!(set.origin(Key::RunContainerSignals), Some(Origin::File));
}
