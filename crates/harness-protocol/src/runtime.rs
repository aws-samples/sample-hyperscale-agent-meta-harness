//! AgentCore Runtime contract between the dispatcher and `agentd`:
//! `GET /ping` and `POST /invocations` on port 8080.
//!
//! `/invocations` is asynchronous: AgentCore cuts synchronous requests at 15 minutes (not
//! adjustable), so `agentd` answers [`InvocationAccepted`] at once, runs the turn in the
//! background with `/ping` at `HealthyBusy`, posts `/internal/turn-complete`, and writes the
//! [`InvocationResponse`] to [`turn_result_path`] before `/ping` returns to `Healthy`.
//!
//! The dispatcher mints the per-session credentials outside the microVM and passes them in the
//! invocation with a rotating nonce; `agentd` keeps them in memory and refreshes them through
//! `/internal/creds`. Nothing here may print a secret: [`InvocationRequest`]'s `Debug` redacts
//! the nonce, [`ProcessCredentials`]'s redacts the secret key and token, and [`PackagesToken`]
//! prints `<redacted>` and wipes its buffer on drop.

use crate::common::Usage;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// The AgentCore Runtime container contract port (`0.0.0.0:8080`).
pub const RUNTIME_PORT: u16 = 8080;

/// Where `agentd` writes turn `n`'s [`InvocationResponse`] (written last, by rename).
pub fn turn_result_path(data_root: &Path, run_id: &str, agent_id: &str, n: u32) -> PathBuf {
    data_root
        .join(run_id)
        .join(agent_id)
        .join("turns")
        .join(n.to_string())
        .join("result.json")
}

/// `/ping` status: `HealthyBusy` while a turn runs, so AgentCore keeps the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum PingStatus {
    Healthy,
    HealthyBusy,
}

/// `GET /ping` body; field names are the AgentCore contract's own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PingResponse {
    pub status: PingStatus,
    /// Unix seconds of the last status change.
    pub time_of_last_update: u64,
}

/// Where `agentd` restores the worktree from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RepoSource {
    /// `s3://<bucket>/runs/<runId>/...`: a git bundle staged by the dispatcher.
    S3Bundle { uri: String },
    /// An absolute local path (local runs and tests only).
    Local { path: String },
}

/// Precedence, highest first.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum RuleLayer {
    /// Harness invariants, carried by the supplied system prompt.
    Harness,
    /// Tenant and run rules sent with the run.
    Tenant,
    /// `AGENTS.md`, `CLAUDE.md`, `.clinerules/**`, `.cline/rules/**`, `.roo/rules-<role>/**`,
    /// `.cursorrules` in the repository.
    Repository,
    /// The role's customInstructions.
    Role,
}

/// How a rule reaches the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RuleDelivery {
    SystemPrompt,
    /// The engine loads the file itself.
    Native,
    /// Written into agentd's compiled rules file.
    Compiled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuleEntry {
    pub layer: RuleLayer,
    /// A repository-relative path, a rule name, or `system prompt`.
    pub source: String,
    pub sha256: String,
    pub delivery: RuleDelivery,
}

/// The per-session rules manifest: every rule in precedence order and how it was delivered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RulesManifest {
    pub entries: Vec<RuleEntry>,
    /// SHA-256 of the compiled rules file, when one was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compiled_sha256: Option<String>,
}

/// What the test probe of a [`PackagesToken`] receives: where the bytes were stored and what they held just before release.
#[cfg(test)]
type Probe = std::sync::Arc<std::sync::Mutex<Option<(usize, Vec<u8>)>>>;

/// The CodeArtifact authorization token `run-session` minted outside the VM with its own role,
/// the one secret of an invocation that is neither an AWS credential nor the nonce. On the wire it is a plain string (the
/// invocation member `packagesToken`; the exported schema keeps its released description). In memory it is a newtype whose
/// field is private: [`PackagesToken::new`] builds it, [`PackagesToken::expose`] is the one way to its text (so every use is
/// visible to grep), `Debug` and `Display` print `<redacted>`, and the buffer is wiped when the value is dropped. agentd hands
/// it to the loopback package proxy (`pkgproxy::Secret`) and writes it nowhere.
pub struct PackagesToken {
    value: String,
    /// Test hook: told where the bytes were stored and what they held, just before the buffer is released.
    #[cfg(test)]
    probe: Option<Probe>,
}

impl PackagesToken {
    pub fn new(token: String) -> Self {
        Self {
            value: token,
            #[cfg(test)]
            probe: None,
        }
    }

    /// The text, for the one place that hands it on (agentd, to the package proxy's `Secret`).
    pub fn expose(&self) -> &str {
        self.value.as_str()
    }

    /// CodeArtifact's token alphabet: non-empty, `[A-Za-z0-9._=-]` only, so it cannot break out of
    /// the index URL or the config line agentd writes.
    pub fn shape_ok(&self) -> bool {
        !self.value.is_empty()
            && self
                .value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'='))
    }
}

impl Clone for PackagesToken {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            #[cfg(test)]
            probe: None,
        }
    }
}

impl PartialEq for PackagesToken {
    /// Compared without stopping at the first difference.
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (self.value.as_bytes(), other.value.as_bytes());
        a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

impl Eq for PackagesToken {}

impl Drop for PackagesToken {
    fn drop(&mut self) {
        // The bytes leave the `String` without a copy, are overwritten where they were stored, and then the whole allocation
        // (its spare capacity too) is wiped before it is released.
        let mut bytes = std::mem::take(&mut self.value).into_bytes();
        bytes.as_mut_slice().zeroize();
        #[cfg(test)]
        if let Some(probe) = &self.probe
            && let Ok(mut seen) = probe.lock()
        {
            *seen = Some((bytes.as_ptr() as usize, bytes.clone()));
        }
        bytes.zeroize();
    }
}

// `Drop` above wipes the memory; this marks `PackagesToken` as doing so.
impl ZeroizeOnDrop for PackagesToken {}

impl fmt::Debug for PackagesToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for PackagesToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Serialize for PackagesToken {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.value)
    }
}

impl<'de> Deserialize<'de> for PackagesToken {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(PackagesToken::new)
    }
}

impl JsonSchema for PackagesToken {
    fn schema_name() -> Cow<'static, str> {
        "PackagesToken".into()
    }

    fn schema_id() -> Cow<'static, str> {
        concat!(module_path!(), "::PackagesToken").into()
    }

    // The released definition (protocol 1.38.0), kept apart from the doc comment above so that the type's own documentation
    // can move without a schema byte moving: a change here needs a version bump and a new FROZEN line.
    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "description": "A CodeArtifact authorization token the dispatcher minted outside the VM with its own role; agentd writes it only to `package_index_path`, never where the\nengine can read it. `Debug` never shows it.",
        })
    }
}

/// `POST /invocations` answer: the turn was accepted and runs in the background.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InvocationAccepted {
    pub run_id: String,
    pub agent_id: String,
    pub turn: u32,
    pub accepted: bool,
}

/// How a turn ended, as `agentd` observed it. Telemetry only: the harness owns "done" (invariant 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

/// A finished turn: the body of [`turn_result_path`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InvocationResponse {
    /// The agent's final message of the turn, as `TurnCompleteRequest::summary` carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub run_id: String,
    pub agent_id: String,
    pub turn: u32,
    pub outcome: TurnOutcome,
    /// The engine's ACP `stopReason` (for example `end_turn`, `max_tokens`).
    pub stop_reason: Option<String>,
    pub last_commit_sha: Option<String>,
    pub bundle_uri: Option<String>,
    /// Tokens the engine reported for the turn; `null` when it reported none (Cline 3.0.65 over
    /// ACP), never zeros standing in for an absent measurement. Invocation logs stay authoritative.
    #[serde(default)]
    pub usage: Option<Usage>,
    pub error: Option<String>,
    /// SHA-256 (hex) of the prompt agentd supplied at `session/new`, when an engine ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_sha256: Option<String>,
    /// SHA-256 of the canonical JSON of `rulesManifest`, when an engine ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules_manifest: Option<RulesManifest>,
    /// Hidden paths (repository hooks, plugins, engine configs) that existed at launch and were
    /// moved out of the engine's view; absent when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quarantined: Vec<String>,
    /// Repository paths the engine changed outside the role's write globs, which agentd restored to
    /// the turn's base before the commit; absent when none (or without a role policy).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reverted: Vec<String>,
    /// The kinds of permission request agentd refused this turn, grouped by tool and reason
    /// (`TurnCompleteRequest.refused` carries the first `MAX_REFUSALS`); absent when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<crate::api::internal::Refusal>,
    /// The provider-fault evidence `TurnCompleteRequest.faultHint` carries;
    /// absent when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault_hint: Option<crate::api::internal::FaultHint>,
    /// The longest engine silence of the turn, as `TurnCompleteRequest.maxSilentGapS` carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_silent_gap_s: Option<u32>,
}

pub(crate) fn is_id(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Checks everything `agentd` relies on before it touches the worktree or the engine.
/// `<area>-<place>-<n>`: lowercase letters, then lowercase letters, then digits (`us-east-1`).
pub(crate) fn region_name_ok(r: &str) -> bool {
    let parts: Vec<&str> = r.split('-').collect();
    parts.len() == 3
        && parts[..2]
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase()))
        && !parts[2].is_empty()
        && parts[2].bytes().all(|b| b.is_ascii_digit())
}

/// The `runtime.schema.json` contract document covers both directions.
#[derive(Debug, JsonSchema)]
pub struct RuntimeContract {
    pub ping: PingResponse,
    pub invocation_accepted: InvocationAccepted,
    pub invocation_response: InvocationResponse,
    /// The direct-run contract (1.30.0): the `InvokeAgentRuntime` body, `summary.json` and the packager's evidence.
    pub direct_invocation_request: crate::direct::DirectInvocationRequest,
    pub direct_summary: crate::direct::DirectSummary,
    pub direct_evidence: crate::direct::DirectEvidence,
    /// The research route's stored objects: the item manifest, the audit object, the policy row and the summary note.
    pub research: crate::research::ResearchContract,
    /// The clean-checkout test report (1.38.0): the worker's `test-report.json` and the checked copy in `evidence/`.
    pub direct_test_report: crate::direct::TestReport,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// The one test that needs the token's private bytes: the probe is told, inside `Drop`, where the bytes were stored and what
    /// they held just before the buffer was released (the pattern of `pkgproxy::Secret`).
    #[test]
    fn dropping_a_package_token_wipes_the_buffer_it_was_stored_in() {
        let text = "t".repeat(40);
        let seen: Probe = Arc::new(Mutex::new(None));
        let mut token = PackagesToken::new(text);
        let stored_at = token.value.as_ptr() as usize;
        token.probe = Some(seen.clone());
        drop(token);
        let Some((wiped_at, bytes)) = seen.lock().ok().and_then(|mut g| g.take()) else {
            panic!("dropping a PackagesToken did not reach the wipe");
        };
        assert_eq!(
            wiped_at, stored_at,
            "the buffer that held the token is the one that was wiped, not a copy"
        );
        assert_eq!(bytes.len(), 40);
        assert!(
            bytes.iter().all(|&b| b == 0),
            "the bytes were still readable when the buffer was released"
        );
    }
}
