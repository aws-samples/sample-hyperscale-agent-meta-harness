//! `POST /v1/direct/runs`: a run that passes the user's prompt, unchanged, to
//! Cline on one model with the user's rules. No planner, roles, gates or evaluator read it.
//!
//! The request is its own type (`StartDirectRunRequest`, which refuses unknown members). Nothing is written until the request
//! is valid: the model must be in `models.json`, the rules and the nudge must pass `ham_core::rules`.
//!
//! After validation the start is: the idempotency claim, the request in S3, the Run item, the root Goal and `run.created`
//! (whose payload says `"mode": "direct"`), and one execution named after the run. Between the claim and the request in S3 a request that
//! would launch a run asks the stage's deploy lock: a stage that is being updated starts no new runs.

use crate::body::parse_body;
use crate::direct_models;
use crate::handler::{
    Blobs, Deps, Execution, OBJECTIVE_ITEM_MAX_BYTES, Reply, Store, StoreError, V1Request,
    compute_not_set_up, error, head_bytes, json, not_found, owned, unavailable,
    valid_idempotency_key,
};
use crate::ids::{new_ulid, now_rfc3339, sha256_hex};
use harness_protocol::api::v1::{StartDirectRunRequest, StartRunResponse};
use harness_protocol::common::Engine;
use harness_protocol::direct::{
    ComputeMode, ContinueFrom, DEFAULT_MAX_RUN_S, EngineConfigProblem, MAX_INPUT_WAIT_S,
    MAX_MAX_RUN_S, MAX_REPORT_STEP_S, MIN_MAX_RUN_S, check_attachments, check_continue_from,
    check_engine_config, check_goal, check_limits, check_review, check_skills, max_run_s_ok,
    report_step_s_ok,
};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::goal::{GoalPhase, GoalStatus};
use harness_protocol::store::{GoalItem, IdempotencyItem, RunItem, s3};
use harness_protocol::target::{RepoTarget, RunTarget};
use serde_json::json as value;

const RETRY: &str = "retry with the same Idempotency-Key";

/// The status of the reasoning-level refusal: 422, like the model check beside it.
pub const REASONING_REFUSAL_STATUS: u16 = 422;

/// Why the engine settings of a request ask for a reasoning level the chosen model's row does not list as accepted, or `None`. `default` and
/// `off` are never checked against the row, and a request without the key is not checked at all.
pub fn reasoning_refusal(
    models: &direct_models::DirectModels,
    r: &StartDirectRunRequest,
) -> Option<ham_core::models::ReasoningRefusal> {
    let level = r
        .engine_config
        .as_ref()?
        .get("reasoning.effort")?
        .as_str()?;
    if matches!(level, "default" | "off") {
        return None;
    }
    models.check_reasoning(&r.model, level).err()
}

/// Why the engine member of a request is refused: the status, the stable `code` and the words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineRefusal {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

/// The engines a run may name: the engines that have a profile in this build (`harness_core::engine_profile::known_engine`, the one list) and at least one
/// row with a passing smoke record in `catalog`. The default engine first.
fn supported_engines(catalog: &direct_models::Catalog) -> Vec<Engine> {
    catalog
        .engines()
        .into_iter()
        .filter(|e| harness_core::engine_profile::known_engine(e.as_str()).is_ok())
        .filter(|e| {
            catalog
                .for_engine(*e)
                .is_some_and(|l| l.models().iter().any(|m| m.selectable()))
        })
        .collect()
}

/// The refusal of the engine of a request and of its (engine, model) pair, or `None`. A request that names no engine is a request for the default
/// engine and is checked exactly as it was: the model must be a selectable row of that engine's list, with the same words. A request that names an engine needs a profile for it
/// in this build (422 `unknown-engine`) and a row of that engine for the model with a passing smoke record (422 `unknown-model` or `model-not-smoked`; the words
/// name the pair).
pub fn engine_refusal(
    catalog: &direct_models::Catalog,
    r: &StartDirectRunRequest,
) -> Option<EngineRefusal> {
    let engine = Engine::or_default(r.engine);
    if let Some(named) = r.engine
        && harness_core::engine_profile::known_engine(named.as_str()).is_err()
    {
        let names: Vec<String> = supported_engines(catalog)
            .into_iter()
            .map(|e| e.to_string())
            .collect();
        return Some(EngineRefusal {
            status: 422,
            code: "unknown-engine",
            message: format!(
                "engine {:?} is not one of the supported engines: {}",
                named.as_str(),
                names.join(", ")
            ),
        });
    }
    if let Err(e) = catalog.check(engine, &r.model) {
        return Some(EngineRefusal {
            status: 422,
            code: e.code(),
            message: e.to_string(),
        });
    }
    None
}

/// The refusal of settings that are for another engine than the run uses, or `None`. The engine of the run is the request's, or the default engine
/// when it names none. `refusal` calls it after the settings check itself, so a member the check already refuses (an engine with no key table) keeps its
/// words; this one closes the case of settings for an engine that has a table but is not the run's.
pub fn engine_config_mismatch(r: &StartDirectRunRequest) -> Option<EngineRefusal> {
    let engine = Engine::or_default(r.engine);
    let cfg = r.engine_config.as_ref()?;
    (cfg.engine != engine.as_str()).then(|| EngineRefusal {
        status: 400,
        code: "invalid-engine-config",
        message: format!(
            "engineConfig is for engine {:?}, but the run uses engine {:?}; send the settings of the engine the run uses",
            cfg.engine,
            engine.as_str()
        ),
    })
}

/// The status of the provider-sharing refusal: 400, because the request lacks a member the chosen model needs.
pub const PROVIDER_SHARING_REFUSAL_STATUS: u16 = 400;

/// Why the request names a model that needs provider data sharing without agreeing to it, or `None`. `models` is the list of the engine the request
/// names. A model this list does not know is the business of the model check, so it
/// passes here. `providerSharing: false` is the same as no member.
pub fn provider_sharing_refusal(
    models: &direct_models::DirectModels,
    r: &StartDirectRunRequest,
) -> Option<ham_core::models::SharingRefusal> {
    let model = models.models().iter().find(|m| m.id == r.model)?;
    model.check_sharing(r.provider_sharing == Some(true)).err()
}

/// The 4xx a direct request earns before anything is written, or `None`.
fn refusal(r: &StartDirectRunRequest) -> Option<Reply> {
    refusal_with(direct_models::builtin_catalog(), r)
}

/// [`refusal`] over another catalog (tests).
fn refusal_with(catalog: &direct_models::Catalog, r: &StartDirectRunRequest) -> Option<Reply> {
    if r.prompt.trim().is_empty() {
        return Some(error(400, "bad-request", "prompt is empty"));
    }
    // The one limit of a request: what the platform takes in one call. The prompt, the rules, the skills, the attachments and the two prompt
    // files have no size of their own; the refusal names the bound and the part that weighs most.
    if let Err(e) = harness_protocol::direct::check_request(r) {
        return Some(error(413, "payload-too-large", &e.to_string()));
    }
    if let Some(s) = r.input_wait_s
        && s > MAX_INPUT_WAIT_S
    {
        return Some(error(
            400,
            "bad-request",
            &format!(
                "inputWaitS is {s} seconds; the limit is {MAX_INPUT_WAIT_S} (0, or leaving it out, means do not wait)"
            ),
        ));
    }
    // The seconds one command of the clean-checkout test report may run: the check agentd calls again.
    if let Some(s) = r.report_step_s
        && !report_step_s_ok(s)
    {
        return Some(error(
            400,
            "bad-request",
            &format!("reportStepS is {s} seconds; it must be 1 to {MAX_REPORT_STEP_S}"),
        ));
    }
    // The total seconds the run may use across its sessions: the one check `ham` and run-session also call.
    if let Some(s) = r.max_run_s
        && !max_run_s_ok(s)
    {
        return Some(error(
            400,
            "bad-request",
            &format!(
                "maxRunS is {s} seconds; it must be {} to {} (leaving it out means {})",
                MIN_MAX_RUN_S, MAX_MAX_RUN_S, DEFAULT_MAX_RUN_S
            ),
        ));
    }
    // The container signals of the `compute` member: the one check `ham` also calls.
    if let Some(list) = r
        .compute
        .as_ref()
        .and_then(|c| c.container_signals.as_deref())
        && let Err(e) = ham_core::container_signals::check_list(list)
    {
        return Some(error(400, "invalid-container-signals", &e));
    }
    // The engine of the run and the (engine, model) pair; for a request that names no engine this is the model check it always was.
    if let Some(e) = engine_refusal(catalog, r) {
        return Some(error(e.status, e.code, &e.message));
    }
    // The list of the engine the run uses; the pair passed `engine_refusal`, so the engine has rows (the default list is the answer of a request that names none).
    let engine_list = catalog
        .for_engine(Engine::or_default(r.engine))
        .unwrap_or_else(|| catalog.default_list());
    // A model that needs provider data sharing: the request must agree to it, in the same words `ham` refuses with before it sends anything.
    if let Some(e) = provider_sharing_refusal(engine_list, r) {
        return Some(error(
            PROVIDER_SHARING_REFUSAL_STATUS,
            e.code(),
            &e.server_message(),
        ));
    }
    if let Err(e) = ham_core::rules::validate_rule_files(&r.rules) {
        return Some(error(400, "invalid-rules", &e.to_string()));
    }
    if let Some(n) = &r.nudge
        && let Err(e) = ham_core::rules::validate_continue(n)
    {
        return Some(error(400, "invalid-nudge", &e.to_string()));
    }
    // The one skills validator that `ham` and agentd also call.
    if let Err(e) = check_skills(&r.skills) {
        return Some(error(400, "invalid-skills", &e.to_string()));
    }
    // The attachments: the one validator of `ham` and agentd, with words that name the item and never
    // its content. The `goal`, `research` and `thirdParty` members are checked by the request type itself (a wrong type or an unknown key is a
    // 400 `bad-request` before anything is written) and are stored as sent.
    if let Err(e) = check_attachments(&r.attachments) {
        return Some(error(400, "invalid-attachments", &e.to_string()));
    }
    // The engine settings (protocol 1.39.0): the one check that `ham` and agentd also call. The reply names the problem, the key (when it is plain
    // text) and the rule, and never a value; nothing is written.
    if let Some(c) = &r.engine_config
        && let Err(p) = check_engine_config(c)
    {
        return Some(engine_config_refusal(&p));
    }
    // Settings for another engine than the run uses, after the settings check so that its words stay for the members it refuses.
    if let Some(e) = engine_config_mismatch(r) {
        return Some(error(e.status, e.code, &e.message));
    }
    // The reasoning level the user asked for: the row of the chosen model must list it as accepted.
    if let Some(e) = reasoning_refusal(engine_list, r) {
        return Some(error(REASONING_REFUSAL_STATUS, e.code(), &e.to_string()));
    }
    // The stall limits of the `goal` member: the one check that `ham` and agentd also call. The reply names the member
    // and its range and never the value; nothing is written.
    if let Some(g) = &r.goal
        && let Err(p) = check_goal(g)
    {
        return Some(error(400, "invalid-goal", &p.to_string()));
    }
    // The per-run limits: the one check that `ham` and agentd also call. The reply names the member and its range and never
    // the value; nothing is written.
    if let Some(l) = &r.limits
        && let Err(p) = check_limits(l)
    {
        return Some(error(400, "invalid-limits", &p.to_string()));
    }
    // The review round's member: the one check that `ham` and agentd also call. The reply names the member and the rule and
    // never the text or a hash; nothing is written.
    if let Some(v) = &r.review
        && let Err(p) = check_review(v)
    {
        return Some(error(400, "invalid-review", &p.to_string()));
    }
    if let Some(c) = &r.continue_from {
        if let Some(why) = continue_target_refusal(r, &c.run_id) {
            return Some(error(400, "invalid-continue-from", &why));
        }
        if let Err(e) = check_continue_from(c) {
            return Some(error(400, "invalid-continue-from", &e.to_string()));
        }
    }
    None
}

/// The 400 `invalid-engine-config` reply: `code`, the words of the check in `message`, the stable word of the problem in `problem`, the key in `key`
/// (left out when it has unusual characters) and the rule in `rule`. A client that reads only `code` and `message` (the `ApiError` shape) sees
/// the same words as every other refusal.
fn engine_config_refusal(p: &EngineConfigProblem) -> Reply {
    let mut body = serde_json::Map::new();
    body.insert("code".into(), value!("invalid-engine-config"));
    body.insert("message".into(), value!(p.to_string()));
    body.insert("problem".into(), value!(p.code()));
    if let Some(key) = p.key() {
        body.insert("key".into(), value!(key));
    }
    body.insert("rule".into(), value!(p.rule()));
    json(400, &serde_json::Value::Object(body))
}

/// Why the target and base of a request with `continueFrom` cannot be right, or `None`. A follow-up starts from the commit
/// the source run delivered: the `s3-only` target takes no `baseRef`; a CodeCommit repository takes no `baseRef` or the source's own branch
/// `harness/<source run>` (the base is that commit whatever has become of the branch); a new repository or any other kind of repository cannot be
/// continued. That the repository is the one the source run used is checked against the source run ([`resolve_continue`]).
fn continue_target_refusal(r: &StartDirectRunRequest, source: &str) -> Option<String> {
    match &r.target {
        RunTarget::S3Only if r.base_ref.is_some() => Some(
            "continueFrom with the s3-only target takes no baseRef (a repository follow-up may name only harness/<runId> of the run it continues)"
                .to_string(),
        ),
        RunTarget::S3Only => None,
        RunTarget::NewRepo { .. } => Some(
            "continueFrom does not start a --new-repo run: name the repository the run used (cc:<name>) or use the s3-only target".to_string(),
        ),
        RunTarget::Repo { repo } if !matches!(repo, RepoTarget::CodeCommit { .. }) => Some(
            "continueFrom takes the s3-only target or a CodeCommit repository (cc:<name>)".to_string(),
        ),
        RunTarget::Repo { .. } => {
            let own = [
                format!("harness/{source}"),
                format!("refs/heads/harness/{source}"),
            ];
            match &r.base_ref {
                Some(b) if !own.contains(b) => Some(format!(
                    "a repository follow-up starts from the commit run {source} delivered; baseRef {b} is not accepted (name harness/{source} or leave it out)"
                )),
                _ => None,
            }
        }
    }
}

/// The source of a follow-up run. The run must be the caller's (any other case answers like a missing run, so a run id never
/// reveals another principal's run), finished, and its verified bundle must name a head on `refs/heads/harness/<runId>`; a head the client
/// named must be that one. Returns the `continueFrom` to store, with the head control-api resolved, or the reply that refuses the start.
/// Nothing is written here.
async fn resolve_continue<S: Store, B: Blobs, E: Execution>(
    c: &ContinueFrom,
    target: &RunTarget,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Result<ContinueFrom, Reply> {
    let source = match owned(&deps.store, &c.run_id, caller).await {
        Ok(Some(run)) => run,
        Ok(None) => return Err(not_found(&format!("no run {} for this caller", c.run_id))),
        Err(e) => return Err(unavailable(&e, RETRY)),
    };
    // A repository follow-up continues a run of the same repository (the pull request it opens is aimed at that run's branch or at the
    // destination of that run's pull request); the source run kept its own target on its Run item.
    if let RunTarget::Repo { repo } = target {
        let same = match &source.target {
            RunTarget::Repo { repo: used } | RunTarget::NewRepo { repo: used } => used == repo,
            RunTarget::S3Only => false,
        };
        if !same {
            return Err(error(
                400,
                "invalid-continue-from",
                &format!(
                    "run {} targeted {}; a follow-up on {repo} continues a run of the same repository",
                    c.run_id,
                    ham_core::run_index::target_text(&source.target)
                ),
            ));
        }
    }
    if source.status == GoalStatus::Active {
        return Err(error(
            409,
            "run-active",
            &format!(
                "run {} has not finished; a follow-up starts from the code a finished run delivered",
                c.run_id
            ),
        ));
    }
    let key = s3::verified_bundle_key(&c.run_id);
    let no_head = || {
        error(
            409,
            "no-delivered-head",
            &format!("run {} delivered no code to continue from", c.run_id),
        )
    };
    // Whether the bundle exists is asked by a listing by the key, not by a read: a role that may not list the bucket gets 403 for a GET of an
    // absent key, which `read_prefix` reports as unavailable, and a run that delivered nothing would answer "retry" for ever.
    match deps.blobs.exists(&key).await {
        Ok(true) => {}
        Ok(false) => return Err(no_head()),
        Err(e) => return Err(unavailable(&e, RETRY)),
    }
    let object = match deps
        .blobs
        .read_prefix(&key, ham_core::snapshot::HEADER_BYTES)
        .await
    {
        Ok(Some(o)) => o,
        // Removed between the listing and the read.
        Ok(None) => return Err(no_head()),
        Err(e) => return Err(unavailable(&e, RETRY)),
    };
    let wanted = format!("refs/heads/harness/{}", c.run_id);
    let head = match ham_core::snapshot::bundle_head_of(&object.bytes, &wanted) {
        Ok(h) => h,
        Err(e) => {
            // The cause is for the log; the answer says only that the bundle cannot be read as a head.
            let (reply, _) = error(
                422,
                "delivered-head-unreadable",
                &format!(
                    "the delivered bundle of run {} cannot be read as a head",
                    c.run_id
                ),
            );
            return Err((reply, vec![format!("continueFrom {}: {e}", c.run_id)]));
        }
    };
    if let Some(named) = &c.head_sha
        && *named != head
    {
        return Err(error(
            409,
            "head-mismatch",
            &format!(
                "run {} delivered commit {}; the request named {}",
                c.run_id,
                &head[..12],
                &named[..named.len().min(12)]
            ),
        ));
    }
    Ok(ContinueFrom {
        run_id: c.run_id.clone(),
        head_sha: Some(head),
    })
}

/// `GET /v1/direct/models`: the models a direct run may name, with the default marked (`ham models --server`). It reads the
/// compiled-in `models.json` and nothing else.
pub(crate) fn list_models() -> Reply {
    // The rows of the default engine as ever, then the rows of other engines (each names its engine); a build with rows of one engine lists what it always listed.
    json(200, &direct_models::builtin_catalog().list())
}

pub(crate) async fn start_direct_run<S: Store, B: Blobs, E: Execution>(
    req: &V1Request,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    let Some(key) = req.headers.get("idempotency-key") else {
        return error(
            400,
            "idempotency-key-required",
            "POST /v1/direct/runs needs an Idempotency-Key header",
        );
    };
    if !valid_idempotency_key(key) {
        return error(
            400,
            "bad-request",
            "Idempotency-Key must be 1-128 visible ASCII characters",
        );
    }
    let request: StartDirectRunRequest = match parse_body("StartDirectRunRequest", &req.body) {
        Ok(r) => r,
        Err(reply) => return reply,
    };
    let prepared = match prepare(request, caller, deps).await {
        Ok(p) => p,
        Err(reply) => return reply,
    };
    let claim_sha256 = sha256_hex(&prepared.canonical);
    let linked_to = prepared
        .request
        .continue_from
        .as_ref()
        .map(|c| c.run_id.clone());
    start_prepared(
        &prepared,
        key,
        claim_sha256,
        linked_to.as_deref(),
        caller,
        deps,
    )
    .await
}

/// A request that passed every check of a start, with the bytes that are stored for it.
pub(crate) struct Prepared {
    pub(crate) request: StartDirectRunRequest,
    /// The canonical bytes of `request`: what `runs/<id>/input/request.json` holds and what agentd reads.
    pub(crate) canonical: Vec<u8>,
}

/// Every check of a start that needs no write: the request's own limits ([`refusal`]) and, for a follow-up, the source run and its delivered
/// head ([`resolve_continue`]). `POST /v1/direct/runs` and the retry of a blocked run (`crate::retry`) both end here, so a retry is held to
/// what a fresh start is held to.
pub(crate) async fn prepare<S: Store, B: Blobs, E: Execution>(
    mut request: StartDirectRunRequest,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Result<Prepared, Reply> {
    if let Some(reply) = refusal(&request) {
        return Err(reply);
    }
    // A request that names the default engine is the request of one that names none: stored without the member, so the bytes of a default run do not depend
    // on whether the client said the word, and an image that predates the member still reads them.
    if request.engine == Some(Engine::DEFAULT) {
        request.engine = None;
    }
    // A window of 0 is no window: the stored object (and its digest, and the idempotency check) is then the one a client that left the
    // member out gets, so an agentd image that predates the member can still parse it.
    if request.input_wait_s == Some(0) {
        request.input_wait_s = None;
    }
    // The follow-up's source is read before anything is written; the stored request carries the head resolved here.
    if let Some(c) = request.continue_from.clone() {
        request.continue_from = Some(resolve_continue(&c, &request.target, caller, deps).await?);
    }
    match serde_json::to_vec(&request) {
        Ok(canonical) => Ok(Prepared { request, canonical }),
        Err(e) => Err(error(
            400,
            "bad-request",
            &format!("StartDirectRunRequest: {e}"),
        )),
    }
}

/// The write side of a start, after [`prepare`]: the idempotency claim, the request in S3, the Run item, the root Goal and `run.created`,
/// one execution named after the run, and the run's search item. `claim_sha256` is what the claim compares for `key` (a fresh start hashes
/// the request; a retry hashes the blocked run's id with it, so a key cannot return the run of another start). `linked_to` is the run the new
/// Run item names in `continueFrom`: the source of a follow-up, or the blocked run a retry starts again.
pub(crate) async fn start_prepared<S: Store, B: Blobs, E: Execution>(
    prepared: &Prepared,
    key: &str,
    claim_sha256: String,
    linked_to: Option<&str>,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    let Prepared { request, canonical } = prepared;
    let fresh = IdempotencyItem {
        principal_arn: caller.to_string(),
        idempotency_key: key.to_string(),
        run_id: new_ulid(),
        request_sha256: claim_sha256.clone(),
        claimed_at: now_rfc3339(),
    };
    // Read before any write, so a failed read leaves nothing behind.
    let deployed = match deps.execution.deployment().await {
        Ok(d) => d,
        Err(e) => return unavailable(&e, RETRY),
    };
    // The compute choice: `instances` needs the stage's Instances worker runtime. The stage is asked only for that word, and
    // before anything is written; `microvm`, `auto` and no member are never refused. The stage decides what `auto` means (run-session).
    if request
        .compute
        .as_ref()
        .is_some_and(|c| c.mode == ComputeMode::Instances)
    {
        match deps.execution.instances_available().await {
            Ok(true) => {}
            Ok(false) => {
                return error(
                    422,
                    "compute-unavailable",
                    "this stage has no Instances worker runtime; start the run with --compute microvm, or set run.compute to microvm or auto",
                );
            }
            Err(e @ StoreError::NotSetUp(_)) => return compute_not_set_up(&e),
            Err(e) => return unavailable(&e, RETRY),
        }
    }
    let claim = match deps.store.claim_idempotency(&fresh).await {
        Ok(c) => c,
        Err(e) => return unavailable(&e, RETRY),
    };
    if claim.request_sha256 != claim_sha256 {
        return error(
            422,
            "idempotency-key-reused",
            "this Idempotency-Key was used for a different request; send a new key",
        );
    }
    let run_id = claim.run_id;
    // What the look at the stage's deploy lock leaves for the log.
    let mut notes: Vec<String> = Vec::new();
    let run = match deps.store.get_run(&run_id).await {
        // A repeat of a start that was admitted and launched (its first session pinned the worker runtime on the Run item): it returns its
        // run whatever the stage is doing, and the StartExecution below repeats a start that happened.
        Ok(Some(run)) if run.runtime_endpoint.is_some() => run,
        Ok(found) => {
            // This request would launch work: a new run, or a run that nothing shows to have started. The stage's deploy lock is read now,
            // right before the write that creates the run, and a deploy that holds the stage refuses the start (`crate::admission`). Only
            // the claim above is written by then, and a repeat of the key after the update creates the run.
            match crate::admission::admit(&deps.execution, RETRY).await {
                Ok(lines) => notes = lines,
                Err(reply) => return reply,
            }
            match found {
                Some(run) => run,
                None => {
                    match create_run(
                        &run_id, caller, request, canonical, linked_to, deployed, deps,
                    )
                    .await
                    {
                        Ok(run) => run,
                        Err(e) => return unavailable(&e, RETRY),
                    }
                }
            }
        }
        Err(e) => return unavailable(&e, RETRY),
    };
    // Step Functions keeps one execution per name, so a retry never starts a second one.
    if let Err(e) = deps
        .execution
        .start(&run_id, &value!({ "runId": run_id }))
        .await
    {
        let (resp, mut logs) = unavailable(
            &e,
            &format!("run {run_id} is recorded but did not start; {RETRY}"),
        );
        logs.extend(notes);
        return (resp, logs);
    }
    // The search item: the run's own `createdAt` addresses it, so a retry reaches the same key and completes a first attempt's miss.
    // A failure is logged and never fails the start; the backfill makes the item later.
    notes.extend(
        crate::run_index::write_at_start(
            &deps.store,
            caller,
            &run,
            &request.prompt,
            &request.model,
        )
        .await,
    );
    let (resp, _) = json(
        201,
        &StartRunResponse {
            run_id,
            status: run.status,
        },
    );
    (resp, notes)
}

/// The request object first, then the Run item, root Goal and `run.created` in one transaction, so a run never exists without
/// its request. `linked_to` is the id the Run item keeps in `continueFrom` (see [`start_prepared`]).
async fn create_run<S: Store, B: Blobs, E: Execution>(
    run_id: &str,
    caller: &str,
    request: &StartDirectRunRequest,
    canonical: &[u8],
    linked_to: Option<&str>,
    deployed: crate::handler::DeployedAs,
    deps: &Deps<S, B, E>,
) -> Result<RunItem, StoreError> {
    let request_uri = deps.blobs.put(&s3::request_key(run_id), canonical).await?;
    let now = now_rfc3339();
    let run = RunItem {
        run_id: run_id.to_string(),
        owner_principal_arn: caller.to_string(),
        prompt_sha256: sha256_hex(request.prompt.as_bytes()),
        target: request.target.clone(),
        base_ref: request.base_ref.clone(),
        status: GoalStatus::Active,
        phase: GoalPhase::Planning,
        budget: None,
        // The direct line has no engine policy; the field stays on the item for its readers.
        engine_policy_version: 0,
        created_at: now.clone(),
        deployment_digest: deployed.digest,
        source_tree: deployed.source_tree,
        version: 1,
        runtime_version: None,
        runtime_endpoint: None,
        aborted: None,
        // The id of the source run (a follow-up) or of the blocked run (a retry); the stored request of a follow-up keeps the head control-api
        // resolved beside its own `continueFrom`.
        continue_from: linked_to.map(str::to_string),
        status_reason: None,
        resume_mode: None,
        slice: None,
        // The total time the request named; absent means the default, which the run's slices apply.
        max_run_s: request.max_run_s,
        // The engine the request named, stored as sent; absent means Cline, as in the stored request.
        engine: request.engine,
    };
    let goal = GoalItem {
        run_id: run_id.to_string(),
        task_id: None,
        objective: head_bytes(&request.prompt, OBJECTIVE_ITEM_MAX_BYTES).to_string(),
        requirements: vec![],
        spec_sha256: String::new(),
        acceptance_sha256: String::new(),
        status: GoalStatus::Active,
        phase: GoalPhase::Planning,
        last_blocker_fingerprint: None,
        same_blocker_streak: 0,
        empty_turn_streak: 0,
        exec_failure_streak: 0,
        ladder_rung: 0,
        evaluator_verdicts: vec![],
        token_budget: None,
        tokens_used: 0,
        wall_clock_budget_s: None,
        time_used_s: 0,
        turn_count: 0,
        version: 1,
    };
    let created = Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: new_ulid(),
        run_id: run_id.to_string(),
        task_id: None,
        agent_id: None,
        seq: 0,
        ts: now,
        event_type: EventType::RunCreated,
        authority: Authority::Harness,
        payload: Payload::Inline {
            value: value!({
                "target": request.target,
                "requestUri": request_uri,
                "requestSha256": sha256_hex(canonical),
                "mode": "direct",
            }),
        },
    };
    match deps.store.create_run(&run, &goal, &created).await {
        Ok(()) => Ok(run),
        // A concurrent retry created it first.
        Err(StoreError::Conflict) => deps.store.get_run(run_id).await?.ok_or_else(|| {
            StoreError::Unavailable(format!("run {run_id} conflicted but is absent"))
        }),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod b803_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use harness_protocol::api::v1::StartDirectRunRequest;
    use serde_json::json;

    const FABLE: &str = "global.anthropic.claude-fable-5-2";

    /// The committed catalog with the Fable row given a record, as it reads after the live call.
    fn smoked() -> direct_models::Catalog {
        let text = include_str!("../../../engine/catalog/models.json");
        let mut v: serde_json::Value = serde_json::from_str(text).unwrap();
        let row = v["models"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|r| r["id"] == FABLE)
            .unwrap();
        row["smokeRecordId"] = json!("s6:test-record");
        direct_models::Catalog::parse(&v.to_string()).unwrap()
    }

    /// The committed catalog with the Fable row's record taken off, as the row read before its live checks.
    fn unsmoked() -> direct_models::Catalog {
        let text = include_str!("../../../engine/catalog/models.json");
        let mut v: serde_json::Value = serde_json::from_str(text).unwrap();
        let row = v["models"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|r| r["id"] == FABLE)
            .unwrap()
            .as_object_mut()
            .unwrap();
        for member in ["smokeRecordId", "smokedAt", "reasoning", "alternateRoutes"] {
            row.remove(member);
        }
        direct_models::Catalog::parse(&v.to_string()).unwrap()
    }

    fn request(model: &str, sharing: Option<bool>) -> StartDirectRunRequest {
        let mut v = json!({"prompt": "p", "target": {"mode": "s3-only"}, "model": model});
        if let Some(s) = sharing {
            v["providerSharing"] = json!(s);
        }
        serde_json::from_value(v).unwrap()
    }

    fn status_and_body(reply: &Reply) -> (u16, serde_json::Value) {
        let text = reply.0.body.clone();
        (reply.0.status, serde_json::from_str(&text).unwrap())
    }

    #[test]
    fn b803_a_request_for_a_sharing_model_without_the_member_is_a_400_with_the_plain_reason() {
        let models = smoked();
        for sharing in [None, Some(false)] {
            let reply = refusal_with(&models, &request(FABLE, sharing)).unwrap();
            let (status, body) = status_and_body(&reply);
            assert_eq!(status, 400, "{body}");
            assert_eq!(body["code"], "provider-sharing-required", "{body}");
            let message = body["message"].as_str().unwrap();
            for want in [
                "Claude Fable 5.2",
                FABLE,
                "needs provider data sharing",
                "prompts and answers",
                "30 days",
                "providerSharing",
            ] {
                assert!(message.contains(want), "{want:?} missing: {message}");
            }
        }
    }

    #[test]
    fn b803_the_member_makes_the_request_pass_and_other_models_never_need_it() {
        let models = smoked();
        assert!(refusal_with(&models, &request(FABLE, Some(true))).is_none());
        for id in [
            "global.anthropic.claude-sonnet-5-5",
            "us.anthropic.claude-opus-5-5",
        ] {
            for sharing in [None, Some(true)] {
                assert!(
                    refusal_with(&models, &request(id, sharing)).is_none(),
                    "{id}"
                );
            }
        }
    }

    #[test]
    fn b803_an_unknown_or_unsmoked_model_keeps_its_422_before_the_sharing_check() {
        let models = unsmoked();
        let reply = refusal_with(&models, &request(FABLE, None)).unwrap();
        assert_eq!(status_and_body(&reply).0, 422);
        assert_eq!(status_and_body(&reply).1["code"], "model-not-smoked");
        let reply = refusal_with(&models, &request("us.nobody.model", Some(true))).unwrap();
        assert_eq!(status_and_body(&reply).1["code"], "unknown-model");
    }

    #[test]
    fn b803_the_provider_sharing_refusal_function_is_pure_and_public() {
        let models = smoked();
        let list = models.default_list();
        assert!(provider_sharing_refusal(list, &request(FABLE, None)).is_some());
        assert!(provider_sharing_refusal(list, &request(FABLE, Some(true))).is_none());
    }
}
