//! Run control behind `ham cancel` / `ham stop`.
//!
//! Cancel keeps the work: with a live agent and no `?discard=true` it queues one
//! `Cancel` control for the agent and changes nothing else. agentd ends the prompt within 10 s, runs its end sequence, and the
//! run machine delivers what exists with the label `Cline stopped: you stopped the run`. `?discard=true` (exactly the word
//! `true`), or a run with no live agent (nothing to keep), clears the run: the Run item and the root Goal move to `cleared`
//! at their next versions, the harness `goal.status` is published in the same write ([`Store::update_run`]) and the `run`
//! execution is stopped. A concurrent writer makes the change a 409. A repeat cancel of a cleared run re-sends the stop, so a
//! retry after a failure finishes the job.

use crate::body::parse_body;
use crate::handler::{
    Blobs, Deps, Execution, Reply, Store, StoreError, V1Request, error, json, not_found, owned,
    unavailable,
};
use crate::ids::{new_ulid, now_rfc3339};
use crate::input;
use harness_protocol::api::internal::PendingControl;
use harness_protocol::api::v1::{InputAccepted, InputAnswer, RunControlResponse};
use harness_protocol::common::parse_utc_unix;
use harness_protocol::direct::DIRECT_AGENT_ID;
use harness_protocol::direct_input::InstructOrigin;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::goal::GoalStatus;
use harness_protocol::orchestration::GoalStatusPayload;
use harness_protocol::store::{AgentItem, AgentStatus, ControlItem, RunItem};

fn word(s: GoalStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The status a cancel moves `from` to, `Ok(None)` when the run is cleared already, or the refusal.
fn transition(run_id: &str, from: GoalStatus) -> Result<Option<GoalStatus>, Reply> {
    match from {
        GoalStatus::Complete | GoalStatus::Paused | GoalStatus::Stalled => Err(error(
            409,
            "run-finished",
            &format!("run {run_id} is {}; it cannot change", word(from)),
        )),
        GoalStatus::Cleared => Ok(None),
        _ => Ok(Some(GoalStatus::Cleared)),
    }
}

/// The run of the caller, or the reply that ends the request.
async fn run_of<S: Store>(store: &S, run_id: &str, caller: &str) -> Result<RunItem, Reply> {
    match owned(store, run_id, caller).await {
        Ok(Some(run)) => Ok(run),
        Ok(None) => Err(not_found(&format!("no run {run_id} for this caller"))),
        Err(e) => Err(unavailable(&e, "retry")),
    }
}

fn status_event(run: &RunItem) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: new_ulid(),
        run_id: run.run_id.clone(),
        task_id: None,
        agent_id: None,
        seq: 0,
        ts: now_rfc3339(),
        event_type: EventType::GoalStatus,
        authority: Authority::Harness,
        payload: Payload::Inline {
            value: serde_json::to_value(GoalStatusPayload {
                status: run.status,
                phase: Some(run.phase),
            })
            .unwrap_or_default(),
        },
    }
}

fn live(a: &AgentItem) -> bool {
    !matches!(a.status, AgentStatus::Stopped | AgentStatus::Failed)
}

/// What [`stop_run_work`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StopReport {
    /// Cancel controls queued, one per agent not stopped or failed.
    pub controls: usize,
}

/// Stops a run's work, in cancel's order: a `Cancel` control for every live agent, then the `run` execution when
/// `stop_root`. The first failure ends it; every step is safe to repeat.
pub async fn stop_run_work<S: Store, E: Execution>(
    store: &S,
    execution: &E,
    run_id: &str,
    cause: &str,
    stop_root: bool,
) -> Result<StopReport, StoreError> {
    let mut report = StopReport::default();
    for a in store.list_agents(run_id).await?.iter().filter(|a| live(a)) {
        store
            .put_control(&ControlItem {
                run_id: run_id.to_string(),
                agent_id: a.agent_id.clone(),
                control_id: new_ulid(),
                control: PendingControl::Cancel,
                created_at: now_rfc3339(),
                delivered_at: None,
            })
            .await?;
        report.controls += 1;
    }
    if stop_root {
        execution.stop(run_id, cause).await?;
    }
    Ok(report)
}

/// `POST /v1/runs/{runId}/cancel`; `discard` is the `?discard=true` query.
pub(crate) async fn cancel<S: Store, B: Blobs, E: Execution>(
    run_id: &str,
    caller: &str,
    discard: bool,
    deps: &Deps<S, B, E>,
) -> Reply {
    let mut run = match run_of(&deps.store, run_id, caller).await {
        Ok(r) => r,
        Err(reply) => return reply,
    };
    let to = match transition(run_id, run.status) {
        Ok(t) => t,
        Err(reply) => return reply,
    };
    if !discard && to.is_some() {
        let agents = match deps.store.list_agents(run_id).await {
            Ok(a) => a,
            Err(e) => return unavailable(&e, "retry"),
        };
        if agents.iter().any(live) {
            let cause = format!("cancelled by {caller}; the work is kept");
            // The run is closed to messages BEFORE the Cancel is queued: with the order reversed agentd could claim a message between the
            // two writes and send it to Cline after the user stopped the run.
            let logs = close_messages(deps, run_id).await;
            if let Err(e) = stop_run_work(&deps.store, &deps.execution, run_id, &cause, false).await
            {
                return unavailable(&e, "retry ham stop to ask Cline to stop");
            }
            let (resp, _) = json(
                200,
                &RunControlResponse {
                    run_id: run_id.to_string(),
                    status: run.status,
                },
            );
            return (resp, logs);
        }
    }
    if let Some(status) = to {
        let goal = match deps.store.get_root_goal(run_id).await {
            Ok(g) => g.map(|mut g| {
                g.status = status;
                g.version += 1;
                g
            }),
            Err(e) => return unavailable(&e, "retry"),
        };
        run.status = status;
        run.version += 1;
        let event = status_event(&run);
        match deps
            .store
            .update_run(&run, goal.as_ref(), Some(&event))
            .await
        {
            Ok(()) => {}
            Err(StoreError::Conflict) => {
                return error(
                    409,
                    "conflict",
                    &format!("run {run_id} changed while it was being updated; retry"),
                );
            }
            Err(e) => return unavailable(&e, "retry"),
        }
    }
    const RETRY: &str = "the run is cleared; retry ham cancel to stop it";
    let cause = format!("cancelled by {caller}");
    let logs = close_messages(deps, run_id).await;
    if let Err(e) = stop_run_work(&deps.store, &deps.execution, run_id, &cause, true).await {
        return unavailable(&e, RETRY);
    }
    let (resp, _) = json(
        200,
        &RunControlResponse {
            run_id: run_id.to_string(),
            status: run.status,
        },
    );
    (resp, logs)
}

/// A cancel wins over a message: the run takes no more messages and the open ones end `rejected` (`run-cancelled`). It runs
/// before the Cancel is queued. Best effort: a failure here is a diagnostic only and never stops the cancel, because worker-api's claim
/// refuses while a Cancel waits and finalize closes what is left.
async fn close_messages<S: Store, B: Blobs, E: Execution>(
    deps: &Deps<S, B, E>,
    run_id: &str,
) -> Vec<String> {
    match deps
        .store
        .message_close(run_id, message_ledger::CloseReason::Cancel)
        .await
    {
        Ok(_) => Vec::new(),
        Err(e) => vec![format!(
            "messages of run {run_id} not closed by the cancel: {e}"
        )],
    }
}

fn accepted(run_id: &str, a: &InputAnswer, state: &str, status: u16) -> Reply {
    json(
        status,
        &InputAccepted {
            run_id: run_id.to_string(),
            request_id: a.request_id.clone(),
            state: state.to_string(),
        },
    )
}

/// What a repeat of an answer earns once a control for the request exists: another answer is a 409 that names the winner, the
/// same answer is `already-queued`, and the same answer agentd took at least [`input::REARM_AFTER_S`] ago without acknowledging
/// it is queued again (`re-queued`).
async fn resolve<S: Store, B: Blobs, E: Execution>(
    existing: ControlItem,
    answer: &InputAnswer,
    run_id: &str,
    now: i64,
    deps: &Deps<S, B, E>,
) -> Reply {
    if !input::same_answer(&existing.control, answer) {
        let won = input::stored_action(&existing.control)
            .map_or_else(|| "another answer".to_string(), input::word);
        return error(
            409,
            "already-answered",
            &format!("this question was already answered with {won}"),
        );
    }
    let queued = || accepted(run_id, answer, "already-queued", 200);
    let Some(delivered) = existing.delivered_at.as_deref() else {
        return queued();
    };
    let age = parse_utc_unix(delivered).map_or(0, |t| now - t);
    if age < input::REARM_AFTER_S {
        return queued();
    }
    let events = match deps
        .store
        .events(
            run_id,
            input::direct_shard(),
            Some(&answer.request_id),
            input::ACK_SCAN_EVENTS,
        )
        .await
    {
        Ok(e) => e,
        Err(e) => return unavailable(&e, "retry"),
    };
    if input::acknowledged(&events, &answer.request_id) {
        return queued();
    }
    match deps
        .store
        .rearm_control(run_id, DIRECT_AGENT_ID, &answer.request_id)
        .await
    {
        Ok(true) => accepted(run_id, answer, "re-queued", 202),
        Ok(false) => queued(),
        Err(e) => unavailable(&e, "retry"),
    }
}

/// `POST /v1/runs/{runId}/input` (`answerInput`): the user's answer to the question a stuck Cline raised. One `Instruct` control
/// is queued for the direct agent; its id is the request id, so a question takes one answer and the same answer again is harmless.
/// A control that already exists for the request decides before the clock does, so a resend after the window still reads as what
/// it is.
pub(crate) async fn answer_input<S: Store, B: Blobs, E: Execution>(
    req: &V1Request,
    run_id: &str,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    let answer: InputAnswer = match parse_body("InputAnswer", &req.body) {
        Ok(a) => a,
        Err(reply) => return reply,
    };
    if let Err(reply) = input::validate(&answer) {
        return reply;
    }
    if let Some(model) = &answer.model
        && let Err(reply) = input::check_model(model)
    {
        return reply;
    }
    let run = match run_of(&deps.store, run_id, caller).await {
        Ok(r) => r,
        Err(reply) => return reply,
    };
    if run.status.is_delivered() || run.status == GoalStatus::Cleared {
        return error(
            409,
            "run-finished",
            &format!(
                "run {run_id} is {}; it is not waiting for you",
                word(run.status)
            ),
        );
    }
    match deps.store.list_agents(run_id).await {
        Ok(agents)
            if agents
                .iter()
                .any(|a| a.agent_id == DIRECT_AGENT_ID && live(a)) => {}
        Ok(_) => {
            return error(
                409,
                "no-live-session",
                "the run has no live Cline session to answer",
            );
        }
        Err(e) => return unavailable(&e, "retry"),
    }
    let event = match deps
        .store
        .get_event(run_id, input::direct_shard(), &answer.request_id)
        .await
    {
        Ok(e) => e,
        Err(e) => return unavailable(&e, "retry"),
    };
    let question = match input::admit(run_id, event.as_ref()) {
        Ok(q) => q,
        Err(reply) => return reply,
    };
    let now = input::unix_now();
    if input::expired(&question, now) {
        return match deps
            .store
            .get_control(run_id, DIRECT_AGENT_ID, &answer.request_id)
            .await
        {
            Ok(Some(existing)) => resolve(existing, &answer, run_id, now, deps).await,
            Ok(None) => error(
                410,
                "request-expired",
                &format!(
                    "the answer window closed at {}",
                    question.expires_at.as_deref().unwrap_or("an earlier time")
                ),
            ),
            Err(e) => unavailable(&e, "retry"),
        };
    }
    let control = ControlItem {
        run_id: run_id.to_string(),
        agent_id: DIRECT_AGENT_ID.to_string(),
        control_id: answer.request_id.clone(),
        control: PendingControl::Instruct {
            request_id: answer.request_id.clone(),
            action: answer.action,
            text: answer.text.clone(),
            model: answer.model.clone(),
            origin: InstructOrigin::User,
        },
        created_at: now_rfc3339(),
        delivered_at: None,
    };
    match deps.store.put_control(&control).await {
        Ok(()) => accepted(run_id, &answer, "queued", 202),
        Err(StoreError::Conflict) => match deps
            .store
            .get_control(run_id, DIRECT_AGENT_ID, &answer.request_id)
            .await
        {
            Ok(Some(existing)) => resolve(existing, &answer, run_id, now, deps).await,
            Ok(None) => unavailable(
                &StoreError::Unavailable("the control conflicted but reads as absent".into()),
                "retry",
            ),
            Err(e) => unavailable(&e, "retry"),
        },
        Err(e) => unavailable(&e, "retry"),
    }
}
