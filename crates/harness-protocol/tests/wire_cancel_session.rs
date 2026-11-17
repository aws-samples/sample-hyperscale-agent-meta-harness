//! Protocol 1.63.0: the wire members of a cancel that the service records and of the session facts the run list carries.
//!
//! * `RunItem.cancelRequestedAt`: the one durable record of the user's keep-the-work cancel. control-api writes it, the slice loop of
//!   run-session and worker-api read it. An item written before the member existed reads as a run that was never cancelled.
//! * `RunControlResponse.cancel` ([`CancelRecorded`]): what `ham cancel` is answered once the cancel is recorded.
//! * `RunSummary.cancelRequestedAt` and `RunSummary.sessionSilentSince`: what the rows of `ham runs` say about the session.
//! * `CANCEL_RECORDED_KIND`: the kind of the harness's `run.waiting` event that says the cancel is recorded.
//!
//! Every member is optional and left out when absent, so the 1.62.0 form of each type still reads and still writes byte for byte.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::api::v1::{CancelRecorded, RunControlResponse, RunSummary};
use harness_protocol::direct_input::{CANCEL_RECORDED_KIND, SESSION_LOST_KIND};
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::RunItem;
use serde_json::{Value, json};

const RUN: &str = "01M7HJNPAZVSDBPKFS6FB44ZY4";
const AT: &str = "2026-11-16T07:05:22Z";

/// A Run item as protocol 1.62.0 wrote it.
fn run_item_1_62() -> Value {
    json!({
        "runId": RUN, "ownerPrincipalArn": "arn:aws:sts::123456789012:assumed-role/Admin/me", "promptSha256": "cd".repeat(32),
        "target": {"mode": "s3-only"}, "status": "active", "phase": "green",
        "enginePolicyVersion": 0, "createdAt": "2026-11-16T07:00:00Z", "version": 3
    })
}

/// A row of the run list as protocol 1.62.0 wrote it.
fn summary_1_62() -> Value {
    json!({
        "runId": RUN, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
        "createdAt": "2026-11-16T07:00:00Z", "ownerPrincipalArn": "arn:aws:sts::123456789012:assumed-role/Admin/me"
    })
}

#[test]
fn b853_wire_cancel_and_session_members() {
    assert_eq!(PROTOCOL_VERSION, "1.63.0");

    // The Run item: absent reads as no cancel and is left out again; present round-trips under the name `cancelRequestedAt`.
    let old: RunItem = serde_json::from_value(run_item_1_62()).unwrap();
    assert_eq!(old.cancel_requested_at, None);
    assert_eq!(serde_json::to_value(&old).unwrap(), run_item_1_62());
    let mut with = run_item_1_62();
    with["cancelRequestedAt"] = json!(AT);
    let item: RunItem = serde_json::from_value(with.clone()).unwrap();
    assert_eq!(item.cancel_requested_at.as_deref(), Some(AT));
    assert_eq!(serde_json::to_value(&item).unwrap(), with);

    // The answer of the cancel route: the 1.62.0 form has no `cancel`, and a response without one writes none.
    let plain: RunControlResponse =
        serde_json::from_value(json!({"runId": RUN, "status": "active"})).unwrap();
    assert_eq!(plain.cancel, None);
    assert_eq!(
        serde_json::to_value(&plain).unwrap(),
        json!({"runId": RUN, "status": "active"})
    );
    // A session that answers: every member.
    let live = json!({
        "requestedAt": AT, "session": 1, "sessionLive": true,
        "lastHeartbeatAt": "2026-11-16T07:05:22Z", "lostAfter": "2026-11-16T07:15:22Z"
    });
    let resp: RunControlResponse =
        serde_json::from_value(json!({"runId": RUN, "status": "active", "cancel": live})).unwrap();
    let cancel = resp.cancel.clone().unwrap();
    assert_eq!(
        cancel,
        CancelRecorded {
            requested_at: AT.into(),
            session: Some(1),
            session_live: true,
            delivering: false,
            last_heartbeat_at: Some("2026-11-16T07:05:22Z".into()),
            lost_after: Some("2026-11-16T07:15:22Z".into()),
        }
    );
    assert_eq!(resp.status, GoalStatus::Active);
    // `delivering` is left out when false; the options are left out when absent.
    assert_eq!(serde_json::to_value(&cancel).unwrap(), live);
    let none = CancelRecorded {
        requested_at: AT.into(),
        session: None,
        session_live: false,
        delivering: false,
        last_heartbeat_at: None,
        lost_after: None,
    };
    assert_eq!(
        serde_json::to_value(&none).unwrap(),
        json!({"requestedAt": AT, "sessionLive": false})
    );
    // A run that is delivering.
    let delivering: CancelRecorded = serde_json::from_value(
        json!({"requestedAt": AT, "session": 1, "sessionLive": false, "delivering": true}),
    )
    .unwrap();
    assert!(delivering.delivering && !delivering.session_live);
    assert_eq!(delivering.lost_after, None);
    // The type is lenient: a member a later service adds is ignored, and `sessionLive` left out reads as false.
    let later: CancelRecorded =
        serde_json::from_value(json!({"requestedAt": AT, "somethingNew": 1})).unwrap();
    assert_eq!(later, none);

    // The rows of the run list.
    let old: RunSummary = serde_json::from_value(summary_1_62()).unwrap();
    assert_eq!(
        (
            old.cancel_requested_at.clone(),
            old.session_silent_since.clone()
        ),
        (None, None)
    );
    assert_eq!(serde_json::to_value(&old).unwrap(), summary_1_62());
    let mut with = summary_1_62();
    with["cancelRequestedAt"] = json!(AT);
    with["sessionSilentSince"] = json!("2026-11-16T07:02:00Z");
    let row: RunSummary = serde_json::from_value(with.clone()).unwrap();
    assert_eq!(row.cancel_requested_at.as_deref(), Some(AT));
    assert_eq!(
        row.session_silent_since.as_deref(),
        Some("2026-11-16T07:02:00Z")
    );
    assert_eq!(serde_json::to_value(&row).unwrap(), with);

    // The kind of the event: its own word, not the lost-session word (a client of 5.20 to 5.22 reads `run.waiting` only for that one).
    assert_eq!(CANCEL_RECORDED_KIND, "cancel-recorded");
    assert_ne!(CANCEL_RECORDED_KIND, SESSION_LOST_KIND);
}

/// The exported contract names the members where a client reads them.
#[test]
fn b853_the_contract_files_name_the_new_members() {
    let docs: std::collections::BTreeMap<&str, Value> = harness_protocol::export::all_schemas()
        .into_iter()
        .collect();
    let v1 = &docs["openapi-v1.json"]["components"]["schemas"];
    assert!(
        v1["RunControlResponse"]["properties"]["cancel"].is_object(),
        "{}",
        v1["RunControlResponse"]
    );
    for member in [
        "requestedAt",
        "session",
        "sessionLive",
        "delivering",
        "lastHeartbeatAt",
        "lostAfter",
    ] {
        assert!(
            v1["CancelRecorded"]["properties"][member].is_object(),
            "CancelRecorded.{member}"
        );
    }
    assert_eq!(v1["CancelRecorded"]["required"], json!(["requestedAt"]));
    for member in ["cancelRequestedAt", "sessionSilentSince"] {
        assert!(
            v1["RunSummary"]["properties"][member].is_object(),
            "RunSummary.{member}"
        );
    }
    let items = &docs["items.schema.json"];
    let run_item = items
        .pointer("/$defs/RunItem/properties/cancelRequestedAt")
        .or_else(|| items.pointer("/definitions/RunItem/properties/cancelRequestedAt"));
    assert!(
        run_item.is_some(),
        "items.schema.json names RunItem.cancelRequestedAt"
    );
}
