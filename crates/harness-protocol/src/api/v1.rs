//! Public API `/v1` request and response bodies.

use crate::catalog::{CatalogRow, ExcludedModel};
use crate::common::{Engine, Protocol, RunKind, Usage};
use crate::envelope::Envelope;
use crate::goal::{GoalPhase, GoalStatus};
use crate::store::ArtifactKind;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use crate::common::{ApprovalDecision, BudgetLimit};
pub use crate::target::RunTarget;

/// One file of a `--rules <dir>` upload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuleFile {
    pub path: String,
    pub sha256: String,
    pub content: String,
}

/// The user's nudge file (`~/.config/ham/continue.md`): the text agentd sends as a follow-up prompt when Cline
/// ends a turn without saying it is done, and how many times it may. `ham` splits the file's optional frontmatter
/// (`max_nudges`) from its body; `sha256` is over `text`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContinueSettings {
    pub text: String,
    pub sha256: String,
    /// 0 turns the nudge off; the harness stops after this many nudges.
    pub max_nudges: u32,
}

/// `POST /v1/direct/runs` (with an `Idempotency-Key` header; protocol 1.29.0): the run passes the user's prompt,
/// unchanged, to Cline on ONE model, with the user's rules and no planner, roles, gates or evaluator of the harness. Served
/// only by the `direct` stage. A member that is not here is refused, not ignored.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartDirectRunRequest {
    /// Sent to Cline as typed: no trimming, no wrapping.
    pub prompt: String,
    pub target: RunTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
    /// An `id` of the static model list (`engine/catalog/models.json`); anything else is 422 `unknown-model`.
    pub model: String,
    /// The user's rules, a flat list of `.md` files (`ham_core::rules` validates them); they become Cline's global rules.
    #[serde(default)]
    pub rules: Vec<RuleFile>,
    /// The user's nudge file; absent, agentd uses its built-in text and count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nudge: Option<ContinueSettings>,
    /// Protocol 1.37.0: how many seconds the session may wait for the user's answer when Cline fails or gives up. Absent means 0: no
    /// wait, the run ends at the stop as before (scripts, the release gate and old clients send none). An interactive `ham` sends
    /// [`crate::direct::DEFAULT_CLIENT_INPUT_WAIT_S`] (1,800). 1 to [`crate::direct::MAX_INPUT_WAIT_S`]; 0 is never sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_wait_s: Option<u32>,
    /// Protocol 1.38.0: the skills the engine may load on demand, folders of files that agentd writes beside the rules (outside the
    /// repository). At most [`crate::direct::MAX_SKILLS`] skills, with no limit on the files of a skill or their size; [`crate::direct::check_skills`]
    /// is the one validator of `ham`, control-api and agentd. The request as a whole is held to [`crate::direct::MAX_REQUEST_BYTES`]. Empty
    /// (the default, and what an old client sends) is left out of the stored request, so an agentd image that predates the member can still parse it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<crate::direct::Skill>,
    /// Protocol 1.39.0 (the configuration program): the user's settings for the engine, typed values read from the engine's TOML file on the Mac
    /// (`~/.config/ham/engines/<engine>.toml`); comments never leave the Mac. [`crate::direct::check_engine_config`] is the one check that `ham`,
    /// control-api (400 `invalid-engine-config`) and agentd all call. Absent (the default, and what every client without an engine file sends) is
    /// left out of the stored request, so a request that keeps the defaults is byte for byte the request of protocol 1.38.0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_config: Option<crate::direct::EngineConfig>,
    /// Protocol 1.38.0: start from the head an earlier run delivered (`ham continue`). Accepted only with the `s3-only` target and no
    /// `baseRef`; control-api stores the delivered head it resolved in `headSha`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_from: Option<crate::direct::ContinueFrom>,
    /// Protocol 1.38.0: the user's research switch. Absent means the stage default, which is on; `ham run --no-research` sends
    /// `{"enabled": false}`. `run-session` writes the answer into the run's policy row, which the research route reads on every call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub research: Option<crate::research::ResearchOptions>,
    /// Protocol 1.38.0: `false` switches the clean-checkout test report off for this run. Absent (the default) and `true` leave it on;
    /// only `false` is ever sent or stored, so a request that keeps the default is byte for byte the request of an older client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_tests: Option<bool>,
    /// Protocol 1.41.0: the seconds one command of the clean-checkout test report may run, from the user's `run.report_command_minutes`.
    /// Absent (the default, and what every client without the setting sends) leaves the stage's own limit, 300 s unless the stage says
    /// otherwise, and is left out of the stored request. [`crate::direct::report_step_s_ok`] is the one check: 1 to
    /// [`crate::direct::MAX_REPORT_STEP_S`]; control-api answers 400 outside it and agentd checks again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_step_s: Option<u32>,
    /// Protocol 1.38.0: the user's goal-loop switch. Absent means the stage default, which is on; `ham ... --goal off`
    /// sends `{"enabled": false}` and agentd then places nothing for the engine's goal plugin and starts no loop. `--goal on` sends
    /// `{"enabled": true}`, which is the stage default said out loud.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<crate::direct::GoalOptions>,
    /// Protocol 1.38.0: what a delivered copy of a stored third-party file does. Absent means `block` (the run ends
    /// without a zip); `ham ... --allow-third-party` sends `allow`, which `run-session` passes to the bundle validator. `block` (what another
    /// client may send) is the same as no member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub third_party: Option<crate::research::ThirdPartyMode>,
    /// Protocol 1.38.0: files of the user's own that Cline reads as context, written by agentd read-only into
    /// `context/attachments/` outside the work tree. No limit on the number of items or their size; [`crate::direct::check_attachments`] is the one
    /// validator of `ham`, control-api and agentd. The request as a whole is held to [`crate::direct::MAX_REQUEST_BYTES`]. Empty (the default, and what an old client sends) is left out of the stored request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<crate::direct::Attachment>,
    /// Protocol 1.44.0: the user's switch for the review round and, when the user has a review file, its body. Absent means the stage
    /// default, which is on; `ham` sends the member only when `run.review_round` is `off` or `~/.config/ham/review.md` exists.
    /// [`crate::direct::check_review`] is the one check of `ham`, control-api (400 `invalid-review`) and agentd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<crate::direct::ReviewOptions>,
    /// Protocol 1.45.0: the user's choice of machine for the session, `microvm` or `instances`. Absent means the stage's own policy
    /// (what `auto` means too); `ham` sends the member only for the two explicit words, so a user who never sets `run.compute` sends the request
    /// of an older client. The request type refuses an unknown word before anything is stored. The choice made at session start is recorded on
    /// the Agent item and does not change for the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute: Option<crate::direct::ComputeOptions>,
    /// Protocol 1.48.0: the user's switches for the build defaults of the session (cargo without debug information, cargo without
    /// incremental state, `target/` on the exclude list). Absent means all three are on; `ham` sends only the members the user turned off
    /// (`false`), so a user who never sets one sends the request of an older client. The request type refuses an unknown member before anything is
    /// stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<crate::direct::BuildOptions>,
    /// The most seconds the run may use in all, across the sessions it renews itself into. Absent (the default, and what every client
    /// without the setting sends) means [`crate::direct::DEFAULT_MAX_RUN_S`] and is left out of the stored request. [`crate::direct::max_run_s_ok`]
    /// is the one check: [`crate::direct::MIN_MAX_RUN_S`] to [`crate::direct::MAX_MAX_RUN_S`]; control-api answers 400 outside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_run_s: Option<u32>,
}

/// `GET /v1/direct/models` (protocol 1.31.0; served only by the `direct` stage) and `ham models --json`: every
/// model of the static list (`engine/catalog/models.json`) and which one is the default. `POST /v1/direct/runs` accepts a
/// `model` that is the `id` of a row with `selectable` true.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelList {
    /// The `id` of the default model (Claude Sonnet 5.5 unless the list says otherwise).
    pub default: String,
    pub models: Vec<ModelRow>,
}

/// One model of the [`ModelList`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelRow {
    /// The Bedrock inference profile or model id: what a request's `model` names.
    pub id: String,
    pub name: String,
    pub vendor: String,
    /// The short name `ham` accepts for the model (`--model sonnet`); the server accepts only the `id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    pub protocol: Protocol,
    pub region: String,
    pub context_tokens: u32,
    pub max_output_tokens: u32,
    /// A passing smoke record exists; `false` rows are listed but refused.
    pub selectable: bool,
    pub default: bool,
    /// One line to read before choosing the model; absent for most rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StartRunResponse {
    pub run_id: String,
    pub status: GoalStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    pub run_id: String,
    pub status: GoalStatus,
    pub phase: GoalPhase,
    pub target: RunTarget,
    pub created_at: String,
    pub owner_principal_arn: String,
    /// Why a cleared run ended when its `run` execution ended ABORTED, FAILED or TIMED_OUT before
    /// the run finished.
    // Boxed (wire-identical) so every summary, and the `ham` TUI message that carries one, grows by
    // a pointer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aborted: Option<Box<crate::orchestration::RunAbortedPayload>>,
    /// The Run item's kind: `probe` for a run `hamh-dispatch` wrote, absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<RunKind>,
    /// Protocol 1.37.0: set on an `active` direct run whose Cline is waiting for the user (the open blocking question), so the run list can
    /// show it. control-api fills it from a tail read of the direct agent's events; a failed read leaves it absent.
    // Boxed (wire-identical) like `aborted`, so every summary, and the `ham` TUI message that carries one, grows by a pointer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_input: Option<Box<NeedsInputBrief>>,
    /// Protocol 1.38.0: the id of the run this follow-up run continues from (`ham continue`), the run whose delivered head it started
    /// on. Absent for an ordinary run and for every run that was stored before the member existed. It is the id alone: the request's
    /// `continueFrom` object (with the head control-api resolved) stays in the stored start request. `RunDetail` carries it in its `summary`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_from: Option<String>,
    /// Why a `paused` or `stalled` run stopped, as the Run item stores it (`code: sentence`). Absent for every other status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    /// How a `paused` or `stalled` run can go on. Absent for every other status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_mode: Option<crate::goal::ResumeMode>,
    /// The number of the run's last session (slice); 1 for a run that never renewed its session. Absent for a run written before the
    /// member existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<u32>,
}

/// The one-line form of an open question in the run list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NeedsInputBrief {
    pub request_id: String,
    pub reason: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// `POST /v1/runs/{runId}/input`: the user's answer to a question of a direct run (owner only). The request id is
/// the nonce: the same answer again is idempotent, another answer to the same request is 409 `already-answered`. Rules by action:
/// `continue_with_prompt` needs `text`; `new_session_with_prompt` takes `text` optionally; `try_model` needs `model` and takes `text`
/// optionally; `finish` takes neither. The origin is not a member: control-api writes `user`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputAnswer {
    /// The `requestId` of the open `needs-input` event (a ULID).
    pub request_id: String,
    pub action: crate::direct_input::InstructAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// `try_model` only: a route id from the question's `models`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// The reply to [`InputAnswer`]: 202 `queued` or `re-queued`, 200 `already-queued`. Queued means the answer is in the run's control queue,
/// not that Cline has taken it: the `input-received` event says so.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InputAccepted {
    pub run_id: String,
    pub request_id: String,
    /// `queued`, `already-queued` or `re-queued`.
    pub state: String,
}

impl RunSummary {
    /// A run a probe wrote through `hamh-dispatch`: not one the user started.
    pub fn is_probe(&self) -> bool {
        self.kind == Some(RunKind::Probe)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListRunsResponse {
    pub runs: Vec<RunSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_token: Option<String>,
}

/// Links come only from harness-authored delivery events (invariant 4).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunLinks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zip_artifact_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunDetail {
    pub summary: RunSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetLimit>,
    pub tokens_used: u64,
    pub cost_usd: f64,
    #[serde(default)]
    pub links: RunLinks,
    /// `costUsd` by model and engine; absent until a turn of the run reports usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<RunCost>,
}

/// A run's token cost by model and engine: the engine-reported usage of its turns
/// priced with control-api's pinned price table. The authoritative check is the Bedrock invocation
/// log keyed `<run>.<agent>`: `ham-cost-reconcile` requires the two to agree within 2%.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunCost {
    /// The pinned price table's version.
    pub prices_version: String,
    /// One line per model and engine, in model order.
    pub lines: Vec<CostLine>,
    /// Turns that ended without engine-reported usage: their tokens are in no line.
    pub turns_without_usage: u32,
    /// Models with usage but no row in the price table: their tokens count and price at zero.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unpriced_models: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CostLine {
    pub model_id: String,
    pub engine: Engine,
    pub usage: Usage,
    pub usd: f64,
}

/// Response to pause, resume and cancel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunControlResponse {
    pub run_id: String,
    pub status: GoalStatus,
}

/// `GET .../events?shard=&after=`: one shard, events after the cursor in ULID order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventsPage {
    pub events: Vec<Envelope>,
    /// Cursor for the next read; the last `eventId` returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_after: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactView {
    pub artifact_id: String,
    pub kind: ArtifactKind,
    pub s3_uri: String,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListArtifactsResponse {
    pub artifacts: Vec<ArtifactView>,
}

/// One repository allowlist entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepoAllowView {
    pub target: crate::target::RepoTarget,
    pub added_by: String,
    pub added_at: String,
}

/// `GET /v1/repos`: the allowlist `ham doctor` prints.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListReposResponse {
    pub repos: Vec<RepoAllowView>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RepoAdminAction {
    Allow,
    Deny,
}

/// `POST /v1/admin/repos`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepoAdminRequest {
    pub action: RepoAdminAction,
    pub target: crate::target::RepoTarget,
}

// A request that still carries the retired member `audrecRecordId` (an older `ham-admin` sends it) is accepted and the member is ignored: it is
// neither checked nor kept, and it is not part of the contract. Any other unknown member is refused.
impl<'de> Deserialize<'de> for RepoAdminRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // The members the contract has, and the retired one that is read and dropped.
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Members {
            action: RepoAdminAction,
            target: crate::target::RepoTarget,
            #[serde(default)]
            #[allow(dead_code)]
            audrec_record_id: Option<serde::de::IgnoredAny>,
        }
        let m = Members::deserialize(deserializer)?;
        Ok(Self {
            action: m.action,
            target: m.target,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepoAdminResponse {
    pub action: RepoAdminAction,
    pub target: crate::target::RepoTarget,
    /// The entry after an `allow`; absent after a `deny`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<RepoAllowView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DownloadUrlResponse {
    pub url: String,
    /// The presigned URL's true expiry.
    pub expires_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelsResponse {
    pub rows: Vec<CatalogRow>,
    pub excluded: Vec<ExcludedModel>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WhoAmIResponse {
    pub principal_arn: String,
    pub account: String,
}
