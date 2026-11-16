//! The worker looks at the work tree for a copy of the run's work every 15 seconds
//! unless the run names another interval, and a run may name 15 to 3,600 seconds (60 to 3,600 before). A session that is stopped from outside
//! or dies saves nothing afterwards: its work tree stops answering before the signal arrives. What it loses is what was written after the newest
//! stored copy, so the age of that copy is the one thing the worker can shorten.
//!
//! The wire format of every member is what it was: the start request's `snapshotEveryS` and the invocation's member of the same name take
//! smaller numbers. A stage of an older release answers 400 to a number below 60.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::direct::{
    DEFAULT_SNAPSHOT_EVERY_S, MAX_SNAPSHOT_EVERY_S, MIN_SNAPSHOT_EVERY_S, snapshot_every_s_ok,
};

#[test]
fn b823_look_the_protocol_is_1_61_0() {
    assert_eq!(PROTOCOL_VERSION, "1.61.0");
    let docs: std::collections::BTreeMap<&str, serde_json::Value> =
        harness_protocol::export::all_schemas()
            .into_iter()
            .collect();
    assert_eq!(docs["openapi-v1.json"]["info"]["version"], "1.61.0");
    assert_eq!(docs["openapi-internal.json"]["info"]["version"], "1.61.0");
}

#[test]
fn b823_look_the_timer_is_fifteen_seconds_unless_the_run_names_another() {
    assert_eq!(DEFAULT_SNAPSHOT_EVERY_S, 15);
}

#[test]
fn b823_look_a_run_may_name_fifteen_seconds_to_an_hour() {
    assert_eq!((MIN_SNAPSHOT_EVERY_S, MAX_SNAPSHOT_EVERY_S), (15, 3_600));
    for ok in [15, 16, 30, 59, 60, 600, 3_600] {
        assert!(snapshot_every_s_ok(ok), "{ok}");
    }
    for bad in [0, 1, 14, 3_601, u32::MAX] {
        assert!(!snapshot_every_s_ok(bad), "{bad}");
    }
}

#[test]
fn b823_look_the_contract_file_names_the_range_and_the_key() {
    let docs: std::collections::BTreeMap<&str, serde_json::Value> =
        harness_protocol::export::all_schemas()
            .into_iter()
            .collect();
    let text = docs["openapi-v1.json"].to_string();
    assert!(
        text.contains("run.copy_every_seconds"),
        "the description of snapshotEveryS names the key"
    );
    assert!(text.contains("15 seconds"), "and the default");
}
