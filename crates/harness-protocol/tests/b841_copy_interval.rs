//! The copy interval of a run on the wire: the start request gains the optional member `snapshotEveryS`, the seconds between
//! two looks of the worker at the work tree for a copy of the run's work (the user's `run.copy_every_minutes`). It is additive and left off the wire
//! when the user keeps the default, so the request of a client that never sets the key is byte for byte what protocol 1.59.0 carried. The worker's
//! invocation has carried a member of the same name since the copies exist; run-session copies the request's value into it.
//!
//! A request may name 15 to 3,600 seconds. 0, which the invocation accepts and which switches the copies off while the engine works, is no value of a
//! request: a run always has a copy of its work in the bucket while it works (root invariant 9).

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::api::v1::StartDirectRunRequest;
use serde_json::{Value, json};

fn base() -> Value {
    json!({
        "prompt": "add a function", "target": {"mode": "s3-only"}, "model": "us.openai.gpt-6.1-sol", "rules": []
    })
}

fn parse(extra: Value) -> Result<StartDirectRunRequest, serde_json::Error> {
    let mut v = base();
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    serde_json::from_value(v)
}

#[test]
fn b841_copy_the_protocol_is_1_65_0() {
    assert_eq!(PROTOCOL_VERSION, "1.65.0");
}

#[test]
fn b841_copy_the_member_is_optional_and_left_off_the_wire_when_absent() {
    let plain = parse(json!({})).unwrap();
    let wire = serde_json::to_value(&plain).unwrap();
    assert!(wire.get("snapshotEveryS").is_none(), "{wire}");
    assert_eq!(
        wire,
        base(),
        "a request that keeps the default is the request of 1.59.0"
    );
}

#[test]
fn b841_copy_the_interval_round_trips_under_the_camel_case_name() {
    let set = parse(json!({"snapshotEveryS": 600}))
        .unwrap_or_else(|e| panic!("the start request takes snapshotEveryS: {e}"));
    let wire = serde_json::to_value(&set).unwrap();
    assert_eq!(wire["snapshotEveryS"], json!(600));
    let back: StartDirectRunRequest = serde_json::from_value(wire).unwrap();
    assert_eq!(back, set);
}

#[test]
fn b841_copy_a_value_that_is_no_whole_number_is_refused() {
    for bad in [
        json!("600"),
        json!(600.5),
        json!(-60),
        json!({"s": 600}),
        json!([600]),
        json!(true),
    ] {
        assert!(
            parse(json!({"snapshotEveryS": bad.clone()})).is_err(),
            "{bad}"
        );
    }
    assert!(
        parse(json!({"snapshot_every_s": 600})).is_err(),
        "the snake case spelling is an unknown member"
    );
}

#[test]
fn b841_copy_the_contract_file_describes_the_member() {
    let docs: std::collections::BTreeMap<&str, Value> = harness_protocol::export::all_schemas()
        .into_iter()
        .collect();
    let v1 = &docs["openapi-v1.json"];
    let req = &v1["components"]["schemas"]["StartDirectRunRequest"]["properties"];
    assert_eq!(
        req["snapshotEveryS"]["type"],
        json!(["integer", "null"]),
        "{}",
        req["snapshotEveryS"]
    );
    assert_eq!(v1["info"]["version"], "1.65.0");
    assert_eq!(docs["openapi-internal.json"]["info"]["version"], "1.65.0");
}
