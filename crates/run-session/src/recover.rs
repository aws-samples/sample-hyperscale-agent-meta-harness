//! The op `recover`. A session of the run left no result (its worker gave notice that it is being ended, its heartbeat stopped, or the
//! state ended some other way). Before the next session starts, the service reads what that session's own storage still holds into one copy.
//!
//! The storage of a session belongs to its runtime session id. A call of that id after the session was stopped gets a new machine with the
//! same storage, and the worker there copies the tree into `runs/<run>/agents/recovery/turns/<n>/recovery.bundle` and answers with the
//! facts. This op makes that one call and writes down what came of it. It decides nothing about which copy the run goes on from:
//! [`crate::slices::next_slice`] reads the records and decides, once.
//!
//! The order, which the tests pin: the old generation is fenced on the Agent item (so its worker can no longer act on the run), then the
//! session is stopped, and only after a definite answer to that stop is the old session id called. On an end the worker gave notice of, the
//! stop waits until [`RECOVERY_SETTLE_S`] after the notice. The whole op has [`RECOVERY_LIMIT_S`] by its own clock: every wait, call and
//! write takes from what is left, and a call that is still open at the limit is dropped.
//!
//! A read never blocks or fails a run. Every way it can go wrong is a result record with a reason, and an error this op raises is caught
//! by the state `Recover`, which goes on to `NextSlice` either way. The records, each written once under `runs/<run>/slices/`, where no
//! session can write: `<n>-recovery.json` ([`RecoveryAttempt`]: a read began) and `<n>-recovery-result.json` ([`RecoveryResult`]: how it
//! ended). A second call for the same session finds the attempt and calls nothing.

use crate::error::RunSessionError;
use crate::ops::{Config, Deps, SessionState};
use crate::ports::{Blobs, PackageTokens, Res, Sessions, Store, Tokens};
use crate::slices::{self, LoopState, RestoreKind};
use dispatcher::endpoints::session_qualifier;
use harness_protocol::api::internal::TurnCompleteRequest;
use harness_protocol::direct::{
    CopyMarker, DIRECT_AGENT_ID, RECOVERY_AGENT_ID, RECOVERY_CREDENTIALS_S, RECOVERY_LIMIT_S,
    RECOVERY_SETTLE_S, RECOVERY_STOP_CAP_S, RECOVERY_WORKER_BUDGET_S, RecoverKind, RecoveryAnswer,
    RecoveryBaseline, RecoveryComparison, RecoveryInvocationRequest, SESSION_END_REQUESTED, s3,
    validate_recovery_invocation,
};
use harness_protocol::goal::GoalStatus;
use serde::{Deserialize, Serialize};
use std::future::Future;
use worker_api::creds::MintPlan;
use worker_api::handler::Minter;

/// The version of the two records.
pub const SCHEMA_VERSION: u32 = 1;

/// [`RecoveryResult::state`]: the worker stored the copy and the object is the one it described. It says nothing about use.
pub const STATE_COPIED: &str = "copied";
/// [`RecoveryResult::state`]: no copy came of the read; `reason` says why.
pub const STATE_MISSED: &str = "missed";
/// [`RecoveryResult::state`]: no read was made for this session; `reason` says why.
pub const STATE_SKIPPED: &str = "skipped";
/// [`RecoveryResult::state`]: a read began and left no result; `next-slice` closed it, so nothing that ends late can be used.
pub const STATE_ABANDONED: &str = "abandoned";
/// [`RecoverAnswer::recovery`]: the session ended with a result, never started or is not the one the run is in. Nothing is written.
pub const NOT_NEEDED: &str = "not-needed";
/// [`RecoverAnswer::recovery`]: a read of this session began before. Nothing is called and nothing is written.
pub const DUPLICATE: &str = "duplicate";

/// The cause of a session that ended because its heartbeat stopped (the word of `ham_core::lost_session`).
pub const CAUSE_NO_HEARTBEAT: &str = ham_core::lost_session::CAUSE_NO_HEARTBEAT;
/// The cause of a session that ended without a result for another reason.
pub const CAUSE_NO_RESULT: &str = ham_core::lost_session::CAUSE_NO_RESULT;

/// The seconds between two calls of the old session id after the old machine answered that a turn runs there (HTTP 409).
const RETRY_AFTER_ACTIVE_S: u64 = 5;
/// The seconds between two calls after any other answer that a second call can change (HTTP 424, 429, 5xx, a call that failed).
const RETRY_AFTER_FAULT_S: u64 = 3;
/// The worker gets what the op has left less this many seconds, which the op keeps for checking the object and writing the result.
const KEEP_FOR_THE_RESULT_S: u64 = 10;

/// `runs/<run>/slices/<n>-recovery.json`: a read of session `n`'s storage began. Written once, before anything is called.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryAttempt {
    pub schema_version: u32,
    pub run_id: String,
    pub session: u32,
    pub attempt_id: String,
    pub runtime_session_id: String,
    /// `end-requested`, `no-heartbeat` or `no-result`.
    pub cause: String,
    /// When the service took the worker's notice that the session is being ended; absent when there was none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_known_at_ms: Option<u64>,
    pub started_at_ms: u64,
    pub deadline_ms: u64,
}

/// The op's own stop of the session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopFacts {
    pub started_at_ms: u64,
    pub took_ms: u64,
    /// `true`: the call stopped the session. `false`: the session had already ended. Absent: no definite answer, and the old session id
    /// was not called.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<bool>,
}

/// The newest saved copy of the run when the read started.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BaselineFacts {
    /// `snapshot` (a copy this session saved), else what the session itself started from: `snapshot`, `bundle`, `base` or `recovery`.
    pub kind: String,
    pub key: String,
    pub head_sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
}

/// The stored copy, as the worker described it and the service found the object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyFacts {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    pub bundle_sha256: String,
    pub bundle_bytes: u64,
    pub head_sha: String,
    pub tree_sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<CopyMarker>,
    pub stable: bool,
    pub files: u64,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_write_at: Option<String>,
}

/// The calls of the old session id.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvokeFacts {
    pub tries: u32,
    /// The HTTP status of the last call that failed, when the error named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<u16>,
}

/// What the worker said of itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerFacts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_s: Option<u64>,
    pub agentd_version: String,
}

/// `runs/<run>/slices/<n>-recovery-result.json`: how the read of session `n`'s storage ended. Written once; a result never replaces
/// another, so a read that ends after `next-slice` closed it as abandoned changes nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryResult {
    pub schema_version: u32,
    pub run_id: String,
    pub session: u32,
    /// Absent on a result with no attempt: a read that was skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    /// [`STATE_COPIED`], [`STATE_MISSED`], [`STATE_SKIPPED`] or [`STATE_ABANDONED`].
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub cause: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_known_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopFacts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<BaselineFacts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy: Option<CopyFacts>,
    /// The worker's comparison of the stored copy with the baseline. It is evidence for the decision and no decision; absent when the
    /// worker made none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison: Option<RecoveryComparison>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invoke: Option<InvokeFacts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<WorkerFacts>,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
}

/// What the op answers: one line for the function's log. The machine drops it (`ResultPath: null`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoverAnswer {
    /// A state of [`RecoveryResult`], or [`NOT_NEEDED`] or [`DUPLICATE`].
    pub recovery: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl RecoverAnswer {
    fn word(recovery: &str) -> RecoverAnswer {
        RecoverAnswer {
            recovery: recovery.to_string(),
            reason: None,
        }
    }
}

/// The result of the session in `session.turn`, when the task ended with one (as `next-slice` reads it).
pub(crate) fn turn_complete(session: &SessionState) -> Option<TurnCompleteRequest> {
    session
        .turn
        .as_ref()
        .filter(|v| v.as_object().is_some_and(|o| !o.is_empty()))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// The HTTP status a runtime answered with, as AgentCore words it ("Received error (<status>) from runtime").
fn status_of(message: &str) -> Option<u16> {
    message
        .split("Received error (")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .and_then(|code| code.parse::<u16>().ok())
}

/// A reason word as a record keeps it: lowercase letters, digits and hyphens, at most 40 characters. Anything else the worker sent is
/// not kept.
fn reason_word(s: &str) -> Option<String> {
    let ok = !s.is_empty()
        && s.len() <= 40
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    ok.then(|| s.to_string())
}

/// `fut`, or `None` when it has not ended after `ms` milliseconds; the call is then dropped.
async fn within<F: Future>(cfg: &Config, ms: u64, fut: F) -> Option<F::Output> {
    tokio::select! {
        biased;
        out = fut => Some(out),
        () = cfg.wait_ms(ms) => None,
    }
}

/// The one clock of the op.
struct Limit<'a> {
    cfg: &'a Config,
    deadline_ms: u64,
}

impl Limit<'_> {
    fn left_ms(&self) -> u64 {
        self.deadline_ms.saturating_sub(self.cfg.now_ms())
    }
}

/// What the op knows while it works; [`Work::result`] turns it into the record.
struct Work {
    run_id: String,
    session: u32,
    attempt_id: Option<String>,
    cause: String,
    end_known_at_ms: Option<u64>,
    started_at_ms: u64,
    stop: Option<StopFacts>,
    baseline: Option<BaselineFacts>,
    invoke: Option<InvokeFacts>,
}

impl Work {
    fn result(&self, state: &str, reason: Option<&str>, now_ms: u64) -> RecoveryResult {
        RecoveryResult {
            schema_version: SCHEMA_VERSION,
            run_id: self.run_id.clone(),
            session: self.session,
            attempt_id: self.attempt_id.clone(),
            state: state.to_string(),
            reason: reason.map(str::to_string),
            cause: self.cause.clone(),
            end_known_at_ms: self.end_known_at_ms,
            stop: self.stop.clone(),
            baseline: self.baseline.clone(),
            copy: None,
            comparison: None,
            invoke: self.invoke.clone(),
            worker: None,
            started_at_ms: self.started_at_ms,
            ended_at_ms: now_ms,
        }
    }
}

/// Writes the result once and answers with the result that stands (this one, or the one `next-slice` wrote when it closed the attempt).
async fn finish<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    result: RecoveryResult,
) -> Res<RecoverAnswer>
where
    B: Blobs,
{
    let key = s3::recovery_result_key(&result.run_id, result.session);
    let stands = slices::write_once(&deps.blobs, &key, result).await?;
    println!(
        "{}",
        serde_json::json!({
            "function": "run-session", "op": "recover", "runId": stands.run_id, "session": stands.session,
            "recovery": stands.state, "reason": stands.reason, "cause": stands.cause,
            "tookMs": stands.ended_at_ms.saturating_sub(stands.started_at_ms),
        })
    );
    Ok(RecoverAnswer {
        recovery: stands.state,
        reason: stands.reason,
    })
}

/// The result record of session `n`, when one stands.
pub(crate) async fn read_result<B: Blobs>(
    blobs: &B,
    run_id: &str,
    n: u32,
) -> Res<Option<RecoveryResult>> {
    slices::read_record(blobs, &s3::recovery_result_key(run_id, n)).await
}

/// The attempt record of session `n`, when a read began.
pub(crate) async fn read_attempt<B: Blobs>(
    blobs: &B,
    run_id: &str,
    n: u32,
) -> Res<Option<RecoveryAttempt>> {
    slices::read_record(blobs, &s3::recovery_attempt_key(run_id, n)).await
}

/// The newest saved copy of the run as session `n` left it: the copy the session itself saved (the commit its bundle names; the record's
/// number only when the record names that commit), else what the session started from. The second value is the commit the session started
/// on and the third the run's base; `None` when the plan of the session names no commit to start from.
async fn baseline_of<B: Blobs>(
    blobs: &B,
    run_id: &str,
    n: u32,
) -> Res<Option<(BaselineFacts, String, String)>> {
    let Some(plan) = slices::read_plan(blobs, run_id, n).await? else {
        return Ok(None);
    };
    let Some(base_sha) = plan.base_sha.clone() else {
        return Ok(None);
    };
    // What the session started from: the read-back copy an earlier decision chose, else the plan's restore; the base bundle is at the base.
    let started = match &plan.recovery {
        Some(r) => Some(("recovery".to_string(), r.key.clone(), r.head.clone())),
        None => {
            let kind = match plan.restore.kind {
                RestoreKind::Base => "base",
                RestoreKind::Bundle => "bundle",
                RestoreKind::Snapshot => "snapshot",
            };
            plan.restore
                .head
                .clone()
                .or_else(|| (plan.restore.kind == RestoreKind::Base).then(|| base_sha.clone()))
                .map(|head| (kind.to_string(), plan.restore.key.clone(), head))
        }
    };
    let Some((kind, key, start_head)) = started else {
        return Ok(None);
    };
    if let Some(own) = slices::snapshot_of(blobs, run_id, n).await?
        && let Some(head) = own.head
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Record {
            head_sha: String,
            #[serde(default)]
            count: Option<u32>,
        }
        let count = match blobs.get(&s3::snapshot_json_key_t(run_id, n)).await? {
            Some(bytes) => serde_json::from_slice::<Record>(&bytes)
                .ok()
                .filter(|r| r.head_sha == head)
                .and_then(|r| r.count),
            None => None,
        };
        return Ok(Some((
            BaselineFacts {
                kind: "snapshot".to_string(),
                key: own.key,
                head_sha: head,
                count,
            },
            start_head,
            base_sha,
        )));
    }
    Ok(Some((
        BaselineFacts {
            kind,
            key,
            head_sha: start_head.clone(),
            count: None,
        },
        start_head,
        base_sha,
    )))
}

/// The op `recover`: reads the storage of the session that just ended without a result into one copy, within [`RECOVERY_LIMIT_S`].
pub async fn recover<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run_id: &str,
    session: &SessionState,
    prev: &LoopState,
) -> Res<RecoverAnswer>
where
    S: Store,
    T: Tokens,
    M: Minter,
    R: Sessions,
    B: Blobs,
    P: PackageTokens,
{
    let cfg = &deps.cfg;
    let started_at_ms = cfg.now_ms();
    let limit = Limit {
        cfg,
        deadline_ms: started_at_ms + RECOVERY_LIMIT_S * 1000,
    };
    let n = prev.slice.max(1);

    // 1. Nothing to read: the session ended with a result, was never invoked, or is not the session the run is in.
    if turn_complete(session).is_some() {
        return Ok(RecoverAnswer::word(NOT_NEEDED));
    }
    let invoked = deps
        .store
        .get_turn(run_id, DIRECT_AGENT_ID, n)
        .await?
        .is_some_and(|t| t.invoked_at.is_some());
    if !invoked {
        return Ok(RecoverAnswer::word(NOT_NEEDED));
    }
    let Some(agent) = deps.store.get_agent(run_id, DIRECT_AGENT_ID).await? else {
        return Ok(RecoverAnswer::word(NOT_NEEDED));
    };
    if agent.session_generation != Some(n) {
        return Ok(RecoverAnswer::word(NOT_NEEDED));
    }
    let run = deps
        .store
        .get_run(run_id)
        .await?
        .ok_or_else(|| RunSessionError::Invalid(format!("run {run_id} has no Run item")))?;

    // The cause, as `next-slice` will say it.
    let ended = session.ended();
    let cause = if ended.is_some() {
        SESSION_END_REQUESTED
    } else {
        let age = crate::plan_delivery::session_age_s(deps, run_id, n, started_at_ms).await?;
        if crate::plan_delivery::heartbeat_stopped(session, age) {
            CAUSE_NO_HEARTBEAT
        } else {
            CAUSE_NO_RESULT
        }
    };
    let end_known_at_ms = ended
        .as_ref()
        .and_then(|e| crate::run_ended::parse_rfc3339_ms(&e.at));
    let mut work = Work {
        run_id: run_id.to_string(),
        session: n,
        attempt_id: None,
        cause: cause.to_string(),
        end_known_at_ms,
        started_at_ms,
        stop: None,
        baseline: None,
        invoke: None,
    };

    // 2. No read for this session: its machine keeps no storage, the run is over, or the run switched the read off. A cancel that is
    // recorded does not skip the read: it forbids another engine session, not saving the work.
    let skip = if agent.compute.as_deref() == Some(crate::compute::Compute::Instances.word()) {
        Some("instances")
    } else if run.status != GoalStatus::Active {
        Some("run-ended")
    } else if agent.recover_after_stop == Some(false) {
        Some("off")
    } else {
        None
    };
    if let Some(reason) = skip {
        return finish(deps, work.result(STATE_SKIPPED, Some(reason), cfg.now_ms())).await;
    }

    // 3. One attempt per session: a second call finds the record and calls nothing.
    let attempt = RecoveryAttempt {
        schema_version: SCHEMA_VERSION,
        run_id: run_id.to_string(),
        session: n,
        attempt_id: dispatcher::ids::new_ulid(),
        runtime_session_id: agent.runtime_session_id.clone(),
        cause: cause.to_string(),
        end_known_at_ms,
        started_at_ms,
        deadline_ms: limit.deadline_ms,
    };
    let body = serde_json::to_vec_pretty(&attempt)
        .map_err(|e| RunSessionError::Invalid(format!("recovery attempt: {e}")))?;
    if !deps
        .blobs
        .put_if_absent(&s3::recovery_attempt_key(run_id, n), &body)
        .await?
    {
        return Ok(RecoverAnswer::word(DUPLICATE));
    }
    work.attempt_id = Some(attempt.attempt_id.clone());
    let miss = |work: &Work, reason: &str| work.result(STATE_MISSED, Some(reason), cfg.now_ms());

    // 4. The fence, before the stop: from here on the old worker's calls are refused, whatever its machine still does.
    if !deps.store.fence_agent(run_id, DIRECT_AGENT_ID, n).await? {
        return finish(deps, miss(&work, "no-session")).await;
    }

    // 5. On an end the worker gave notice of, the stop from outside is still in flight: wait until it has had its time.
    if let Some(known) = end_known_at_ms {
        let until = known + RECOVERY_SETTLE_S * 1000;
        let wait = until.saturating_sub(cfg.now_ms()).min(limit.left_ms());
        if wait > 0 {
            cfg.wait_ms(wait).await;
        }
    }

    // 6. The service's own stop. Only a definite answer (stopped, or already gone) lets the old session id be called.
    let stop_started = cfg.now_ms();
    let cap = (RECOVERY_STOP_CAP_S * 1000).min(limit.left_ms());
    let stopped = within(
        cfg,
        cap,
        deps.sessions.stop(
            &agent.runtime_arn,
            session_qualifier(agent.runtime_endpoint.as_deref()),
            &agent.runtime_session_id,
        ),
    )
    .await;
    let definite = match stopped {
        Some(Ok(stopped)) => Some(stopped),
        Some(Err(_)) | None => None,
    };
    work.stop = Some(StopFacts {
        started_at_ms: stop_started,
        took_ms: cfg.now_ms().saturating_sub(stop_started),
        stopped: definite,
    });
    if definite.is_none() {
        return finish(deps, miss(&work, "stop-failed")).await;
    }

    // 7. The baseline: the newest saved copy of the run, which the run goes on from when the copy read back is not used.
    let Some(found) = within(cfg, limit.left_ms(), baseline_of(&deps.blobs, run_id, n)).await
    else {
        return finish(deps, miss(&work, "timeout")).await;
    };
    let Some((baseline, start_head, base_sha)) = found? else {
        return finish(deps, miss(&work, "no-baseline")).await;
    };
    // The worker fetches the baseline's bundle only when the baseline is a copy the session saved: the commit a session started on is in
    // its own repository.
    let baseline_object = (baseline.head_sha != start_head).then(|| baseline.key.clone());
    work.baseline = Some(baseline.clone());

    // 8. The run, once more before anything is minted or called.
    let still_active = deps
        .store
        .get_run(run_id)
        .await?
        .is_some_and(|r| r.status == GoalStatus::Active);
    if !still_active {
        return finish(
            deps,
            work.result(STATE_SKIPPED, Some("run-ended"), cfg.now_ms()),
        )
        .await;
    }

    // 9. Credentials for one PUT and at most one GET, tagged as the recovery agent, for the least time STS gives.
    let bucket = cfg.bucket.as_str();
    let key = s3::recovery_bundle_key(run_id, n);
    let minted = match (
        agent.creds.as_ref(),
        dispatcher::policy::recovery_policy(bucket, &key, baseline_object.as_deref()),
        dispatcher::ids::role_session_name(run_id, RECOVERY_AGENT_ID),
    ) {
        (Some(binding), Ok(session_policy), Ok(role_session_name)) => {
            let plan = MintPlan {
                role_session_name,
                source_identity: RECOVERY_AGENT_ID.to_string(),
                tags: vec![
                    ("run".into(), run_id.to_string()),
                    ("agent".into(), RECOVERY_AGENT_ID.to_string()),
                    ("tenant".into(), binding.tenant.clone()),
                    ("mode".into(), binding.mode.clone()),
                    ("engine".into(), agent.engine.as_str().into()),
                ],
                session_policy,
                duration_seconds: RECOVERY_CREDENTIALS_S,
            };
            within(cfg, limit.left_ms(), deps.minter.mint(&plan))
                .await
                .and_then(Result::ok)
        }
        _ => None,
    };
    let Some(credentials) = minted else {
        return finish(deps, miss(&work, "mint-failed")).await;
    };

    // 10. The call of the old session id, again after an answer a second call can change, until the op's time is used.
    let mut invoke = InvokeFacts::default();
    let mut active = false;
    let answer: Result<RecoveryAnswer, &'static str> = loop {
        let left_s = limit.left_ms() / 1000;
        if left_s <= KEEP_FOR_THE_RESULT_S {
            break Err(if invoke.tries == 0 {
                "timeout"
            } else if active {
                "session-active"
            } else {
                "no-answer"
            });
        }
        let budget_s = u32::try_from(left_s - KEEP_FOR_THE_RESULT_S)
            .unwrap_or(RECOVERY_WORKER_BUDGET_S)
            .min(RECOVERY_WORKER_BUDGET_S);
        let req = RecoveryInvocationRequest {
            kind: RecoverKind::Recover,
            attempt_id: attempt.attempt_id.clone(),
            run_id: run_id.to_string(),
            session: n,
            runtime_session_id: agent.runtime_session_id.clone(),
            region: cfg.region.clone(),
            credentials: credentials.clone(),
            object: s3::uri(bucket, &key),
            base_sha: base_sha.clone(),
            start_head: start_head.clone(),
            baseline: RecoveryBaseline {
                head_sha: baseline.head_sha.clone(),
                count: baseline.count,
                object: baseline_object.as_deref().map(|k| s3::uri(bucket, k)),
            },
            budget_s,
        };
        if validate_recovery_invocation(&req).is_err() {
            break Err("bad-request");
        }
        let Ok(body) = serde_json::to_vec(&req) else {
            break Err("bad-request");
        };
        invoke.tries += 1;
        let called = within(
            cfg,
            limit.left_ms(),
            deps.sessions.invoke(
                &agent.runtime_arn,
                session_qualifier(agent.runtime_endpoint.as_deref()),
                &agent.runtime_session_id,
                &body,
            ),
        )
        .await;
        let pause_s = match called {
            // The call was still open when the op's time ended.
            None => break Err("timeout"),
            Some(Ok(bytes)) => match serde_json::from_slice::<RecoveryAnswer>(&bytes) {
                Ok(answer) => break Ok(answer),
                Err(_) => break Err("no-answer"),
            },
            Some(Err(e)) => {
                let status = status_of(&e.to_string());
                invoke.last_status = status;
                match status {
                    // A worker image that predates the read refuses the body: one miss, no second call.
                    Some(400) => break Err("worker-refused"),
                    // The stopped session's machine still answers: a turn runs there, or ran.
                    Some(409) => {
                        active = true;
                        RETRY_AFTER_ACTIVE_S
                    }
                    _ => {
                        active = false;
                        RETRY_AFTER_FAULT_S
                    }
                }
            }
        };
        cfg.wait_ms((pause_s * 1000).min(limit.left_ms())).await;
    };
    work.invoke = Some(invoke);
    let answer = match answer {
        Ok(answer) => answer,
        Err(reason) => return finish(deps, miss(&work, reason)).await,
    };

    // 11. The answer is checked against the object: the service uses nothing it did not find in the bucket.
    if answer.attempt_id != attempt.attempt_id || answer.run_id != run_id || answer.session != n {
        return finish(deps, miss(&work, "not-whole")).await;
    }
    let worker = Some(WorkerFacts {
        uptime_s: answer.uptime_s,
        agentd_version: answer.agentd_version.chars().take(40).collect(),
    });
    let Some(copy) = answer.copy.as_ref().filter(|_| answer.recovered) else {
        let reason = answer
            .reason
            .as_deref()
            .and_then(reason_word)
            .unwrap_or_else(|| "no-answer".to_string());
        let mut result = miss(&work, &reason);
        result.worker = worker;
        return finish(deps, result).await;
    };
    let whole = async {
        let facts = deps.blobs.head_version(&key, None).await?;
        let checksum_ok = facts
            .and_then(|f| f.sha256_base64)
            .is_some_and(|sum| sum == copy.bundle_sha256);
        if !checksum_ok {
            return Ok::<bool, RunSessionError>(false);
        }
        let header = deps
            .blobs
            .get_prefix(&key, ham_core::snapshot::HEADER_BYTES)
            .await?;
        Ok(header
            .and_then(|h| ham_core::snapshot::bundle_head(&h).ok())
            .is_some_and(|head| head == copy.head_sha))
    };
    let whole = match within(cfg, limit.left_ms(), whole).await {
        Some(Ok(whole)) => whole,
        // The object could not be read in the op's time: nothing is said about it here, and `next-slice` looks for it once more.
        Some(Err(_)) | None => {
            let mut result = miss(&work, "timeout");
            result.worker = worker;
            return finish(deps, result).await;
        }
    };
    let mut result = if whole {
        work.result(STATE_COPIED, None, cfg.now_ms())
    } else {
        miss(&work, "not-whole")
    };
    result.worker = worker;
    if whole {
        result.copy = Some(CopyFacts {
            key: key.clone(),
            version_id: copy.version_id.clone(),
            bundle_sha256: copy.bundle_sha256.clone(),
            bundle_bytes: copy.bundle_bytes,
            head_sha: copy.head_sha.clone(),
            tree_sha: copy.tree_sha.clone(),
            parent_sha: copy.parent_sha.clone(),
            marker: copy.marker.clone(),
            stable: copy.stable,
            files: copy.files,
            bytes: copy.bytes,
            newest_write_at: copy.newest_write_at.clone(),
        });
        result.comparison = answer.comparison.clone();
    }

    // 12. The result, written once.
    finish(deps, result).await
}
