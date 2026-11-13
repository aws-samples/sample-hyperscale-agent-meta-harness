//! Orchestration contracts: the
//! gate report `harness-gates` writes and the worker's redacted view of it, the evaluator verdict,
//! the payloads of harness-authored events (the fields `ham` reads), the Step Functions input of a
//! dispatched turn, and the deterministic repository profile.

use crate::common::Engine;
use crate::goal::{GoalPhase, GoalStatus};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `goal.status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GoalStatusPayload {
    pub status: GoalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<GoalPhase>,
}

/// `goal.blocked`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GoalBlockedPayload {
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// What would unblock it (the worker's `flag_blocked` needs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
}

/// `delivery.pr`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryPrPayload {
    pub url: String,
}

/// `delivery.ready`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryReadyPayload {
    pub artifact_id: String,
    pub sha256: String,
    pub size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
}

/// `run.completed`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunCompletedPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<GoalStatus>,
    /// How a direct run's Cline session ended (1.30.0); absent when the event records none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<crate::direct::DirectOutcome>,
    /// `outcome.label()`: the sentence the product shows instead of a bare "complete" (1.30.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The specifics behind the outcome, at most 300 characters (1.30.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Facts the harness read at the end of the run and records as it found them (1.38.0): the last entry of the run log when it is red.
    /// Never a claim about the work; left out when there is none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observations: Vec<crate::direct::DirectObservation>,
}

/// How a `run` execution ended without the run finishing, in Step Functions' own status words
/// (the Execution Status Change event's `detail.status`): an operator's or `ham cancel`'s
/// StopExecution (`ABORTED`), an uncaught error (`FAILED`) or the machine's timeout (`TIMED_OUT`).
/// A finished run ends `SUCCEEDED` through MarkComplete or RunBlocked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExecutionEnd {
    Aborted,
    Failed,
    TimedOut,
}

impl ExecutionEnd {
    /// The end a status-change `status` names; `None` for `SUCCEEDED`, `RUNNING`,
    /// `PENDING_REDRIVE` and anything else.
    pub fn from_status(status: &str) -> Option<Self> {
        match status {
            "ABORTED" => Some(Self::Aborted),
            "FAILED" => Some(Self::Failed),
            "TIMED_OUT" => Some(Self::TimedOut),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aborted => "ABORTED",
            Self::Failed => "FAILED",
            Self::TimedOut => "TIMED_OUT",
        }
    }
}

/// `RunAbortedPayload.error` is at most this many bytes (Step Functions errors are at most 256
/// characters).
pub const MAX_ABORT_ERROR_BYTES: usize = 256;
/// `RunAbortedPayload.cause` is at most this many bytes (StopExecution takes up to 32,768
/// characters), so the event stays inline (`MAX_INLINE_PAYLOAD_BYTES`) even when every byte
/// needs a JSON escape.
pub const MAX_ABORT_CAUSE_BYTES: usize = 1024;

/// `run.aborted` (harness) and the Run item's `aborted`: the run's `run` execution
/// ended ABORTED, FAILED or TIMED_OUT before the run finished, so the run-ended reaper cleared the
/// run and stopped its workers, sessions and leases. `error` and `cause` are the execution's,
/// single-line and bounded ([`RunAbortedPayload::new`]); `stoppedAt` is the execution's stop time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunAbortedPayload {
    pub status: ExecutionEnd,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 256))]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 1024))]
    pub cause: Option<String>,
    pub stopped_at: String,
}

/// `text` with every control character (newlines, escapes) replaced by a space, cut to at most
/// `max` bytes on a character boundary; `None` when nothing but whitespace is left.
fn bounded_line(text: &str, max: usize) -> Option<String> {
    let mut out = String::with_capacity(text.len().min(max));
    for c in text.chars().map(|c| if c.is_control() { ' ' } else { c }) {
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    let out = out.trim().to_string();
    (!out.is_empty()).then_some(out)
}

impl RunAbortedPayload {
    /// The payload of an execution that ended `status`, its `error` and `cause` made single-line
    /// and cut to [`MAX_ABORT_ERROR_BYTES`] and [`MAX_ABORT_CAUSE_BYTES`].
    pub fn new(
        status: ExecutionEnd,
        error: Option<&str>,
        cause: Option<&str>,
        stopped_at: &str,
    ) -> Self {
        Self {
            status,
            error: error.and_then(|e| bounded_line(e, MAX_ABORT_ERROR_BYTES)),
            cause: cause.and_then(|c| bounded_line(c, MAX_ABORT_CAUSE_BYTES)),
            stopped_at: stopped_at.to_string(),
        }
    }
}

/// `session.started`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionStartedPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub engine: Engine,
    pub model_id: String,
    /// The session's role slug; absent from a dispatcher that does not name it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The effective context window of the session's model, in tokens: the
    /// lower of the run's `--context` for the model and its proven window. Absent when the engine's
    /// own window applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,
}

/// The `orchestration.schema.json` contract document.
#[derive(Debug, JsonSchema)]
pub struct OrchestrationContract {
    pub goal_status: GoalStatusPayload,
    pub goal_blocked: GoalBlockedPayload,
    pub delivery_pr: DeliveryPrPayload,
    pub delivery_ready: DeliveryReadyPayload,
    pub run_completed: RunCompletedPayload,
    pub run_aborted: RunAbortedPayload,
    pub session_started: SessionStartedPayload,
    /// `session.capabilities` (1.38.0): written by agentd, an agent-authority event.
    pub session_capabilities: crate::transcript::SessionCapabilitiesPayload,
}
