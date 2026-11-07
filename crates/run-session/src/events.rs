//! Harness-authority events of a direct run.
//!
//! Every id is deterministic, `ulid_from(ms, "<runId>/<event type>/<n>")`, so a Lambda that retries writes the same
//! item again instead of a second one. `ms` is the Turn item's `startedAtMs` for the events of the start, and the
//! delivery plan's `plannedAtMs` plus the row's offset for the events of the end, so the ids sort in the order of the
//! table AND in the order the Lambda writes them. That second order matters: a reader keeps the largest id it has read
//! and asks for the ids above it, so an event written after a larger id was readable, with a smaller id, is never
//! returned to that reader. Two events of one flow must therefore never be ordered by chance. The end events have
//! distinct milliseconds. The two start events share one: `goal.status` takes the lower half of the 80 random bits and
//! `session.started` the upper half ([`Half`]), because their hashes could sort `session.started` below `goal.status`, and a card
//! lost its model name that way. `tests/event_order.rs` pins it
//! for every flow.

use dispatcher::ids::ulid_from;
use dispatcher::items::rfc3339;
use harness_protocol::direct::{DIRECT_AGENT_ID, DIRECT_ENGINE, DIRECT_TASK_ID, DirectOutcome};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::goal::{GoalPhase, GoalStatus};
use harness_protocol::orchestration::{
    DeliveryPrPayload, DeliveryReadyPayload, GoalBlockedPayload, GoalStatusPayload,
    RunCompletedPayload, SessionStartedPayload,
};
use serde::Serialize;
use serde_json::Value;

/// Offsets of the end events from `plannedAtMs` (rows 9 to 13 of the table in section 4.1; blocked: B1 to B3).
pub mod offset {
    pub const GOAL_DELIVERING: u64 = 1;
    pub const DELIVERY_PR: u64 = 2;
    pub const DELIVERY_READY: u64 = 3;
    pub const GOAL_COMPLETE: u64 = 4;
    pub const RUN_COMPLETED: u64 = 5;
    pub const GOAL_BLOCKED: u64 = 1;
    pub const GOAL_BLOCKED_STATUS: u64 = 2;
    pub const RUN_BLOCKED: u64 = 3;
}

fn value<T: Serialize>(payload: &T) -> Value {
    serde_json::to_value(payload).unwrap_or(Value::Null)
}

/// Which half of the 80 random bits of a ULID an id takes. Of two events of one millisecond that a flow writes one after the other, the
/// first takes [`Half::Lower`] and the second [`Half::Upper`], so they sort in the order they are written whatever their hashes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Half {
    /// The hash decides (the default: events of different milliseconds need nothing more).
    Any,
    /// The top random bit is 0.
    Lower,
    /// The top random bit is 1.
    Upper,
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// `ulid_from(ms, seed)` with the top bit of the 80 random bits forced by `half`. Character 10 of a ULID holds the five highest random
/// bits (characters 0 to 9 hold the 48 bits of time), and its value 16 is the top bit.
fn ordered_ulid(ms: u64, seed: &[u8], half: Half) -> String {
    let id = ulid_from(ms, seed);
    if half == Half::Any {
        return id;
    }
    let mut chars: Vec<u8> = id.into_bytes();
    let v = CROCKFORD
        .iter()
        .position(|c| *c == chars[10])
        .unwrap_or_default();
    let v = match half {
        Half::Lower => v & 0b01111,
        _ => v | 0b10000,
    };
    chars[10] = CROCKFORD[v];
    String::from_utf8(chars).unwrap_or_default()
}

/// A harness event with a deterministic id. `n` tells apart two events of one type (the three `goal.status`).
pub fn harness_event<T: Serialize>(
    run_id: &str,
    ms: u64,
    n: u32,
    event_type: EventType,
    task_id: Option<&str>,
    payload: &T,
) -> Envelope {
    harness_event_in(run_id, ms, n, Half::Any, event_type, task_id, payload)
}

/// [`harness_event`] with the half of the random bits the id takes.
pub fn harness_event_in<T: Serialize>(
    run_id: &str,
    ms: u64,
    n: u32,
    half: Half,
    event_type: EventType,
    task_id: Option<&str>,
    payload: &T,
) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: ordered_ulid(
            ms,
            format!("{run_id}/{}/{n}", event_type.as_str()).as_bytes(),
            half,
        ),
        run_id: run_id.to_string(),
        task_id: task_id.map(str::to_string),
        agent_id: None,
        seq: 0,
        ts: rfc3339(ms / 1000),
        event_type,
        authority: Authority::Harness,
        payload: Payload::Inline {
            value: value(payload),
        },
    }
}

/// Row 2: Cline is working. The protocol has no "working" phase, and `green` is the one that means it (OQ-7). Written first, so it takes
/// the lower half of the random bits of its millisecond; `session.started` (row 3), written next at the same millisecond, takes the upper.
pub fn goal_active(run_id: &str, ms: u64) -> Envelope {
    goal_status_in(
        run_id,
        ms,
        1,
        Half::Lower,
        GoalStatus::Active,
        GoalPhase::Green,
    )
}

pub fn goal_status(
    run_id: &str,
    ms: u64,
    n: u32,
    status: GoalStatus,
    phase: GoalPhase,
) -> Envelope {
    goal_status_in(run_id, ms, n, Half::Any, status, phase)
}

fn goal_status_in(
    run_id: &str,
    ms: u64,
    n: u32,
    half: Half,
    status: GoalStatus,
    phase: GoalPhase,
) -> Envelope {
    harness_event_in(
        run_id,
        ms,
        n,
        half,
        EventType::GoalStatus,
        None,
        &GoalStatusPayload {
            status,
            phase: Some(phase),
        },
    )
}

/// Row 3, written right after row 2 at the same millisecond: it takes the upper half of the random bits, so it sorts after
/// `goal_active(run_id, ms)` for every run.
pub fn session_started(run_id: &str, ms: u64, model_id: &str, context_tokens: u32) -> Envelope {
    harness_event_in(
        run_id,
        ms,
        1,
        Half::Upper,
        EventType::SessionStarted,
        Some(DIRECT_TASK_ID),
        &SessionStartedPayload {
            agent_id: Some(DIRECT_AGENT_ID.to_string()),
            engine: harness_protocol::common::Engine::CLINE,
            model_id: model_id.to_string(),
            role: Some(DIRECT_AGENT_ID.to_string()),
            context_tokens: Some(context_tokens),
        },
    )
}

/// Row 9.
pub fn goal_delivering(run_id: &str, planned_at_ms: u64) -> Envelope {
    goal_status(
        run_id,
        planned_at_ms + offset::GOAL_DELIVERING,
        2,
        GoalStatus::Active,
        GoalPhase::Delivering,
    )
}

/// Row 10, only when a pull request was opened.
pub fn delivery_pr(run_id: &str, planned_at_ms: u64, url: &str) -> Envelope {
    harness_event(
        run_id,
        planned_at_ms + offset::DELIVERY_PR,
        1,
        EventType::DeliveryPr,
        None,
        &DeliveryPrPayload {
            url: url.to_string(),
        },
    )
}

/// Row 11.
pub fn delivery_ready(run_id: &str, planned_at_ms: u64, ready: &DeliveryReadyPayload) -> Envelope {
    harness_event(
        run_id,
        planned_at_ms + offset::DELIVERY_READY,
        1,
        EventType::DeliveryReady,
        None,
        ready,
    )
}

/// Row 12: written inside the status transaction.
pub fn goal_complete(run_id: &str, planned_at_ms: u64) -> Envelope {
    goal_status(
        run_id,
        planned_at_ms + offset::GOAL_COMPLETE,
        3,
        GoalStatus::Complete,
        GoalPhase::Delivering,
    )
}

/// Row 12 for a run that stopped delivered: the status is `paused` or `stalled`, not `complete`. Written inside the status transaction.
pub fn goal_stopped(run_id: &str, planned_at_ms: u64, status: GoalStatus) -> Envelope {
    goal_status(
        run_id,
        planned_at_ms + offset::GOAL_COMPLETE,
        3,
        status,
        GoalPhase::Delivering,
    )
}

/// Row 13: the run is complete, and the label says how Cline stopped.
pub fn run_completed(
    run_id: &str,
    planned_at_ms: u64,
    outcome: DirectOutcome,
    detail: Option<String>,
    observations: Vec<harness_protocol::direct::DirectObservation>,
) -> Envelope {
    run_completed_as(
        run_id,
        planned_at_ms,
        GoalStatus::Complete,
        outcome,
        detail,
        observations,
    )
}

/// Row 13 with the status the run ends in: `complete`, or `paused` and `stalled` for a run that stopped delivered. The label is the same.
pub fn run_completed_as(
    run_id: &str,
    planned_at_ms: u64,
    status: GoalStatus,
    outcome: DirectOutcome,
    detail: Option<String>,
    observations: Vec<harness_protocol::direct::DirectObservation>,
) -> Envelope {
    let label = outcome.label_for(detail.as_deref());
    harness_event(
        run_id,
        planned_at_ms + offset::RUN_COMPLETED,
        1,
        EventType::RunCompleted,
        None,
        &RunCompletedPayload {
            status: Some(status),
            outcome: Some(outcome),
            label: Some(label),
            detail,
            observations,
        },
    )
}

/// B1.
pub fn goal_blocked(run_id: &str, base_ms: u64, reason: &str) -> Envelope {
    harness_event(
        run_id,
        base_ms + offset::GOAL_BLOCKED,
        1,
        EventType::GoalBlocked,
        None,
        &GoalBlockedPayload {
            reason: reason.to_string(),
            fingerprint: None,
            needs: Vec::new(),
        },
    )
}

/// B2: written inside the status transaction.
pub fn goal_blocked_status(run_id: &str, base_ms: u64) -> Envelope {
    goal_status(
        run_id,
        base_ms + offset::GOAL_BLOCKED_STATUS,
        4,
        GoalStatus::Blocked,
        GoalPhase::Delivering,
    )
}

/// B3.
pub fn run_blocked(
    run_id: &str,
    base_ms: u64,
    outcome: Option<DirectOutcome>,
    detail: Option<String>,
) -> Envelope {
    let label = outcome.map(|o| o.label_for(detail.as_deref()));
    harness_event(
        run_id,
        base_ms + offset::RUN_BLOCKED,
        1,
        EventType::RunCompleted,
        None,
        &RunCompletedPayload {
            status: Some(GoalStatus::Blocked),
            outcome,
            label,
            detail,
            observations: Vec::new(),
        },
    )
}

/// The engine word of `session.started` and the Agent item.
pub const ENGINE: &str = DIRECT_ENGINE;
