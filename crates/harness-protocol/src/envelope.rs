//! Event envelope and event types.
//!
//! Invariant 4: only harness-authored events move a goal to complete or show links. The
//! authority each type requires is fixed here, and [`validate`] enforces it.

use crate::common::{is_lower_hex, is_ulid, is_utc_timestamp};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;
/// Limit on the compact JSON of an inline payload value: 16 KiB. One event is one DynamoDB item (400 KB), and a poll answers a page that DynamoDB
/// stops at 1 MB. Larger payloads would use an S3 claim check.
pub const MAX_INLINE_PAYLOAD_BYTES: usize = 16 * 1024;

/// Who wrote an event: the deterministic harness or an (untrusted) agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Authority {
    Harness,
    Agent,
}

/// Event types in architecture table order.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub enum EventType {
    #[serde(rename = "run.created")]
    RunCreated,
    #[serde(rename = "session.started")]
    SessionStarted,
    #[serde(rename = "turn.started")]
    TurnStarted,
    #[serde(rename = "turn.progress")]
    TurnProgress,
    #[serde(rename = "tool.summary")]
    ToolSummary,
    #[serde(rename = "turn.ended")]
    TurnEnded,
    #[serde(rename = "agent.flag_blocked")]
    AgentFlagBlocked,
    #[serde(rename = "permission.requested")]
    PermissionRequested,
    #[serde(rename = "gate.result")]
    GateResult,
    #[serde(rename = "evaluator.verdict")]
    EvaluatorVerdict,
    #[serde(rename = "merge.result")]
    MergeResult,
    #[serde(rename = "goal.status")]
    GoalStatus,
    #[serde(rename = "goal.blocked")]
    GoalBlocked,
    #[serde(rename = "delivery.pr")]
    DeliveryPr,
    #[serde(rename = "delivery.ready")]
    DeliveryReady,
    #[serde(rename = "run.completed")]
    RunCompleted,
    // The run's `run` execution ended ABORTED, FAILED or TIMED_OUT before the run finished; the
    // harness cleared the run and stopped its work.
    #[serde(rename = "run.aborted")]
    RunAborted,
    // The provider-fault ladder's narration. Harness authority only;
    // none changes the goal's status or shows a link: `RouteChangedPayload`, `RouteRestoredPayload`
    // and `RunWaitingPayload` in `ladder`.
    #[serde(rename = "route.changed")]
    RouteChanged,
    #[serde(rename = "run.waiting")]
    RunWaiting,
    // Harness authority only; it changes no status. Its payload names requirement IDs, lint class
    // names and counts, never test text.
    #[serde(rename = "hidden.admission")]
    HiddenAdmission,
    // What the engine says it can do in one session.
    // Agent authority: agentd posts it once per session; it changes no status and shows no link.
    #[serde(rename = "session.capabilities")]
    SessionCapabilities,
}

impl EventType {
    pub const ALL: &'static [EventType] = &[
        EventType::RunCreated,
        EventType::SessionStarted,
        EventType::TurnStarted,
        EventType::TurnProgress,
        EventType::ToolSummary,
        EventType::TurnEnded,
        EventType::AgentFlagBlocked,
        EventType::PermissionRequested,
        EventType::GateResult,
        EventType::EvaluatorVerdict,
        EventType::MergeResult,
        EventType::GoalStatus,
        EventType::GoalBlocked,
        EventType::DeliveryPr,
        EventType::DeliveryReady,
        EventType::RunCompleted,
        EventType::RunAborted,
        EventType::RouteChanged,
        EventType::RunWaiting,
        EventType::HiddenAdmission,
        EventType::SessionCapabilities,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EventType::RunCreated => "run.created",
            EventType::SessionStarted => "session.started",
            EventType::TurnStarted => "turn.started",
            EventType::TurnProgress => "turn.progress",
            EventType::ToolSummary => "tool.summary",
            EventType::TurnEnded => "turn.ended",
            EventType::AgentFlagBlocked => "agent.flag_blocked",
            EventType::PermissionRequested => "permission.requested",
            EventType::GateResult => "gate.result",
            EventType::EvaluatorVerdict => "evaluator.verdict",
            EventType::MergeResult => "merge.result",
            EventType::GoalStatus => "goal.status",
            EventType::GoalBlocked => "goal.blocked",
            EventType::DeliveryPr => "delivery.pr",
            EventType::DeliveryReady => "delivery.ready",
            EventType::RunCompleted => "run.completed",
            EventType::RunAborted => "run.aborted",
            EventType::RouteChanged => "route.changed",
            EventType::RunWaiting => "run.waiting",
            EventType::HiddenAdmission => "hidden.admission",
            EventType::SessionCapabilities => "session.capabilities",
        }
    }

    /// The only authority allowed to write this type.
    pub fn required_authority(self) -> Authority {
        match self {
            EventType::TurnStarted
            | EventType::TurnProgress
            | EventType::ToolSummary
            | EventType::TurnEnded
            | EventType::AgentFlagBlocked
            | EventType::PermissionRequested
            | EventType::SessionCapabilities => Authority::Agent,
            _ => Authority::Harness,
        }
    }

    /// True for the events that change goal status or show links in the TUI.
    pub fn changes_goal_status(self) -> bool {
        matches!(
            self,
            EventType::GoalStatus
                | EventType::GoalBlocked
                | EventType::DeliveryPr
                | EventType::DeliveryReady
                | EventType::RunCompleted
                | EventType::RunAborted
        )
    }
}

/// Event body: inline JSON up to [`MAX_INLINE_PAYLOAD_BYTES`], or a claim check on an S3 object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Payload {
    Inline {
        value: serde_json::Value,
    },
    ClaimCheck {
        #[serde(rename = "s3Uri")]
        #[schemars(pattern(r"^s3://[^/]+/.+$"))]
        s3_uri: String,
        #[schemars(pattern(r"^[0-9a-f]{64}$"))]
        sha256: String,
        #[serde(rename = "sizeBytes")]
        size_bytes: u64,
    },
}

/// One event in the run log (DynamoDB `EVT#<runId>#<shard>`, sort key `eventId`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Envelope {
    pub schema_version: u32,
    /// ULID, the event's sort key.
    #[schemars(pattern(r"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"))]
    pub event_id: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Per-agent sequence number.
    pub seq: u64,
    /// RFC 3339 UTC timestamp.
    #[schemars(extend("format" = "date-time"))]
    pub ts: String,
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub authority: Authority,
    pub payload: Payload,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EnvelopeError {
    #[error("schemaVersion {0} is not supported (expected {SCHEMA_VERSION})")]
    SchemaVersion(u32),
    #[error("eventId {0:?} is not a ULID")]
    EventId(String),
    #[error("ts {0:?} is not an RFC 3339 UTC timestamp")]
    Timestamp(String),
    #[error("{} events need authority {expected:?}, got {actual:?}", event_type.as_str())]
    Authority {
        event_type: EventType,
        expected: Authority,
        actual: Authority,
    },
    #[error("agent-authored events need agentId")]
    MissingAgentId,
    #[error(
        "inline payload is {bytes} bytes; the limit is {MAX_INLINE_PAYLOAD_BYTES} (use an S3 claim check)"
    )]
    PayloadTooLarge { bytes: usize },
    #[error("claim check: {0}")]
    ClaimCheck(String),
}

/// Checks one envelope: version, ULID, timestamp, authority for its type, agent id and payload.
pub fn validate(e: &Envelope) -> Result<(), EnvelopeError> {
    if e.schema_version != SCHEMA_VERSION {
        return Err(EnvelopeError::SchemaVersion(e.schema_version));
    }
    if !is_ulid(&e.event_id) {
        return Err(EnvelopeError::EventId(e.event_id.clone()));
    }
    if !is_utc_timestamp(&e.ts) {
        return Err(EnvelopeError::Timestamp(e.ts.clone()));
    }
    let expected = e.event_type.required_authority();
    if e.authority != expected {
        return Err(EnvelopeError::Authority {
            event_type: e.event_type,
            expected,
            actual: e.authority,
        });
    }
    if e.authority == Authority::Agent && e.agent_id.as_deref().is_none_or(str::is_empty) {
        return Err(EnvelopeError::MissingAgentId);
    }
    match &e.payload {
        Payload::Inline { value } => {
            let bytes = serde_json::to_vec(value).map_or(usize::MAX, |v| v.len());
            if bytes > MAX_INLINE_PAYLOAD_BYTES {
                return Err(EnvelopeError::PayloadTooLarge { bytes });
            }
        }
        Payload::ClaimCheck { s3_uri, sha256, .. } => {
            let rest = s3_uri.strip_prefix("s3://").unwrap_or_default();
            if !matches!(rest.split_once('/'), Some((bucket, key)) if !bucket.is_empty() && !key.is_empty())
            {
                return Err(EnvelopeError::ClaimCheck(format!(
                    "{s3_uri:?} is not an s3://bucket/key URI"
                )));
            }
            if !is_lower_hex(sha256, 64) {
                return Err(EnvelopeError::ClaimCheck(format!(
                    "sha256 {sha256:?} is not 64 lowercase hex characters"
                )));
            }
        }
    }
    Ok(())
}
