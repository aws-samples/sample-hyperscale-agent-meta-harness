//! Messages to a running Cline: `POST /v1/runs/{runId}/messages`, `GET /v1/runs/{runId}/messages`,
//! `GET /v1/runs/{runId}/messages/{messageId}` and `POST /v1/runs/{runId}/messages/{messageId}/withdraw`. Owner only, like every run route
//! (another caller's run is a 404, and so is another caller's message id).
//!
//! The rules that move a message are the `message-ledger` crate's; this module is the HTTP side: the body checks, the checks of the run
//! (it exists for the caller, it is not finished, its Agent registered `message.next`, no question waits for the answer), the mapping of a
//! refusal to its status and code, and the order of those checks. The order matters in one place: an id the ledger has seen is answered
//! from the ledger BEFORE the run's state is looked at, so the retry of a message that was accepted just before the run finished returns
//! the stored message, not 409 `run-finished`.

use crate::body::parse_body;
use crate::handler::{
    Blobs, Deps, Execution, Reply, Store, V1Request, error, json, not_found, owned, unavailable,
};
use crate::input;
use harness_protocol::common::is_ulid;
use harness_protocol::direct::DIRECT_AGENT_ID;
use harness_protocol::goal::GoalStatus;
use harness_protocol::messages::{
    MESSAGE_NEXT_CONTROL, MESSAGE_NOW_CONTROL, MessageMode, MessageResponse, SendMessageRequest,
    SendMessageResponse, SendOutcome, codes, validate_send,
};
use harness_protocol::store::{AgentItem, AgentStatus, RunItem};
use message_ledger::{LedgerError, Refusal};

fn refusal_reply(r: &Refusal, engine: &str) -> Reply {
    error(r.status(), r.code(), &r.message_for(engine))
}

/// A ledger error as the reply: a refusal says what it is, anything else is a 503 and the store's reason goes to the log.
fn ledger_reply(e: &LedgerError, engine: &str) -> Reply {
    match e {
        LedgerError::Refused(r) => refusal_reply(r, engine),
        LedgerError::Unavailable(_) => unavailable(
            &crate::handler::StoreError::Unavailable(e.to_string()),
            "retry; the same message id is safe to send again",
        ),
    }
}

async fn run_of<S: Store>(store: &S, run_id: &str, caller: &str) -> Result<RunItem, Reply> {
    match owned(store, run_id, caller).await {
        Ok(Some(run)) => Ok(run),
        Ok(None) => Err(not_found(&format!("no run {run_id} for this caller"))),
        Err(e) => Err(unavailable(&e, "retry")),
    }
}

fn live(a: &AgentItem) -> bool {
    !matches!(a.status, AgentStatus::Stopped | AgentStatus::Failed)
}

/// `POST /v1/runs/{runId}/messages`.
pub(crate) async fn send<S: Store, B: Blobs, E: Execution>(
    req: &V1Request,
    run_id: &str,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    let body: SendMessageRequest = match parse_body("SendMessageRequest", &req.body) {
        Ok(b) => b,
        Err(reply) => return reply,
    };
    if let Err(e) = validate_send(&body) {
        return error(400, "bad-input", &e.to_string());
    }
    let run = match run_of(&deps.store, run_id, caller).await {
        Ok(r) => r,
        Err(reply) => return reply,
    };
    // The words below name the engine of the run.
    let engine = crate::handler::engine_name_of(&run);
    // A message the ledger has seen is answered from the ledger first: a retry returns the stored message or the idempotency conflict.
    match deps.store.message_get(run_id, &body.message_id).await {
        Ok(Some(_)) => return admit(run_id, caller, &body, deps, &engine).await,
        Ok(None) => {}
        Err(e) => return ledger_reply(&e, &engine),
    }
    if run.status.is_delivered() || run.status == GoalStatus::Cleared {
        return error(
            409,
            "run-finished",
            &format!("run {run_id} is finished; start a follow-up run from its delivered result"),
        );
    }
    let agents = match deps.store.list_agents(run_id).await {
        Ok(a) => a,
        Err(e) => return unavailable(&e, "retry"),
    };
    let Some(agent) = agents
        .iter()
        .find(|a| a.agent_id == DIRECT_AGENT_ID && live(a))
    else {
        return error(
            409,
            "no-live-session",
            &format!("the run has no live {engine} session to take a message"),
        );
    };
    let wants = match body.mode {
        MessageMode::Next => MESSAGE_NEXT_CONTROL,
        MessageMode::Now => MESSAGE_NOW_CONTROL,
    };
    if !agent.harness_controls.iter().any(|c| c == wants) {
        return error(
            409,
            codes::NO_MESSAGE_CONTROL,
            &format!(
                "this run's {engine} cannot take messages (it started on an older version); /cancel stops the run and keeps the work"
            ),
        );
    }
    // While a question waits, the text is its answer; a message would sit behind the question for the whole window.
    let mut logs = Vec::new();
    match input::active_tail(&deps.store, run_id).await {
        Ok((Some(q), _)) => {
            return error(
                409,
                codes::QUESTION_OPEN,
                &format!(
                    "{engine} is waiting for your answer to a question (request {}); send the text as the answer",
                    q.request_id
                ),
            );
        }
        Ok((None, _)) => {}
        Err(e) => logs.push(format!("question check of run {run_id}: {e}")),
    }
    let (resp, mut more) = admit(run_id, caller, &body, deps, &engine).await;
    more.splice(0..0, logs);
    (resp, more)
}

async fn admit<S: Store, B: Blobs, E: Execution>(
    run_id: &str,
    caller: &str,
    body: &SendMessageRequest,
    deps: &Deps<S, B, E>,
    engine: &str,
) -> Reply {
    match deps.store.message_admit(run_id, caller, body).await {
        Ok(a) => {
            let status = match a.outcome {
                SendOutcome::Accepted => 202,
                SendOutcome::AlreadyAccepted => 200,
            };
            json(
                status,
                &SendMessageResponse {
                    outcome: a.outcome,
                    message: a.message,
                },
            )
        }
        Err(e) => ledger_reply(&e, engine),
    }
}

/// `GET /v1/runs/{runId}/messages`.
pub(crate) async fn list<S: Store, B: Blobs, E: Execution>(
    run_id: &str,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    let run = match run_of(&deps.store, run_id, caller).await {
        Ok(r) => r,
        Err(reply) => return reply,
    };
    let engine = crate::handler::engine_name_of(&run);
    match deps.store.message_list(run_id).await {
        Ok(page) => json(200, &page),
        Err(e) => ledger_reply(&e, &engine),
    }
}

/// `GET /v1/runs/{runId}/messages/{messageId}`.
pub(crate) async fn get<S: Store, B: Blobs, E: Execution>(
    run_id: &str,
    message_id: &str,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    let run = match run_of(&deps.store, run_id, caller).await {
        Ok(r) => r,
        Err(reply) => return reply,
    };
    let engine = crate::handler::engine_name_of(&run);
    if !is_ulid(message_id) {
        return not_found(&format!("no message {message_id} in run {run_id}"));
    }
    match deps.store.message_get(run_id, message_id).await {
        Ok(Some(message)) => json(200, &MessageResponse { message }),
        Ok(None) => not_found(&format!("no message {message_id} in run {run_id}")),
        Err(e) => ledger_reply(&e, &engine),
    }
}

/// `POST /v1/runs/{runId}/messages/{messageId}/withdraw`: a pending or held message is withdrawn; one agentd has claimed is 409
/// `already-dispatching`. Withdrawing a withdrawn message answers it again.
pub(crate) async fn withdraw<S: Store, B: Blobs, E: Execution>(
    run_id: &str,
    message_id: &str,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    let run = match run_of(&deps.store, run_id, caller).await {
        Ok(r) => r,
        Err(reply) => return reply,
    };
    let engine = crate::handler::engine_name_of(&run);
    if !is_ulid(message_id) {
        return not_found(&format!("no message {message_id} in run {run_id}"));
    }
    match deps.store.message_withdraw(run_id, message_id).await {
        Ok(message) => json(200, &MessageResponse { message }),
        Err(e) => ledger_reply(&e, &engine),
    }
}
