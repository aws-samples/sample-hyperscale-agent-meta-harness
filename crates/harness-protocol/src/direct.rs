//! The direct-run runtime contract (protocol 1.30.0).
//!
//! A direct run is ONE Cline session in ONE AgentCore microVM, driven by ONE prompt.
//!
//! - [`DirectInvocationRequest`]: the `InvokeAgentRuntime` body `run-session` sends and agentd receives,
//!   and [`validate_direct_invocation`], which agentd runs before it touches anything;
//! - [`DirectOutcome`] and its frozen [`DirectOutcome::label`] texts: how Cline stopped, in the words the
//!   product shows. Only `reported_done` says "Cline reported done"; no label claims the harness checked
//!   anything ([`banned_claim_word`]);
//! - [`DirectSummary`]: the `summary.json` agentd uploads at the end of the turn;
//! - [`DirectEvidence`]: what the packager renders into EVIDENCE.md;
//! - [`s3`]: the object keys of one direct run;
//!
//! Reading rule: requests are strict (`deny_unknown_fields`: a sender and a receiver of one deployment agree
//! on every member); `summary.json` and the evidence are read leniently (an unknown key is ignored, and a
//! writer adds a key only with a protocol minor bump).

use crate::api::internal::{FaultHint, ProcessCredentials};
use crate::common::{Engine, Protocol, Usage};
use crate::research::DirectResearchNote;
use crate::runtime::{PackagesToken, RepoSource, TurnOutcome, is_id, region_name_ok};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

pub use crate::direct_attachments::*;
pub use crate::direct_build::*;
pub use crate::direct_compute::*;
pub use crate::direct_continue::*;
pub use crate::direct_engine_config::*;
pub use crate::direct_flow::*;
pub use crate::direct_goal::*;
pub use crate::direct_input::*;
pub use crate::direct_limits::*;
pub use crate::direct_request::*;
pub use crate::direct_review::*;
pub use crate::direct_skills::*;
pub use crate::test_report::*;
pub use crate::tools::{DirectToolsCounts, DirectToolsNote};

// ---- constants -------------------------------------------------------------------------

/// The one agent of a direct run.
pub const DIRECT_AGENT_ID: &str = "direct";
/// The first turn of a direct run. A run that outlives one microVM session is a list of slices, and slice n is turn n of the one direct
/// agent (its own Turn item, its own `turns/n/` prefix, its own runtime session id). A run that never renews its session has this one turn.
pub const DIRECT_TURN: u32 = 1;
/// The most slices (turns) one run may have. A day of 27,900 s slices is 4; a week is 22; the rest is room for restarts after lost sessions
/// and for the short slices of a test.
pub const MAX_SLICES: u32 = 256;
/// The seconds a slice keeps after its prompt deadline for the end sequence (commit, bundle, uploads, turn-complete). It is the 900 s that
/// [`MAX_DIRECT_TIMEOUT_S`] leaves of the microVM lifetime.
pub const SLICE_RESERVE_S: u32 = 900;
/// The least seconds of engine time worth a new slice. A slice with less than this to run is not started; the run ends paused instead.
pub const MIN_SLICE_ENGINE_S: u32 = 900;
/// The total seconds a run may use when the request names none (24 hours).
pub const DEFAULT_MAX_RUN_S: u32 = 24 * 3600;
/// The least total a request may name: one reserve and one useful slice.
pub const MIN_MAX_RUN_S: u32 = SLICE_RESERVE_S + MIN_SLICE_ENGINE_S;
/// The most total a request may name (30 days). Step Functions keeps an execution for a year; the limit is the product's.
pub const MAX_MAX_RUN_S: u32 = 30 * 24 * 3600;
/// Sessions in a row that were lost (no result, no heartbeat, a start that failed) before the run is called stalled.
pub const MAX_LOST_SLICES: u32 = 3;
/// Slices in a row that ran to their deadline and left the tree as they found it before the run is called stalled.
pub const MAX_EMPTY_SLICES: u32 = 3;

/// Whether `s` is a total run time a request may name.
pub fn max_run_s_ok(s: u32) -> bool {
    (MIN_MAX_RUN_S..=MAX_MAX_RUN_S).contains(&s)
}
/// `AgentItem.task_id` of the direct agent (the field is required by the store).
pub const DIRECT_TASK_ID: &str = "root";
/// The engine of this release: the profile `engine/profiles/cline.toml`.
pub const DIRECT_ENGINE: &str = "cline";
/// The microVM lifetime (28,800 s) less a 900 s reserve for commit, bundle, upload and turn-complete.
pub const MAX_DIRECT_TIMEOUT_S: u32 = 27_900;
/// How often agentd looks at the work tree for a copy while the prompt runs.
/// agentd also copies after a tool call that changed the tree, and when the engine
/// has stopped.
pub const DEFAULT_SNAPSHOT_EVERY_S: u32 = 60;
/// The longest silence limit an invocation may ask for.
pub const MAX_PROVIDER_SILENCE_S: u32 = 3_600;
/// The shortest non-zero snapshot interval.
pub const MIN_SNAPSHOT_EVERY_S: u32 = 60;
/// The longest snapshot interval.
pub const MAX_SNAPSHOT_EVERY_S: u32 = 3_600;
/// The largest `InvokeAgentRuntime` body; the prompt, the rules and the nudge are not in it.
pub const MAX_DIRECT_BODY_BYTES: usize = 64 * 1024;
/// `DirectSummary.schema_version`.
pub const SUMMARY_SCHEMA_VERSION: u32 = 1;
/// The largest `summary.json`: 1 MiB. The file is an S3 object that run-session and the head route read whole (Lambda functions with 256 MB), so
/// the bound guards a runaway, not the message.
pub const MAX_DIRECT_SUMMARY_BYTES: usize = 1024 * 1024;
/// The most characters of Cline's final message that `summary.json` keeps (the end of the text): 200,000 characters, about 50,000 tokens, more than
/// a model writes in one reply. The message sits whole in `summary.json` and in `evidence/direct.json`; the copies that ride in Step Functions
/// state hold [`MAX_STATE_MESSAGE_BYTES`].
pub const MAX_CLINE_MESSAGE_CHARS: usize = 200_000;
/// The most bytes of the JSON text of Cline's final message in one copy that rides in Step Functions state ([`message_preview`]). A state holds
/// 262,144 bytes in all and the message sits in it twice (the turn result and the evidence of the plan); going over ends the run with an error
/// that no `Catch` sees.
pub const MAX_STATE_MESSAGE_BYTES: usize = 32 * 1024;
/// The most characters of a `detail` or an `error`.
pub const MAX_DIRECT_DETAIL_CHARS: usize = 1_000;
/// The most tries `summary.json` lists. With failover a run can try many times (the resumes on each route, the waits with every route
/// down); past this agentd lists the first and the last ones and cuts the middle.
pub const MAX_DIRECT_ATTEMPTS: usize = 96;
/// The most routes a request lists after the preferred model (protocol 1.34.0, provider failover).
pub const MAX_DIRECT_ROUTES: usize = 8;
/// The most switches `summary.json` lists (protocol 1.34.0).
pub const MAX_DIRECT_SWITCHES: usize = 96;
/// The stage word of the direct stage.
pub const DIRECT_STAGE: &str = "direct";
/// The resource-name prefix of the direct stage.
pub const DIRECT_PREFIX: &str = "hamh-direct";
/// The prefix where a name may not contain a hyphen (AgentCore runtime names).
pub const DIRECT_PREFIX_UNDERSCORE: &str = "hamh_direct";

// ---- the invocation ----------------------------------------------------------------------------------------------------

/// The `kind` member of the invocation: the one word that tells agentd this is a direct turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum DirectKind {
    #[serde(rename = "direct")]
    Direct,
}

/// An object in the harness bucket that the receiver fetches and checks against its SHA-256.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputRef {
    pub uri: String,
    /// 64 lowercase hex characters.
    pub sha256: String,
}

/// One row of `engine/catalog/models.json`, as the session needs it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DirectModelSpec {
    /// The Bedrock model id, for example `us.anthropic.claude-opus-5-5`.
    pub id: String,
    /// The inference Region the engine calls; it may differ from the stage Region.
    pub region: String,
    /// `converse` in v1.
    pub protocol: Protocol,
    /// Written to the engine's provider settings as the context window.
    pub context_tokens: u32,
    /// Written to the engine's provider settings as the output limit.
    pub max_output_tokens: u32,
    /// Protocol 1.47.0: the reasoning level this model's row of `engine/catalog/models.json` gives, one of
    /// [`REASONING_LEVELS`]. agentd launches the engine with it (`--thinking <level>`) when the user's `reasoning.effort` is `default` (or the key
    /// is not set), and with the user's own level or none (`off`) otherwise. Absent: the engine is launched as in 1.45.0, with no reasoning
    /// argument. The launch level is set once for the engine process, so it is the level of the model the run starts on, also after a switch to
    /// a route of another model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<String>,
}

/// `POST /invocations` body of a direct run. At most [`MAX_DIRECT_BODY_BYTES`].
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DirectInvocationRequest {
    pub kind: DirectKind,
    pub run_id: String,
    /// [`DIRECT_AGENT_ID`].
    pub agent_id: String,
    /// [`DIRECT_TURN`].
    pub turn: u32,
    pub runtime_session_id: String,
    /// Session credentials of `harness-worker-session`, minted outside the VM.
    pub credentials: ProcessCredentials,
    /// Rotating refresh nonce for `/internal/creds`.
    pub nonce: String,
    /// `https://<id>.execute-api.<region>.amazonaws.com/direct`.
    pub internal_api_url: String,
    /// The stage Region: S3, the internal API and CodeArtifact.
    pub region: String,
    /// The engine profile name, `cline`.
    pub engine: String,
    pub model: DirectModelSpec,
    /// Protocol 1.34.0: the routes agentd may switch to after `model`, in the order it tries them: the other
    /// profile of the same model first, then the other supported models. Each has a smoke record in `engine/catalog/models.json`; the
    /// control plane builds the list and the session credentials cover exactly these routes. Absent or empty: no failover. At most
    /// [`MAX_DIRECT_ROUTES`]; none repeats `model` or another route.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failover: Vec<DirectModelSpec>,
    /// `runs/<runId>/input/request.json` and its SHA-256: the prompt, the rules and the optional nudge.
    pub request: InputRef,
    /// `S3Bundle { uri: runs/<runId>/input/repo.bundle }`.
    pub repo: RepoSource,
    /// 1 to [`MAX_DIRECT_TIMEOUT_S`]; run-session sends the maximum.
    pub timeout_s: u32,
    /// The engine silence limit; agentd's default is 900 s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_silence_s: Option<u32>,
    /// Snapshot interval; absent means [`DEFAULT_SNAPSHOT_EVERY_S`], 0 turns snapshots off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_every_s: Option<u32>,
    /// The CodeArtifact token, kept in agentd's memory only; absent on a run without packages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packages_token: Option<PackagesToken>,
    /// When `packages_token` stops working, in Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packages_token_expires_at: Option<u64>,
    /// Protocol 1.37.0: how many seconds agentd keeps the session alive for a person's answer when
    /// Cline fails or gives up. Absent means 0: no question is raised and the run ends at the stop, as before. At most
    /// [`MAX_INPUT_WAIT_S`]; run-session copies the request's value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_wait_s: Option<u32>,
    /// What a slice of a run that renews its session needs to know. Absent for a run of one session,
    /// so an agentd image that predates the member parses what it always parsed. Required from turn 2 on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slice: Option<DirectSlice>,
}

/// The slice member of an invocation (protocol 1.55.0).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DirectSlice {
    /// No slice follows this one: when its prompt deadline comes the run ends (paused, at its total time). When `false`, the deadline is a
    /// handoff: agentd ends the slice the same way and says so in turn-complete (`handoff`), and the service starts the next slice.
    pub last: bool,
    // The value is the original base commit, as the service holds it. agentd measures from the DOX starter commit it made on that head when the
    // history the slice restored holds that commit (`dox::settle`), else from the original base. The description below is the text that
    // `schemas/runtime.schema.json` pins for this protocol version, so it keeps its words until the next protocol version.
    /// Turn 2 and later: the run's original base commit (40 lowercase hex). Changed-file counts, the no-changes label, the review round and the
    /// summary are measured against it, not against the head the slice restored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// Turn 2 and later: the commit the restored repository must be at (40 lowercase hex); agentd refuses to start the engine on any other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_head: Option<String>,
    /// Turn 2 and later: how many engine sessions the earlier slices used, so the `new-session` events of this slice go on counting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions_before: Option<u32>,
    /// Turn 2 and later: the last message the earlier slice's Cline wrote (at most [`MAX_SLICE_BRIEF_BYTES`] bytes), from the turn-complete the
    /// service already holds. The new session's first prompt quotes it as data, never as an instruction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief: Option<String>,
}

/// The most bytes of the earlier slice's last message that an invocation carries.
pub const MAX_SLICE_BRIEF_BYTES: usize = 16 * 1024;

impl fmt::Debug for DirectInvocationRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectInvocationRequest")
            .field("kind", &self.kind)
            .field("run_id", &self.run_id)
            .field("agent_id", &self.agent_id)
            .field("turn", &self.turn)
            .field("runtime_session_id", &self.runtime_session_id)
            .field("credentials", &self.credentials)
            .field("nonce", &"<redacted>")
            .field("internal_api_url", &self.internal_api_url)
            .field("region", &self.region)
            .field("engine", &self.engine)
            .field("model", &self.model)
            .field("failover", &self.failover)
            .field("request", &self.request)
            .field("repo", &self.repo)
            .field("timeout_s", &self.timeout_s)
            .field("provider_silence_s", &self.provider_silence_s)
            .field("snapshot_every_s", &self.snapshot_every_s)
            .field("packages_token", &self.packages_token)
            .field("packages_token_expires_at", &self.packages_token_expires_at)
            .field("input_wait_s", &self.input_wait_s)
            .finish()
    }
}

/// The first rule of [`validate_direct_invocation`] that a request breaks. The message names the rule and never
/// a value, so it is safe in the `400` body agentd returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectInvocationError {
    BadAgentId,
    BadTurn,
    /// A slice after the first without the member that names its original base and the head it restores.
    BadSlice,
    BadRunId,
    /// `<runId>.direct` is the STS role session name, at most 64 characters.
    RoleSessionNameTooLong,
    BadRuntimeSessionId,
    NotSessionCredentials,
    MissingNonce,
    BadPackagesToken,
    InternalApiNotHttps,
    BadTimeout,
    BadProviderSilence,
    BadSnapshotInterval,
    /// `inputWaitS` is 0 or above [`MAX_INPUT_WAIT_S`] (1.37.0).
    BadInputWait,
    BadModelId,
    BadModelRegion,
    BadModelWindow,
    /// `defaultReasoningEffort` of the model or of a failover route is not one of [`REASONING_LEVELS`] (1.47.0).
    BadReasoningEffort,
    /// A failover route breaks the rules of `model` (id, Region, windows) or names a Mantle route.
    BadFailoverRoute,
    /// More than [`MAX_DIRECT_ROUTES`] failover routes.
    TooManyFailoverRoutes,
    /// A failover route repeats the model or another route.
    DuplicateFailoverRoute,
    BadEngine,
    BadRequestRef,
    BadRepo,
}

impl fmt::Display for DirectInvocationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadAgentId => write!(f, "agentId must be \"{DIRECT_AGENT_ID}\""),
            Self::BadTurn => write!(f, "turn must be 1 to {MAX_SLICES}"),
            Self::BadSlice => write!(
                f,
                "a slice after the first needs a slice member with baseSha and restoreHead, each a 40 character lowercase commit id"
            ),
            Self::BadRunId => write!(f, "runId must be [A-Za-z0-9-]+"),
            Self::RoleSessionNameTooLong => {
                write!(f, "runId.{DIRECT_AGENT_ID} exceeds 64 characters")
            }
            Self::BadRuntimeSessionId => {
                write!(f, "runtimeSessionId must be 33 to 256 characters")
            }
            Self::NotSessionCredentials => write!(
                f,
                "credentials must be version-1 session credentials (ASIA...)"
            ),
            Self::MissingNonce => write!(f, "nonce is required"),
            Self::BadPackagesToken => write!(f, "packagesToken must be [A-Za-z0-9._=-]+"),
            Self::InternalApiNotHttps => write!(f, "internalApiUrl must be https"),
            Self::BadTimeout => write!(f, "timeoutS must be 1 to {MAX_DIRECT_TIMEOUT_S}"),
            Self::BadProviderSilence => {
                write!(f, "providerSilenceS must be 1 to {MAX_PROVIDER_SILENCE_S}")
            }
            Self::BadSnapshotInterval => write!(
                f,
                "snapshotEveryS must be 0 or {MIN_SNAPSHOT_EVERY_S} to {MAX_SNAPSHOT_EVERY_S}"
            ),
            Self::BadInputWait => write!(f, "inputWaitS must be 1 to {MAX_INPUT_WAIT_S}"),
            Self::BadModelId => write!(f, "model.id must be [A-Za-z0-9._:-]+"),
            Self::BadModelRegion => write!(f, "model.region must be an AWS Region name"),
            Self::BadModelWindow => {
                write!(
                    f,
                    "model.contextTokens and model.maxOutputTokens must be at least 1"
                )
            }
            Self::BadReasoningEffort => write!(
                f,
                "defaultReasoningEffort of the model and of each failover route must be one of {}",
                REASONING_LEVELS.join(", ")
            ),
            Self::BadFailoverRoute => write!(
                f,
                "each failover route needs an id of [A-Za-z0-9._:-]+, an AWS Region name and windows of at least 1"
            ),
            Self::TooManyFailoverRoutes => {
                write!(f, "failover lists at most {MAX_DIRECT_ROUTES} routes")
            }
            Self::DuplicateFailoverRoute => {
                write!(f, "a failover route repeats the model or another route")
            }
            Self::BadEngine => write!(
                f,
                "engine must be an engine profile name ({})",
                crate::common::ENGINE_NAME_PATTERN
            ),
            Self::BadRequestRef => write!(
                f,
                "request must be s3://<bucket>/runs/<runId>/input/request.json with a lowercase hex SHA-256"
            ),
            Self::BadRepo => write!(
                f,
                "repo must be an S3Bundle at s3://<bucket>/runs/<runId>/input/repo.bundle in the request's bucket"
            ),
        }
    }
}

impl std::error::Error for DirectInvocationError {}

pub fn validate_direct_invocation(
    req: &DirectInvocationRequest,
) -> Result<(), DirectInvocationError> {
    use DirectInvocationError as E;
    // 1. identity
    if req.agent_id != DIRECT_AGENT_ID {
        return Err(E::BadAgentId);
    }
    if !(DIRECT_TURN..=MAX_SLICES).contains(&req.turn) {
        return Err(E::BadTurn);
    }
    if req.turn > DIRECT_TURN {
        let ok = req.slice.as_ref().is_some_and(|s| {
            s.base_sha.as_deref().is_some_and(commit_id_ok)
                && s.restore_head.as_deref().is_some_and(commit_id_ok)
        });
        if !ok {
            return Err(E::BadSlice);
        }
    }
    if req
        .slice
        .as_ref()
        .and_then(|s| s.brief.as_deref())
        .is_some_and(|b| b.len() > MAX_SLICE_BRIEF_BYTES)
    {
        return Err(E::BadSlice);
    }
    if !is_id(&req.run_id) {
        return Err(E::BadRunId);
    }
    if req.run_id.len() + 1 + DIRECT_AGENT_ID.len() > 64 {
        return Err(E::RoleSessionNameTooLong);
    }
    // 2. the runtime session
    if !(33..=256).contains(&req.runtime_session_id.len()) {
        return Err(E::BadRuntimeSessionId);
    }
    // 3. credentials and nonce
    if req.credentials.version != 1 || !req.credentials.access_key_id.starts_with("ASIA") {
        return Err(E::NotSessionCredentials);
    }
    if req.nonce.is_empty() {
        return Err(E::MissingNonce);
    }
    // 4. the package token
    if req.packages_token.as_ref().is_some_and(|t| !t.shape_ok()) {
        return Err(E::BadPackagesToken);
    }
    // 5. the internal API
    if !req.internal_api_url.starts_with("https://") {
        return Err(E::InternalApiNotHttps);
    }
    // 6. limits
    if req.timeout_s == 0 || req.timeout_s > MAX_DIRECT_TIMEOUT_S {
        return Err(E::BadTimeout);
    }
    if req
        .provider_silence_s
        .is_some_and(|s| s == 0 || s > MAX_PROVIDER_SILENCE_S)
    {
        return Err(E::BadProviderSilence);
    }
    if req
        .snapshot_every_s
        .is_some_and(|s| s != 0 && !(MIN_SNAPSHOT_EVERY_S..=MAX_SNAPSHOT_EVERY_S).contains(&s))
    {
        return Err(E::BadSnapshotInterval);
    }
    if req
        .input_wait_s
        .is_some_and(|s| s == 0 || s > MAX_INPUT_WAIT_S)
    {
        return Err(E::BadInputWait);
    }
    // 7. the model
    let m = &req.model;
    if m.id.is_empty()
        || !m
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
    {
        return Err(E::BadModelId);
    }
    if !region_name_ok(&m.region) {
        return Err(E::BadModelRegion);
    }
    if m.context_tokens == 0 || m.max_output_tokens == 0 {
        return Err(E::BadModelWindow);
    }
    if !reasoning_level_ok(m.default_reasoning_effort.as_deref()) {
        return Err(E::BadReasoningEffort);
    }
    // 7b. the failover routes
    if req.failover.len() > MAX_DIRECT_ROUTES {
        return Err(E::TooManyFailoverRoutes);
    }
    for r in &req.failover {
        let id_ok = !r.id.is_empty()
            && r.id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'));
        if !id_ok || !region_name_ok(&r.region) || r.context_tokens == 0 || r.max_output_tokens == 0
        {
            return Err(E::BadFailoverRoute);
        }
        if !reasoning_level_ok(r.default_reasoning_effort.as_deref()) {
            return Err(E::BadReasoningEffort);
        }
    }
    let mut seen = std::collections::BTreeSet::from([m.id.as_str()]);
    if !req.failover.iter().all(|r| seen.insert(r.id.as_str())) {
        return Err(E::DuplicateFailoverRoute);
    }
    // 8. the engine
    if Engine::new(&req.engine).is_err() {
        return Err(E::BadEngine);
    }
    // 9. the inputs: the same bucket, this run's keys
    let request_bucket = match s3::split_uri(&req.request.uri) {
        Some((bucket, key)) if key == s3::request_key(&req.run_id) => bucket,
        _ => return Err(E::BadRequestRef),
    };
    if !sha256_hex_ok(&req.request.sha256) {
        return Err(E::BadRequestRef);
    }
    match &req.repo {
        RepoSource::S3Bundle { uri } => match s3::split_uri(uri) {
            Some((bucket, key))
                if bucket == request_bucket
                    && (key == s3::repo_bundle_key(&req.run_id)
                        || (req.turn > DIRECT_TURN
                            && earlier_slice_bundle(&req.run_id, req.turn, key))) => {}
            _ => return Err(E::BadRepo),
        },
        RepoSource::Local { .. } => return Err(E::BadRepo),
    }
    Ok(())
}

/// 40 lowercase hexadecimal characters: a full git commit id.
fn commit_id_ok(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether `key` is the bundle or the snapshot bundle of one of the run's slices before `turn`: the work a later slice restores.
fn earlier_slice_bundle(run_id: &str, turn: u32, key: &str) -> bool {
    (DIRECT_TURN..turn)
        .any(|n| key == s3::bundle_key_t(run_id, n) || key == s3::snapshot_bundle_key_t(run_id, n))
}

/// Absent, or one of [`REASONING_LEVELS`].
fn reasoning_level_ok(level: Option<&str>) -> bool {
    level.is_none_or(|l| REASONING_LEVELS.contains(&l))
}

fn sha256_hex_ok(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// ---- outcomes and labels ----------------------------------------------------------------

/// How a direct session ended, as the harness records it. The word is the wire form (`snake_case`); the label is
/// the text the product shows. The labels are frozen: a change to one needs the maintainers' decision.
///
/// Only [`DirectOutcome::ReportedDone`] says "Cline reported done", and that is the sentence the product uses when
/// Cline's own stop reason is `end_turn` with a message. The harness never claims more.
// The variants carry plain comments, not doc comments: a doc comment would turn the schema's `enum` into a `oneOf`
// of constants, and `tests/wire_direct_runtime.rs` reads the `enum`.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DirectOutcome {
    // row 2: stop reason end_turn and a message with a non-blank character
    ReportedDone,
    // row 1: a failure before the prompt was sent
    StoppedSetupFailed,
    // row 3: end_turn with a blank message
    StoppedNoMessage,
    // row 4: max_tokens
    StoppedMaxTokens,
    // row 5: max_turn_requests
    StoppedMaxRequests,
    // row 6: refusal
    StoppedRefusal,
    // rows 7 and 9: cancelled after the client sent a cancel, or the engine ignored the cancel
    StoppedByUser,
    // row 8: cancelled with no client cancel (Cline's own stop: the loop guard or the mistake limit; the detail names which)
    StoppedEngineCancelled,
    // row 10: a JSON-RPC error answering the prompt
    StoppedProviderError,
    // row 11: the engine process closed or broke the protocol during the prompt
    StoppedEngineDied,
    // row 12: no output for the silence limit
    StoppedSilence,
    // row 13: the deadline
    StoppedTimeout,
    // row 14: any other stop reason
    StoppedOther,
    // row 15: no turn-complete arrived; set by run-session, never by agentd
    StoppedSessionLost,
    // row 16: end_turn with a message, but no file differs between the base and the delivered head; set by agentd's end sequence
    StoppedNoChanges,
}

impl DirectOutcome {
    /// Every outcome, in declaration order.
    pub const ALL: [DirectOutcome; 15] = [
        Self::ReportedDone,
        Self::StoppedSetupFailed,
        Self::StoppedNoMessage,
        Self::StoppedMaxTokens,
        Self::StoppedMaxRequests,
        Self::StoppedRefusal,
        Self::StoppedByUser,
        Self::StoppedEngineCancelled,
        Self::StoppedProviderError,
        Self::StoppedEngineDied,
        Self::StoppedSilence,
        Self::StoppedTimeout,
        Self::StoppedOther,
        Self::StoppedSessionLost,
        Self::StoppedNoChanges,
    ];

    /// The wire word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReportedDone => "reported_done",
            Self::StoppedSetupFailed => "stopped_setup_failed",
            Self::StoppedNoMessage => "stopped_no_message",
            Self::StoppedMaxTokens => "stopped_max_tokens",
            Self::StoppedMaxRequests => "stopped_max_requests",
            Self::StoppedRefusal => "stopped_refusal",
            Self::StoppedByUser => "stopped_by_user",
            Self::StoppedEngineCancelled => "stopped_engine_cancelled",
            Self::StoppedProviderError => "stopped_provider_error",
            Self::StoppedEngineDied => "stopped_engine_died",
            Self::StoppedSilence => "stopped_silence",
            Self::StoppedTimeout => "stopped_timeout",
            Self::StoppedOther => "stopped_other",
            Self::StoppedSessionLost => "stopped_session_lost",
            Self::StoppedNoChanges => "stopped_no_changes",
        }
    }

    /// The frozen label. No label says "verified", "tested", "gated" or "evaluated".
    pub fn label(self) -> &'static str {
        match self {
            Self::ReportedDone => "Cline reported done",
            Self::StoppedSetupFailed => "Cline did not start",
            Self::StoppedNoMessage => "Cline stopped: it ended its turn without a message",
            Self::StoppedMaxTokens => "Cline stopped: it reached the output limit",
            Self::StoppedMaxRequests => "Cline stopped: it reached its request limit",
            Self::StoppedRefusal => "Cline stopped: the model refused",
            Self::StoppedByUser => "Cline stopped: you stopped the run",
            Self::StoppedEngineCancelled => "Cline stopped: Cline ended the run itself",
            Self::StoppedProviderError => "Cline stopped: the model provider failed",
            Self::StoppedEngineDied => "Cline stopped: the Cline process failed",
            Self::StoppedSilence => "Cline stopped: no output for the silence limit",
            Self::StoppedTimeout => "Cline stopped: the run reached its time limit",
            Self::StoppedOther => "Cline stopped: the engine reported an unknown stop reason",
            Self::StoppedSessionLost => "Cline stopped: the session was lost",
            Self::StoppedNoChanges => "Cline stopped: it ended its turn without changing a file",
        }
    }

    /// The sentence of the outcome for a run of `engine`: the frozen sentence with the engine's display name ([`Engine::display_name`]) where the Cline
    /// sentence says "Cline" (twice in two sentences). For Cline it is byte for byte [`label`](Self::label); the engine's name is the only part that changes.
    pub fn label_text(self, engine: Engine) -> String {
        self.label_text_named(engine.display_name())
    }

    /// [`label_text`](Self::label_text) with the name given as text: for code that holds the engine's display name and not the engine itself.
    pub fn label_text_named(self, name: &str) -> String {
        self.label().replace("Cline", name)
    }

    /// The label of a run of `engine` that ended with `detail`. It is the sentence of the outcome ([`label_text`](Self::label_text)), except for one case: a
    /// `stopped_other` run whose detail begins with [`HARNESS_STOP_PREFIX`] was ended by the harness (the goal loop's stall rules), and
    /// its label says so with the first sentence of the detail as the reason: "<Engine> stopped: the harness ended the run (<reason>)".
    /// A pure function of the three arguments; the reason is cut to [`LABEL_REASON_CHARS`] characters on one line, never quotes the
    /// engine's own words (they follow the first sentence) and is left out when it uses a word of [`BANNED_CLAIM_WORDS`].
    pub fn label_for_engine(self, engine: Engine, detail: Option<&str>) -> String {
        let name = engine.display_name();
        match (self, detail.and_then(harness_stop_reason)) {
            (Self::StoppedOther, Some(reason)) if reason.is_empty() => {
                format!("{name} stopped: the harness ended the run")
            }
            (Self::StoppedOther, Some(reason)) => {
                format!("{name} stopped: the harness ended the run ({reason})")
            }
            _ => self.label_text(engine),
        }
    }

    /// The label of a Cline run: [`label_for_engine`](Self::label_for_engine) with [`Engine::CLINE`]. The product code of a run passes the engine of the run;
    /// this form is for Cline-only code and the tests that pin Cline's sentences.
    pub fn label_for(self, detail: Option<&str>) -> String {
        self.label_for_engine(Engine::CLINE, detail)
    }

    /// True only for `reported_done`.
    pub fn is_reported_done(self) -> bool {
        self == Self::ReportedDone
    }
}

/// The sentence a detail begins with when the harness ended the run itself.
pub const HARNESS_STOP_PREFIX: &str = "The harness ended the run:";

/// The longest reason [`DirectOutcome::label_for`] puts in a label.
const LABEL_REASON_CHARS: usize = 120;

/// The reason of a stop by the harness: `None` when `detail` does not begin with [`HARNESS_STOP_PREFIX`], else its first sentence on
/// one line (empty when there is none or when it would carry a banned claim word).
fn harness_stop_reason(detail: &str) -> Option<String> {
    let rest = detail.strip_prefix(HARNESS_STOP_PREFIX)?;
    let first = rest.split(". ").next().unwrap_or(rest);
    let one_line = first
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let whole = one_line.trim_end_matches('.').trim();
    // A reason that is too long is cut at a word and ends with an ellipsis.
    let reason = if whole.chars().count() > LABEL_REASON_CHARS {
        let cut = head_chars(whole, LABEL_REASON_CHARS);
        format!(
            "{}…",
            cut.rsplit_once(' ')
                .map_or(cut, |(head, _)| head)
                .trim_end()
        )
    } else {
        whole.to_string()
    };
    if reason.is_empty() || banned_claim_word(&reason).is_some() {
        return Some(String::new());
    }
    Some(reason)
}

// ---- observations: facts the harness read at the end of a run ----------------------------------------------------------------

/// What kinds of fact [`DirectObservation`] records. An observation is something the harness read and writes down as it found
/// it; it says nothing about whether the work is good.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DirectObservationKind {
    /// The last entry of the run log (`docs/lifecycle/LOG.md`) is a run with a non-zero exit code or failed tests.
    LastLogEntryRed,
}

/// At most this many observations are listed in a summary.
pub const MAX_DIRECT_OBSERVATIONS: usize = 4;
/// The longest `text` of an observation.
pub const MAX_OBSERVATION_CHARS: usize = 300;
/// The longest `command` of an observation.
const OBSERVATION_COMMAND_CHARS: usize = 120;

/// A fact the harness read at the end of the run (protocol 1.38.0). Its `text` is one plain sentence the harness wrote; no field holds a
/// word of [`BANNED_CLAIM_WORDS`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectObservation {
    pub kind: DirectObservationKind,
    /// The sentence the card shows beside the label, at most [`MAX_OBSERVATION_CHARS`].
    pub text: String,
    /// `last_log_entry_red`: the command of the logged run as the log wrote it, cleaned and cut; left out when it holds a banned claim word.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// `last_log_entry_red`: the exit code as logged (a number or `unknown`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<String>,
    /// `last_log_entry_red`: the failed count as logged, when it was above zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<String>,
}

/// A token of a log line that is safe to carry: 1 to 12 letters, digits or hyphens.
fn plain_token(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()
        && s.chars().count() <= 12
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
    .then(|| s.to_string())
}

impl DirectObservation {
    /// The last entry of the run log is a red run. `command`, `exit` and `failed` are as the log wrote them: the command is put on one
    /// line and cut (and left out when it holds a banned claim word); an exit code that is no plain token reads `unknown`; a failed count
    /// is carried only when it is a number above zero.
    pub fn last_log_entry_red(command: &str, exit: &str, failed: Option<&str>) -> Self {
        let one_line = command
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let command = if one_line.is_empty() || banned_claim_word(&one_line).is_some() {
            None
        } else if one_line.chars().count() > OBSERVATION_COMMAND_CHARS {
            let cut = head_chars(&one_line, OBSERVATION_COMMAND_CHARS);
            Some(format!(
                "{}…",
                cut.rsplit_once(' ')
                    .map_or(cut, |(head, _)| head)
                    .trim_end()
            ))
        } else {
            Some(one_line)
        };
        let exit = plain_token(exit).unwrap_or_else(|| "unknown".to_string());
        let failed = failed
            .and_then(plain_token)
            .filter(|f| f.parse::<u64>().is_ok_and(|n| n > 0));
        let who = match &command {
            Some(c) => format!("{c} exited {exit}"),
            None => format!("exit {exit}"),
        };
        let text = match &failed {
            Some(f) => format!("The last entry of the run log is a red run: {who} and {f} failed."),
            None => format!("The last entry of the run log is a red run: {who}."),
        };
        Self {
            kind: DirectObservationKind::LastLogEntryRed,
            text,
            command,
            exit: Some(exit),
            failed,
        }
    }

    /// The observation holds no more than the rules allow: a text of at most [`MAX_OBSERVATION_CHARS`] and no banned claim word in any member.
    pub fn is_valid(&self) -> bool {
        self.text.chars().count() <= MAX_OBSERVATION_CHARS
            && [
                Some(&self.text),
                self.command.as_ref(),
                self.exit.as_ref(),
                self.failed.as_ref(),
            ]
            .into_iter()
            .flatten()
            .all(|s| banned_claim_word(s).is_none())
    }
}

/// Words no label, pull request text or EVIDENCE.md of a direct run may use about the work: the harness hosted the
/// session and checked nothing.
pub const BANNED_CLAIM_WORDS: [&str; 4] = ["verified", "tested", "gated", "evaluated"];

/// The first of [`BANNED_CLAIM_WORDS`] that `text` contains as a whole word, in any letter case. `attested` and
/// `unverified` are different words and do not match. A caller that quotes the user's own prompt checks the
/// text it wrote, not the quote.
pub fn banned_claim_word(text: &str) -> Option<&'static str> {
    text.split(|c: char| !c.is_alphanumeric()).find_map(|word| {
        BANNED_CLAIM_WORDS
            .iter()
            .copied()
            .find(|banned| word.eq_ignore_ascii_case(banned))
    })
}

/// The first `n` characters of `s` (never splits a character).
pub fn head_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// The bytes one character takes inside a JSON string, as `serde_json` writes it: two for a quote, a backslash, a new line, a carriage return,
/// a tab, a backspace and a form feed; six for another control character; else its length in UTF-8.
fn json_char_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if u32::from(c) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// The bytes `s` takes inside a JSON string, the two quotes not counted (what `serde_json` writes).
pub fn json_len(s: &str) -> usize {
    s.chars().map(json_char_len).sum()
}

/// The longest start of `s` whose JSON text is at most `max` bytes (never splits a character).
pub fn head_json_bytes(s: &str, max: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        used += json_char_len(c);
        if used > max {
            return &s[..i];
        }
    }
    s
}

/// The longest end of `s` whose JSON text is at most `max` bytes (never splits a character).
pub fn tail_json_bytes(s: &str, max: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices().rev() {
        used += json_char_len(c);
        if used > max {
            return &s[i + c.len_utf8()..];
        }
    }
    s
}

/// `s` as it may ride in Step Functions state: unchanged when its JSON text is at most `max` bytes, else a quarter of the room for its start, a
/// marker line that says how many characters were left out (and `note`, a short remark such as where the whole text is), and the rest of the room
/// for its end, where a report puts its conclusions. The JSON text of the result is at most `max` bytes whatever `s` holds.
pub fn message_preview(s: &str, max: usize, note: &str) -> String {
    if json_len(s) <= max {
        return s.to_string();
    }
    // A count has at most 20 digits.
    let widest = format!("\n[... {} characters omitted{note} ...]\n", usize::MAX);
    if json_len(&widest) > max {
        // No room for a marker: the end of the text that fits.
        return tail_json_bytes(s, max).to_string();
    }
    let room = max - json_len(&widest);
    let head = head_json_bytes(s, room / 4);
    // The start ends at a line when one ends in its second half.
    let head = match head.rfind('\n') {
        Some(i) if i > head.len() / 2 => &head[..i],
        _ => head,
    };
    let tail = tail_json_bytes(s, room.saturating_sub(json_len(head)));
    let kept = head.chars().count() + tail.chars().count();
    let omitted = s.chars().count().saturating_sub(kept);
    format!("{head}\n[... {omitted} characters omitted{note} ...]\n{tail}")
}

/// The last `n` characters of `s` (never splits a character).
pub fn tail_chars(s: &str, n: usize) -> &str {
    let total = s.chars().count();
    if total <= n {
        return s;
    }
    match s.char_indices().nth(total - n) {
        Some((i, _)) => &s[i..],
        None => s,
    }
}

// ---- summary.json -------------------------------------------------------------------------

/// Where the delivered head came from: the final bundle of the session, or the last periodic snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HeadSource {
    Final,
    Snapshot,
}

/// A rules file the session was given. `bytes` is in `summary.json`; the evidence leaves it out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectRuleRef {
    pub name: String,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectSummaryModel {
    pub id: String,
    pub region: String,
    /// The model id the engine reported back at `session/new`; absent when the engine never got that far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectSummaryEngine {
    pub name: String,
    /// The profile's `version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// SHA-256 of the engine binary, computed at start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_sha256: Option<String>,
    /// Protocol 1.47.0: the reasoning level the engine was launched with (`--thinking <level>`), one of [`REASONING_LEVELS`] today. Absent
    /// when the launch had no reasoning argument. It is the level of the model the run started on; a switch to another model does not change it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Protocol 1.47.0: where that level came from, one of [`REASONING_SOURCES`] today: `default` (the model's catalog level), `explicit` (the
    /// user's `reasoning.effort`) or `off` (the user turned reasoning off). Absent when the request gave agentd no level to launch with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_source: Option<String>,
    /// Protocol 1.49.0: who the engine ran as. `nobody` is the nobody route of an Instances session (the overflow user in user and IPC namespaces of its
    /// own), `agentd` is the engine as agentd's own user, and any other word is the name of the engine user of a microVM session. Absent in a summary written
    /// before this version. Lower-case letters, digits, `-` and `_`, 1 to [`MAX_ENGINE_IDENTITY_CHARS`] characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
}

/// The longest `engine.identity` word of a summary.
pub const MAX_ENGINE_IDENTITY_CHARS: usize = 32;

impl DirectSummaryEngine {
    /// Whether `identity` is absent or a plain name: 1 to [`MAX_ENGINE_IDENTITY_CHARS`] of lower-case letters, digits, `-` and `_`.
    pub fn identity_is_valid(&self) -> bool {
        self.identity.as_deref().is_none_or(|w| {
            (1..=MAX_ENGINE_IDENTITY_CHARS).contains(&w.chars().count())
                && w.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectRulesPreflight {
    pub ran: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectRequestRef {
    pub sha256: String,
    pub prompt_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectNudge {
    pub requested: bool,
    pub honored: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectTiming {
    pub started_at: String,
    pub ended_at: String,
    pub wall_s: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_silent_gap_s: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectGit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// Present only when the bundle upload succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    /// True when the end sequence made a commit for what Cline left uncommitted.
    pub committed_by_harness: bool,
    pub files_changed: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_uri: Option<String>,
    /// Present only when the bundle upload succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_bytes: Option<u64>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectSnapshots {
    pub count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_at: Option<String>,
    /// Snapshots that failed (a log line each); left out when 0.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub failures: u32,
}

/// What the loopback package proxy counted (`pkgproxy::StatsSnapshot`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectPackages {
    pub enabled: bool,
    pub requests: u64,
    pub bytes: u64,
    pub by_prefix: BTreeMap<String, u64>,
    /// HTTP status code to count; JSON object keys are strings.
    pub statuses: BTreeMap<u16, u64>,
    /// Requests the proxy refused; left out when 0.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub refused: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectVersions {
    pub agentd: String,
    pub protocol: String,
}

/// What a try of the prompt sent (protocol 1.32.0, the first slice of provider failover).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DirectAttemptKind {
    /// The user's text, byte for byte. Always try 1.
    Prompt,
    /// The one fixed sentence agentd sends on the same engine session after a provider failure.
    Resume,
    /// Protocol 1.37.0: the first send of a fresh Cline session: harness-written text, the user's words
    /// when there are some, and a summary of the earlier session.
    FreshSession,
    /// Protocol 1.37.0: the first send after a person's (or the analyzer's) message in the same session.
    Instruction,
}

/// How a try ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DirectAttemptResult {
    /// The engine answered the prompt (with any stop reason).
    Ok,
    /// The prompt ended in a JSON-RPC error: the provider failed.
    ProviderError,
    /// The engine answered `cancelled` and agentd had written no cancel: it ended the prompt itself (its mistake limit, a stop from a
    /// hook, or a stream it lost).
    EngineCancelled,
}

/// One try of the prompt in a run that retried (`DirectSummary.attempts`). A run with one try lists none.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectAttempt {
    /// 1 for the user's prompt, then 2 and up for each resume.
    pub n: u32,
    pub kind: DirectAttemptKind,
    /// RFC 3339.
    pub started_at: String,
    /// RFC 3339.
    pub ended_at: String,
    pub result: DirectAttemptResult,
    /// For a failed try: what kind of failure decided the next step: `transient`, `permanent`, `unavailable` (the route itself is refused) or `never`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_class: Option<String>,
    /// For a failed try: the engine's error, scrubbed; at most [`MAX_DIRECT_DETAIL_CHARS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// For a failed try that another followed: the seconds agentd waited before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_s: Option<u64>,
    /// The model id (the route) the try ran on; set in a run that has failover routes (protocol 1.34.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
}

/// What a switch changed (protocol 1.34.0).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DirectSwitchKind {
    /// The same model through its other inference profile (`us.` to `global.` or back).
    Route,
    /// Another supported model.
    Model,
    /// Back to the model the user chose, after it had failed and the wait since its failure passed.
    Return,
}

/// One change of route in a run with failover (`DirectSummary.switches`). Only a switch the engine confirmed counts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectSwitch {
    /// 1 for the first switch.
    pub n: u32,
    /// RFC 3339.
    pub at: String,
    /// The model id the run left.
    pub from: String,
    /// The model id the engine reported after the switch.
    pub to: String,
    pub kind: DirectSwitchKind,
    /// What kind of failure caused it: `transient`, `permanent` or `unavailable` (the route was down or refused before any prompt).
    pub class: String,
    /// The failure that caused it, scrubbed; at most [`MAX_DIRECT_DETAIL_CHARS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What agentd used and did for failover in this run (protocol 1.34.0).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectFailoverNote {
    /// Routes the run could use, the preferred one included.
    pub routes: u32,
    /// The output limit in the engine's provider settings: the smallest of the routes, so no switch sends more than a route allows.
    pub effective_max_output_tokens: u32,
    /// The context window in the engine's provider settings: the smallest of the routes.
    pub effective_context_tokens: u32,
    /// Times every route had failed and agentd waited, then began again at the preferred one.
    pub passes: u32,
    /// Small test requests agentd sent to a route while everything was down.
    pub probes: u32,
    /// Seconds spent waiting with every route down.
    pub waited_s: u64,
}

/// `summary.json`, uploaded by agentd to `runs/<run>/agents/direct/turns/1/summary.json`.
///
/// At most [`MAX_DIRECT_SUMMARY_BYTES`]; every string is cut to its stated limit. `usage` is `null` when Cline
/// reported none, never zeros standing in for it. The reader ignores unknown keys.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectSummary {
    /// [`SUMMARY_SCHEMA_VERSION`].
    pub schema_version: u32,
    pub run_id: String,
    /// [`DIRECT_AGENT_ID`].
    pub agent_id: String,
    /// [`DIRECT_TURN`].
    pub turn: u32,
    pub outcome: DirectOutcome,
    /// `outcome.label()`.
    pub label: String,
    /// The specifics (silence seconds, JSON-RPC code, exit status); at most [`MAX_DIRECT_DETAIL_CHARS`].
    pub detail: Option<String>,
    /// The engine's raw ACP stop reason.
    pub stop_reason: Option<String>,
    /// Telemetry only; the machine never branches on it.
    pub turn_outcome: TurnOutcome,
    /// The last [`MAX_CLINE_MESSAGE_CHARS`] characters of Cline's final message.
    pub cline_message: Option<String>,
    pub error: Option<String>,
    pub model: DirectSummaryModel,
    pub engine: DirectSummaryEngine,
    pub rules: Vec<DirectRuleRef>,
    /// Protocol 1.38.0: the skills the session was given; left out when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<DirectSkillRef>,
    /// Protocol 1.39.0: the engine settings the session was given (`StartDirectRunRequest.engineConfig`): the engine, the schema, the canonical hash
    /// and the number of keys, never a value. Left out for a run that had none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_config: Option<DirectEngineConfigRef>,
    /// Protocol 1.38.0: the research lane of the run, written by agentd's research module through the `on_summary` hook; left out when
    /// the module did not decide anything (a stage without the research route). Counts only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub research: Option<DirectResearchNote>,
    /// Protocol 1.40.0 (wire B of the tool layer): what the harness's tool server did in this run, written by agentd's tools module through the
    /// `on_summary` hook; left out when the module decided nothing (a stage without the layer, or a profile with no `[mcp]` table). Words and
    /// counts only (see [`DirectToolsNote`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<DirectToolsNote>,
    pub rules_preflight: DirectRulesPreflight,
    pub request: DirectRequestRef,
    pub nudge: DirectNudge,
    pub usage: Option<Usage>,
    pub tool_calls: u64,
    pub timing: DirectTiming,
    pub git: DirectGit,
    pub snapshots: DirectSnapshots,
    pub packages: DirectPackages,
    pub fault_hint: Option<FaultHint>,
    pub versions: DirectVersions,
    /// Every try of the prompt when the run retried after a provider failure (protocol 1.32.0); left out for a run with one try.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<DirectAttempt>,
    /// Every confirmed switch of route or model (protocol 1.34.0); left out when there was none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub switches: Vec<DirectSwitch>,
    /// The model id the run ended on, when it switched at least once (protocol 1.34.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_model: Option<String>,
    /// Present in a run that had failover routes (protocol 1.34.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failover: Option<DirectFailoverNote>,
    /// Every question the run raised and what became of it (protocol 1.37.0); at most [`MAX_DIRECT_INTERVENTIONS`], the last ones; left
    /// out when there was none. Never carries the user's text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interventions: Vec<DirectIntervention>,
    /// Every fresh Cline session of the run (protocol 1.37.0); at most [`MAX_DIRECT_INTERVENTIONS`]; left out when there was none.
    /// `usage` sums over every session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fresh_sessions: Vec<DirectFreshSession>,
    /// Facts the harness read at the end of the run and records as it found them: today the last entry of
    /// the run log when it is red. At most [`MAX_DIRECT_OBSERVATIONS`]; left out when there is none. Never a claim about the work.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observations: Vec<DirectObservation>,
    /// Protocol 1.44.0: what the review round did, or why it did not run; left out when the run had no stage loop to decide it. A fact the
    /// harness recorded, never a claim about the work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<DirectReview>,
    /// Protocol 1.48.0: the flow the run declared, read by agentd from the `Flow:` trailer of the final task commit's message, else from the
    /// first line of the last message (see [`DirectFlow`]). A fact recorded beside the label: nothing reads it to decide an outcome. Left out
    /// when the harness could not read the run's commits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow: Option<DirectFlow>,
}

/// The first rule of [`DirectSummary::validate`] a summary breaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectSummaryError {
    SchemaVersion,
    /// `agentId` is not `direct` or `turn` is not 1.
    Identity,
    /// `label` is not the frozen text of `outcome`.
    LabelMismatch,
    DetailTooLong,
    MessageTooLong,
    /// More than [`MAX_DIRECT_ATTEMPTS`] tries are listed.
    TooManyAttempts,
    /// More than [`MAX_DIRECT_SWITCHES`] switches are listed.
    TooManySwitches,
    /// More than [`MAX_DIRECT_INTERVENTIONS`] interventions or fresh sessions are listed (protocol 1.37.0).
    TooManyInterventions,
    /// More than [`MAX_DIRECT_OBSERVATIONS`] observations, or one that is too long or holds a banned claim word (protocol 1.38.0).
    ObservationInvalid,
    /// `engineConfig` has an engine that is not a name, a schema of 0, more keys than a member may hold or a hash that is not 64 lowercase hex
    /// digits (protocol 1.39.0).
    EngineConfigInvalid,
    /// `tools` holds a word of the wrong form, more than 16 tools, a repeated or prefixed name, a counted tool that was not listed, counts for a
    /// module that was off, or a `p50Ms` above its `p95Ms` (protocol 1.40.0).
    ToolsInvalid,
    /// `review` holds a word of the wrong form or a commit id that is not one (protocol 1.44.0).
    ReviewInvalid,
    /// `engine.reasoningEffort` or `engine.reasoningSource` is not a plain word: 1 to 16 characters of `[a-z0-9_-]` (protocol 1.47.0). The words are
    /// plain strings on purpose, so a reader older than the writer does not fail on a level or a source added later.
    ReasoningInvalid,
    /// `engine.identity` is not a plain name (protocol 1.49.0).
    EngineIdentityInvalid,
    TooLarge,
}

impl fmt::Display for DirectSummaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SchemaVersion => write!(f, "schemaVersion must be {SUMMARY_SCHEMA_VERSION}"),
            Self::Identity => write!(
                f,
                "agentId must be \"{DIRECT_AGENT_ID}\" and turn 1 to {MAX_SLICES}"
            ),
            Self::LabelMismatch => write!(f, "label must be the frozen text of the outcome"),
            Self::DetailTooLong => write!(
                f,
                "detail and error are at most {MAX_DIRECT_DETAIL_CHARS} characters"
            ),
            Self::MessageTooLong => write!(
                f,
                "clineMessage is at most {MAX_CLINE_MESSAGE_CHARS} characters"
            ),
            Self::TooManyAttempts => {
                write!(f, "attempts lists at most {MAX_DIRECT_ATTEMPTS} tries")
            }
            Self::TooManySwitches => {
                write!(f, "switches lists at most {MAX_DIRECT_SWITCHES} switches")
            }
            Self::TooManyInterventions => write!(
                f,
                "interventions and freshSessions list at most {MAX_DIRECT_INTERVENTIONS} entries each"
            ),
            Self::ObservationInvalid => write!(
                f,
                "observations list at most {MAX_DIRECT_OBSERVATIONS} entries of at most {MAX_OBSERVATION_CHARS} characters, with no claim word"
            ),
            Self::EngineConfigInvalid => write!(
                f,
                "engineConfig holds an engine name, a schema of 1 or more, at most {MAX_ENGINE_CONFIG_LEAVES} keys and a lowercase SHA-256"
            ),
            Self::ToolsInvalid => write!(
                f,
                "tools holds plain words, at most {} bare tool names each once, counts only for a module that was on, and only listed tools counted",
                crate::tools::MAX_TOOLS_LISTED
            ),
            Self::ReviewInvalid => write!(f, "review holds plain words and full commit ids only"),
            Self::ReasoningInvalid => write!(
                f,
                "engine.reasoningEffort and engine.reasoningSource are plain words of 1 to 16 characters of a-z, 0-9, _ and -"
            ),
            Self::EngineIdentityInvalid => write!(
                f,
                "engine.identity is a name of 1 to {MAX_ENGINE_IDENTITY_CHARS} lower-case letters, digits, - and _"
            ),
            Self::TooLarge => write!(
                f,
                "summary.json is at most {MAX_DIRECT_SUMMARY_BYTES} bytes"
            ),
        }
    }
}

impl std::error::Error for DirectSummaryError {}

impl DirectSummary {
    /// A writer calls it before the upload, a reader after the download.
    pub fn validate(&self) -> Result<(), DirectSummaryError> {
        if self.schema_version != SUMMARY_SCHEMA_VERSION {
            return Err(DirectSummaryError::SchemaVersion);
        }
        if self.agent_id != DIRECT_AGENT_ID || !(DIRECT_TURN..=MAX_SLICES).contains(&self.turn) {
            return Err(DirectSummaryError::Identity);
        }
        // The label is the sentence of the engine that ran: a label of another engine, and a summary whose engine name is no profile name, are mismatches.
        let Ok(engine) = Engine::new(&self.engine.name) else {
            return Err(DirectSummaryError::LabelMismatch);
        };
        if self.label
            != self
                .outcome
                .label_for_engine(engine, self.detail.as_deref())
        {
            return Err(DirectSummaryError::LabelMismatch);
        }
        let too_long = |s: &Option<String>| {
            s.as_deref()
                .is_some_and(|t| t.chars().count() > MAX_DIRECT_DETAIL_CHARS)
        };
        if too_long(&self.detail)
            || too_long(&self.error)
            || self.attempts.iter().any(|a| too_long(&a.detail))
            || self.switches.iter().any(|w| too_long(&w.detail))
        {
            return Err(DirectSummaryError::DetailTooLong);
        }
        if self.attempts.len() > MAX_DIRECT_ATTEMPTS {
            return Err(DirectSummaryError::TooManyAttempts);
        }
        if self.switches.len() > MAX_DIRECT_SWITCHES {
            return Err(DirectSummaryError::TooManySwitches);
        }
        if self.interventions.len() > MAX_DIRECT_INTERVENTIONS
            || self.fresh_sessions.len() > MAX_DIRECT_INTERVENTIONS
        {
            return Err(DirectSummaryError::TooManyInterventions);
        }
        if self.observations.len() > MAX_DIRECT_OBSERVATIONS
            || !self.observations.iter().all(DirectObservation::is_valid)
        {
            return Err(DirectSummaryError::ObservationInvalid);
        }
        if self.engine_config.as_ref().is_some_and(|c| !c.is_valid()) {
            return Err(DirectSummaryError::EngineConfigInvalid);
        }
        if self.tools.as_ref().is_some_and(|t| !t.is_valid()) {
            return Err(DirectSummaryError::ToolsInvalid);
        }
        if self.review.as_ref().is_some_and(|r| !r.is_valid()) {
            return Err(DirectSummaryError::ReviewInvalid);
        }
        let plain_word = |w: &Option<String>| {
            w.as_deref().is_none_or(|w| {
                (1..=16).contains(&w.len())
                    && w.bytes()
                        .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
            })
        };
        if !plain_word(&self.engine.reasoning_effort) || !plain_word(&self.engine.reasoning_source)
        {
            return Err(DirectSummaryError::ReasoningInvalid);
        }
        if !self.engine.identity_is_valid() {
            return Err(DirectSummaryError::EngineIdentityInvalid);
        }
        if self
            .cline_message
            .as_deref()
            .is_some_and(|m| m.chars().count() > MAX_CLINE_MESSAGE_CHARS)
        {
            return Err(DirectSummaryError::MessageTooLong);
        }
        match serde_json::to_vec(self) {
            Ok(bytes) if bytes.len() <= MAX_DIRECT_SUMMARY_BYTES => Ok(()),
            _ => Err(DirectSummaryError::TooLarge),
        }
    }

    /// The part of the summary the packager renders (`PackageRequest.direct`), for the given head source.
    pub fn to_evidence(&self, head_source: HeadSource) -> DirectEvidence {
        DirectEvidence {
            outcome: self.outcome,
            label: self.label.clone(),
            detail: self.detail.clone(),
            stop_reason: self.stop_reason.clone(),
            cline_message: self.cline_message.clone(),
            model_id: self.model.id.clone(),
            engine_version: self.engine.version.clone(),
            rules: self.rules.clone(),
            usage: self.usage,
            wall_s: Some(self.timing.wall_s),
            head_source,
            final_model_id: self.final_model.clone(),
            model_switches: u32::try_from(self.switches.len()).unwrap_or(u32::MAX),
            test_report: None,
            review: self.review.clone(),
            flow: self.flow,
            engine: Engine::new(&self.engine.name)
                .ok()
                .filter(|e| *e != Engine::CLINE),
        }
    }
}

// ---- evidence for the packager ------------------------------------------------------------

/// `PackageRequest.direct`: what EVIDENCE.md says about a direct run. Built from `summary.json` when it exists, from
/// the Step Functions failure when it does not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectEvidence {
    pub outcome: DirectOutcome,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// At most [`MAX_CLINE_MESSAGE_CHARS`] characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cline_message: Option<String>,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_version: Option<String>,
    pub rules: Vec<DirectRuleRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_s: Option<u64>,
    pub head_source: HeadSource,
    /// The model id the run ended on, when it switched (protocol 1.34.0); `model_id` stays the model the user chose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_model_id: Option<String>,
    /// How many confirmed switches the run made (protocol 1.34.0); 0 is left out.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub model_switches: u32,
    /// The clean-checkout test report, summarised by run-session; absent when the run made none. It never changes
    /// `outcome` or `label`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_report: Option<TestReportSummary>,
    /// What the review round did, or why it did not run; absent for a run that decided nothing. It never changes `outcome`
    /// or `label`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<DirectReview>,
    /// The flow the run declared, from the summary; absent when the summary has none. It never changes `outcome` or
    /// `label`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow: Option<DirectFlow>,
    /// The engine that ran the session, so that EVIDENCE.md names it ("OpenCode's last message"). Absent means Cline, and `to_evidence`
    /// leaves it out for a Cline run, so the evidence of a Cline run is byte for byte what it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<Engine>,
}

impl DirectEvidence {
    /// The engine that ran the session; Cline when the member is absent.
    pub fn engine_or_default(&self) -> Engine {
        self.engine.unwrap_or(Engine::CLINE)
    }
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

// ---- S3 keys ------------------------------------------------------------------------------

/// The object keys of one direct run. The worker-session role writes only under `runs/<run>/agents/direct/` and
/// reads `runs/<run>/input/`.
pub mod s3 {
    use super::{DIRECT_AGENT_ID, DIRECT_TURN};
    use crate::store::s3 as store;

    /// `runs/<run>/agents/direct/turns/1/`
    pub fn turn_prefix(run_id: &str) -> String {
        store::turn_prefix(run_id, DIRECT_AGENT_ID, DIRECT_TURN)
    }
    /// `runs/<run>/agents/direct/turns/<turn>/`: the prefix of slice `turn`. Turn 1 is [`turn_prefix`].
    pub fn turn_prefix_t(run_id: &str, turn: u32) -> String {
        store::turn_prefix(run_id, DIRECT_AGENT_ID, turn)
    }
    /// The bundle of slice `turn`; [`bundle_key`] is turn 1. worker-api refuses any other `bundleUri`.
    pub fn bundle_key_t(run_id: &str, turn: u32) -> String {
        format!("{}bundle", turn_prefix_t(run_id, turn))
    }
    pub fn summary_key_t(run_id: &str, turn: u32) -> String {
        format!("{}summary.json", turn_prefix_t(run_id, turn))
    }
    pub fn snapshot_bundle_key_t(run_id: &str, turn: u32) -> String {
        format!("{}snapshot.bundle", turn_prefix_t(run_id, turn))
    }
    pub fn snapshot_json_key_t(run_id: &str, turn: u32) -> String {
        format!("{}snapshot.json", turn_prefix_t(run_id, turn))
    }
    pub fn test_report_key_t(run_id: &str, turn: u32) -> String {
        format!("{}test-report.json", turn_prefix_t(run_id, turn))
    }
    /// The brief a slice leaves for the next one: what the earlier session did and where it stopped, written by agentd at a handoff
    /// after every other artifact of the slice.
    pub fn brief_key_t(run_id: &str, turn: u32) -> String {
        format!("{}brief.json", turn_prefix_t(run_id, turn))
    }
    /// `runs/<run>/slices/<n>.json`: the service's record of slice `n` (written by run-session, never by the microVM).
    pub fn slice_record_key(run_id: &str, n: u32) -> String {
        format!("runs/{run_id}/slices/{n}.json")
    }
    /// The full-history bundle of `harness/<run>/direct`. worker-api refuses any other `bundleUri`.
    pub fn bundle_key(run_id: &str) -> String {
        format!("{}bundle", turn_prefix(run_id))
    }
    pub fn summary_key(run_id: &str) -> String {
        format!("{}summary.json", turn_prefix(run_id))
    }
    pub fn snapshot_bundle_key(run_id: &str) -> String {
        format!("{}snapshot.bundle", turn_prefix(run_id))
    }
    /// Written after the bundle, so a reader that sees it finds the bundle.
    pub fn snapshot_json_key(run_id: &str) -> String {
        format!("{}snapshot.json", turn_prefix(run_id))
    }
    /// The stop digest agentd writes at every stop (protocol 1.37.0, recorder depth); at most [`super::MAX_STOP_DIGEST_BYTES`].
    pub fn stop_digest_key(run_id: &str, n: u32) -> String {
        store::direct_stop_digest_key(run_id, n)
    }
    /// The audit copy of the digest of fresh session `n` (protocol 1.37.0); it is never delivered.
    pub fn fresh_session_key(run_id: &str, n: u32) -> String {
        store::direct_fresh_session_key(run_id, n)
    }
    /// `runs/<run>/agents/direct/turns/1/transcript/`: the stored conversation's chunk objects (protocol 1.38.0).
    pub fn transcript_prefix(run_id: &str) -> String {
        format!("{}transcript/", turn_prefix(run_id))
    }
    /// `runs/<run>/agents/direct/turns/1/transcript/{seq:012}.ndjson.gz`.
    pub fn transcript_chunk_key(run_id: &str, seq: u64) -> String {
        format!("{}{seq:012}.ndjson.gz", transcript_prefix(run_id))
    }
    pub fn acp_transcript_key(run_id: &str) -> String {
        format!("{}acp.ndjson.gz", turn_prefix(run_id))
    }
    /// `runs/<run>/evidence/direct.json`, written by run-session `plan-delivery`; the packager zips it.
    pub fn evidence_key(run_id: &str) -> String {
        format!("{}direct.json", store::evidence_prefix(run_id))
    }
    /// The clean-checkout test report agentd writes (protocol 1.38.0): `runs/<run>/agents/direct/turns/1/test-report.json`, the only place the
    /// worker role may write it. run-session `plan-delivery` reads it.
    pub fn test_report_key(run_id: &str) -> String {
        format!("{}test-report.json", turn_prefix(run_id))
    }
    /// The placement listing agentd writes when the research lane ran: `runs/<run>/agents/direct/context-listing.json`, the
    /// SHA-256 of every file it placed in the context directory, hashed from the disk. Only agentd can hash what it placed on the VM, so the
    /// research checks read this object. The worker role may write under `runs/<run>/agents/direct/` (not under its `research/` part).
    pub fn context_listing_key(run_id: &str) -> String {
        format!("runs/{run_id}/agents/{DIRECT_AGENT_ID}/context-listing.json")
    }
    /// The checked copy `plan-delivery` writes (protocol 1.38.0): `runs/<run>/evidence/test-report.json`, which the packager zips into `evidence/`.
    pub fn evidence_test_report_key(run_id: &str) -> String {
        format!("{}test-report.json", store::evidence_prefix(run_id))
    }
    /// The canonical `StartDirectRunRequest`.
    pub fn request_key(run_id: &str) -> String {
        store::request_key(run_id)
    }
    /// The base the session restores.
    pub fn repo_bundle_key(run_id: &str) -> String {
        store::repo_bundle_key(run_id)
    }

    /// `s3://<bucket>/<key>`.
    pub fn uri(bucket: &str, key: &str) -> String {
        format!("s3://{bucket}/{key}")
    }

    /// `(bucket, key)` of an `s3://` URI, or `None` unless the bucket has an S3 name's shape (3 to 63 characters of
    /// `a-z0-9.-`) and the key is not empty.
    pub fn split_uri(uri: &str) -> Option<(&str, &str)> {
        let rest = uri.strip_prefix("s3://")?;
        let (bucket, key) = rest.split_once('/')?;
        let bucket_ok = (3..=63).contains(&bucket.len())
            && bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'));
        (bucket_ok && !key.is_empty()).then_some((bucket, key))
    }
}
