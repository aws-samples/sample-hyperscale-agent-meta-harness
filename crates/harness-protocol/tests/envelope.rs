//! Event envelope contract (invariant 4).

use harness_protocol::envelope::{
    Authority, Envelope, EnvelopeError, EventType, MAX_INLINE_PAYLOAD_BYTES, Payload,
    SCHEMA_VERSION, validate,
};
use serde_json::json;

const ULID: &str = "01J8ZQ4Y6X3M2K7N9P5R8T1V0W";

fn envelope(event_type: EventType, authority: Authority) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: ULID.to_string(),
        run_id: "01J8ZQ4Y6X3M2K7N9P5R8T1V0A".to_string(),
        task_id: Some("t1".to_string()),
        agent_id: Some("a1".to_string()),
        seq: 7,
        ts: "2026-09-25T01:02:03.456Z".to_string(),
        event_type,
        authority,
        payload: Payload::Inline {
            value: json!({"text": "hello"}),
        },
    }
}

#[test]
fn wire_names_are_camel_case_with_dotted_types() {
    let v = serde_json::to_value(envelope(EventType::AgentFlagBlocked, Authority::Agent)).unwrap();
    for key in [
        "schemaVersion",
        "eventId",
        "runId",
        "taskId",
        "agentId",
        "seq",
        "ts",
        "type",
        "authority",
        "payload",
    ] {
        assert!(v.get(key).is_some(), "missing {key} in {v}");
    }
    assert_eq!(v["type"], "agent.flag_blocked");
    assert_eq!(v["authority"], "agent");
    assert_eq!(v["payload"]["kind"], "inline");
}

#[test]
fn only_status_events_change_goal_status_or_show_links() {
    let changing: Vec<&str> = EventType::ALL
        .iter()
        .filter(|t| t.changes_goal_status())
        .map(|t| t.as_str())
        .collect();
    assert_eq!(
        changing,
        [
            "goal.status",
            "goal.blocked",
            "delivery.pr",
            "delivery.ready",
            "run.completed",
            "run.aborted"
        ]
    );
}

#[test]
fn valid_envelopes_pass() {
    validate(&envelope(EventType::TurnProgress, Authority::Agent)).unwrap();
    validate(&envelope(EventType::GoalStatus, Authority::Harness)).unwrap();
}

#[test]
fn agent_cannot_author_status_or_delivery_events() {
    for t in [
        EventType::GoalStatus,
        EventType::GoalBlocked,
        EventType::DeliveryPr,
        EventType::DeliveryReady,
        EventType::RunCompleted,
        EventType::RunAborted,
    ] {
        let err = validate(&envelope(t, Authority::Agent)).unwrap_err();
        assert!(
            matches!(err, EnvelopeError::Authority { .. }),
            "{t:?}: {err}"
        );
    }
}

#[test]
fn harness_cannot_author_agent_narrative() {
    let err = validate(&envelope(EventType::TurnEnded, Authority::Harness)).unwrap_err();
    assert!(matches!(err, EnvelopeError::Authority { .. }));
}

#[test]
fn agent_events_need_an_agent_id() {
    let mut e = envelope(EventType::ToolSummary, Authority::Agent);
    e.agent_id = None;
    assert!(matches!(
        validate(&e).unwrap_err(),
        EnvelopeError::MissingAgentId
    ));
}

#[test]
fn inline_payload_limit_is_16_kib() {
    assert_eq!(MAX_INLINE_PAYLOAD_BYTES, 16 * 1024);
    let mut e = envelope(EventType::TurnProgress, Authority::Agent);
    // {"t":"..."} is 8 bytes of framing around the string.
    e.payload = Payload::Inline {
        value: json!({"t": "x".repeat(MAX_INLINE_PAYLOAD_BYTES - 8)}),
    };
    validate(&e).unwrap();
    e.payload = Payload::Inline {
        value: json!({"t": "x".repeat(MAX_INLINE_PAYLOAD_BYTES - 7)}),
    };
    assert!(matches!(
        validate(&e).unwrap_err(),
        EnvelopeError::PayloadTooLarge { bytes: 16385 }
    ));
}

#[test]
fn large_payloads_use_an_s3_claim_check() {
    let mut e = envelope(EventType::GateResult, Authority::Harness);
    e.payload = Payload::ClaimCheck {
        s3_uri: "s3://hamh-live-artifacts/runs/r/evidence/gates.json".to_string(),
        sha256: "a".repeat(64),
        size_bytes: 1_000_000,
    };
    validate(&e).unwrap();
    let v = serde_json::to_value(&e).unwrap();
    assert_eq!(
        v["payload"],
        json!({"kind": "claimCheck", "s3Uri": "s3://hamh-live-artifacts/runs/r/evidence/gates.json", "sha256": "a".repeat(64), "sizeBytes": 1_000_000})
    );

    e.payload = Payload::ClaimCheck {
        s3_uri: "https://x".to_string(),
        sha256: "a".repeat(64),
        size_bytes: 1,
    };
    assert!(matches!(
        validate(&e).unwrap_err(),
        EnvelopeError::ClaimCheck(_)
    ));
    e.payload = Payload::ClaimCheck {
        s3_uri: "s3://b/k".to_string(),
        sha256: "XYZ".to_string(),
        size_bytes: 1,
    };
    assert!(matches!(
        validate(&e).unwrap_err(),
        EnvelopeError::ClaimCheck(_)
    ));
}

#[test]
fn event_ids_are_ulids_and_timestamps_are_utc() {
    let mut e = envelope(EventType::TurnStarted, Authority::Agent);
    for bad in [
        "",
        "not-a-ulid",
        "01J8ZQ4Y6X3M2K7N9P5R8T1V0",
        "81J8ZQ4Y6X3M2K7N9P5R8T1V0W",
        "01J8ZQ4Y6X3M2K7N9P5R8T1V0I",
    ] {
        e.event_id = bad.to_string();
        assert!(
            matches!(validate(&e).unwrap_err(), EnvelopeError::EventId(_)),
            "{bad}"
        );
    }
    e.event_id = ULID.to_string();
    for bad in [
        "2026-09-25 01:02:03Z",
        "2026-09-25T01:02:03+00:00",
        "yesterday",
    ] {
        e.ts = bad.to_string();
        assert!(
            matches!(validate(&e).unwrap_err(), EnvelopeError::Timestamp(_)),
            "{bad}"
        );
    }
    e.ts = "2026-09-25T01:02:03Z".to_string();
    validate(&e).unwrap();
}

#[test]
fn schema_version_is_checked() {
    let mut e = envelope(EventType::TurnStarted, Authority::Agent);
    e.schema_version = 2;
    assert!(matches!(
        validate(&e).unwrap_err(),
        EnvelopeError::SchemaVersion(2)
    ));
}
