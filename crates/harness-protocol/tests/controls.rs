//! Control items: a steer, cancel or approval answer queued for one agent, keyed so one
//! query reads an agent's controls in creation order. `/internal/events` hands each out once.
#![allow(clippy::unwrap_used)]

use harness_protocol::api::internal::PendingControl;
use harness_protocol::common::ApprovalDecision;
use harness_protocol::store::{ControlItem, Keyed, StoreItem, keys};
use serde_json::json;

const ID: &str = "01J8ZQ4Y6X3M2K7N9P5R8T1V0W";

fn item(control: PendingControl) -> ControlItem {
    ControlItem {
        run_id: "r1".into(),
        agent_id: "a1".into(),
        control_id: ID.into(),
        control,
        created_at: "2026-09-25T12:00:00Z".into(),
        delivered_at: None,
    }
}

#[test]
fn control_keys_group_an_agents_controls_in_creation_order() {
    assert_eq!(keys::control_prefix("a1"), "CTRL#a1#");
    assert_eq!(keys::control_sk("a1", ID), format!("CTRL#a1#{ID}"));
    // One agent's prefix never matches another agent whose id extends it.
    assert!(!keys::control_sk("a10", ID).starts_with(&keys::control_prefix("a1")));
    let k = item(PendingControl::Cancel).key();
    assert_eq!(k.pk, "RUN#r1");
    assert_eq!(k.sk, format!("CTRL#a1#{ID}"));
    // ULIDs sort by time, so the sort key orders an agent's controls by creation.
    let mut sks = [
        keys::control_sk("a1", "01J8ZQ4Y6X3M2K7N9P5R8T1V0X"),
        keys::control_sk("a1", ID),
    ];
    sks.sort();
    assert_eq!(sks[0], keys::control_sk("a1", ID));
}

#[test]
fn a_control_item_keeps_its_control_under_one_attribute() {
    let v = serde_json::to_value(item(PendingControl::Steer {
        text: "use pytest".into(),
        now: true,
    }))
    .unwrap();
    assert_eq!(
        v,
        json!({
            "runId": "r1",
            "agentId": "a1",
            "controlId": ID,
            "control": {"kind": "steer", "text": "use pytest", "now": true},
            "createdAt": "2026-09-25T12:00:00Z"
        })
    );
    let mut answered = item(PendingControl::ApprovalAnswer {
        approval_id: "ap-1".into(),
        decision: ApprovalDecision::Deny,
    });
    answered.delivered_at = Some("2026-09-25T12:00:03Z".into());
    let back: ControlItem =
        serde_json::from_value(serde_json::to_value(&answered).unwrap()).unwrap();
    assert_eq!(back, answered);
}

#[test]
fn control_items_are_store_items() {
    let s = StoreItem::Control(item(PendingControl::Cancel));
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v["entity"], "control");
    assert_eq!(v["control"], json!({"kind": "cancel"}));
    assert_eq!(s.key(), item(PendingControl::Cancel).key());
}
