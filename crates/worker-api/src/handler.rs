//! `/internal/*` routing independent of the Lambda event types, so tests drive it directly.
//! Store and STS access are traits; `crate::aws` implements them for DynamoDB and STS.

use crate::caller::{Caller, parse_caller};
use crate::creds::{Denial, MintPlan, decide, new_nonce};
use harness_protocol::api::ApiError;
use harness_protocol::api::internal::{
    CredsRequest, CredsResponse, EventsBatchRequest, EventsBatchResponse, HeartbeatRequest,
    HeartbeatResponse, MAX_TURN_SUMMARY_BYTES, PendingControl, ProcessCredentials,
    TurnCompleteRequest, TurnCompleteResponse, validate_events_batch,
};
use harness_protocol::common::Usage;
use harness_protocol::direct::{
    ComputeMode, DIRECT_AGENT_ID, SESSION_END_REQUESTED, SessionEnded, message_preview,
};
use harness_protocol::envelope::Envelope;
use harness_protocol::goal::GoalStatus;
use harness_protocol::messages::{
    ClaimRequest, ClaimResponse, HistoryRequest, HistoryResponse, MessageNotice, OutcomeRequest,
    OutcomeResponse, SealRequest, SealResponse, SessionAck, SessionReport,
};
use harness_protocol::store::{AgentItem, CompletionKeyCheck, CredsBinding, TurnItem};
use harness_protocol::transcript::TranscriptSummary;
use message_ledger::LedgerError;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::future::Future;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// A conditional write lost to a concurrent writer.
    #[error("conditional write conflict")]
    Conflict,
    #[error("store unavailable: {0}")]
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("mint failed: {0}")]
pub struct MintError(pub String);

pub trait Store: Send + Sync {
    fn get_agent(
        &self,
        run: &str,
        agent: &str,
    ) -> impl Future<Output = Result<Option<AgentItem>, StoreError>> + Send;
    /// Status of the run's root goal.
    fn get_goal_status(
        &self,
        run: &str,
    ) -> impl Future<Output = Result<Option<GoalStatus>, StoreError>> + Send;
    /// Replaces the binding's nonce hash only if it still equals `old_sha256`.
    fn rotate_nonce(
        &self,
        run: &str,
        agent: &str,
        old_sha256: &str,
        new_sha256: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    fn put_events(
        &self,
        run: &str,
        agent: &str,
        events: &[Envelope],
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    fn record_heartbeat(
        &self,
        run: &str,
        agent: &str,
        turn: u32,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    fn record_turn_complete(
        &self,
        run: &str,
        agent: &str,
        turn: u32,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// Sets the engine-reported `usage` on `TURN#<agent>#<n>`. A turn
    /// without an item (a dispatch outside Step Functions) is left alone.
    fn record_turn_usage(
        &self,
        run: &str,
        agent: &str,
        turn: u32,
        usage: &Usage,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// Sets the transcript writer's summary on `TURN#<agent>#<n>`.
    /// A turn without an item is left alone.
    fn record_turn_transcript(
        &self,
        run: &str,
        agent: &str,
        turn: u32,
        summary: &TranscriptSummary,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// `TURN#<agent>#<n>`, written by the dispatcher with the encrypted task token.
    fn get_turn(
        &self,
        run: &str,
        agent: &str,
        n: u32,
    ) -> impl Future<Output = Result<Option<TurnItem>, StoreError>> + Send;
    /// Sets `endedAt` unless it is set; an already-ended turn is not an error.
    fn mark_turn_ended(
        &self,
        run: &str,
        agent: &str,
        n: u32,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// Fences session generation `generation` of the agent, so its worker can no longer act on the run: one write sets
    /// `fencedGeneration`, revokes the credential refresh and raises the version, and holds only while the item is still at that
    /// generation. `true`: the generation is fenced (also when it was fenced before). `false`: the item is at another generation, has no
    /// credential binding or does not exist. A store that cannot fence says so, and the worker's notice is then answered as unavailable.
    fn fence_agent(
        &self,
        _run: &str,
        _agent: &str,
        _generation: u32,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send {
        async {
            Err(StoreError::Unavailable(
                "this store cannot fence a session".into(),
            ))
        }
    }
    /// The agent's undelivered controls, oldest first. Each is marked delivered by a conditional
    /// write, so concurrent calls never hand out the same control twice.
    fn take_controls(
        &self,
        run: &str,
        agent: &str,
    ) -> impl Future<Output = Result<Vec<PendingControl>, StoreError>> + Send;
    /// The messages of the user that are pending in the run's current session: the notices agentd is told
    /// about in every events response. One read; it changes nothing. A store without a ledger has none.
    fn message_notices(
        &self,
        _run: &str,
    ) -> impl Future<Output = Result<Vec<MessageNotice>, LedgerError>> + Send {
        async { Ok(Vec::new()) }
    }
    /// Whether a run-level Cancel waits for the agent (undelivered): it wins over a message.
    fn cancel_queued(
        &self,
        _run: &str,
        _agent: &str,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send {
        async { Ok(false) }
    }
    /// Whether the user's keep-the-work cancel is recorded on the Run item (`cancelRequestedAt`): one consistent read of that one
    /// attribute. While it is, every events answer to the run's direct agent carries a Cancel and no message is handed out, so a
    /// control that a dying session took, or whose answer was lost, cannot lose the cancel. A store that holds no Run items has none.
    fn cancel_requested(
        &self,
        _run: &str,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send {
        async { Ok(false) }
    }
    /// agentd's incarnation and session (`POST /internal/message-session`).
    fn message_session(
        &self,
        _run: &str,
        _report: &SessionReport,
    ) -> impl Future<Output = Result<SessionAck, LedgerError>> + Send {
        async { Err(no_ledger()) }
    }
    /// `POST /internal/message-claim`; `cancel_queued` is the answer of [`Store::cancel_queued`].
    fn message_claim(
        &self,
        _req: &ClaimRequest,
        _cancel_queued: bool,
    ) -> impl Future<Output = Result<ClaimResponse, LedgerError>> + Send {
        async { Err(no_ledger()) }
    }
    /// `POST /internal/message-seal`.
    fn message_seal(
        &self,
        _req: &SealRequest,
    ) -> impl Future<Output = Result<SealResponse, LedgerError>> + Send {
        async { Err(no_ledger()) }
    }
    /// `POST /internal/message-outcome`.
    fn message_outcome(
        &self,
        _req: &OutcomeRequest,
    ) -> impl Future<Output = Result<OutcomeResponse, LedgerError>> + Send {
        async { Err(no_ledger()) }
    }
    /// `POST /internal/message-history`: the messages of `run` that were delivered to an engine, as the ledger holds them. A read;
    /// it changes nothing. A store without a ledger cannot answer, which is an error and never an empty list.
    fn message_history(
        &self,
        _run: &str,
    ) -> impl Future<Output = Result<HistoryResponse, LedgerError>> + Send {
        async { Err(no_ledger()) }
    }
}

fn no_ledger() -> LedgerError {
    LedgerError::Unavailable("this store keeps no messages".into())
}

/// The run, agent and turn a task token is bound to: its KMS encryption context, so a caller can
/// only complete its own open turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenContext {
    pub run_id: String,
    pub agent_id: String,
    pub turn: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CallbackError {
    /// The task is gone (timed out, closed, or the token does not decrypt under this context).
    #[error("task closed: {0}")]
    Closed(String),
    #[error("callback unavailable: {0}")]
    Unavailable(String),
}

/// Step Functions callbacks for a dispatched turn: the token is decrypted here and
/// never leaves worker-api.
pub trait Callbacks: Send + Sync {
    fn turn_done(
        &self,
        token_ciphertext: &str,
        ctx: &TokenContext,
        output: &str,
    ) -> impl Future<Output = Result<(), CallbackError>> + Send;
    fn turn_alive(
        &self,
        token_ciphertext: &str,
        ctx: &TokenContext,
    ) -> impl Future<Output = Result<(), CallbackError>> + Send;
}

pub trait Minter: Send + Sync {
    fn mint(
        &self,
        plan: &MintPlan,
    ) -> impl Future<Output = Result<ProcessCredentials, MintError>> + Send;
}

#[derive(Debug)]
pub struct Deps<S, M, C> {
    pub store: S,
    pub minter: M,
    pub callbacks: C,
    /// Name of the `harness-worker-session` role every caller must be a session of.
    pub role_name: String,
    /// The harness bucket, where agentd puts every turn bundle (`HARNESS_BUCKET`): a turn-complete may name only the
    /// caller's own turn bundle in it ([`own_turn_output`]).
    pub bucket: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InternalRequest {
    pub method: String,
    pub path: String,
    /// `requestContext.identity.userArn` from API Gateway IAM auth.
    pub caller_arn: Option<String>,
    /// The header `x-hamh-completion-key` (`harness_protocol::store::COMPLETION_KEY_HEADER`): the secret only agentd holds. `/internal/events`
    /// and `/internal/turn-complete` require it when the run's Agent item names one; no other route reads it.
    pub completion_key: Option<String>,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InternalResponse {
    pub status: u16,
    pub body: String,
}

fn json<T: Serialize>(status: u16, v: &T) -> InternalResponse {
    InternalResponse {
        status,
        body: serde_json::to_string(v).unwrap_or_else(|_| "{}".into()),
    }
}

fn error(status: u16, code: &str, message: &str) -> InternalResponse {
    json(
        status,
        &ApiError {
            code: code.into(),
            message: message.into(),
            request_id: None,
        },
    )
}

fn deny(d: Denial) -> InternalResponse {
    error(403, d.code(), d.message())
}

fn parse<T: DeserializeOwned>(body: &str) -> Result<T, InternalResponse> {
    serde_json::from_str(body).map_err(|e| error(400, "bad_request", &format!("invalid body: {e}")))
}

fn store_failure(e: &StoreError) -> InternalResponse {
    match e {
        StoreError::Conflict => error(
            409,
            "conflict",
            "a concurrent refresh won; retry with the new nonce",
        ),
        StoreError::Unavailable(_) => {
            error(503, "store_unavailable", "the harness store is unavailable")
        }
    }
}

/// The route name after `/internal/`, accepting a stage prefix such as `/live/internal/creds`.
fn route(path: &str) -> Option<&str> {
    let i = path.find("/internal/")?;
    let op = &path[i + "/internal/".len()..];
    (!op.is_empty() && !op.contains('/')).then_some(op)
}

pub async fn handle<S: Store, M: Minter, C: Callbacks>(
    req: InternalRequest,
    deps: &Deps<S, M, C>,
) -> InternalResponse {
    handle_logged(req, deps).await.0
}

/// [`handle`] plus the diagnostic lines the Lambda writes to its log (never a body, nonce or
/// credential).
pub async fn handle_logged<S: Store, M: Minter, C: Callbacks>(
    req: InternalRequest,
    deps: &Deps<S, M, C>,
) -> (InternalResponse, Vec<String>) {
    let mut log = Vec::new();
    let resp = route_request(req, deps, &mut log).await;
    (resp, log)
}

async fn route_request<S: Store, M: Minter, C: Callbacks>(
    req: InternalRequest,
    deps: &Deps<S, M, C>,
    log: &mut Vec<String>,
) -> InternalResponse {
    let Some(op) = route(&req.path) else {
        return error(404, "not_found", "no such internal route");
    };
    if !matches!(
        op,
        "creds"
            | "heartbeat"
            | "turn-complete"
            | "events"
            | "message-session"
            | "message-claim"
            | "message-seal"
            | "message-outcome"
            | "message-history"
    ) {
        return error(404, "not_found", "no such internal route");
    }
    if req.method != "POST" {
        return error(
            405,
            "method_not_allowed",
            "internal routes accept POST only",
        );
    }
    let caller = match req
        .caller_arn
        .as_deref()
        .map(|arn| parse_caller(arn, &deps.role_name))
    {
        Some(Ok(c)) => c,
        _ => {
            return error(
                403,
                "not_worker_session",
                "the caller is not a harness worker session",
            );
        }
    };
    // The caller's Agent item, read once for the route: the completion key, the fence and the generation are all on it. The run and the
    // agent come from the caller's IAM session, so a request cannot choose another run's item.
    let agent = match deps.store.get_agent(&caller.run_id, &caller.agent_id).await {
        Ok(a) => a,
        Err(e) => return store_failure(&e),
    };
    let binding = agent.as_ref().and_then(|a| a.creds.as_ref());
    let key = binding.map(|c| c.check_completion_key(req.completion_key.as_deref()));
    // The engine holds the same session credentials as agentd and could post these two itself; they need the key only agentd holds.
    // An Agent item that names no key (a run started before the key existed), and no Agent item at all, leave the route as it was: the
    // engine cannot create or change an Agent item.
    // A call that proved the key of a run that requires the version records of its end-of-run objects may carry them.
    let mut pins_proven = false;
    if matches!(op, "events" | "turn-complete") {
        match key {
            Some(CompletionKeyCheck::Refused) => return deny(Denial::CompletionKey),
            Some(CompletionKeyCheck::Valid) => {
                pins_proven = binding.is_some_and(CredsBinding::output_pins_required);
            }
            Some(CompletionKeyCheck::Legacy) | None => {}
        }
    }
    // A fenced generation is a session the service has taken off the run (its worker gave notice that it is being ended, or it left
    // no result and is being stopped). Its machine can live on for about a minute. Nothing it sends may move the run: every route is
    // refused, except that its events are stored (they are the worker's last words) with no control and no message notice in the answer,
    // and that its heartbeat is looked at once more, because a repeated notice of the end may still have to end the wait.
    let is_fenced = caller.agent_id == DIRECT_AGENT_ID && agent.as_ref().is_some_and(fenced);
    if is_fenced && !matches!(op, "events" | "heartbeat") {
        return stale_generation();
    }
    match op {
        "creds" => creds(&caller, &req.body, deps, agent.as_ref()).await,
        "heartbeat" => {
            let key_valid = key == Some(CompletionKeyCheck::Valid);
            heartbeat(&caller, &req.body, deps, log, agent.as_ref(), key_valid).await
        }
        "turn-complete" => turn_complete(&caller, &req.body, deps, log, pins_proven).await,
        "message-session" => message_session(&caller, &req.body, deps).await,
        "message-claim" => message_claim(&caller, &req.body, deps, log).await,
        "message-seal" => message_seal(&caller, &req.body, deps).await,
        "message-outcome" => message_outcome(&caller, &req.body, deps).await,
        "message-history" => message_history(&caller, &req.body, deps).await,
        _ => events(&caller, &req.body, deps, log, is_fenced).await,
    }
}

/// Whether the generation the Agent item is at is fenced.
fn fenced(a: &AgentItem) -> bool {
    a.fenced_generation.is_some() && a.fenced_generation == a.session_generation
}

fn stale_generation() -> InternalResponse {
    error(
        409,
        "stale-generation",
        "this session is not the one the run is in now",
    )
}

async fn creds<S: Store, M: Minter, C: Callbacks>(
    caller: &Caller,
    body: &str,
    deps: &Deps<S, M, C>,
    agent: Option<&AgentItem>,
) -> InternalResponse {
    let req: CredsRequest = match parse(body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    if req.run_id != caller.run_id || req.agent_id != caller.agent_id {
        return deny(Denial::CallerMismatch);
    }
    let goal = match deps.store.get_goal_status(&caller.run_id).await {
        Ok(g) => g,
        Err(e) => return store_failure(&e),
    };
    let plan = match decide(caller, &req, agent, goal) {
        Ok(p) => p,
        Err(d) => return deny(d),
    };
    // Rotate first: a replay of this nonce must fail even if minting later fails.
    let next_nonce = new_nonce();
    let old = CredsBinding::hash_nonce(&req.nonce);
    if let Err(e) = deps
        .store
        .rotate_nonce(
            &caller.run_id,
            &caller.agent_id,
            &old,
            &CredsBinding::hash_nonce(&next_nonce),
        )
        .await
    {
        return store_failure(&e);
    }
    match deps.minter.mint(&plan).await {
        Ok(credentials) => json(
            200,
            &CredsResponse {
                credentials,
                next_nonce,
            },
        ),
        Err(_) => error(
            502,
            "mint_failed",
            "session credentials could not be minted",
        ),
    }
}

fn same_caller(caller: &Caller, run: &str, agent: &str) -> bool {
    caller.run_id == run && caller.agent_id == agent
}

/// Whether a turn-complete's output locations are the caller's own (sweep SW-67): no `snapshotUri` (no producer writes
/// one), and a `bundleUri`, when there is one, exactly the key agentd puts the turn's bundle at,
/// `s3://<harness bucket>/runs/<run>/agents/<agent>/turns/<turn>/bundle`, for the caller and this turn. The engine holds
/// the session's credentials and can post turn-complete itself: a bundle it named elsewhere in the bucket would become the
/// run's head and reach delivery, which pushes where the owner reads. The comparison is exact, so no encoding, `..`
/// segment, query, other bucket, other turn or other object under the prefix passes.
fn own_turn_output(req: &TurnCompleteRequest, caller: &Caller, bucket: &str) -> bool {
    req.snapshot_uri.is_none()
        && req.bundle_uri.as_deref().is_none_or(|b| {
            !bucket.is_empty()
                && b == format!(
                    "s3://{bucket}/runs/{}/agents/{}/turns/{}/bundle",
                    caller.run_id, caller.agent_id, req.turn
                )
        })
}

async fn heartbeat<S: Store, M: Minter, C: Callbacks>(
    caller: &Caller,
    body: &str,
    deps: &Deps<S, M, C>,
    log: &mut Vec<String>,
    agent: Option<&AgentItem>,
    key_valid: bool,
) -> InternalResponse {
    let req: HeartbeatRequest = match parse(body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    if !same_caller(caller, &req.run_id, &req.agent_id) {
        return deny(Denial::CallerMismatch);
    }
    let direct = req.agent_id == DIRECT_AGENT_ID;
    // The worker's notice that its session is being ended. It acts only with the run's completion key (an item that names no key
    // proves nothing) and only for a session whose storage is read back: the run did not switch the read off, and the session runs on
    // the microVM. A session on Instances compute has no storage to read, so nothing would wait for the stopped machine's last copy
    // after an early end of the wait; its notice is a plain heartbeat and the silence ends the wait as before.
    let notice = direct && req.ending == Some(true);
    let acts = notice
        && key_valid
        && agent.is_some_and(|a| {
            a.recover_after_stop != Some(false)
                && a.compute.as_deref() != Some(ComputeMode::Instances.word())
        });
    if notice && !acts {
        log.push(format!(
            "heartbeat turn {}: an ending notice without the run's completion key, or of a session whose storage is not read back, is a plain heartbeat",
            req.turn
        ));
    }
    // A call that names any other turn than the Agent item's own comes from a session the run has left.
    if direct
        && agent
            .and_then(|a| a.session_generation)
            .is_some_and(|current| current != req.turn)
    {
        return stale_generation();
    }
    if acts {
        return session_ending(&req, deps, log).await;
    }
    // A plain heartbeat of a fenced generation keeps nothing alive.
    if direct && agent.is_some_and(fenced) {
        return stale_generation();
    }
    if let Err(e) = deps
        .store
        .record_heartbeat(&req.run_id, &req.agent_id, req.turn)
        .await
    {
        return store_failure(&e);
    }
    // SendTaskHeartbeat for the open dispatched turn; a failure never fails the heartbeat.
    match open_token(deps, &req.run_id, &req.agent_id, req.turn).await {
        Ok(Some((ct, ctx))) => {
            if let Err(e) = deps.callbacks.turn_alive(&ct, &ctx).await {
                log.push(format!("heartbeat turn {}: {e}", req.turn));
            }
        }
        Ok(None) => {}
        Err(e) => log.push(format!("heartbeat turn {}: {e}", req.turn)),
    }
    json(200, &HeartbeatResponse { ok: true })
}

/// The worker of the session the run is in gave notice, with the run's completion key, that the session is being ended. The
/// generation is fenced, and the wait for the session ends with the output `{"sessionEnded": {...}}`. That output is no result: the
/// session counts as lost, and run-session reads its storage back before the next one starts. A turn that already ended (its
/// turn-complete came first) is left as it is. A callback that could not be made is answered 503 with the fence in place, and the
/// worker's repeat of the notice tries the callback again; without one the 600 seconds of silence end the wait as before.
async fn session_ending<S: Store, M: Minter, C: Callbacks>(
    req: &HeartbeatRequest,
    deps: &Deps<S, M, C>,
    log: &mut Vec<String>,
) -> InternalResponse {
    let (ct, ctx) = match open_token(deps, &req.run_id, &req.agent_id, req.turn).await {
        Ok(Some(t)) => t,
        Ok(None) => return json(200, &HeartbeatResponse { ok: true }),
        Err(e) => return store_failure(&e),
    };
    match deps
        .store
        .fence_agent(&req.run_id, &req.agent_id, req.turn)
        .await
    {
        Ok(true) => {}
        Ok(false) => return stale_generation(),
        Err(e) => return store_failure(&e),
    }
    let ended = SessionEnded {
        session: req.turn,
        cause: SESSION_END_REQUESTED.to_string(),
        at: crate::aws::now_rfc3339(),
    };
    let output = serde_json::json!({ "sessionEnded": ended }).to_string();
    match deps.callbacks.turn_done(&ct, &ctx, &output).await {
        Ok(()) => {}
        Err(e @ CallbackError::Closed(_)) => {
            log.push(format!("heartbeat turn {} (ending): {e}", req.turn));
        }
        Err(e) => {
            log.push(format!("heartbeat turn {} (ending): {e}", req.turn));
            return error(
                503,
                "callback_unavailable",
                "the end of the session could not be reported; retry",
            );
        }
    }
    if let Err(e) = deps
        .store
        .mark_turn_ended(&req.run_id, &req.agent_id, req.turn)
        .await
    {
        log.push(format!("heartbeat turn {} (ending): {e}", req.turn));
    }
    json(200, &HeartbeatResponse { ok: true })
}

/// A run is a list of slices (turn n is slice n), and `start-session` moves the Agent item to the new slice, with a new session id and
/// nonce, before it invokes the new session. A call that names any other turn than the Agent item's own comes from a session the run has left:
/// it is refused with 409 `stale-generation` before it records anything. An item with no session generation (written before slices) cannot tell
/// the turns apart and lets every call through, as it always did.
async fn stale_slice<S: Store, M: Minter, C: Callbacks>(
    deps: &Deps<S, M, C>,
    run: &str,
    agent: &str,
    turn: u32,
) -> Option<InternalResponse> {
    // Only the direct agent numbers its turns by slice; the session generation of any other agent counts microVM restarts, not turns.
    if agent != harness_protocol::direct::DIRECT_AGENT_ID {
        return None;
    }
    match deps.store.get_agent(run, agent).await {
        Ok(Some(a)) => match a.session_generation {
            Some(current) if current != turn => Some(error(
                409,
                "stale-generation",
                "this session is not the one the run is in now",
            )),
            _ => None,
        },
        Ok(None) => None,
        Err(e) => Some(store_failure(&e)),
    }
}

/// The encrypted token of the caller's turn `n` when Step Functions dispatched it and it has not
/// ended.
async fn open_token<S: Store, M: Minter, C: Callbacks>(
    deps: &Deps<S, M, C>,
    run: &str,
    agent: &str,
    n: u32,
) -> Result<Option<(String, TokenContext)>, StoreError> {
    Ok(deps
        .store
        .get_turn(run, agent, n)
        .await?
        .filter(|t| t.ended_at.is_none() && !t.task_token_ciphertext.is_empty())
        .map(|t| {
            (
                t.task_token_ciphertext,
                TokenContext {
                    run_id: run.to_string(),
                    agent_id: agent.to_string(),
                    turn: n,
                },
            )
        }))
}

async fn turn_complete<S: Store, M: Minter, C: Callbacks>(
    caller: &Caller,
    body: &str,
    deps: &Deps<S, M, C>,
    log: &mut Vec<String>,
    pins_proven: bool,
) -> InternalResponse {
    let mut req: TurnCompleteRequest = match parse(body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    if !same_caller(caller, &req.run_id, &req.agent_id) {
        return deny(Denial::CallerMismatch);
    }
    // The version records of the end-of-run objects are kept only from a call that proved the run's key, for a run that requires them,
    // and then only well formed. Anything else is dropped here, so the state of the run never holds a record the engine could have written.
    match req.output_pins.take() {
        Some(pins) if pins_proven => match pins.validate() {
            Ok(()) => req.output_pins = Some(pins),
            Err(why) => return error(400, "bad_output_pins", &why),
        },
        _ => {}
    }
    // The request goes whole into SendTaskSuccess and the state of the run holds 262,144 bytes in all: a state over that ends the run with an
    // error no `Catch` sees. Whatever agentd sent, the summary leaves here inside its byte budget (a summary within it is unchanged).
    req.summary = req
        .summary
        .take()
        .map(|s| message_preview(&s, MAX_TURN_SUMMARY_BYTES, ""));
    if let Some(stale) = stale_slice(deps, &req.run_id, &req.agent_id, req.turn).await {
        return stale;
    }
    if !own_turn_output(&req, caller, &deps.bucket) {
        return deny(Denial::ForeignBundle);
    }
    if let Err(e) = deps
        .store
        .record_turn_complete(&req.run_id, &req.agent_id, req.turn)
        .await
    {
        return store_failure(&e);
    }
    if let Some(usage) = &req.usage
        && let Err(e) = deps
            .store
            .record_turn_usage(&req.run_id, &req.agent_id, req.turn, usage)
            .await
    {
        return store_failure(&e);
    }
    // The stored conversation's summary is telemetry: a summary that is not well formed or a write that fails is logged and never
    // fails the turn.
    if let Some(summary) = &req.transcript {
        if !summary.validate() {
            log.push(format!(
                "turn-complete turn {}: the transcript summary is not well formed and was not stored",
                req.turn
            ));
        } else if let Err(e) = deps
            .store
            .record_turn_transcript(&req.run_id, &req.agent_id, req.turn, summary)
            .await
        {
            log.push(format!(
                "turn-complete turn {}: the transcript summary was not stored: {e}",
                req.turn
            ));
        }
    }
    let (ct, ctx) = match open_token(deps, &req.run_id, &req.agent_id, req.turn).await {
        Ok(Some(t)) => t,
        Ok(None) => return json(200, &TurnCompleteResponse { accepted: true }),
        Err(e) => return store_failure(&e),
    };
    // The output is the request itself: telemetry and the bundle location, never a verdict.
    let output = serde_json::to_string(&req).unwrap_or_else(|_| "{}".into());
    match deps.callbacks.turn_done(&ct, &ctx, &output).await {
        Ok(()) => {}
        Err(e @ CallbackError::Closed(_)) => {
            log.push(format!("turn-complete turn {}: {e}", req.turn));
        }
        Err(e) => {
            log.push(format!("turn-complete turn {}: {e}", req.turn));
            return error(
                503,
                "callback_unavailable",
                "the turn could not be reported; retry",
            );
        }
    }
    if let Err(e) = deps
        .store
        .mark_turn_ended(&req.run_id, &req.agent_id, req.turn)
        .await
    {
        log.push(format!("turn-complete turn {}: {e}", req.turn));
    }
    json(200, &TurnCompleteResponse { accepted: true })
}

async fn events<S: Store, M: Minter, C: Callbacks>(
    caller: &Caller,
    body: &str,
    deps: &Deps<S, M, C>,
    log: &mut Vec<String>,
    is_fenced: bool,
) -> InternalResponse {
    let req: EventsBatchRequest = match parse(body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    if !same_caller(caller, &req.run_id, &req.agent_id) {
        return deny(Denial::CallerMismatch);
    }
    if let Err(e) = validate_events_batch(&req) {
        return error(400, "bad_batch", &e.to_string());
    }
    if let Err(e) = deps
        .store
        .put_events(&req.run_id, &req.agent_id, &req.events)
        .await
    {
        return store_failure(&e);
    }
    // The batch of a fenced generation is stored and answered with no control and no message notice. Nothing is taken for it:
    // a control that waits (the user's cancel) stays queued for the session the run goes on in, and stored events move no run state.
    if is_fenced {
        return json(
            200,
            &EventsBatchResponse {
                accepted: u32::try_from(req.events.len()).unwrap_or(u32::MAX),
                pending: vec![],
            },
        );
    }
    // A control read failure leaves the controls queued for the next batch; a final
    // flush (`takeControls: false`) leaves them for the next turn.
    let pending = if req.take_controls == Some(false) {
        vec![]
    } else {
        match deps.store.take_controls(&req.run_id, &req.agent_id).await {
            Ok(c) => c,
            Err(e) => {
                log.push(format!(
                    "controls for {}/{} not read: {e}",
                    req.run_id, req.agent_id
                ));
                vec![]
            }
        }
    };
    let mut pending = pending;
    // A Cancel on every answer while the user's cancel is recorded on the Run item (level-triggered, like the notices below). A
    // queued control is handed out once and is marked delivered before the answer reaches the worker: a session that took it and died,
    // or an answer that was lost, would lose the cancel. The record is read, not taken. A repeated Cancel is harmless in the worker. A
    // failed read is a diagnostic and no Cancel: the queued control still works.
    if req.take_controls != Some(false)
        && req.agent_id == DIRECT_AGENT_ID
        && !pending.contains(&PendingControl::Cancel)
    {
        match deps.store.cancel_requested(&req.run_id).await {
            Ok(true) => pending.push(PendingControl::Cancel),
            Ok(false) => {}
            Err(e) => log.push(format!(
                "cancel record of {}/{} not read: {e}",
                req.run_id, req.agent_id
            )),
        }
    }
    // The notices of pending messages ride along with the controls; they are read, not taken, so they repeat until agentd claims the message.
    if req.take_controls != Some(false) && req.agent_id == DIRECT_AGENT_ID {
        match deps.store.message_notices(&req.run_id).await {
            Ok(notices) => pending.extend(notices.into_iter().map(PendingControl::Message)),
            Err(e) => log.push(format!(
                "message notices for {}/{} not read: {e}",
                req.run_id, req.agent_id
            )),
        }
    }
    json(
        200,
        &EventsBatchResponse {
            accepted: u32::try_from(req.events.len()).unwrap_or(u32::MAX),
            pending,
        },
    )
}

/// A ledger error as the answer: a refusal keeps its status and code (409 `stale-generation` tells this agentd to stop acting on
/// messages), anything else is a 503.
fn ledger_failure(e: &LedgerError) -> InternalResponse {
    match e {
        LedgerError::Refused(r) => error(r.status(), r.code(), &r.message()),
        LedgerError::Unavailable(_) => {
            error(503, "store_unavailable", "the harness store is unavailable")
        }
    }
}

/// Messages belong to the direct agent; any other caller of these routes is refused.
fn direct_only(caller: &Caller, run: &str, agent: &str) -> Result<(), InternalResponse> {
    if !same_caller(caller, run, agent) {
        return Err(deny(Denial::CallerMismatch));
    }
    if agent != DIRECT_AGENT_ID {
        return Err(error(
            403,
            "not_direct_agent",
            "messages belong to the direct agent",
        ));
    }
    Ok(())
}

async fn message_session<S: Store, M: Minter, C: Callbacks>(
    caller: &Caller,
    body: &str,
    deps: &Deps<S, M, C>,
) -> InternalResponse {
    let req: SessionReport = match parse(body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    if let Err(resp) = direct_only(caller, &req.run_id, &req.agent_id) {
        return resp;
    }
    match deps.store.message_session(&req.run_id, &req).await {
        Ok(ack) => json(200, &ack),
        Err(e) => ledger_failure(&e),
    }
}

async fn message_claim<S: Store, M: Minter, C: Callbacks>(
    caller: &Caller,
    body: &str,
    deps: &Deps<S, M, C>,
    log: &mut Vec<String>,
) -> InternalResponse {
    let req: ClaimRequest = match parse(body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    if let Err(resp) = direct_only(caller, &req.run_id, &req.agent_id) {
        return resp;
    }
    // A Cancel that waits for agentd wins over a message. A failed read is a diagnostic: agentd handles a Cancel before a message anyway.
    let cancel = match deps.store.cancel_queued(&req.run_id, &req.agent_id).await {
        Ok(c) => c,
        Err(e) => {
            log.push(format!(
                "cancel check of {}/{}: {e}",
                req.run_id, req.agent_id
            ));
            false
        }
    };
    // So does a cancel that is recorded on the Run item: a control that was taken must not reopen the claim.
    let cancel = cancel
        || match deps.store.cancel_requested(&req.run_id).await {
            Ok(c) => c,
            Err(e) => {
                log.push(format!(
                    "cancel record of {}/{}: {e}",
                    req.run_id, req.agent_id
                ));
                false
            }
        };
    match deps.store.message_claim(&req, cancel).await {
        Ok(r) => json(200, &r),
        Err(e) => ledger_failure(&e),
    }
}

async fn message_seal<S: Store, M: Minter, C: Callbacks>(
    caller: &Caller,
    body: &str,
    deps: &Deps<S, M, C>,
) -> InternalResponse {
    let req: SealRequest = match parse(body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    if let Err(resp) = direct_only(caller, &req.run_id, &req.agent_id) {
        return resp;
    }
    match deps.store.message_seal(&req).await {
        Ok(r) => json(200, &r),
        Err(e) => ledger_failure(&e),
    }
}

async fn message_outcome<S: Store, M: Minter, C: Callbacks>(
    caller: &Caller,
    body: &str,
    deps: &Deps<S, M, C>,
) -> InternalResponse {
    let req: OutcomeRequest = match parse(body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    if let Err(resp) = direct_only(caller, &req.run_id, &req.agent_id) {
        return resp;
    }
    match deps.store.message_outcome(&req).await {
        Ok(r) => json(200, &r),
        Err(e) => ledger_failure(&e),
    }
}

/// The messages of the caller's run that were delivered to an engine, for the first prompt of a session that follows. The run of
/// the read is the run of the caller's IAM session; a body that names another run or agent is refused before anything is read. The route
/// reads: it takes no generation and no session, moves no message and replaces no incarnation of agentd. The engine holds the same
/// session credentials as agentd and can ask too; what it gets are the messages its own run was sent, and it can add none (the texts
/// are the ones control-api stored, and a worker session cannot call that API). A read that fails is an error: an empty list would say
/// that the user sent nothing.
async fn message_history<S: Store, M: Minter, C: Callbacks>(
    caller: &Caller,
    body: &str,
    deps: &Deps<S, M, C>,
) -> InternalResponse {
    let req: HistoryRequest = match parse(body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    if let Err(resp) = direct_only(caller, &req.run_id, &req.agent_id) {
        return resp;
    }
    match deps.store.message_history(&caller.run_id).await {
        Ok(r) => json(200, &r),
        Err(e) => ledger_failure(&e),
    }
}
