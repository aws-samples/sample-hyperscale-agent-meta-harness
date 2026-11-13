//! DynamoDB items, key builders and the S3 layout.
//!
//! One table `harness`. Items carry their natural ids; [`Keyed::key`] derives the `PK` and `SK`
//! attributes, so a writer adds them from the item and a key can never disagree with its item.
//! Every mutable item carries `version` for optimistic locking.

use crate::api::internal::PendingControl;
use crate::common::{BudgetLimit, Engine, Transport, Usage};
use crate::envelope::{Authority, Envelope};
use crate::goal::{GoalPhase, GoalStatus, VerdictRef};
use crate::messages::{MessageCtlItem, MessageItem, OwnerRateItem};
use crate::target::{RepoTarget, RunTarget};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Event partitions per run; harness-authored events use shard 0.
pub const EVENT_SHARDS: u8 = 32;

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Event shard: 0 for harness events, FNV-1a 64 of the agent id mod 32 for agent events.
pub fn event_shard(authority: Authority, agent_id: Option<&str>) -> u8 {
    match (authority, agent_id) {
        (Authority::Agent, Some(id)) => (fnv1a64(id.as_bytes()) % u64::from(EVENT_SHARDS)) as u8,
        _ => 0,
    }
}

/// Partition and sort key builders.
pub mod keys {
    use crate::common::{Engine, Protocol};
    use crate::target::RepoTarget;

    pub const META: &str = "META";
    pub const PR: &str = "PR";
    /// The allowlist's listing item (`ham doctor`); no target key can equal it (`cc:` and `gl:`
    /// keys carry a scheme, `gh:` keys a slash).
    pub const REPO_ALLOW_INDEX_PK: &str = "REPOALLOW#INDEX";

    pub fn run_pk(run_id: &str) -> String {
        format!("RUN#{run_id}")
    }
    /// `GOAL#root` for the run goal, `GOAL#<taskId>` for a sub-goal.
    pub fn goal_sk(task_id: Option<&str>) -> String {
        format!("GOAL#{}", task_id.unwrap_or("root"))
    }
    pub fn agent_sk(agent_id: &str) -> String {
        format!("AGENT#{agent_id}")
    }
    /// Zero-padded so turns sort numerically (n below 1,000,000).
    pub fn turn_sk(agent_id: &str, n: u32) -> String {
        format!("TURN#{agent_id}#{n:06}")
    }
    /// Every control queued for one agent: `begins_with(SK, control_prefix(agent))`.
    pub fn control_prefix(agent_id: &str) -> String {
        format!("CTRL#{agent_id}#")
    }
    /// A message of the user (protocol 1.38.0): `MSG#<messageId>`; the ULID sorts the messages of a run by when the client minted them.
    pub fn message_sk(message_id: &str) -> String {
        format!("{MESSAGE_PREFIX}{message_id}")
    }
    /// Every message of a run: `begins_with(SK, MESSAGE_PREFIX)`. It does not match [`MESSAGE_CTL`].
    pub const MESSAGE_PREFIX: &str = "MSG#";
    /// The one item every message transition of a run goes through.
    pub const MESSAGE_CTL: &str = "MSGCTL";
    /// The per-owner admission counters of the message limit: `MSGRATE#<principalArn>`.
    pub fn owner_rate_pk(principal_arn: &str) -> String {
        format!("MSGRATE#{principal_arn}")
    }
    /// One clock minute, zero padded so the counters of an owner sort by time.
    pub fn owner_rate_sk(minute: u64) -> String {
        format!("{minute:012}")
    }
    /// `control_id` is a ULID, so an agent's controls sort by creation.
    pub fn control_sk(agent_id: &str, control_id: &str) -> String {
        format!("{}{control_id}", control_prefix(agent_id))
    }
    pub fn event_pk(run_id: &str, shard: u8) -> String {
        format!("EVT#{run_id}#{shard:02}")
    }
    pub fn artifact_sk(artifact_id: &str) -> String {
        format!("ART#{artifact_id}")
    }
    pub fn catalog_sk(
        profile_id: &str,
        protocol: Protocol,
        region: &str,
        engine: Engine,
    ) -> String {
        format!(
            "{profile_id}#{}#{region}#{}",
            protocol.as_str(),
            engine.as_str()
        )
    }
    pub fn repo_allow_pk(target: &RepoTarget) -> String {
        target.allowlist_key()
    }
    /// An IAM ARN never contains `#`, so the caller part is unambiguous.
    pub fn idempotency_pk(principal_arn: &str, idempotency_key: &str) -> String {
        format!("IDEM#{principal_arn}#{idempotency_key}")
    }
}

/// S3 object layout under the artifacts bucket.
pub mod s3 {
    pub fn input_prefix(run_id: &str) -> String {
        format!("runs/{run_id}/input/")
    }
    pub fn repo_bundle_key(run_id: &str) -> String {
        format!("{}repo.bundle", input_prefix(run_id))
    }
    /// The full `POST /v1/runs` body, which the `run` execution reads.
    pub fn request_key(run_id: &str) -> String {
        format!("{}request.json", input_prefix(run_id))
    }
    /// A full-history bundle of `refs/heads/harness/<runId>` at the delivered head.
    pub fn verified_bundle_key(run_id: &str) -> String {
        format!("runs/{run_id}/deliverable/verified.bundle")
    }
    pub fn turn_prefix(run_id: &str, agent_id: &str, n: u32) -> String {
        format!("runs/{run_id}/agents/{agent_id}/turns/{n}/")
    }
    /// The stop digest of a direct run, written at every stop (protocol 1.37.0): `runs/<run>/agents/direct/turns/1/stop-digest-<n>.json`.
    pub fn direct_stop_digest_key(run_id: &str, n: u32) -> String {
        format!("{}stop-digest-{n}.json", turn_prefix(run_id, "direct", 1))
    }
    /// The audit copy of a fresh session's digest (protocol 1.37.0): `runs/<run>/agents/direct/turns/1/fresh-session-<n>.md`.
    pub fn direct_fresh_session_key(run_id: &str, n: u32) -> String {
        format!("{}fresh-session-{n}.md", turn_prefix(run_id, "direct", 1))
    }
    pub fn evidence_prefix(run_id: &str) -> String {
        format!("runs/{run_id}/evidence/")
    }
    pub fn deliverable_key(run_id: &str, repo: &str, sha7: &str) -> String {
        format!("runs/{run_id}/deliverable/{repo}-{run_id}-{sha7}.zip")
    }
    /// A held run's zip.
    pub fn partial_deliverable_key(run_id: &str, repo: &str, sha7: &str) -> String {
        format!("runs/{run_id}/deliverable/{repo}-{run_id}-{sha7}-partial.zip")
    }
    /// EVIDENCE.md, `evidence/` and their manifest, without `src/` and the bundle.
    pub fn evidence_archive_key(run_id: &str, repo: &str, sha7: &str) -> String {
        format!("runs/{run_id}/deliverable/{repo}-{run_id}-{sha7}-evidence.zip")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub pk: String,
    pub sk: String,
}

pub trait Keyed {
    fn key(&self) -> Key;
}

fn key(pk: String, sk: impl Into<String>) -> Key {
    Key { pk, sk: sk.into() }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Starting,
    Running,
    Idle,
    Stopped,
    Failed,
}

/// How a turn's session found its workspace,
/// as the dispatcher recorded it before the invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ResumePath {
    /// A new session: the worktree comes from the turn's base bundle.
    Fresh,
    /// The same microVM generation as the previous turn: nothing restarted.
    Same,
    /// A new microVM on the same session ID found session storage with `HEAD == baseSha` and a
    /// clean tree.
    Fast,
    /// A new microVM on the same session ID found no worktree, another head or a dirty tree, so
    /// the worktree is restored from the last committed bundle.
    Slow,
}

impl ResumePath {
    pub fn as_str(&self) -> &'static str {
        match self {
            ResumePath::Fresh => "fresh",
            ResumePath::Same => "same",
            ResumePath::Fast => "fast",
            ResumePath::Slow => "slow",
        }
    }
}

/// Turn-end class from the classifier (harness-core).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnClass {
    /// The engine reported completion; the harness runs gates.
    Completed,
    /// No tool call and no change; the harness runs gates.
    Idle,
    /// The turn produced nothing (counts toward `emptyTurnStreak`).
    Empty,
    /// The engine or its tools failed to execute (counts toward `execFailureStreak`).
    ExecFailure,
    /// The worker called `flag_blocked`.
    FlagBlocked,
    Cancelled,
    /// Bedrock throttling or quota beyond the retry window.
    Throttled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactKind {
    /// The deliverable zip of a complete run.
    Zip,
    /// The zip of a held run.
    Partial,
    /// One task's bundle (`ham pull --artifact bundle:<taskId>`).
    Bundle,
    /// The evidence tree without `src/` and the bundle.
    Evidence,
    /// One agent's goal log (`ham pull --artifact log:<agentId>`).
    Log,
}

/// `RUN#<runId>` / `META`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunItem {
    pub run_id: String,
    pub owner_principal_arn: String,
    pub prompt_sha256: String,
    pub target: RunTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
    pub status: GoalStatus,
    pub phase: GoalPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetLimit>,
    pub engine_policy_version: u32,
    pub created_at: String,
    /// The stage's `deployment-digest` SSM parameter when the run was created.
    /// Absent before the first deploy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_digest: Option<String>,
    /// The stage's `deployed-source-tree` SSM parameter when the run was created (drill evidence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_tree: Option<String>,
    /// The python worker runtime version the run's first dispatch found on the `live` endpoint
    /// (evidence; never a qualifier).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_version: Option<String>,
    /// The endpoint every call on the run's python sessions uses as the qualifier: `v<N>`, the
    /// endpoint deploy.sh keeps on `runtimeVersion`, so a repoint of `live` leaves an in-flight run
    /// on its version; `live` when that endpoint was missing at the first dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_endpoint: Option<String>,
    /// How the run's `run` execution ended when it ended ABORTED, FAILED or TIMED_OUT before the
    /// run finished: the run-ended reaper writes it with the run's `cleared` status.
    // Boxed (wire-identical) so the item grows by a pointer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aborted: Option<Box<crate::orchestration::RunAbortedPayload>>,
    /// Protocol 1.38.0: the id of the run a follow-up run continues from, written when control-api creates the run from a start
    /// request that carries `continueFrom`. Absent for an ordinary run and for an item written before the member existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_from: Option<String>,
    /// Why the run is `paused` or `stalled`, as a short code, a colon and a plain sentence (`total_time: the run used the 24 hours it
    /// was given`). Written by `finalize` with the status; absent for every other status and for an item written before the member existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    /// How a `paused` or `stalled` run can go on. Written with `status_reason`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_mode: Option<crate::goal::ResumeMode>,
    /// The number of the run's last slice (the session the run ended in, or is in now); absent for a run of one session written before the
    /// member existed. `start-session` writes it when it starts a slice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slice: Option<u32>,
    /// The total seconds the run may use across its slices, copied from the start request; absent means the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_run_s: Option<u32>,
    /// The engine the start request named, written when control-api creates the run. Absent means Cline: a run whose request named
    /// no engine, and every item written before the member existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<Engine>,
    pub version: u64,
}

/// `RUN#<runId>` / `GOAL#root` or `GOAL#<taskId>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GoalItem {
    pub run_id: String,
    /// `None` for the run-level goal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub objective: String,
    pub requirements: Vec<String>,
    pub spec_sha256: String,
    pub acceptance_sha256: String,
    pub status: GoalStatus,
    pub phase: GoalPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_blocker_fingerprint: Option<String>,
    #[serde(default)]
    pub same_blocker_streak: u32,
    #[serde(default)]
    pub empty_turn_streak: u32,
    #[serde(default)]
    pub exec_failure_streak: u32,
    /// Escalation ladder rung within the blocked audit (0-2).
    #[serde(default)]
    pub ladder_rung: u32,
    #[serde(default)]
    pub evaluator_verdicts: Vec<VerdictRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    #[serde(default)]
    pub tokens_used: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_clock_budget_s: Option<u64>,
    #[serde(default)]
    pub time_used_s: u64,
    #[serde(default)]
    pub turn_count: u32,
    pub version: u64,
}

/// `RUN#<runId>` / `AGENT#<agentId>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentItem {
    pub run_id: String,
    pub agent_id: String,
    pub task_id: String,
    pub attempt: u32,
    pub runtime_arn: String,
    pub runtime_session_id: String,
    pub generation: u32,
    pub engine: Engine,
    pub engine_version: String,
    pub transport: Transport,
    pub model_id: String,
    pub pool: String,
    pub status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_at: Option<String>,
    /// Credential binding for `/internal/creds`; written by the dispatcher when it mints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creds: Option<CredsBinding>,
    /// The microVM generation on this agent's sessions: 1 at the first turn, one more at
    /// every planned handoff, lost-turn recovery or fresh session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_generation: Option<u32>,
    /// When the current generation started (the dispatcher's clock); its remaining lifetime is
    /// the runtime's `maxLifetime` less the time since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_started_at: Option<String>,
    /// The runtime version of the agent's run (evidence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_version: Option<String>,
    /// The endpoint (the qualifier) every call on this agent's session uses; absent: `live`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_endpoint: Option<String>,
    /// The engine-policy version whose route picked the session's engine; absent when the engine
    /// came from the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_policy_version: Option<u32>,
    /// The harness's own controls this agent takes: `message.next` and `message.now` (`messages::MESSAGE_NEXT_CONTROL`,
    /// `MESSAGE_NOW_CONTROL`), written at registration. control-api refuses a message to a run whose Agent lists none (409
    /// `no-message-control`: a run on an image that cannot take one). Absent on an item written before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harness_controls: Vec<String>,
    /// The features of the worker image this agent runs: `transcript.v1` when agentd writes the stored conversation
    /// (`transcript::TRANSCRIPT_FEATURE`), written at registration beside `harnessControls`. The transcript route answers `unsupported`
    /// for a run whose Agent lists none. Absent on an item written before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// The machine kind this agent's session runs on: `microvm` or `instances` (`direct::ComputeMode::word`). Written
    /// once, when the session starts, from the user's choice and the stage's policy; a retry reads it back and never decides again. Absent on an
    /// item written before: the microVM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute: Option<String>,
    /// The AgentCore capacity provider of an `instances` session (its id, `<name>-<10 characters>`): what `DeleteCapacityProviderSession` needs
    /// beside `runtime_session_id` to end the session and its billing. Absent for the microVM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_provider_id: Option<String>,
    /// Written only with the rotation to a new slice; absent on every other item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce_ciphertext: Option<String>,
    pub version: u64,
}

/// What `/internal/creds` checks before it re-mints: the current nonce (stored only as
/// SHA-256), the revocation flag, and the tags and session policy it must reproduce exactly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CredsBinding {
    /// Lowercase hex SHA-256 of the current refresh nonce.
    pub nonce_sha256: String,
    pub revoked: bool,
    /// Session tags besides run, agent and engine (which the Agent item already holds).
    pub tenant: String,
    pub mode: String,
    /// The inline STS session policy (JSON) that narrows Bedrock to the leased models.
    pub session_policy: String,
    /// Lowercase hex SHA-256 of the run's completion key: the secret that only agentd holds and that worker-api
    /// requires on `/internal/events` and `/internal/turn-complete`, so that the engine, which holds the same session credentials, cannot
    /// complete a turn or write events itself. The key is derived from the run's first nonce ([`CredsBinding::completion_key`]) and is
    /// never stored; this is the hash of it. Absent on an item written before this version: such a run is accepted as it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_key_sha256: Option<String>,
    /// Protocol 1.57.0: `true` when this run's agentd sends the version records of its end-of-run objects (`TurnCompleteRequest.outputPins`)
    /// and the service must read those versions, never the current object. run-session sets it when it launches the session, from the same code
    /// that stores the completion key; a request cannot set or clear it. Absent on an item written before this version: such a run is read as it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_pins_required: Option<bool>,
}

/// The request header that carries the completion key to `/internal/events` and `/internal/turn-complete`.
pub const COMPLETION_KEY_HEADER: &str = "x-hamh-completion-key";

/// What worker-api finds when it compares the key a request carries with the one the run's Agent item names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKeyCheck {
    /// The item names no key (it was written before the key existed): the request is accepted as it was.
    Legacy,
    /// The request carries the run's key.
    Valid,
    /// The item names a key and the request carries none, or another one, or one that is not a key.
    Refused,
}

impl CredsBinding {
    pub fn hash_nonce(nonce: &str) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(nonce.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// True when `nonce` is the current one; an empty nonce never matches.
    pub fn nonce_matches(&self, nonce: &str) -> bool {
        !nonce.is_empty() && Self::hash_nonce(nonce) == self.nonce_sha256
    }

    /// The run's completion key: lowercase hex SHA-256 of a fixed label, a zero byte and the bytes of the run's FIRST nonce. The first nonce is
    /// made once by run-session and handed to agentd in the invocation; later refreshes rotate the nonce and leave the key as it was. Only
    /// run-session and agentd ever hold the nonce, so only they can make the key; the engine is given neither.
    pub fn completion_key(first_nonce: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(b"hamh-completion-key-v1\0");
        h.update(first_nonce.as_bytes());
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Whether the service reads the run's end-of-run objects by the version records agentd sent.
    pub fn output_pins_required(&self) -> bool {
        self.output_pins_required == Some(true)
    }

    /// Lowercase hex SHA-256 of a completion key, as the Agent item stores it.
    pub fn hash_completion_key(key: &str) -> String {
        Self::hash_nonce(key)
    }

    /// Compares the key a request carried with the item's. The comparison does not stop at the first byte that differs.
    pub fn check_completion_key(&self, presented: Option<&str>) -> CompletionKeyCheck {
        let Some(want) = self.completion_key_sha256.as_deref() else {
            return CompletionKeyCheck::Legacy;
        };
        let Some(key) = presented.filter(|k| !k.is_empty()) else {
            return CompletionKeyCheck::Refused;
        };
        let got = Self::hash_completion_key(key);
        let same = got.len() == want.len()
            && got
                .bytes()
                .zip(want.bytes())
                .fold(0u8, |diff, (a, b)| diff | (a ^ b))
                == 0;
        if same {
            CompletionKeyCheck::Valid
        } else {
            CompletionKeyCheck::Refused
        }
    }
}

/// `RUN#<runId>` / `TURN#<agentId>#<n>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnItem {
    pub run_id: String,
    pub agent_id: String,
    pub n: u32,
    /// KMS ciphertext of the Step Functions task token.
    pub task_token_ciphertext: String,
    pub started_at: String,
    /// Set by the dispatcher once InvokeAgentRuntime accepted the turn (a retry never re-invokes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invoked_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_commit_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_result_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_class: Option<TurnClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Engine-reported tokens; absent when the engine reported none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// SHA-256 of the compiled rules manifest used on this turn.
    pub rules_digest: String,
    /// The turn's role slug, written by the dispatcher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The turn's rules manifest, written with `rulesDigest` at turn-complete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules_manifest: Option<crate::runtime::RulesManifest>,
    /// The agent's microVM generation this turn runs in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_generation: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_session_id: Option<String>,
    /// The runtime version the turn was invoked on (evidence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_version: Option<String>,
    /// The endpoint (the qualifier) the turn was invoked at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_endpoint: Option<String>,
    /// The committed head the turn starts from (the previous turn's `lastCommitSha`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<ResumePath>,
    /// Why a new generation started or took the slow path, for example `planned-handoff`,
    /// `session-lost`, `storage-reset`, `head-mismatch` or `dirty-worktree`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_reason: Option<String>,
    /// The HEAD the dispatcher's workspace probe found in session storage (`none` without one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<Engine>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Set by the sweeper when the turn stayed in flight without a heartbeat for 600 s:
    /// the session died, so the next dispatch recovers it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lost_at: Option<String>,
    /// The Region the engine called the model in, when the goal step chose a route (1.22.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// The attempt on this route when the fault ended the turn (1 is the first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    /// The ladder level the turn ran at: 1 same route, 2 another route, 3 another model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_level: Option<u8>,
    /// The `ROUTELADDER` version the run froze.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ladder_version: Option<u32>,
    /// The longest silence, in seconds, between two engine updates in the turn (agentd's metric).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_silent_gap_s: Option<u32>,
    /// The output-token cap dispatch resolved for the turn,
    /// written with the item so a retried dispatch sends the same cap and a
    /// catalog change never moves a running agent (1.24.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// The context window dispatch resolved for the turn,
    /// written with the item so a retried dispatch sends the same window (1.25.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,
    /// The transport of the engine dispatch resolved for the turn, written with `engine` so a retried
    /// dispatch reads the engine, its transport and the policy row from this one item (1.25.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    /// The engine-policy version whose route picked `engine`; absent when the engine came from the
    /// run or the ladder member (1.25.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_policy_version: Option<u32>,
    /// The millisecond (Unix epoch) the first attempt of dispatch created this item (1.25.0). The
    /// turn's `context.changed` takes the time of its ULID from it, so the id sorts after every
    /// event written before the turn (viewers read `SK > :after`, so an older id would be skipped by
    /// a poll that already moved its cursor) and a retried dispatch writes the same item. Absent on
    /// an item from before, which falls back to the second of `startedAt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    pub version: u64,
}

/// `RUN#<runId>` / `CTRL#<agentId>#<controlId>`: a steer, cancel or approval answer queued for one
/// agent. `/internal/events` hands each out once: the response that delivers it first
/// sets `deliveredAt` by a conditional update.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ControlItem {
    pub run_id: String,
    pub agent_id: String,
    /// A ULID.
    pub control_id: String,
    pub control: PendingControl,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<String>,
}

/// `EVT#<runId>#<shard>` / `<eventId>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventItem {
    pub envelope: Envelope,
}

impl EventItem {
    pub fn shard(&self) -> u8 {
        event_shard(self.envelope.authority, self.envelope.agent_id.as_deref())
    }
}

/// `RUN#<runId>` / `ART#<artifactId>`; immutable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactItem {
    pub run_id: String,
    pub artifact_id: String,
    pub kind: ArtifactKind,
    pub s3_uri: String,
    pub sha256: String,
    pub size_bytes: u64,
}

/// Repository allowlist entry, changed only by `ham-admin repos`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepoAllowItem {
    pub target: RepoTarget,
    pub added_by: String,
    pub added_at: String,
}

/// `IDEM#<principalArn>#<key>` / `META`: one `POST /v1/runs` `Idempotency-Key` claim.
/// Field names avoid `ownerPrincipalArn` and `createdAt`, so the claim stays out of the sparse GSI1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IdempotencyItem {
    pub principal_arn: String,
    pub idempotency_key: String,
    pub run_id: String,
    /// SHA-256 of the canonical request; a reuse of the key with another request is refused.
    pub request_sha256: String,
    pub claimed_at: String,
}

impl Keyed for RunItem {
    fn key(&self) -> Key {
        key(keys::run_pk(&self.run_id), keys::META)
    }
}
impl Keyed for GoalItem {
    fn key(&self) -> Key {
        key(
            keys::run_pk(&self.run_id),
            keys::goal_sk(self.task_id.as_deref()),
        )
    }
}
impl Keyed for AgentItem {
    fn key(&self) -> Key {
        key(keys::run_pk(&self.run_id), keys::agent_sk(&self.agent_id))
    }
}
impl Keyed for TurnItem {
    fn key(&self) -> Key {
        key(
            keys::run_pk(&self.run_id),
            keys::turn_sk(&self.agent_id, self.n),
        )
    }
}
impl Keyed for ControlItem {
    fn key(&self) -> Key {
        key(
            keys::run_pk(&self.run_id),
            keys::control_sk(&self.agent_id, &self.control_id),
        )
    }
}
impl Keyed for EventItem {
    fn key(&self) -> Key {
        key(
            keys::event_pk(&self.envelope.run_id, self.shard()),
            self.envelope.event_id.clone(),
        )
    }
}
impl Keyed for ArtifactItem {
    fn key(&self) -> Key {
        key(
            keys::run_pk(&self.run_id),
            keys::artifact_sk(&self.artifact_id),
        )
    }
}
impl Keyed for RepoAllowItem {
    fn key(&self) -> Key {
        key(keys::repo_allow_pk(&self.target), keys::META)
    }
}
impl Keyed for IdempotencyItem {
    fn key(&self) -> Key {
        key(
            keys::idempotency_pk(&self.principal_arn, &self.idempotency_key),
            keys::META,
        )
    }
}

/// Every item type, tagged by `entity`; `items.schema.json` is this type's schema.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "entity", rename_all = "camelCase")]
pub enum StoreItem {
    Run(RunItem),
    Goal(GoalItem),
    // Boxed: the Agent item grew past the size clippy accepts for the largest variant. The
    // wire form is unchanged.
    Agent(Box<AgentItem>),
    Turn(Box<TurnItem>),
    Control(ControlItem),
    Event(EventItem),
    Artifact(ArtifactItem),
    RepoAllow(RepoAllowItem),
    Idempotency(IdempotencyItem),
    /// Protocol 1.38.0: one message of the user (`MSG#<id>`).
    Message(Box<MessageItem>),
    /// Protocol 1.38.0: the item every message transition of a run goes through (`MSGCTL`).
    MessageCtl(Box<MessageCtlItem>),
    /// Protocol 1.38.0: an owner's admissions in one clock minute.
    OwnerRate(OwnerRateItem),
}

impl Keyed for StoreItem {
    fn key(&self) -> Key {
        match self {
            StoreItem::Run(i) => i.key(),
            StoreItem::Goal(i) => i.key(),
            StoreItem::Agent(i) => i.key(),
            StoreItem::Turn(i) => i.key(),
            StoreItem::Control(i) => i.key(),
            StoreItem::Event(i) => i.key(),
            StoreItem::Artifact(i) => i.key(),
            StoreItem::RepoAllow(i) => i.key(),
            StoreItem::Idempotency(i) => i.key(),
            StoreItem::Message(i) => i.key(),
            StoreItem::MessageCtl(i) => i.key(),
            StoreItem::OwnerRate(i) => i.key(),
        }
    }
}
