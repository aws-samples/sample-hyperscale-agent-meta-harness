//! `run-ended`: the EventBridge rule `hamh-direct-run-ended` sends every Step Functions
//! status change of the `hamh-direct-run` machine that ends ABORTED, FAILED or TIMED_OUT. An operator's discard, an
//! uncaught error and the machine's timeout take one path: the run is cleared with the execution's end
//! (`RunItem.aborted`, `run.aborted`), and its session is stopped. It delivers nothing. Repeating it is harmless: a
//! run that is already `complete` or `cleared` is not touched, and stopping a stopped session is a success.

use crate::error::RunSessionError;
use crate::events;
use crate::ops::Deps;
use crate::ports::{Blobs, PackageTokens, Res, Sessions, Store, Tokens};
use harness_protocol::common::is_ulid;
use harness_protocol::envelope::{Envelope, EventType};
use harness_protocol::goal::{GoalPhase, GoalStatus};
use harness_protocol::orchestration::{ExecutionEnd, RunAbortedPayload};
use serde::Serialize;
use serde_json::Value;
use worker_api::handler::Minter;

/// Run writes lost to a concurrent writer (run control) before the reap gives up for this invocation.
pub const CONFLICT_ATTEMPTS: u32 = 5;

/// A run execution's end, from its status-change event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunEnd {
    pub run_id: String,
    pub execution_arn: String,
    pub status: ExecutionEnd,
    pub error: Option<String>,
    pub cause: Option<String>,
    /// `detail.stopDate` as milliseconds; `None` when the event has none.
    pub stopped_at_ms: Option<u64>,
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// True for the shape of an EventBridge "Step Functions Execution Status Change" event.
pub fn is_status_change(event: &Value) -> bool {
    event.get("source").and_then(Value::as_str) == Some("aws.states")
}

/// The end of `run_machine_arn`'s execution an EventBridge event reports.
pub fn parse_run_end(event: &Value, run_machine_arn: &str) -> Res<RunEnd> {
    let invalid = |m: String| Err(RunSessionError::Invalid(m));
    let status_change = is_status_change(event)
        && event.get("detail-type").and_then(Value::as_str)
            == Some("Step Functions Execution Status Change");
    let Some(d) = event
        .get("detail")
        .filter(|d| status_change && d.is_object())
    else {
        return invalid("not a Step Functions execution status change".into());
    };
    let machine = text(d, "stateMachineArn").unwrap_or_default();
    if machine != run_machine_arn {
        return invalid(format!(
            "the execution of {machine} is not one of the run machine {run_machine_arn}"
        ));
    }
    let word = text(d, "status").unwrap_or_default();
    let Some(status) = ExecutionEnd::from_status(&word) else {
        return invalid(format!(
            "status {word}: only ABORTED, FAILED and TIMED_OUT leave a run unfinished"
        ));
    };
    let name = text(d, "name").unwrap_or_default();
    if !is_ulid(&name) {
        return invalid(format!("execution name {name} is not a run id"));
    }
    Ok(RunEnd {
        run_id: name,
        execution_arn: text(d, "executionArn").unwrap_or_default(),
        status,
        error: text(d, "error"),
        cause: text(d, "cause"),
        stopped_at_ms: d.get("stopDate").and_then(Value::as_u64),
    })
}

/// `YYYY-MM-DDTHH:MM:SSZ` as Unix milliseconds.
pub fn parse_rfc3339_ms(ts: &str) -> Option<u64> {
    let b = ts.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| ts.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from((days * 86_400 + hh * 3600 + mm * 60 + ss) * 1000).ok()
}

/// What a reap did: the Lambda's one JSON log line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reaped {
    pub run_id: String,
    pub status: ExecutionEnd,
    /// The run has a Run item (a run control-api never created has nothing to reap).
    pub found: bool,
    /// This reap moved the run to `cleared` (false when it was already `complete` or `cleared`).
    pub cleared: bool,
    pub session_stopped: bool,
}

fn abort_events(run_id: &str, phase: GoalPhase, why: &RunAbortedPayload, ms: u64) -> Vec<Envelope> {
    vec![
        events::goal_status(run_id, ms, 5, GoalStatus::Cleared, phase),
        events::harness_event(run_id, ms + 1, 1, EventType::RunAborted, None, why),
    ]
}

pub async fn reap<S, T, M, R, B, P>(deps: &Deps<S, T, M, R, B, P>, end: &RunEnd) -> Res<Reaped>
where
    S: Store,
    T: Tokens,
    M: Minter,
    R: Sessions,
    B: Blobs,
    P: PackageTokens,
{
    let run_id = end.run_id.as_str();
    let ms = end.stopped_at_ms.unwrap_or_else(|| deps.cfg.now_ms());
    let why = RunAbortedPayload::new(
        end.status,
        end.error.as_deref(),
        end.cause.as_deref(),
        &dispatcher::items::rfc3339(ms / 1000),
    );
    let mut out = Reaped {
        run_id: run_id.to_string(),
        status: end.status,
        found: false,
        cleared: false,
        session_stopped: false,
    };
    let mut settled = false;
    for _ in 0..CONFLICT_ATTEMPTS {
        let Some(mut run) = deps.store.get_run(run_id).await? else {
            return Ok(out);
        };
        out.found = true;
        if run.status.is_terminal() {
            settled = true;
            break;
        }
        let goal = deps.store.get_root_goal(run_id).await?.map(|mut g| {
            g.status = GoalStatus::Cleared;
            g.version += 1;
            g
        });
        let phase = run.phase;
        run.status = GoalStatus::Cleared;
        run.version += 1;
        run.aborted = Some(Box::new(why.clone()));
        let events = abort_events(run_id, phase, &why, ms);
        if deps.store.abort_run(&run, goal.as_ref(), &events).await? {
            crate::finalize::touch_index(deps, &run, None, ms).await;
            out.cleared = true;
            settled = true;
            break;
        }
    }
    if !settled {
        return Err(RunSessionError::Unavailable(format!(
            "run {run_id} changed under {CONFLICT_ATTEMPTS} attempts to clear it"
        )));
    }
    // The machine is gone: nothing else will stop the session (a failure here fails the invocation and EventBridge retries).
    out.session_stopped = crate::finalize::end_session(deps, run_id).await?;
    Ok(out)
}
