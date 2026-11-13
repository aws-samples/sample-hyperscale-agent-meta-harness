//! Internal API `/internal` bodies, called by `agentd` inside the microVM.

use crate::common::Usage;
use crate::direct_input::{InstructAction, InstructOrigin};
use crate::envelope::{Authority, Envelope, EnvelopeError, validate};
use crate::messages::MessageNotice;
use crate::runtime::TurnOutcome;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

pub use crate::common::ApprovalDecision;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CredsRequest {
    pub run_id: String,
    pub agent_id: String,
    pub runtime_session_id: String,
    /// The rotating nonce from the previous response.
    pub nonce: String,
}

/// AWS `credential_process` output format (PascalCase field names, `Version` 1).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ProcessCredentials {
    pub version: u8,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: String,
    pub expiration: String,
}

impl fmt::Debug for ProcessCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessCredentials")
            .field("version", &self.version)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field("session_token", &"<redacted>")
            .field("expiration", &self.expiration)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CredsResponse {
    pub credentials: ProcessCredentials,
    pub next_nonce: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatRequest {
    pub run_id: String,
    pub agent_id: String,
    pub turn: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatResponse {
    pub ok: bool,
}

/// A turn's `summary` keeps at most this many characters (the end of the agent's text).
pub const MAX_TURN_SUMMARY_CHARS: usize = 32_000;
/// A turn's `summary` is also at most this many bytes of JSON text. The request goes whole into `SendTaskSuccess`, and a summary over it is a
/// preview of the agent's text ([`crate::direct::message_preview`]); the whole message is in `summary.json` and `evidence/direct.json`.
pub const MAX_TURN_SUMMARY_BYTES: usize = crate::direct::MAX_STATE_MESSAGE_BYTES;

/// End of a turn. It carries no verdict: the harness gates and evaluates independently.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompleteRequest {
    /// The agent's final message of the turn (its text, control characters removed, the last
    /// [`MAX_TURN_SUMMARY_CHARS`] characters): a read-only task's report, a claim otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub run_id: String,
    pub agent_id: String,
    pub turn: u32,
    /// Engine finish reason, recorded as telemetry only.
    pub finish_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_commit_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_uri: Option<String>,
    /// Engine-reported tokens; absent when the engine reported none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// How the turn ended as `agentd` observed it: telemetry, never a verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<TurnOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// SHA-256 of the canonical JSON of `rulesManifest`, when an engine ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules_digest: Option<String>,
    /// The turn's rules manifest: every rule in precedence order and how it was delivered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules_manifest: Option<crate::runtime::RulesManifest>,
    /// Repository paths the engine changed outside the role's write globs, which agentd restored
    /// before the commit;
    /// build outputs of the role's own commands are not listed. At most [`MAX_REVERTED_PATHS`],
    /// each at most [`MAX_REVERTED_PATH_CHARS`] characters ([`cap_reverted`]); absent when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reverted: Vec<String>,
    /// `reverted` left paths out or shortened one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reverted_truncated: bool,
    /// The kinds of permission request agentd's role policy refused this turn, grouped by tool and
    /// reason with secrets redacted; the goal step lists them in the agent's
    /// next prompt. At most [`MAX_REFUSALS`] ([`cap_refused`]); absent when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<Refusal>,
    /// `refused` left kinds out or shortened a field.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub refused_truncated: bool,
    /// What agentd could tell about a provider fault that ended the turn:
    /// structured evidence for the goal step's classifier. Absent for every turn that ended
    /// without one and for an agentd that sets none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault_hint: Option<FaultHint>,
    /// The longest silence, in seconds, between two engine updates in the turn (1.22.0): the data
    /// for tightening the stall budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_silent_gap_s: Option<u32>,
    /// What the transcript writer declares: the last chunk it stored and what it left out or lost.
    /// Absent from an agentd that writes no transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<crate::transcript::TranscriptSummary>,
    /// `true` when the slice ended at its prompt deadline and the invocation said more time was left (`slice.last` false): the work is
    /// committed, bundled and uploaded, and the service starts the next slice. Absent for every other end, so an agentd that predates the member
    /// is read as a slice that ended the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<bool>,
    /// What agentd knows about the three end-of-run objects it wrote: the SHA-256 of each body and the version id S3
    /// gave to its own write. Only a call that carries the run's completion key reaches `SendTaskSuccess`, so the engine cannot set this; worker-api
    /// drops the member from a call that did not prove the key. `plan-delivery` and the bundle validator read exactly these versions. Absent
    /// from an agentd that predates the member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_pins: Option<crate::output_pins::OutputPins>,
}

/// Where in agentd the engine failure was seen: only failures on the engine transport can be
/// infrastructure-unknown; agentd-local ones (worktree, engine start, credential socket,
/// write-policy) set none and keep the ordinary failed-turn path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum FaultTransport {
    /// The engine answered a request with a JSON-RPC internal error (-32603).
    #[serde(rename = "rpc-32603")]
    Rpc32603,
    /// Reading or writing the engine's pipes failed.
    #[serde(rename = "io")]
    Io,
    /// The engine closed its output.
    #[serde(rename = "closed")]
    Closed,
}

/// Structured evidence of a provider fault, as agentd reads it from the ACP error data (Cline
/// patch 0011, OpenCode) or from its own watchdog. Every member is optional; a hint with none set
/// is the same as no hint.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FaultHint {
    /// A short machine code: `stall` from the watchdog, else what the engine named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    /// The AWS exception type (`ThrottlingException`, `ServiceUnavailableException`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_error_type: Option<String>,
    /// The provider's `Retry-After`, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_s: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<FaultTransport>,
    /// For a stall: how long, in seconds, the engine was silent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub silent_s: Option<u32>,
}

/// One kind of permission request agentd's role policy refused in a turn: the tool, what the
/// first such request asked for, agentd's reason, and how many requests of the turn got it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Refusal {
    /// The tool the engine named (`run_commands`, `editor`, `read_files`), `-` when none.
    pub tool: String,
    /// The first such request's command or paths, the worktree path shown as `.`, control
    /// characters escaped, secrets redacted; at most [`MAX_REFUSAL_ACTION_CHARS`] characters.
    pub action: String,
    /// agentd's reason, the same way; at most [`MAX_REFUSAL_REASON_CHARS`] characters.
    pub reason: String,
    /// Requests of the turn refused with this tool and reason (1 or more).
    pub count: u32,
}

/// At most this many `TurnCompleteRequest.refused` kinds (the request goes whole into
/// SendTaskSuccess and the next prompt).
pub const MAX_REFUSALS: usize = 10;
/// At most this many characters of a [`Refusal`]'s `action`.
pub const MAX_REFUSAL_ACTION_CHARS: usize = 240;
/// At most this many characters of a [`Refusal`]'s `reason`.
pub const MAX_REFUSAL_REASON_CHARS: usize = 320;

/// The first [`MAX_REFUSALS`] refusals, each field cut to its limit, and whether anything was
/// left out or cut.
pub fn cap_refused(refused: &[Refusal]) -> (Vec<Refusal>, bool) {
    let mut cut = refused.len() > MAX_REFUSALS;
    let mut clip = |s: &str, max: usize| {
        if s.chars().count() > max {
            cut = true;
            s.chars().take(max).collect()
        } else {
            s.to_string()
        }
    };
    let kept = refused
        .iter()
        .take(MAX_REFUSALS)
        .map(|r| Refusal {
            tool: clip(&r.tool, 64),
            action: clip(&r.action, MAX_REFUSAL_ACTION_CHARS),
            reason: clip(&r.reason, MAX_REFUSAL_REASON_CHARS),
            count: r.count.max(1),
        })
        .collect();
    (kept, cut)
}

/// At most this many `TurnCompleteRequest.reverted` paths: the request goes whole into
/// SendTaskSuccess (256 KiB) and the next prompt.
pub const MAX_REVERTED_PATHS: usize = 20;
/// At most this many characters of one `TurnCompleteRequest.reverted` path.
pub const MAX_REVERTED_PATH_CHARS: usize = 256;

/// The first [`MAX_REVERTED_PATHS`] paths, each cut to [`MAX_REVERTED_PATH_CHARS`] characters,
/// and whether anything was left out or cut.
pub fn cap_reverted(paths: &[String]) -> (Vec<String>, bool) {
    let mut cut = paths.len() > MAX_REVERTED_PATHS;
    let kept = paths
        .iter()
        .take(MAX_REVERTED_PATHS)
        .map(|p| {
            if p.chars().count() > MAX_REVERTED_PATH_CHARS {
                cut = true;
                p.chars().take(MAX_REVERTED_PATH_CHARS).collect()
            } else {
                p.clone()
            }
        })
        .collect();
    (kept, cut)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompleteResponse {
    pub accepted: bool,
}

/// A batch of agent events. Stored with `authority=agent` only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventsBatchRequest {
    pub run_id: String,
    pub agent_id: String,
    pub events: Vec<Envelope>,
    /// `false` on a turn's final flush: the response carries no controls, so they stay queued for
    /// the next turn. Absent means the response carries them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub take_controls: Option<bool>,
}

/// Controls waiting for the agent, delivered in the events response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PendingControl {
    Steer {
        text: String,
        now: bool,
    },
    Cancel,
    ApprovalAnswer {
        #[serde(rename = "approvalId")]
        approval_id: String,
        decision: ApprovalDecision,
    },
    /// Protocol 1.37.0: a person's (later the analyzer's) answer to a `needs-input` question of a
    /// direct run. The `ControlItem`'s `control_id` is the request id, which is the nonce: one answer per request.
    Instruct {
        #[serde(rename = "requestId")]
        request_id: String,
        action: InstructAction,
        /// `continue_with_prompt` needs it; `new_session_with_prompt` and `try_model` take it optionally; `finish` takes none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        /// `try_model` only: a route id of this run (the `models` list of the question).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// control-api always writes `user`.
        #[serde(default = "InstructOrigin::user")]
        origin: InstructOrigin,
    },
    /// Protocol 1.38.0: a message of the user is pending in the current session. worker-api adds one to the `pending` list of
    /// every events response while the message is pending; it is a notice (no text), it is never stored as a control item and handing it out
    /// changes nothing, so it repeats until agentd claims the message (`POST /internal/message-claim`). An agentd built before 1.38.0 sets the
    /// item aside (`lenient_pending`) and acts on the rest of the list.
    Message(MessageNotice),
}

/// Splits a list of raw controls into the ones this build can read and the ones it cannot (a future `kind`, an unknown `action`), in
/// order. A reader that must not lose a `Cancel` beside an item it cannot parse uses this (agentd logs the second list).
pub fn split_pending(
    items: Vec<serde_json::Value>,
) -> (Vec<PendingControl>, Vec<serde_json::Value>) {
    let mut ok = Vec::new();
    let mut skipped = Vec::new();
    for v in items {
        match serde_json::from_value::<PendingControl>(v.clone()) {
            Ok(c) => ok.push(c),
            Err(_) => skipped.push(v),
        }
    }
    (ok, skipped)
}

/// `deserialize_with` for [`EventsBatchResponse::pending`]: each item is parsed alone, and an item that does not parse is skipped, so
/// one control this build cannot read never fails the batch or loses a `Cancel` beside it. A list that is not an array is an error.
pub fn lenient_pending<'de, D>(d: D) -> Result<Vec<PendingControl>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let items = Vec::<serde_json::Value>::deserialize(d)?;
    Ok(split_pending(items).0)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventsBatchResponse {
    pub accepted: u32,
    #[serde(default, deserialize_with = "lenient_pending")]
    pub pending: Vec<PendingControl>,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum BatchError {
    #[error("event {index}: only agent-authority events may be posted to /internal/events")]
    Authority { index: usize },
    #[error("event {index}: agentId differs from the caller's agent")]
    AgentMismatch { index: usize },
    #[error("event {index}: runId differs from the caller's run")]
    RunMismatch { index: usize },
    #[error("event {index}: {source}")]
    Envelope { index: usize, source: EnvelopeError },
}

/// Checks a batch before storage: agent authority, the caller's agent and run, and each envelope.
pub fn validate_events_batch(req: &EventsBatchRequest) -> Result<(), BatchError> {
    for (index, e) in req.events.iter().enumerate() {
        if e.authority != Authority::Agent {
            return Err(BatchError::Authority { index });
        }
        if e.agent_id.as_deref() != Some(req.agent_id.as_str()) {
            return Err(BatchError::AgentMismatch { index });
        }
        if e.run_id != req.run_id {
            return Err(BatchError::RunMismatch { index });
        }
        validate(e).map_err(|source| BatchError::Envelope { index, source })?;
    }
    Ok(())
}
