//! The compute choice on the wire: the `compute` member of the start request, the words of the setting, and the two
//! members of the Agent item that record where a run's session lives. The member is additive: a request that leaves it out is byte for byte
//! the request of protocol 1.44.0, and the request type still refuses every member and every word it does not know.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{COMPUTE_WORDS, ComputeMode, ComputeOptions};
use harness_protocol::store::AgentItem;
use serde_json::{Value, json};

/// A request with no optional member: the serialization of protocol 1.44.0.
const MINIMAL: &str = r#"{"prompt":"p","target":{"mode":"s3-only"},"model":"m","rules":[]}"#;

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";

fn with_member(member: Value) -> Value {
    let mut v: Value = serde_json::from_str(MINIMAL).unwrap();
    v["compute"] = member;
    v
}

fn agent_json() -> Value {
    json!({
        "runId": RUN, "agentId": "direct", "taskId": "root", "attempt": 1,
        "runtimeArn": "arn:aws:bedrock-agentcore:us-east-1:123456789012:runtime/hamh_direct_harness_worker_python_instances-AbCdEfGhIj",
        "runtimeSessionId": format!("{RUN}-direct-g1"), "generation": 1, "engine": "cline",
        "engineVersion": "3.0.71", "transport": "acp", "modelId": "m", "pool": "direct", "status": "running",
        "version": 1
    })
}

#[test]
fn the_protocol_version_is_1_45_0_or_later() {
    let mut parts = PROTOCOL_VERSION
        .split('.')
        .map(|p| p.parse::<u32>().unwrap());
    let (major, minor) = (parts.next().unwrap(), parts.next().unwrap());
    assert!(major == 1 && minor >= 45, "{PROTOCOL_VERSION}");
}

#[test]
fn the_three_words_parse_and_print() {
    assert_eq!(COMPUTE_WORDS, ["auto", "microvm", "instances"]);
    for (word, mode) in [
        ("auto", ComputeMode::Auto),
        ("microvm", ComputeMode::Microvm),
        ("instances", ComputeMode::Instances),
    ] {
        assert_eq!(ComputeMode::parse(word), Some(mode));
        assert_eq!(mode.word(), word);
    }
    for bad in ["gpu", "", "Instances", "MICROVM", " auto", "instance"] {
        assert_eq!(ComputeMode::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn a_request_without_the_member_serializes_as_1_44_0() {
    let r: StartDirectRunRequest = serde_json::from_str(MINIMAL).unwrap();
    assert!(r.compute.is_none());
    assert_eq!(serde_json::to_string(&r).unwrap(), MINIMAL);
    assert!(!MINIMAL.contains("compute"));
}

#[test]
fn the_member_round_trips_in_camel_case() {
    for word in COMPUTE_WORDS {
        let v = with_member(json!({ "mode": word }));
        let r: StartDirectRunRequest = serde_json::from_value(v.clone()).unwrap();
        let mode = ComputeMode::parse(word).unwrap();
        assert_eq!(r.compute, Some(ComputeOptions { mode }));
        assert_eq!(serde_json::to_value(&r).unwrap(), v);
    }
}

#[test]
fn an_unknown_word_member_or_shape_is_refused() {
    for bad in [
        json!({"mode": "gpu"}),
        json!({"mode": "Instances"}),
        json!({"mode": ""}),
        json!({"mode": 3}),
        json!({}),
        json!({"mode": "instances", "heavyFiles": ["Cargo.toml"]}),
        json!("instances"),
    ] {
        let v = with_member(bad.clone());
        assert!(
            serde_json::from_value::<StartDirectRunRequest>(v).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn the_agent_item_members_default_and_are_left_out_when_unset() {
    let a: AgentItem = serde_json::from_value(agent_json()).unwrap();
    assert_eq!(a.compute, None);
    assert_eq!(a.capacity_provider_id, None);
    assert_eq!(serde_json::to_value(&a).unwrap(), agent_json());

    let mut v = agent_json();
    v["compute"] = json!("instances");
    v["capacityProviderId"] = json!("hamh_direct_instances-AbCdEfGhIj");
    let a: AgentItem = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(a.compute.as_deref(), Some("instances"));
    assert_eq!(
        a.capacity_provider_id.as_deref(),
        Some("hamh_direct_instances-AbCdEfGhIj")
    );
    assert_eq!(serde_json::to_value(&a).unwrap(), v);
}
