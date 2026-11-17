//! The client key `run.recover_after_stop`. On (the default), the service reads the storage of a session that was stopped from
//! outside or stopped answering back before the next session starts, and a stop the worker hears of ends the wait for the session
//! early. Off, the start request carries `recoverAfterStop: false`: no read, no early end, the 600 seconds decide as before. Only `false`
//! reaches a request: a user who keeps the default, or sets the key to on, sends the request an older client sends, which a stage of an
//! older release starts.
#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, parse};
use ham_core::config::parse_config;
use ham_core::request::{DirectInputs, Origin, resolve_run_settings};
use ham_core::settings::{DefaultValue, Group, Key, Kind};
use harness_protocol::api::v1::StartDirectRunRequest;

fn resolved(config: &str) -> ham_core::request::ResolvedRun {
    let Command::StartDirect(args) = parse(&["Add a health check", "--s3-only", "--direct"])
        .unwrap()
        .command
    else {
        panic!("not a start");
    };
    let cfg = parse_config(config).unwrap();
    resolve_run_settings(&cfg, &|_| None, &args, false).unwrap()
}

fn request(config: &str) -> StartDirectRunRequest {
    resolved(config).request(DirectInputs::default()).unwrap()
}

#[test]
fn b854_the_registry_has_the_key_as_a_switch_that_is_on_by_default() {
    let key = Key::lookup("run.recover_after_stop").expect("the registry knows the key");
    assert_eq!(key, Key::RunRecoverAfterStop);
    let row = key.row();
    assert_eq!(row.file, ("run", "recover_after_stop"));
    assert_eq!(row.kind, Kind::Bool);
    assert_eq!(row.default, DefaultValue::Bool(true));
    assert_eq!(row.default_text, "on");
    assert_eq!(row.wire, Some("recoverAfterStop"));
    assert_eq!(row.group, Group::Limits);
    assert_eq!(row.flag, "", "the key is set in the file");
    assert!(!row.security);
    for words in [
        "reads that session's own storage back before the next session starts",
        "only when the lineage and content checks pass",
        // A check may refuse to use the files, it never hides them: the help names the command that downloads them.
        "when it does not, ham pull <runId> --read-back <n> downloads them",
        // On Instances compute the worker's notice does not shorten the wait, and no read is made.
        "A session on Instances compute keeps no storage of that kind: it gets no read, and its notice does not end the wait.",
        "Off, the service makes no read and does not act on that notice",
        "after 600 seconds",
        "The saved copies are kept either way.",
        // `ham resume` and `/retry` start the stored request of the run they come from.
        "A run that ham resume or /retry starts keeps the setting of the run it comes from.",
    ] {
        assert!(
            row.help.contains(words),
            "{words:?} is not in: {}",
            row.help
        );
    }
}

#[test]
fn b854_off_reaches_the_request_as_recover_after_stop_false() {
    let r = request("configVersion = 1\n[run]\nrecover_after_stop = false\n");
    assert_eq!(r.recover_after_stop, Some(false));
    let body = serde_json::to_value(&r).unwrap();
    assert_eq!(body["recoverAfterStop"], false);
}

#[test]
fn b854_the_default_and_on_send_the_request_an_older_client_sends() {
    let plain = serde_json::to_vec(&request("configVersion = 1\n")).unwrap();
    let on = serde_json::to_vec(&request(
        "configVersion = 1\n[run]\nrecover_after_stop = true\n",
    ))
    .unwrap();
    assert_eq!(plain, on, "on is the default: the request is the same");
    assert!(
        !String::from_utf8(on).unwrap().contains("recoverAfterStop"),
        "no member for the default"
    );
    assert_eq!(request("configVersion = 1\n").recover_after_stop, None);
}

#[test]
fn b854_the_resolved_run_says_where_the_value_came_from() {
    assert_eq!(
        resolved("configVersion = 1\n[run]\nrecover_after_stop = false\n")
            .origin(Key::RunRecoverAfterStop),
        Some(Origin::File)
    );
    assert_eq!(
        resolved("configVersion = 1\n").origin(Key::RunRecoverAfterStop),
        Some(Origin::Default)
    );
}

#[test]
fn b854_the_file_takes_the_key_only_in_its_table_and_as_a_boolean() {
    assert!(parse_config("configVersion = 1\n[run]\nrecover_after_stop = false\n").is_ok());
    assert!(parse_config("configVersion = 1\n[run]\nrecover_after_stop = \"off\"\n").is_err());
}
