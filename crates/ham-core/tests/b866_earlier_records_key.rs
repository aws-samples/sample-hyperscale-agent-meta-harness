//! The client key `run.remove_earlier_records`. On (the default), a follow-up of a run that reported done starts without the
//! lifecycle records of that run: before the first prompt the harness takes them out of the work tree in a commit of its own, and they
//! stay in the history of the code and in the earlier run's zip. Off, the request of a follow-up carries `removeEarlierRecords: false`
//! and the records stay where they are. Only `false` reaches a request, and only a request that carries `continueFrom`: the member
//! means nothing in a first request, so a first request is the one an older client sends whatever the key says, and a stage of an
//! older release starts it. A user who keeps the default, or sets the key to on, sends the request an older client sends in a
//! follow-up too. The key can only keep the records: whether they go is decided by the service, from how the earlier run ended.
#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, parse};
use ham_core::config::{KEYS, parse_config};
use ham_core::request::{DirectInputs, Origin, resolve_run_settings};
use ham_core::settings::{DefaultValue, Group, Key, Kind, ROWS};
use harness_protocol::api::v1::StartDirectRunRequest;

fn resolved(config: &str, argv: &[&str]) -> ham_core::request::ResolvedRun {
    let args = match parse(argv).unwrap().command {
        Command::StartDirect(args) | Command::Continue(args, _) => args,
        other => panic!("not a start: {other:?}"),
    };
    let cfg = parse_config(config).unwrap();
    resolve_run_settings(&cfg, &|_| None, &args, false).unwrap()
}

const START: [&str; 3] = ["Add a health check", "--s3-only", "--direct"];

/// A first request with this config file.
fn request(config: &str) -> StartDirectRunRequest {
    resolved(config, &START)
        .request(DirectInputs::default())
        .unwrap()
}

/// `ham continue <runId> '<text>'` with this config file: the same start with `continueFrom`.
fn follow_up(config: &str) -> StartDirectRunRequest {
    let request = resolved(
        config,
        &["continue", SOURCE, "Now make it upper case", "--s3-only"],
    )
    .request(DirectInputs::default())
    .unwrap();
    assert_eq!(
        request.continue_from.as_ref().map(|c| c.run_id.as_str()),
        Some(SOURCE),
        "TRIGGER PRESENT: the request is a follow-up"
    );
    request
}

const OFF: &str = "configVersion = 1\n[run]\nremove_earlier_records = false\n";
const ON: &str = "configVersion = 1\n[run]\nremove_earlier_records = true\n";
const PLAIN: &str = "configVersion = 1\n";
/// The run a follow-up continues.
const SOURCE: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";

#[test]
fn b866_the_key_reaches_the_request_only_as_false() {
    // Off, in a follow-up: the member, as `false`.
    let off = follow_up(OFF);
    assert_eq!(off.remove_earlier_records, Some(false));
    assert_eq!(
        serde_json::to_value(&off).unwrap()["removeEarlierRecords"],
        false
    );
    // Unset and on: the request an older client sends, byte for byte.
    let plain = serde_json::to_vec(&follow_up(PLAIN)).unwrap();
    let on = serde_json::to_vec(&follow_up(ON)).unwrap();
    assert_eq!(plain, on, "on is the default: the request is the same");
    assert!(
        !String::from_utf8(on)
            .unwrap()
            .contains("removeEarlierRecords"),
        "no member for the default"
    );
    assert_eq!(follow_up(PLAIN).remove_earlier_records, None);
    assert_eq!(follow_up(ON).remove_earlier_records, None);
}

#[test]
fn b866_r2_a_first_request_never_carries_the_member() {
    // The member means nothing without `continueFrom`, and a stage that predates it refuses a request that holds it. So a first request
    // is the same request whatever the key says, and it starts on every stage.
    let plain = serde_json::to_vec(&request(PLAIN)).unwrap();
    for config in [OFF, ON] {
        let first = request(config);
        assert!(
            first.continue_from.is_none(),
            "TRIGGER PRESENT: a first request"
        );
        assert_eq!(first.remove_earlier_records, None, "{config:?}");
        assert_eq!(serde_json::to_vec(&first).unwrap(), plain, "{config:?}");
    }
    // TRIGGER PRESENT: the same file puts the member into a follow-up.
    assert_eq!(follow_up(OFF).remove_earlier_records, Some(false));
}

#[test]
fn b866_the_registry_has_the_key_as_a_switch_that_is_on_by_default() {
    let key = Key::lookup("run.remove_earlier_records").expect("the registry knows the key");
    assert_eq!(key, Key::RunRemoveEarlierRecords);
    let row = key.row();
    assert_eq!(row.file, ("run", "remove_earlier_records"));
    assert_eq!(row.name, "run.remove_earlier_records");
    assert_eq!(row.kind, Kind::Bool);
    assert_eq!(row.default, DefaultValue::Bool(true));
    assert_eq!(row.default_text, "on");
    assert_eq!(row.wire, Some("removeEarlierRecords"));
    assert_eq!(row.group, Group::Limits);
    assert_eq!(row.label, "Earlier records");
    assert_eq!(row.flag, "", "the key is set in the file");
    assert!(row.env.is_empty(), "no variable");
    assert!(!row.security);
    for words in [
        "a follow-up of a run that reported done (ham continue, or a follow-up typed on a finished run) starts without the lifecycle records of that run",
        "before the first prompt the harness takes the goal card, the run log, the red records and the research notes that runs wrote under .ham out of the work tree in a commit of its own",
        // Nothing is lost, and the help says where the records are.
        "They stay in the history of the code and in the earlier run's zip.",
        "Off, they stay where they are and the engine may take them for its own.",
        // The key can only keep the records: these cases keep them whatever it says.
        "A follow-up of a run that did not report done, ham resume, a records folder that .ham/records names and a file you committed yourself always keep them.",
    ] {
        assert!(
            row.help.contains(words),
            "{words:?} is not in: {}",
            row.help
        );
    }
    // The row is the last one, after the row of the update notice, which follows the switch of reading a stopped session's
    // storage back.
    assert_eq!(Key::ALL.len(), 52);
    assert_eq!(ROWS.len(), 52);
    assert_eq!(KEYS.len(), 52);
    assert_eq!(Key::ALL[Key::ALL.len() - 1], Key::RunRemoveEarlierRecords);
    assert_eq!(Key::ALL[Key::ALL.len() - 2], Key::UiUpdateNotice);
    assert_eq!(Key::ALL[Key::ALL.len() - 3], Key::RunRecoverAfterStop);
    assert_eq!(
        KEYS[KEYS.len() - 1],
        ("run.remove_earlier_records", "run.remove_earlier_records")
    );
}

#[test]
fn b866_a_follow_up_carries_the_member_beside_the_run_it_continues() {
    // `ham continue <runId> '<text>'` is the same start with `continueFrom`: the one request the key rides in.
    let argv = ["continue", SOURCE, "Now make it upper case", "--s3-only"];
    let off = resolved(OFF, &argv)
        .request(DirectInputs::default())
        .unwrap();
    assert_eq!(
        off.continue_from.as_ref().map(|c| c.run_id.as_str()),
        Some(SOURCE),
        "TRIGGER PRESENT: the request is a follow-up"
    );
    assert_eq!(off.remove_earlier_records, Some(false));
    let plain = resolved(PLAIN, &argv)
        .request(DirectInputs::default())
        .unwrap();
    assert!(plain.continue_from.is_some());
    assert_eq!(
        plain.remove_earlier_records, None,
        "the service decides for a user who keeps the default"
    );
}

#[test]
fn b866_the_resolved_run_says_where_the_value_came_from() {
    assert_eq!(
        resolved(OFF, &START).origin(Key::RunRemoveEarlierRecords),
        Some(Origin::File)
    );
    assert_eq!(
        resolved(PLAIN, &START).origin(Key::RunRemoveEarlierRecords),
        Some(Origin::Default)
    );
}

#[test]
fn b866_the_file_takes_the_key_only_in_its_table_and_as_a_boolean() {
    assert!(parse_config(OFF).is_ok());
    assert!(parse_config("configVersion = 1\n[run]\nremove_earlier_records = \"off\"\n").is_err());
    assert!(parse_config("configVersion = 1\nremove_earlier_records = false\n").is_err());
}
