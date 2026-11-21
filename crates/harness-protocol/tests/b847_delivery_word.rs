//! How a run delivers, on the wire. A run with the zip target left out a workflow file its prompt asked for: the rules
//! every run gets say that a pull request refuses such a file and that a zip takes it, and nothing told the engine which of the two the run was.
//! The invocation of a session gains the optional member `delivery`: `{"kind": "zip"}` for a run that delivers a zip alone, and
//! `{"kind": "pull-request", "repo": "cc:<name>"}` for a run that delivers a pull request, with the repository as the user named it. The worker
//! says it to the engine at the start of every session. Absent means unknown: the invocation of a service that predates the member.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::direct::{
    DirectDelivery, DirectDeliveryKind, DirectInvocationError, DirectInvocationRequest,
    MAX_DELIVERY_REPO_BYTES, REPOSITORY_REFUSES, validate_direct_invocation,
};
use serde_json::{Value, json};

const RUN: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const BUCKET: &str = "hamh-direct-harness-111122223333";

fn invocation(delivery: Value) -> Value {
    let mut v = json!({
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
        "timeoutS": 600, "snapshotEveryS": 600
    });
    if !delivery.is_null() {
        v["delivery"] = delivery;
    }
    v
}

fn parse(v: Value) -> DirectInvocationRequest {
    serde_json::from_value(v).unwrap()
}

#[test]
fn b847_the_protocol_is_1_64_0() {
    assert_eq!(PROTOCOL_VERSION, "1.64.0");
}

#[test]
fn b847_an_invocation_without_the_member_is_the_invocation_of_a_service_that_predates_it() {
    let req = parse(invocation(Value::Null));
    assert_eq!(req.delivery, None, "absent means unknown");
    assert_eq!(validate_direct_invocation(&req), Ok(()));
    let wire = serde_json::to_value(&req).unwrap();
    assert!(
        wire.get("delivery").is_none(),
        "an unknown delivery stays off the wire: {wire}"
    );
}

#[test]
fn b847_a_zip_only_run_says_zip_and_names_no_repository() {
    let req = parse(invocation(json!({"kind": "zip"})));
    assert_eq!(req.delivery, Some(DirectDelivery::zip()));
    assert_eq!(
        req.delivery.as_ref().map(|d| d.kind),
        Some(DirectDeliveryKind::Zip)
    );
    assert_eq!(validate_direct_invocation(&req), Ok(()));
    assert_eq!(
        serde_json::to_value(&req).unwrap()["delivery"],
        json!({"kind": "zip"})
    );
    let with_repo = parse(invocation(json!({"kind": "zip", "repo": "cc:demo"})));
    assert_eq!(
        validate_direct_invocation(&with_repo),
        Err(DirectInvocationError::BadDelivery),
        "a zip names no repository"
    );
}

#[test]
fn b847_a_pull_request_run_names_the_repository_as_the_user_named_it() {
    let req = parse(invocation(
        json!({"kind": "pull-request", "repo": "cc:demo-repo"}),
    ));
    assert_eq!(
        req.delivery,
        Some(DirectDelivery::pull_request("cc:demo-repo"))
    );
    assert_eq!(validate_direct_invocation(&req), Ok(()));
    assert_eq!(
        serde_json::to_value(&req).unwrap()["delivery"],
        json!({"kind": "pull-request", "repo": "cc:demo-repo"})
    );
}

/// The worker writes the repository into the first words the engine reads, so the check takes only a repository target: one line of the
/// characters a target has, and no longer than a target can be.
#[test]
fn b847_a_pull_request_without_a_repository_target_is_refused() {
    let long = format!("cc:{}", "a".repeat(MAX_DELIVERY_REPO_BYTES));
    for bad in [
        json!({"kind": "pull-request"}),
        json!({"kind": "pull-request", "repo": ""}),
        json!({"kind": "pull-request", "repo": "demo"}),
        json!({"kind": "pull-request", "repo": "s3-only"}),
        json!({"kind": "pull-request", "repo": "cc:two words"}),
        json!({"kind": "pull-request", "repo": "cc:demo\nand a second line"}),
        json!({"kind": "pull-request", "repo": long}),
    ] {
        let req = parse(invocation(bad.clone()));
        assert_eq!(
            validate_direct_invocation(&req),
            Err(DirectInvocationError::BadDelivery),
            "{bad}"
        );
    }
}

#[test]
fn b847_an_unknown_kind_or_member_is_refused_when_the_body_is_read() {
    for bad in [
        json!({"kind": "email"}),
        json!({"kind": "zip", "extra": 1}),
        json!({"repo": "cc:demo"}),
        json!("zip"),
    ] {
        assert!(
            serde_json::from_value::<DirectInvocationRequest>(invocation(bad.clone())).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn b847_the_error_names_the_rule_and_no_value() {
    let text = DirectInvocationError::BadDelivery.to_string();
    for needle in ["delivery", "zip", "pull-request", "cc:<name>"] {
        assert!(text.contains(needle), "{needle:?} in {text:?}");
    }
}

/// The two paths a delivery to a repository refuses and a zip takes: the ones the default rules name, the bundle validator protects and the
/// worker's words to the engine repeat. Each of those is tested against this list.
#[test]
fn b847_the_paths_a_repository_delivery_refuses_are_a_workflow_folder_and_the_root_build_specification()
 {
    assert_eq!(REPOSITORY_REFUSES, [".github/workflows/", "buildspec.yml"]);
}

#[test]
fn b847_the_contract_file_describes_the_member() {
    let docs: std::collections::BTreeMap<&str, Value> = harness_protocol::export::all_schemas()
        .into_iter()
        .collect();
    let text = docs["runtime.schema.json"].to_string();
    for needle in ["DirectDelivery", "DirectDeliveryKind", "pull-request"] {
        assert!(text.contains(needle), "{needle:?} in runtime.schema.json");
    }
    assert_eq!(docs["openapi-v1.json"]["info"]["version"], "1.64.0");
    assert_eq!(docs["openapi-internal.json"]["info"]["version"], "1.64.0");
}

#[test]
fn b847_the_debug_text_of_an_invocation_shows_the_delivery() {
    let req = parse(invocation(json!({"kind": "zip"})));
    assert!(format!("{req:?}").contains("delivery: Some("), "{req:?}");
}
