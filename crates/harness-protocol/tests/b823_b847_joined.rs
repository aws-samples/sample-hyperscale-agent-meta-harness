//! The two wire changes of protocol 1.61.0 in one tree: a look for changes every 15 seconds
//! and how a run delivers. Each was built on its own. These
//! tests hold what an invocation may carry when both are in force, and what a worker and a service of different releases read from each
//! other: the facts the pages on a rollback state.
#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::direct::{
    DEFAULT_SNAPSHOT_EVERY_S, DirectDelivery, DirectInvocationRequest, MIN_SNAPSHOT_EVERY_S,
    snapshot_every_s_ok, validate_direct_invocation,
};
use harness_protocol::export::all_schemas;
use serde_json::{Value, json};

const RUN: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const BUCKET: &str = "hamh-direct-harness-111122223333";
const REPO: &str = "cc:hamh-e2e-python-calc";

/// An invocation with neither member: what a service that predates both sends.
fn invocation() -> Value {
    json!({
        "kind": "direct", "runId": RUN, "agentId": "direct", "turn": 1,
        "runtimeSessionId": format!("{RUN}-direct-g00000001"),
        "credentials": {"Version": 1, "AccessKeyId": "ASIAEXAMPLEEXAMPLE", "SecretAccessKey": "s", "SessionToken": "t",
                        "Expiration": "2026-11-03T18:10:00Z"},
        "nonce": "n", "internalApiUrl": "https://abc.execute-api.us-east-1.amazonaws.com/direct", "region": "us-east-1",
        "engine": "cline",
        "model": {"id": "us.anthropic.claude-sonnet-5-5", "region": "us-east-1", "contextTokens": 200000, "maxOutputTokens": 64000,
                  "protocol": "converse"},
        "request": {"uri": format!("s3://{BUCKET}/runs/{RUN}/input/request.json"), "sha256": "ab".repeat(32)},
        "repo": {"kind": "s3Bundle", "uri": format!("s3://{BUCKET}/runs/{RUN}/input/repo.bundle")},
        "timeoutS": 600
    })
}

fn with(members: Value) -> Value {
    let mut v = invocation();
    for (k, m) in members.as_object().unwrap() {
        v[k] = m.clone();
    }
    v
}

fn parse(v: Value) -> Result<DirectInvocationRequest, serde_json::Error> {
    serde_json::from_value(v)
}

/// One version, 1.61.0, carried both changes; the version asserted below is the one this tree is at.
#[test]
fn b823_b847_one_protocol_version_carries_the_look_every_15_seconds_and_the_delivery() {
    assert_eq!(PROTOCOL_VERSION, "1.62.0");
    assert_eq!((MIN_SNAPSHOT_EVERY_S, DEFAULT_SNAPSHOT_EVERY_S), (15, 15));
    let docs: std::collections::BTreeMap<&str, Value> = all_schemas().into_iter().collect();
    assert_eq!(docs["openapi-v1.json"]["info"]["version"], "1.62.0");
    assert_eq!(docs["openapi-internal.json"]["info"]["version"], "1.62.0");
    let invocation = &docs["runtime.schema.json"]["$defs"]["DirectInvocationRequest"]["properties"];
    assert!(
        invocation.get("delivery").is_some(),
        "the invocation of the runtime schema has the member delivery"
    );
    assert_eq!(
        invocation["snapshotEveryS"]["type"],
        json!(["integer", "null"]),
        "the invocation of the runtime schema has the member snapshotEveryS: {}",
        invocation["snapshotEveryS"]
    );
    // The schema gives the member its type and no range in numbers, because 0 (no copies) is a value of an invocation too. The range is the
    // validator's: it starts at 15 and ends at 3,600.
    for (seconds, taken) in [
        (0, true),
        (1, false),
        (14, false),
        (15, true),
        (3_600, true),
        (3_601, false),
    ] {
        let req = parse(with(json!({"snapshotEveryS": seconds}))).unwrap();
        assert_eq!(
            validate_direct_invocation(&req).is_ok(),
            taken,
            "an invocation that names {seconds} seconds"
        );
    }
    let start = docs["openapi-v1.json"]["components"]["schemas"]["StartDirectRunRequest"]
        ["properties"]["snapshotEveryS"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(
        start.contains("15 seconds"),
        "the start request of the public contract names the 15 seconds: {start}"
    );
}

/// The invocation of a run that asked for a copy every 15 seconds, for each kind of delivery: the worker takes both members together.
#[test]
fn b823_b847_an_invocation_with_a_look_every_15_seconds_and_a_delivery_is_valid() {
    assert!(snapshot_every_s_ok(15));
    for (delivery, typed) in [
        (json!({"kind": "zip"}), DirectDelivery::zip()),
        (
            json!({"kind": "pull-request", "repo": REPO}),
            DirectDelivery::pull_request(REPO),
        ),
    ] {
        let req = parse(with(
            json!({"snapshotEveryS": 15, "delivery": delivery.clone()}),
        ))
        .unwrap();
        assert_eq!(validate_direct_invocation(&req), Ok(()), "{delivery}");
        assert_eq!(req.snapshot_every_s, Some(15));
        assert_eq!(req.delivery, Some(typed));
        let wire = serde_json::to_value(&req).unwrap();
        assert_eq!(wire["snapshotEveryS"], json!(15));
        assert_eq!(wire["delivery"], delivery);
    }
}

/// A service of the release before this one names 60 seconds in every invocation and does not know the delivery. A worker of this
/// version reads that invocation: it copies on the interval named, and the delivery is unknown.
#[test]
fn b823_b847_a_worker_reads_the_invocation_of_the_service_of_the_release_before() {
    let req = parse(with(json!({"snapshotEveryS": 60}))).unwrap();
    assert_eq!(validate_direct_invocation(&req), Ok(()));
    assert_eq!(req.snapshot_every_s, Some(60));
    assert_eq!(req.delivery, None, "absent means unknown");
}

/// A service that predates both members names neither. A worker of this version reads that invocation too: the interval is its own
/// default and the delivery is unknown.
#[test]
fn b823_b847_a_worker_reads_an_invocation_that_names_neither_member() {
    let req = parse(invocation()).unwrap();
    assert_eq!(validate_direct_invocation(&req), Ok(()));
    assert_eq!(
        req.snapshot_every_s, None,
        "absent means the worker's default"
    );
    assert_eq!(req.delivery, None, "absent means unknown");
    let wire = serde_json::to_value(&req).unwrap();
    assert!(
        wire.get("snapshotEveryS").is_none() && wire.get("delivery").is_none(),
        "{wire}"
    );
}

/// The other direction. The parser of an invocation takes no member it does not know, in this version as in the ones before it. So a
/// worker that predates `delivery` refuses the invocation of this version's service, whether or not the invocation names an interval:
/// leaving `snapshotEveryS` out does not make an invocation one an earlier worker reads. The service and the worker image roll back
/// together.
#[test]
fn b823_b847_the_parser_of_an_invocation_takes_no_member_it_does_not_know() {
    let e = parse(with(json!({"aMemberOfALaterRelease": {"kind": "zip"}})))
        .expect_err("a member the parser does not know is refused");
    assert!(e.to_string().contains("unknown field"), "{e}");
    let schema: std::collections::BTreeMap<&str, Value> = all_schemas().into_iter().collect();
    assert_eq!(
        schema["runtime.schema.json"]["$defs"]["DirectInvocationRequest"]["additionalProperties"],
        json!(false),
        "the runtime schema says the same"
    );
}
