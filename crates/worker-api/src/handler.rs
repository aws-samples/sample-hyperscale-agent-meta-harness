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
use harness_protocol::direct::{DIRECT_AGENT_ID, message_preview};
use harness_protocol::envelope::Envelope;
use harness_protocol::goal::GoalStatus;
use harness_protocol::messages::{
    ClaimRequest, ClaimResponse, MessageNotice, OutcomeRequest, OutcomeResponse, SealRequest,
    SealResponse, SessionAck, SessionReport,
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
    // The engine holds the same session credentials as agentd and could post these two itself; they need the key only agentd holds.
    if matches!(op, "events" | "turn-complete")
        && let Err(resp) = completion_guard(&caller, req.completion_key.as_deref(), deps).await
    {
        return resp;
    }
    match op {
        "creds" => creds(&caller, &req.body, deps).await,
        "heartbeat" => heartbeat(&caller, &req.body, deps, log).await,
        "turn-complete" => turn_complete(&caller, &req.body, deps, log).await,
        "message-session" => message_session(&caller, &req.body, deps).await,
        "message-claim" => message_claim(&caller, &req.body, deps, log).await,
        "message-seal" => message_seal(&caller, &req.body, deps).await,
        "message-outcome" => message_outcome(&caller, &req.body, deps).await,
        _ => events(&caller, &req.body, deps, log).await,
    }
}

async fn creds<S: Store, M: Minter, C: Callbacks>(
    caller: &Caller,
    body: &str,
    deps: &Deps<S, M, C>,
) -> InternalResponse {
    let req: CredsRequest = match parse(body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    if req.run_id != caller.run_id || req.agent_id != caller.agent_id {
        return deny(Denial::CallerMismatch);
    }
    let agent = match deps.store.get_agent(&caller.run_id, &caller.agent_id).await {
        Ok(a) => a,
        Err(e) => return store_failure(&e),
    };
    let goal = match deps.store.get_goal_status(&caller.run_id).await {
        Ok(g) => g,
        Err(e) => return store_failure(&e),
    };
    let plan = match decide(caller, &req, agent.as_ref(), goal) {
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

/// The completion-key check of `/internal/events` and `/internal/turn-complete`, before anything is read from the body, written or sent to Step
/// Functions. The run and the agent come from the caller's IAM session, so a request cannot choose another run's key. An Agent item that names no key (a
/// run started before the key existed), and no Agent item at all, leave the route as it was: the engine cannot create or change an Agent item.
async fn completion_guard<S: Store, M: Minter, C: Callbacks>(
    caller: &Caller,
    presented: Option<&str>,
    deps: &Deps<S, M, C>,
) -> Result<(), InternalResponse> {
    let agent = match deps.store.get_agent(&caller.run_id, &caller.agent_id).await {
        Ok(a) => a,
        Err(e) => return Err(store_failure(&e)),
    };
    match agent
        .as_ref()
        .and_then(|a| a.creds.as_ref())
        .map(|c| c.check_completion_key(presented))
    {
        Some(CompletionKeyCheck::Refused) => Err(deny(Denial::CompletionKey)),
        Some(CompletionKeyCheck::Legacy | CompletionKeyCheck::Valid) | None => Ok(()),
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
) -> InternalResponse {
    let req: HeartbeatRequest = match parse(body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    if !same_caller(caller, &req.run_id, &req.agent_id) {
        return deny(Denial::CallerMismatch);
    }
    if let Some(stale) = stale_slice(deps, &req.run_id, &req.agent_id, req.turn).await {
        return stale;
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
) -> InternalResponse {
    let mut req: TurnCompleteRequest = match parse(body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    if !same_caller(caller, &req.run_id, &req.agent_id) {
        return deny(Denial::CallerMismatch);
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
    // The notices of pending messages ride along with the controls; they are read, not taken, so they repeat until agentd claims the message.
    let mut pending = pending;
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
