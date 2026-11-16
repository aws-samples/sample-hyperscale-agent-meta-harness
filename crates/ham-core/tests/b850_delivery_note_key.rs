//! The client key `run.delivery_note`. On (the default), the harness tells the engine at the start of every session how the run
//! delivers. Off, the start request carries `deliveryNote: false` and the worker writes no block. Only `false` reaches a request: a user
//! who keeps the default, or sets the key to on, sends the request an older client sends, which a stage of an older release starts.
#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, parse};
use ham_core::config::parse_config;
use ham_core::request::{DirectInputs, Origin, resolve_run_settings};
use ham_core::settings::{DefaultValue, Group, Key, Kind};
use harness_protocol::api::v1::StartDirectRunRequest;

fn resolved(config: &str) -> ham_core::request::ResolvedRun {
    let Command::StartDirect(args) = parse(&["Add a workflow file", "--s3-only", "--direct"])
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
fn b850_the_registry_has_the_key_as_a_switch_that_is_on_by_default() {
    let key = Key::lookup("run.delivery_note").expect("the registry knows the key");
    assert_eq!(key, Key::RunDeliveryNote);
    let row = key.row();
    assert_eq!(row.file, ("run", "delivery_note"));
    assert_eq!(row.kind, Kind::Bool);
    assert_eq!(row.default, DefaultValue::Bool(true));
    assert_eq!(row.default_text, "on");
    assert_eq!(row.wire, Some("deliveryNote"));
    assert_eq!(row.group, Group::Limits);
    assert_eq!(row.flag, "", "the key is set in the file");
    assert!(!row.security);
    for words in [
        "how the run delivers",
        "Off",
        "the first thing it reads",
        // `ham resume` and `/retry` start the stored request of the run they come from (control-api `b850_delivery_note.rs`).
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
fn b850_off_reaches_the_request_as_delivery_note_false() {
    let r = request("configVersion = 1\n[run]\ndelivery_note = false\n");
    assert_eq!(r.delivery_note, Some(false));
    let body = serde_json::to_value(&r).unwrap();
    assert_eq!(body["deliveryNote"], false);
}

#[test]
fn b850_the_default_and_on_send_the_request_an_older_client_sends() {
    let plain = serde_json::to_vec(&request("configVersion = 1\n")).unwrap();
    let on =
        serde_json::to_vec(&request("configVersion = 1\n[run]\ndelivery_note = true\n")).unwrap();
    assert_eq!(plain, on, "on is the default: the request is the same");
    assert!(
        !String::from_utf8(on).unwrap().contains("deliveryNote"),
        "no member for the default"
    );
    assert_eq!(request("configVersion = 1\n").delivery_note, None);
}

#[test]
fn b850_the_resolved_run_says_where_the_value_came_from() {
    assert_eq!(
        resolved("configVersion = 1\n[run]\ndelivery_note = false\n").origin(Key::RunDeliveryNote),
        Some(Origin::File)
    );
    assert_eq!(
        resolved("configVersion = 1\n").origin(Key::RunDeliveryNote),
        Some(Origin::Default)
    );
}

#[test]
fn b850_the_file_takes_the_key_only_in_its_table_and_as_a_boolean() {
    assert!(parse_config("configVersion = 1\n[run]\ndelivery_note = false\n").is_ok());
    assert!(parse_config("configVersion = 1\n[run]\ndelivery_note = \"off\"\n").is_err());
}
