//! The Turn item records the output-token cap dispatch resolved for the turn
//! (`TurnItem.maxOutputTokens`), so a catalog change never changes a running agent silently and a
//! retried dispatch keeps the first attempt's cap. The field is optional on the wire: a Turn item
//! written before 1.24.0 loads, and an item that sets none keeps its 1.22.0 bytes.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::export::all_schemas;
use harness_protocol::store::TurnItem;
use serde_json::{Value, json};

fn old_turn_item() -> Value {
    json!({
        "runId": "01JABCDEFGHJKMNPQRSTVWXYZ0", "agentId": "t1-a1", "n": 2,
        "taskTokenCiphertext": "AQIDBA==", "startedAt": "2026-09-30T12:00:00Z",
        "rulesDigest": "d".repeat(64), "version": 3
    })
}

#[test]
fn the_version_is_at_least_the_h_feedback2_placeholder() {
    let parts: Vec<u32> = PROTOCOL_VERSION
        .split('.')
        .map(|p| p.parse().unwrap())
        .collect();
    assert!(parts[..] >= [1, 24, 0][..], "{PROTOCOL_VERSION}");
}

#[test]
fn an_older_turn_item_loads_without_a_cap_and_is_written_back_byte_for_byte() {
    let old: TurnItem = serde_json::from_value(old_turn_item()).unwrap();
    assert_eq!(old.max_output_tokens, None);
    assert_eq!(serde_json::to_value(&old).unwrap(), old_turn_item());
}

#[test]
fn the_resolved_cap_rides_beside_the_old_fields() {
    let mut t: TurnItem = serde_json::from_value(old_turn_item()).unwrap();
    t.max_output_tokens = Some(32_000);
    let j = serde_json::to_value(&t).unwrap();
    assert_eq!(j["maxOutputTokens"], 32_000);
    let back: TurnItem = serde_json::from_value(j).unwrap();
    assert_eq!(back, t);
}

#[test]
fn the_items_schema_names_the_cap_as_an_optional_whole_number() {
    let schemas = all_schemas();
    let items = &schemas
        .iter()
        .find(|(n, _)| n.starts_with("items"))
        .unwrap()
        .1;
    let turn = &items["$defs"]["TurnItem"];
    let cap = &turn["properties"]["maxOutputTokens"];
    assert!(cap.is_object(), "{turn}");
    let required = turn["required"].as_array().unwrap();
    assert!(
        !required.iter().any(|r| r == "maxOutputTokens"),
        "{required:?}"
    );
}
