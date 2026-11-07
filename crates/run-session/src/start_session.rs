//! `start-session`: binds the agent, mints the session credentials and the package token,
//! and invokes the microVM. The Step Functions task that called it is a `waitForTaskToken` state, so it completes
//! only when worker-api calls `SendTaskSuccess` with the token this op stored on the Turn item (or when the heartbeat
//! or the timeout fires).
//!
//! Every step is safe to repeat, with three differences from the old dispatcher's `dispatch_turn`:
//!
//! - **the newest token wins.** A retried `waitForTaskToken` state holds a new task token, so a retry replaces the
//!   ciphertext on the Turn item. The old dispatcher kept the first attempt's token and answered `AlreadyInvoked`, so
//!   a retried state would have waited on a token nobody completes;
//! - **the nonce is the run's, made once.** It is stored KMS-encrypted at [`nonce_key`] (the Agent item holds only its
//!   hash) and a retry reads it back, so an attempt whose acknowledgement was lost never gives the running agentd a
//!   second nonce; the Agent item is written once, so a retry cannot reset what worker-api recorded on it;
//! - **agentd's 409 for the run's own session is an acceptance.** A retry after an invocation the microVM accepted finds
//!   agentd running the turn and answering 409; the op records the turn as invoked and goes on (review S1);
//! - **the start events are written on every path.** A retry that finds the turn invoked still writes events 2 and 3
//!   (their ids are deterministic), so a write that failed after the invocation is not lost.

use crate::compute::{self, Compute};
use crate::error::RunSessionError;
use crate::events;
use crate::finalize::end_session;
use crate::ops::Deps;
use crate::ports::{Blobs, PackageTokens, Res, Sessions, Store, Tokens};
use crate::prepare::{failover_specs, model_spec, read_request};
use crate::slices;
use dispatcher::endpoints::{LIVE, Pin, choose_pin, version_endpoint};
use dispatcher::ids::{role_session_name, runtime_session_id};
use dispatcher::items::{Dispatch, agent_item, mint_plan, rfc3339};
use dispatcher::policy::session_policy_for_routes;
use harness_core::engine_profile::bundled_profile;
use harness_protocol::common::{Engine, Transport};
use harness_protocol::direct::{
    DEFAULT_SNAPSHOT_EVERY_S, DIRECT_AGENT_ID, DIRECT_TASK_ID, DIRECT_TURN,
    DirectInvocationRequest, DirectKind, DirectSlice, InputRef, MAX_DIRECT_BODY_BYTES,
    MAX_DIRECT_TIMEOUT_S, MAX_SLICES, validate_direct_invocation,
};
use harness_protocol::goal::{GoalPhase, GoalStatus};
use harness_protocol::runtime::{InvocationAccepted, PackagesToken, RepoSource};
use harness_protocol::store::{CredsBinding, ResumePath, TurnItem};
use serde::Serialize;
use serde_json::Value;
use std::future::Future;
use worker_api::creds::new_nonce;
use worker_api::handler::{Minter, TokenContext};

/// What the op answers (the machine ignores it: the task's output is worker-api's `SendTaskSuccess`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartAck {
    pub runtime_session_id: String,
    pub qualifier: String,
    /// This attempt found the turn already invoked and invoked nothing.
    pub already_invoked: bool,
}

/// The seconds the session may run: the microVM's lifetime less the 900 s reserve for the end sequence, at most the
/// protocol maximum.
pub fn session_timeout_s(max_lifetime_s: u64) -> u32 {
    let budget = max_lifetime_s
        .saturating_sub(900)
        .clamp(1, u64::from(MAX_DIRECT_TIMEOUT_S));
    u32::try_from(budget).unwrap_or(MAX_DIRECT_TIMEOUT_S)
}

/// The agent word of the encryption context the nonce is sealed under. IAM constrains the context keys, not their
/// values, so the nonce gets a context of its own and a ciphertext of the task token cannot be passed off as it.
pub const NONCE_CONTEXT_AGENT: &str = "direct-nonce";

/// The encryption context of the nonce of slice `n`: turn `n`, so a ciphertext of one slice's nonce cannot be passed off as another's.
fn slice_nonce_context(run_id: &str, n: u32) -> TokenContext {
    TokenContext {
        run_id: run_id.to_string(),
        agent_id: NONCE_CONTEXT_AGENT.to_string(),
        turn: n,
    }
}

/// `runs/<run>/agents/direct/nonce.enc`: the session nonce, KMS-encrypted (base64), written once with `If-None-Match: *`.
pub fn nonce_key(run_id: &str) -> String {
    format!("runs/{run_id}/agents/{DIRECT_AGENT_ID}/nonce.enc")
}

/// The run's session nonce: read back when an earlier attempt stored it, else made and stored (create-only; an attempt
/// that loses the write uses the winner's).
async fn session_nonce<S, T, M, R, B, P>(deps: &Deps<S, T, M, R, B, P>, run_id: &str) -> Res<String>
where
    T: Tokens,
    B: Blobs,
{
    let ctx = TokenContext {
        run_id: run_id.to_string(),
        agent_id: NONCE_CONTEXT_AGENT.to_string(),
        turn: DIRECT_TURN,
    };
    let key = nonce_key(run_id);
    let read = || async {
        match deps.blobs.get(&key).await? {
            Some(ct) => {
                let ct = String::from_utf8(ct).map_err(|_| {
                    RunSessionError::Invalid(format!("{key} is not a base64 ciphertext"))
                })?;
                deps.tokens.decrypt(&ct, &ctx).await.map(Some)
            }
            None => Ok(None),
        }
    };
    if let Some(nonce) = read().await? {
        return Ok(nonce);
    }
    let nonce = new_nonce();
    let ciphertext = deps.tokens.encrypt(&nonce, &ctx).await?;
    if deps
        .blobs
        .put_if_absent(&key, ciphertext.as_bytes())
        .await?
    {
        return Ok(nonce);
    }
    read().await?.ok_or_else(|| {
        RunSessionError::Unavailable(format!("{key} was written and is not readable"))
    })
}

/// Runs `op` once more when it failed: the steps after an accepted invocation are cheap, safe to repeat, and the
/// Step Functions retry that follows a failure here costs a 409 round trip and a new task token.
async fn twice<T, F, Fut>(op: F) -> Res<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Res<T>>,
{
    match op().await {
        Ok(v) => Ok(v),
        Err(_) => op().await,
    }
}

/// Ends the session this attempt started when the run is no longer active, and says so. A discard clears the run, aborts the execution and starts
/// the reaper, which ends the session it finds. A reaper that looks before the Agent item is written finds nothing and finishes, and one that
/// stops a session before the invoke has created it stops nothing; the aborted execution then runs no later state, so nobody would end a machine
/// this attempt starts afterwards. The op therefore reads the run again after it wrote the Agent item and before it invokes, and once more when the
/// invoke has returned (a first call to an Instances machine takes minutes), and ends what it started with `end_session`, the function the reaper
/// calls. A failure of that end (tried twice) is the op's failure; when the reaper has not run yet it repeats the end until it works, and when it has
/// run, only the session's own limits end the machine.
async fn end_if_discarded<S, T, M, R, B, P>(deps: &Deps<S, T, M, R, B, P>, run_id: &str) -> Res<()>
where
    S: Store,
    R: Sessions,
{
    let active = deps
        .store
        .get_run(run_id)
        .await?
        .is_some_and(|r| r.status == GoalStatus::Active);
    if active {
        return Ok(());
    }
    // The end is tried twice at once: the reaper of the discard may have finished already, and an aborted execution retries nothing.
    twice(|| end_session(deps, run_id)).await?;
    Err(RunSessionError::Invalid(format!(
        "run {run_id} is no longer active; the session this attempt started was ended"
    )))
}

fn engine_version() -> Res<String> {
    bundled_profile(Engine::CLINE)
        .ok_or_else(|| {
            RunSessionError::Invalid(
                "no engine profile engine/profiles/cline.toml in this build".into(),
            )
        })?
        .map(|p| p.version)
        .map_err(|e| RunSessionError::Invalid(format!("engine profile cline: {e}")))
}

/// The first 12 characters of a commit id, for a message.
fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

fn target_mode(run: &harness_protocol::store::RunItem) -> String {
    serde_json::to_value(&run.target)
        .ok()
        .and_then(|v| v.get("mode").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "repo".into())
}

pub async fn start_session<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run_id: &str,
    task_token: &str,
    staged: &Value,
    slice: u32,
) -> Res<StartAck>
where
    S: Store,
    T: Tokens,
    M: Minter,
    R: Sessions,
    B: Blobs,
    P: PackageTokens,
{
    let invalid = RunSessionError::Invalid;
    let cfg = &deps.cfg;
    role_session_name(run_id, DIRECT_AGENT_ID).map_err(|e| invalid(e.to_string()))?;
    // Slice n is turn n.
    let n = slice;
    if !(1..=MAX_SLICES).contains(&n) {
        return Err(invalid(format!("slice {n} is not 1 to {MAX_SLICES}")));
    }
    // 1. the run, its request and its model
    let run = deps
        .store
        .get_run(run_id)
        .await?
        .ok_or_else(|| invalid(format!("run {run_id} has no Run item")))?;
    if run.status != GoalStatus::Active {
        return Err(invalid(format!("run {run_id} is no longer active")));
    }
    let (request, request_sha256) = read_request(&deps.store, &deps.blobs, run_id).await?;
    let model = model_spec(&request.model)?;
    let base_sha = staged
        .get("baseSha")
        .and_then(Value::as_str)
        .map(str::to_string);
    // A follow-up runs on the head the earlier run delivered. A repo-gateway image that predates `fromRun` ignores the member and
    // stages the empty root commit, and the run would start on an empty tree and be delivered with no error. The start is refused instead.
    if let Some(c) = &request.continue_from {
        let want = c.head_sha.as_deref().unwrap_or_default();
        if want.is_empty() || base_sha.as_deref() != Some(want) {
            return Err(invalid(format!(
                "continueFrom {}: the staged base {} is not the head the earlier run delivered ({}); the repo-gateway did not stage the earlier run's code",
                c.run_id,
                short(base_sha.as_deref().unwrap_or("none")),
                short(want)
            )));
        }
    }

    // 2. the machine, its runtime and the endpoint the run is pinned to. The machine is chosen once: a run whose Agent item
    // exists (an earlier attempt got that far) keeps what the item records, and a fresh run takes the user's explicit word, else the stage's
    // policy, so no deploy between two attempts moves a run. The worker runtime's ARN is the config's when the deployment fixes one, else it is
    // read now (the function does not read it at start): a stage whose runtimes stack has not written it ends this run with the reason.
    let prior = deps.store.get_agent(run_id, DIRECT_AGENT_ID).await?;
    let compute = match prior.as_ref() {
        Some(a) => compute::recorded(a).ok_or_else(|| {
            invalid(format!(
                "the Agent item of run {run_id} records a machine kind this build does not know"
            ))
        })?,
        None => compute::choose_compute(request.compute, cfg.compute.policy),
    };
    let mut provider: Option<String> = None;
    let arn: String = match compute {
        Compute::Microvm if !cfg.runtime_arn.is_empty() => cfg.runtime_arn.clone(),
        Compute::Microvm => deps
            .sessions
            .runtime_arn()
            .await?
            .ok_or_else(|| invalid("no worker runtime is deployed".into()))?,
        Compute::Instances => {
            let arn = deps
                .sessions
                .instances_runtime_arn()
                .await?
                .ok_or_else(|| invalid("the Instances worker runtime is not deployed".into()))?;
            let id = deps
                .sessions
                .capacity_provider_id()
                .await?
                .ok_or_else(|| {
                    invalid("the stage names no capacity provider for its Instances runtime (CAPACITY_PROVIDER_ID_PARAMETER)".into())
                })?;
            // The stage may cap its Instances sessions. The claim comes before any write of this run's items, and the same run claims once.
            if let Some(max) = cfg.compute.max_active
                && !deps.store.claim_instances_slot(run_id, max).await?
            {
                return Err(invalid(format!(
                    "the stage is running the most Instances sessions it allows ({max}); try again when one ends, or start the run with --compute microvm"
                )));
            }
            provider = Some(id);
            arn
        }
    };
    let arn = arn.as_str();
    let pin = match run.runtime_endpoint.as_deref() {
        Some(e) => Pin {
            version: run.runtime_version.clone().unwrap_or_default(),
            endpoint: e.to_string(),
        },
        None => match (deps.sessions.endpoint(arn, LIVE).await?, compute) {
            (Some(live), _) => {
                let versioned = match live.live_version.as_deref().and_then(version_endpoint) {
                    Some(name) => deps.sessions.endpoint(arn, &name).await?,
                    None => None,
                };
                let choice =
                    choose_pin(&live, versioned.as_ref()).map_err(RunSessionError::Unavailable)?;
                deps.store.pin_runtime(run_id, &choice.pin).await?
            }
            // An Instances runtime need not have endpoints; AgentCore's default endpoint always exists.
            (None, Compute::Instances) => {
                let pin = Pin {
                    version: String::new(),
                    endpoint: compute::QUALIFIER_DEFAULT.to_string(),
                };
                deps.store.pin_runtime(run_id, &pin).await?
            }
            (None, Compute::Microvm) => {
                return Err(RunSessionError::Unavailable(format!(
                    "the worker runtime has no {LIVE} endpoint"
                )));
            }
        },
    };
    let qualifier = pin.endpoint.clone();
    let session = runtime_session_id(run_id, DIRECT_AGENT_ID, n);
    let plan = if n == 1 {
        slices::plan_first(deps, run_id, request.max_run_s, staged).await?
    } else {
        slices::read_plan(&deps.blobs, run_id, n)
            .await?
            .ok_or_else(|| invalid(format!("run {run_id} has no plan for slice {n}")))?
    };

    // 3. the Turn item, with the newest token
    let ctx = TokenContext {
        run_id: run_id.to_string(),
        agent_id: DIRECT_AGENT_ID.to_string(),
        turn: n,
    };
    let ciphertext = deps.tokens.encrypt(task_token, &ctx).await?;
    let now_ms = cfg.now_ms();
    let existing = deps.store.get_turn(run_id, DIRECT_AGENT_ID, n).await?;
    let created = if existing.is_none() {
        let turn = TurnItem {
            run_id: run_id.to_string(),
            agent_id: DIRECT_AGENT_ID.to_string(),
            n,
            task_token_ciphertext: ciphertext.clone(),
            started_at: rfc3339(now_ms / 1000),
            invoked_at: None,
            ended_at: None,
            last_commit_sha: None,
            bundle_uri: None,
            snapshot_uri: None,
            gate_result_uri: None,
            turn_class: None,
            fingerprint: None,
            usage: None,
            rules_digest: String::new(),
            role: Some(DIRECT_AGENT_ID.to_string()),
            rules_manifest: None,
            session_generation: Some(n),
            runtime_session_id: Some(session.clone()),
            runtime_version: Some(pin.version.clone()),
            runtime_endpoint: Some(qualifier.clone()),
            base_sha: base_sha.clone(),
            resume: Some(if n == 1 {
                ResumePath::Fresh
            } else {
                ResumePath::Slow
            }),
            resume_reason: (n > 1)
                .then(|| format!("slice {n} restores the work of an earlier slice")),
            resume_head: plan.restore.head.clone(),
            engine: Some(Engine::CLINE),
            model_id: Some(model.id.clone()),
            lost_at: None,
            version: 1,
            attempt: None,
            ladder_version: None,
            max_silent_gap_s: None,
            max_output_tokens: Some(model.max_output_tokens),
            context_tokens: Some(model.context_tokens),
            region: None,
            route_level: None,
            transport: Some(Transport::Acp),
            engine_policy_version: None,
            started_at_ms: Some(now_ms),
        };
        deps.store.create_turn(&turn).await?
    } else {
        false
    };
    if !created {
        // A retry of the state (or a concurrent attempt that wrote the item first): the newest token completes the task.
        deps.store
            .replace_turn_token(run_id, DIRECT_AGENT_ID, n, &ciphertext)
            .await?;
    }
    let turn = deps
        .store
        .get_turn(run_id, DIRECT_AGENT_ID, n)
        .await?
        .ok_or_else(|| {
            RunSessionError::Unavailable(format!("the Turn item of {run_id} is not readable"))
        })?;
    let started_ms = turn.started_at_ms.unwrap_or(now_ms);

    // 4. a turn that was invoked is not invoked again
    if turn.invoked_at.is_some() {
        if n == 1 {
            announce(deps, run_id, started_ms, &model.id, model.context_tokens).await?;
        }
        return Ok(StartAck {
            runtime_session_id: session,
            qualifier,
            already_invoked: true,
        });
    }

    // 5. the Agent item with the session's nonce (its hash only) and the credentials minted from it.
    // nonce; a later slice moves the item to the new session: a new session id, a new nonce and a new generation in one conditional write,
    // so the session of the slice before can neither refresh its credentials nor pass worker-api's generation check from then on.
    // The credentials must cover every route agentd may switch to, not only the run's own model: a switch to a route the policy does
    // not name would be refused by IAM.
    let failover = failover_specs(&request.model);
    let routes: Vec<(&str, &str)> = std::iter::once((model.id.as_str(), model.region.as_str()))
        .chain(failover.iter().map(|r| (r.id.as_str(), r.region.as_str())))
        .collect();
    let (agent, nonce) = if n == 1 {
        let nonce = session_nonce(deps, run_id).await?;
        let session_policy =
            session_policy_for_routes(&routes).map_err(|e| invalid(e.to_string()))?;
        let d = Dispatch {
            run_id: run_id.to_string(),
            agent_id: DIRECT_AGENT_ID.to_string(),
            task_id: DIRECT_TASK_ID.to_string(),
            engine: Engine::CLINE,
            engine_version: engine_version()?,
            model_id: model.id.clone(),
            runtime_arn: arn.to_string(),
            runtime_session_id: session.clone(),
            tenant: cfg.tenant.clone(),
            mode: target_mode(&run),
        };
        let mut agent = agent_item(&d, &nonce, &session_policy);
        agent.pool = "direct".into();
        agent.generation = 1;
        agent.transport = Transport::Acp;
        agent.session_generation = Some(1);
        agent.generation_started_at = Some(rfc3339(now_ms / 1000));
        agent.runtime_version = Some(pin.version.clone());
        agent.runtime_endpoint = Some(qualifier.clone());
        agent.engine_policy_version = None;
        // The machine this run lives on, recorded before the first invoke: a retry reads it back (step 2) and never decides again. The capacity
        // provider is what `DeleteCapacityProviderSession` needs at the end of the run.
        agent.compute = Some(compute.word().to_string());
        agent.capacity_provider_id = provider;
        mint_plan(&agent, &nonce).map_err(|e| invalid(e.message().to_string()))?;
        deps.store.put_agent(&agent).await?;
        (agent, nonce)
    } else {
        move_agent_to_slice(deps, run_id, n, &session, now_ms, prior).await?
    };
    let mint = mint_plan(&agent, &nonce).map_err(|e| invalid(e.message().to_string()))?;
    let credentials = deps
        .minter
        .mint(&mint)
        .await
        .map_err(|e| RunSessionError::Unavailable(e.to_string()))?;

    // 6. the invocation. The slice's prompt deadline is the plan's: the microVM's lifetime less the reserve, less what the run's total leaves.
    let timeout_s = plan.engine_s;
    let bucket = cfg.bucket.as_str();
    // A run of one session that is already on its last slice carries no slice member.
    let slice_member = (n > 1 || !plan.last).then(|| DirectSlice {
        last: plan.last,
        base_sha: (n > 1).then(|| plan.base_sha.clone()).flatten(),
        restore_head: (n > 1).then(|| plan.restore.head.clone()).flatten(),
        sessions_before: (n > 1).then_some(plan.sessions_before),
        brief: (n > 1).then(|| plan.brief.clone()).flatten(),
    });
    let mut req = DirectInvocationRequest {
        kind: DirectKind::Direct,
        run_id: run_id.to_string(),
        agent_id: DIRECT_AGENT_ID.to_string(),
        turn: n,
        runtime_session_id: session.clone(),
        credentials,
        nonce,
        internal_api_url: cfg.internal_api_url.clone(),
        region: cfg.region.clone(),
        engine: Engine::CLINE.as_str().to_string(),
        model: model.clone(),
        failover,
        request: InputRef {
            uri: harness_protocol::direct::s3::uri(
                bucket,
                &harness_protocol::direct::s3::request_key(run_id),
            ),
            sha256: request_sha256,
        },
        repo: RepoSource::S3Bundle {
            uri: harness_protocol::direct::s3::uri(bucket, &plan.restore.key),
        },
        timeout_s,
        provider_silence_s: None,
        snapshot_every_s: Some(DEFAULT_SNAPSHOT_EVERY_S),
        packages_token: None,
        packages_token_expires_at: None,
        // The answer window the client asked for. None, or 0 in a request stored by another version, leaves the member out of the
        // body, so an agentd image that predates it parses what it always parsed; a value over the protocol maximum fails the
        // validation below.
        input_wait_s: request.input_wait_s.filter(|s| *s != 0),
        slice: slice_member,
    };
    // The package token is minted with this Lambda's own role, for the whole session plus agentd's margin. A failed
    // mint is logged and the run goes on without one: installs fail where Cline reports them.
    let token_s = agentd::packages::token_duration_s(timeout_s);
    let asked_at = cfg.now_ms() / 1000;
    match deps.packages.token(token_s).await {
        Ok(Some(token)) => {
            req.packages_token = Some(PackagesToken::new(token));
            req.packages_token_expires_at = Some(asked_at + u64::from(token_s));
        }
        Ok(None) => {}
        Err(e) => println!(
            "{}",
            serde_json::json!({ "warning": "package-token", "runId": run_id, "reason": e })
        ),
    }
    validate_direct_invocation(&req).map_err(|e| invalid(format!("invocation: {e}")))?;
    let body = serde_json::to_vec(&req).map_err(|e| invalid(e.to_string()))?;
    if body.len() > MAX_DIRECT_BODY_BYTES {
        return Err(invalid(format!(
            "the invocation is {} bytes; the limit is {MAX_DIRECT_BODY_BYTES}",
            body.len()
        )));
    }
    // The Run item names the slice the run is in; the number only goes up.
    deps.store.set_run_slice(run_id, n).await?;
    // A discard may have landed since step 1: the Agent item exists now, so the reaper of a discard that comes from here on finds it.
    end_if_discarded(deps, run_id).await?;
    let invoked: Res<bool> = match deps.sessions.invoke(arn, &qualifier, &session, &body).await {
        Ok(ack_body) => serde_json::from_slice::<InvocationAccepted>(&ack_body)
            .map_err(|_| {
                RunSessionError::Unavailable(
                    "the session answered with no InvocationAccepted".into(),
                )
            })
            .and_then(|ack| {
                if !ack.accepted || ack.turn != n {
                    return Err(RunSessionError::Unavailable(format!(
                        "the session did not accept turn {n} (accepted={}, turn={})",
                        ack.accepted, ack.turn
                    )));
                }
                Ok(false)
            }),
        // agentd runs one turn per session and the session id is this run's: its 409 means an earlier attempt's
        // invocation was accepted and its acknowledgement was lost (review S1). The nonce it holds is the one stored.
        Err(RunSessionError::Conflict(reason)) => {
            println!(
                "{}",
                serde_json::json!({ "warning": "invoke-conflict", "runId": run_id, "reason": reason })
            );
            Ok(true)
        }
        Err(e) => Err(e),
    };
    // The invoke may have taken minutes, and a reaper that ran meanwhile stopped a session that did not exist yet. Whatever the call answered,
    // an error included (a call can time out after the machine was created), the run is read again before the answer is used.
    end_if_discarded(deps, run_id).await?;
    let already_running = invoked?;

    // 7. recorded: invoked, working, announced
    twice(|| deps.store.mark_turn_invoked(run_id, DIRECT_AGENT_ID, n)).await?;
    if n == 1 {
        twice(|| announce(deps, run_id, started_ms, &model.id, model.context_tokens)).await?;
    } else {
        // A renewed session announces itself through agentd's own new-session event; the run is working again.
        twice(|| deps.store.set_run_phase(run_id, GoalPhase::Green)).await?;
    }
    Ok(StartAck {
        runtime_session_id: session,
        qualifier,
        already_invoked: already_running,
    })
}

/// Moves the Agent item to slice `n` and returns it with the nonce the new session holds. The item still names the slice before; one
/// conditional write replaces it with the new session id, generation and refresh binding, and keeps the nonce's ciphertext on the item so that a
/// retry of the start (the invocation was accepted, the acknowledgement lost) finds the nonce agentd holds. An item already at slice `n` was moved by
/// an earlier attempt of this start: its nonce is read back, nothing is written. Any other state, or a write that lost to worker-api, is a conflict
/// the machine retries.
async fn move_agent_to_slice<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run_id: &str,
    n: u32,
    session: &str,
    now_ms: u64,
    prior: Option<harness_protocol::store::AgentItem>,
) -> Res<(harness_protocol::store::AgentItem, String)>
where
    S: Store,
    T: Tokens,
{
    let invalid = RunSessionError::Invalid;
    let ctx = slice_nonce_context(run_id, n);
    let prior = prior.ok_or_else(|| {
        invalid(format!(
            "run {run_id} has no Agent item to move to slice {n}"
        ))
    })?;
    match prior.session_generation {
        Some(g) if g == n => {
            let ct = prior.nonce_ciphertext.as_deref().ok_or_else(|| {
                invalid(format!(
                    "the Agent item of slice {n} holds no nonce ciphertext"
                ))
            })?;
            let nonce = deps.tokens.decrypt(ct, &ctx).await?;
            Ok((prior, nonce))
        }
        Some(g) if g + 1 == n => {
            let nonce = new_nonce();
            let ct = deps.tokens.encrypt(&nonce, &ctx).await?;
            let mut next = prior.clone();
            next.runtime_session_id = session.to_string();
            next.generation = n;
            next.session_generation = Some(n);
            next.generation_started_at = Some(rfc3339(now_ms / 1000));
            next.status = harness_protocol::store::AgentStatus::Running;
            next.heartbeat_at = None;
            let binding = next.creds.as_mut().ok_or_else(|| {
                invalid(format!(
                    "the Agent item of run {run_id} has no credential binding"
                ))
            })?;
            // The completion key is made from the first nonce of the session that holds it, and agentd of this slice makes it from the nonce it is
            // started with. The item names the hash of that key, so worker-api accepts this session's events and turn-complete and refuses the earlier one's.
            // An item that names no key (written before the key existed) keeps naming none.
            let completion_key_sha256 = binding
                .completion_key_sha256
                .as_ref()
                .map(|_| CredsBinding::hash_completion_key(&CredsBinding::completion_key(&nonce)));
            *binding = CredsBinding {
                nonce_sha256: CredsBinding::hash_nonce(&nonce),
                revoked: false,
                completion_key_sha256,
                ..binding.clone()
            };
            next.nonce_ciphertext = Some(ct);
            next.version = prior.version + 1;
            if !deps.store.rotate_agent(&next, g, prior.version).await? {
                return Err(RunSessionError::Conflict(format!(
                    "the Agent item of run {run_id} changed while slice {n} was starting"
                )));
            }
            Ok((next, nonce))
        }
        other => Err(invalid(format!(
            "the Agent item of run {run_id} is at slice {other:?}; slice {n} cannot start from it"
        ))),
    }
}

/// Rows 2 and 3 of the event table, and the Run item's phase: Cline is working.
async fn announce<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run_id: &str,
    started_ms: u64,
    model_id: &str,
    context_tokens: u32,
) -> Res<()>
where
    S: Store,
{
    deps.store.set_run_phase(run_id, GoalPhase::Green).await?;
    deps.store
        .put_event(&events::goal_active(run_id, started_ms))
        .await?;
    deps.store
        .put_event(&events::session_started(
            run_id,
            started_ms,
            model_id,
            context_tokens,
        ))
        .await
}
