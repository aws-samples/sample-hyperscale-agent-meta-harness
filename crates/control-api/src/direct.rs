//! `POST /v1/direct/runs`: a run that passes the user's prompt, unchanged, to
//! Cline on one model with the user's rules. No planner, roles, gates or evaluator read it.
//!
//! The request is its own type (`StartDirectRunRequest`, which refuses unknown members). Nothing is written until the request
//! is valid: the model must be in `models.json`, the rules and the nudge must pass `ham_core::rules`.
//!
//! After validation the start is: the idempotency claim, the request in S3, the Run item, the root Goal and `run.created`
//! (whose payload says `"mode": "direct"`), and one execution named after the run.

use crate::body::parse_body;
use crate::direct_models;
use crate::handler::{
    Blobs, Deps, Execution, OBJECTIVE_ITEM_MAX_BYTES, Reply, Store, StoreError, V1Request,
    compute_not_set_up, error, head_bytes, json, not_found, owned, unavailable,
    valid_idempotency_key,
};
use crate::ids::{new_ulid, now_rfc3339, sha256_hex};
use harness_protocol::api::v1::{StartDirectRunRequest, StartRunResponse};
use harness_protocol::direct::{
    ComputeMode, ContinueFrom, DEFAULT_MAX_RUN_S, EngineConfigProblem, MAX_INPUT_WAIT_S,
    MAX_MAX_RUN_S, MAX_REPORT_STEP_S, MIN_MAX_RUN_S, check_attachments, check_continue_from,
    check_engine_config, check_goal, check_review, check_skills, max_run_s_ok, report_step_s_ok,
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

/// The 4xx a direct request earns before anything is written, or `None`.
fn refusal(r: &StartDirectRunRequest) -> Option<Reply> {
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
    if let Err(e) = direct_models::builtin().check(&r.model) {
        return Some(error(422, e.code(), &e.to_string()));
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
    // The reasoning level the user asked for: the row of the chosen model must list it as accepted.
    if let Some(e) = reasoning_refusal(direct_models::builtin(), r) {
        return Some(error(REASONING_REFUSAL_STATUS, e.code(), &e.to_string()));
    }
    // The stall limits of the `goal` member: the one check that `ham` and agentd also call. The reply names the member
    // and its range and never the value; nothing is written.
    if let Some(g) = &r.goal
        && let Err(p) = check_goal(g)
    {
        return Some(error(400, "invalid-goal", &p.to_string()));
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
    json(200, &direct_models::builtin().list())
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
    let run = match deps.store.get_run(&run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => {
            match create_run(
                &run_id, caller, request, canonical, linked_to, deployed, deps,
            )
            .await
            {
                Ok(run) => run,
                Err(e) => return unavailable(&e, RETRY),
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
        return unavailable(
            &e,
            &format!("run {run_id} is recorded but did not start; {RETRY}"),
        );
    }
    // The search item: the run's own `createdAt` addresses it, so a retry reaches the same key and completes a first attempt's miss.
    // A failure is logged and never fails the start; the backfill makes the item later.
    let logs: Vec<String> = crate::run_index::write_at_start(
        &deps.store,
        caller,
        &run,
        &request.prompt,
        &request.model,
    )
    .await
    .into_iter()
    .collect();
    let (resp, _) = json(
        201,
        &StartRunResponse {
            run_id,
            status: run.status,
        },
    );
    (resp, logs)
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
