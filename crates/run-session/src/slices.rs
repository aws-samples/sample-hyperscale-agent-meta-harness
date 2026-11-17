//! A run that outlives one microVM session.
//!
//! AgentCore ends a microVM session after 28,800 s. A run is therefore a list of slices: slice n is turn n of the one direct agent (its own
//! Turn item, its own `turns/n/` prefix in the bucket, its own runtime session id). The state machine owns the loop; this module is what the
//! `next-slice` op decides after each slice and what `start-session` obeys when it starts the next.
//!
//! Records, both written once (create-only) under `runs/<run>/slices/`, so a repeated call returns what the first call decided:
//!
//! * `<n>.json` ([`SlicePlan`]): what slice n restores, how many seconds its engine may run and whether it is the last.
//! * `<n>-end.json` ([`SliceEnd`]): how slice n ended and what was decided: go on with slice n+1, or deliver and end the run.
//!
//! A slice that was lost is also told to the run's events ([`announce_lost`]): one `run.waiting` event of the harness that names
//! the session, why it was lost and what follows, so a client can say it while the run goes on.
//!
//! A cancel that sticks. The user's keep-the-work cancel is one attribute of the Run item (`cancelRequestedAt`, written by control-api).
//! `next-slice` reads the Run item again right before its decision, after everything that takes time, and with the record set it starts
//! no further session: a verdict to go on, and every end the loop would count itself (a lost session, a failed restore, no progress, no time
//! left), becomes "deliver" with the reason [`EndReason::Cancelled`]. An end the engine made and the run's total time keep their own reason.
//! `start-session` refuses a slice that was not invoked yet, so a cancel that lands after a stored decision to go on still starts nothing:
//! the machine's Catch leads back to `next-slice` for that slice, whose decision is made fresh. `plan-delivery` reads the record too.
//!
//! What the loop guarantees: the service writes the plan of slice n+1 only after it stopped the session of slice n and read how slice n ended;
//! the Agent item moves to slice n+1 (new session id, new refresh nonce) before the new session is invoked, so the old session can neither refresh
//! its credentials nor pass worker-api's generation check; work that was in the tree at the deadline is committed and bundled by agentd before it
//! says `handoff`; work since the last periodic snapshot of a session that vanished is lost, which is the recovery interval of the snapshot.

use crate::error::RunSessionError;
use crate::finalize::stop_agent_machine;
use crate::ops::{Deps, SessionState};
use crate::ports::{Blobs, PackageTokens, Res, Sessions, Store, Tokens};
use crate::prepare::read_request;
use crate::recover::{self, RecoveryResult, STATE_ABANDONED, STATE_COPIED, STATE_SKIPPED};
use ham_core::lost_session::{
    CAUSE_END_REQUESTED, CAUSE_NO_HEARTBEAT, CAUSE_NO_RESULT, CAUSE_RESTORE_FAILED, FROM_COPY,
    FROM_RECOVERED, FROM_START, KIND, LostRecovery, SessionLost, THEN_END, THEN_RESTART,
};
use harness_protocol::api::internal::TurnCompleteRequest;
use harness_protocol::common::is_utc_timestamp;
use harness_protocol::direct::{
    DEFAULT_MAX_RUN_S, DIRECT_AGENT_ID, DirectOutcome, DirectSummary, MAX_DIRECT_TIMEOUT_S,
    MAX_EMPTY_SLICES, MAX_LOST_SLICES, MAX_SLICE_BRIEF_BYTES, MAX_SLICES, MIN_SLICE_ENGINE_S,
    OLD_WORKER_TAIL_S, SLICE_RESERVE_S, SessionEnded, max_run_s_ok, s3,
};
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::{AgentItem, RecoveredCopyRef};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use worker_api::handler::Minter;

/// Why a run left the slice loop. `finalize` turns it into the status, the reason and the resume mode of the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// The run used the total time it was given: paused.
    TotalTime,
    /// Sessions in a row stopped answering (or could not start): stalled.
    SessionLost,
    /// Sessions in a row could not restore the earlier slice's work: stalled.
    RestoreFailed,
    /// Sessions in a row ran to their deadline and left the work as they found it: stalled.
    NoProgress,
    /// The run was ended by someone else (discarded) while the loop ran; `finalize` keeps its status.
    RunEnded,
    /// The user's keep-the-work cancel is recorded on the Run item, and the slice ended without a session that goes on. No further
    /// session is started and what exists is delivered; `finalize` ends such a run `complete`.
    Cancelled,
}

impl EndReason {
    /// The wire word, which is also the code in front of the run's `statusReason`.
    pub fn code(self) -> &'static str {
        match self {
            Self::TotalTime => "total_time",
            Self::SessionLost => "session_lost",
            Self::RestoreFailed => "restore_failed",
            Self::NoProgress => "no_progress",
            Self::RunEnded => "run_ended",
            Self::Cancelled => "cancelled",
        }
    }
}

fn one() -> u32 {
    1
}

/// `$.loop` of the state machine: which slice the machine is in and what the loop has counted. `Init` sets `{"slice": 1}`; `next-slice` answers
/// the new value, and the machine goes back to `StartSession` when `continue` is true.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopState {
    #[serde(default = "one")]
    pub slice: u32,
    /// Slices in a row that were lost.
    #[serde(default)]
    pub lost: u32,
    /// Slices in a row that ran to their deadline and changed nothing.
    #[serde(default)]
    pub empty: u32,
    #[serde(default, rename = "continue")]
    pub cont: bool,
    /// Always written (null when the loop has no reason of its own): the machine's ResultSelector reads the path.
    #[serde(default)]
    pub end_reason: Option<EndReason>,
}

impl Default for LoopState {
    fn default() -> Self {
        LoopState {
            slice: 1,
            lost: 0,
            empty: 0,
            cont: false,
            end_reason: None,
        }
    }
}

/// Where a slice's work comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreKind {
    Base,
    /// The final bundle of an earlier slice.
    Bundle,
    /// The last periodic snapshot of an earlier slice.
    Snapshot,
}

/// The object a slice restores and the commit it must be at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Restore {
    pub kind: RestoreKind,
    /// The key in the harness bucket.
    pub key: String,
    /// The commit the restored repository must be at; absent for [`RestoreKind::Base`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

/// `runs/<run>/slices/<n>.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlicePlan {
    pub slice: u32,
    /// The slice's prompt deadline in seconds (`timeoutS` of the invocation).
    pub engine_s: u32,
    /// No slice follows this one.
    pub last: bool,
    pub restore: Restore,
    /// The copy the service read back from the storage of the session before and chose to start this session from, when it chose
    /// one. `restore` stays what the session would restore without it (the saved copy), so a reader that does not know this member starts
    /// from the saved copy. `start-session` takes this copy when the member is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<PlanRecovery>,
    /// The run's original base commit (`staged.baseSha`), when the stage reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// Engine sessions that earlier slices used (the slices before this one).
    pub sessions_before: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief: Option<String>,
    pub planned_at_ms: u64,
}

/// The read-back copy a plan names: its key in the harness bucket and the commit the restored repository must be at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanRecovery {
    pub key: String,
    pub head: String,
}

/// How a slice ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndKind {
    /// Cline (or the harness) ended the run: delivered as it is.
    Done,
    /// The last slice reached its deadline: the run used its total time.
    Deadline,
    /// A slice that was not the last reached its deadline and said so.
    Handoff,
    /// No result: the heartbeat stopped, the state timed out, the start failed.
    Lost,
    /// The slice's agentd could not restore the earlier slice's work.
    RestoreFailed,
}

/// `runs/<run>/slices/<n>-end.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SliceEnd {
    pub slice: u32,
    pub kind: EndKind,
    #[serde(rename = "continue")]
    pub cont: bool,
    pub lost: u32,
    pub empty: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<EndReason>,
    /// The commit the slice ended on, when it reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    pub decided_at_ms: u64,
    /// Why a lost slice was lost (`no-heartbeat` or `no-result`), kept with the decision so that the event that announces it says
    /// the same however often it is written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lost_cause: Option<String>,
    /// The last heartbeat worker-api had recorded for a lost slice when the decision was made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_at: Option<String>,
    /// When the service took the worker's notice that the session is being ended, for a slice that ended with one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// What was decided about the copy read back from this slice's storage. Absent when no read concerned the slice (it ended with
    /// a result, or it never ran on a machine).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RecoveryDecision>,
    /// The time of the user's cancel, when the loop ended for it ([`EndReason::Cancelled`]); kept with the decision, so the event
    /// that announces a lost slice says the same however often it is written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_requested_at: Option<String>,
}

/// What `next-slice` decided about the copy read back from a lost slice's storage. The decision is made once, with the end record.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryDecision {
    /// A read of the storage began (the service wrote an attempt record).
    pub tried: bool,
    /// The run goes on from the copy read back: the next slice starts from it, or the run delivers it when it ends here.
    pub used: bool,
    /// Why the copy read back is not used, as one word, when a word says it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The key of the read-back object, when one is in the bucket. It stays there whether it is used or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The commit of the copy read back, when the worker stored one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_write_at: Option<String>,
    /// The files of which the saved copy holds more than the storage did, when there are any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regressed: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regressed_path: Option<String>,
}

/// What `next-slice` finds when it decides: the read-back object and the run's newest saved copy as they are at that moment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadNow {
    /// The key of the read-back object, when the bucket holds it.
    pub key: Option<String>,
    /// The commit the first bytes of that object name, when they name one.
    pub object_head: Option<String>,
    /// The commit of the run's newest saved copy now (the baseline, read again after the stopped machine had its last word).
    pub baseline_head: Option<String>,
}

/// The use rule. The copy read back is used only when nothing leaves a doubt: the worker stored it and the object is whole; it has a
/// parent (a tree without readable history is kept and never started from); two passes over the tree agreed; the comparison with the
/// saved copy was made; the tree differs from the saved copy's; the storage's marker names exactly the saved copy's commit, and its number
/// when both are known; no file of the saved copy holds more than the storage did; no file of the storage is the version below the saved
/// copy's; the saved copy has not moved since the read; and the object still names the commit. Anything else is doubt, and the run goes on
/// from the saved copy. Lineage is never judged by a time stamp. The result's own miss or skip keeps its reason word; a doubt for which no
/// word is settled has none.
pub fn use_recovery(result: &RecoveryResult, now: &ReadNow) -> RecoveryDecision {
    let copy = result.copy.as_ref();
    let mut d = RecoveryDecision {
        tried: result.attempt_id.is_some(),
        used: false,
        reason: None,
        key: now.key.clone(),
        head: copy.map(|c| c.head_sha.clone()),
        newest_write_at: copy.and_then(|c| c.newest_write_at.clone()),
        regressed: None,
        regressed_path: None,
    };
    let no = |mut d: RecoveryDecision, reason: Option<&str>| {
        d.reason = reason.map(str::to_string);
        d
    };
    if result.state != STATE_COPIED {
        let reason = result
            .reason
            .clone()
            .or_else(|| (result.state == STATE_ABANDONED).then(|| STATE_ABANDONED.to_string()));
        d.reason = reason;
        return d;
    }
    let (Some(copy), Some(baseline)) = (copy, result.baseline.as_ref()) else {
        return no(d, Some("not-whole"));
    };
    if now.key.is_none() {
        return no(d, Some("not-whole"));
    }
    // A saved copy that landed after the read wins, whatever the read found: the worker compared with the copy before it.
    if now.baseline_head.as_deref() != Some(baseline.head_sha.as_str()) {
        return no(d, Some("superseded"));
    }
    // No word is settled for a copy without a parent: it is kept, and no slice starts from it.
    if copy.parent_sha.is_none() {
        return no(d, None);
    }
    if !copy.stable {
        return no(d, Some("unstable"));
    }
    // No word is settled for a comparison that was not made: an unknown comparison is never read as "nothing is older".
    let Some(cmp) = result.comparison.as_ref().filter(|c| c.done()) else {
        return no(d, None);
    };
    if cmp.same_as_baseline {
        return no(d, Some("nothing-newer"));
    }
    let marker_ok = copy.marker.as_ref().is_some_and(|m| {
        m.head_sha == baseline.head_sha && baseline.count.is_none_or(|count| count == m.count)
    });
    if !marker_ok {
        return no(d, Some("storage-older"));
    }
    if cmp.regressed > 0 {
        d.regressed = Some(cmp.regressed);
        d.regressed_path = cmp.regressed_paths.first().cloned();
        return no(d, Some("older-file"));
    }
    // No word is settled for a file that is the version below the saved copy's: it is doubt, and the saved copy wins.
    if cmp.rewritten > 0 {
        return no(d, None);
    }
    if now.object_head.as_deref() != Some(copy.head_sha.as_str()) {
        return no(d, Some("not-whole"));
    }
    d.used = true;
    d
}

/// Whether the read of slice `n`'s storage proves that the slice restored the read-back copy its plan named and went to work on
/// it: the storage's marker, which the worker writes right after its restore check passed and before the engine starts, names this
/// slice and that commit.
fn restore_proven(result: &RecoveryResult, n: u32, planned: Option<&PlanRecovery>) -> bool {
    let marker = result.copy.as_ref().and_then(|c| c.marker.as_ref());
    match (marker, planned) {
        (Some(m), Some(p)) => m.session == n && m.head_sha == p.head,
        _ => false,
    }
}

/// The result of the read of slice `n`'s storage, once the stopped machine has had its last word. A copy the old machine was
/// uploading when it was stopped can still land for about a minute, and it can hold more of a file that was open than the storage does,
/// so nothing is decided before [`OLD_WORKER_TAIL_S`] after the service first knew that the session was ending: the worker's notice, else
/// the start of the service's own stop. The wait is the same whether the read looks usable or not. An attempt that left no result is
/// closed as abandoned first, so a result that arrives late is never used. No wait for a read that was skipped (the run switched it off,
/// its machine keeps no storage, the run is over) and for a slice that never ran on a machine.
async fn await_read<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run_id: &str,
    n: u32,
    agent: Option<&AgentItem>,
    ended: Option<&SessionEnded>,
    stop_began_ms: u64,
) -> Res<Option<RecoveryResult>>
where
    S: Store,
    B: Blobs,
{
    let cfg = &deps.cfg;
    let mut result = recover::read_result(&deps.blobs, run_id, n).await?;
    if result.as_ref().is_some_and(|r| r.state == STATE_SKIPPED) {
        return Ok(result);
    }
    let attempt = recover::read_attempt(&deps.blobs, run_id, n).await?;
    if attempt.is_none() && result.is_none() {
        // No read was made (an execution of the machine from before the read, or a read that failed before its first record). The wait
        // still applies to a session that ran on a machine with storage of its own, in a run that reads storage back.
        let reads_back = agent.is_some_and(|a| {
            a.recover_after_stop != Some(false)
                && a.session_generation == Some(n)
                && a.compute.as_deref() != Some(crate::compute::Compute::Instances.word())
        });
        let invoked = deps
            .store
            .get_turn(run_id, DIRECT_AGENT_ID, n)
            .await?
            .is_some_and(|t| t.invoked_at.is_some());
        if !(reads_back && invoked) {
            return Ok(None);
        }
    }
    if let (Some(a), None) = (&attempt, &result) {
        result = Some(close_abandoned(deps, run_id, n, a).await?);
    }
    let known_ms = attempt
        .as_ref()
        .and_then(|a| a.end_known_at_ms)
        .or_else(|| {
            result
                .as_ref()
                .and_then(|r| r.stop.as_ref())
                .map(|s| s.started_at_ms)
        })
        .or_else(|| attempt.as_ref().map(|a| a.started_at_ms))
        .or_else(|| ended.and_then(|e| crate::run_ended::parse_rfc3339_ms(&e.at)))
        .unwrap_or(stop_began_ms);
    let wait = (known_ms + OLD_WORKER_TAIL_S * 1000).saturating_sub(cfg.now_ms());
    if wait > 0 {
        cfg.wait_ms(wait).await;
    }
    // The state `Recover` can end on its Catch while the function has not written its first record yet (an error of the invoke, a
    // slow start): the read then goes on beside this op. So when no record was found before the wait, the records are read once more
    // after it. A result that landed is the read's. An attempt without one is closed as abandoned, so a result that arrives later
    // is never used, and the decision then looks for the object and names a copy the read stored.
    if result.is_none() {
        result = recover::read_result(&deps.blobs, run_id, n).await?;
        if result.is_none()
            && let Some(a) = recover::read_attempt(&deps.blobs, run_id, n).await?
        {
            result = Some(close_abandoned(deps, run_id, n, &a).await?);
        }
    }
    Ok(result)
}

/// Closes the read of slice `n`'s storage that began with attempt `a` and left no result: the result `abandoned` is written
/// once, so whatever the read writes later is not used. Returns the result that stands, which is the read's own when it landed first.
async fn close_abandoned<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run_id: &str,
    n: u32,
    a: &recover::RecoveryAttempt,
) -> Res<RecoveryResult>
where
    B: Blobs,
{
    let closed = RecoveryResult {
        schema_version: recover::SCHEMA_VERSION,
        run_id: run_id.to_string(),
        session: n,
        attempt_id: Some(a.attempt_id.clone()),
        state: STATE_ABANDONED.to_string(),
        reason: Some(STATE_ABANDONED.to_string()),
        cause: a.cause.clone(),
        end_known_at_ms: a.end_known_at_ms,
        stop: None,
        baseline: None,
        copy: None,
        comparison: None,
        invoke: None,
        worker: None,
        started_at_ms: a.started_at_ms,
        ended_at_ms: deps.cfg.now_ms(),
    };
    write_once(&deps.blobs, &s3::recovery_result_key(run_id, n), closed).await
}

/// The decision about the copy read back from slice `n`, against the object and the saved copy as they are now. A read of the
/// object that fails says nothing about it: the object is then neither named nor used, and the run goes on from the saved copy.
async fn decide_read<B: Blobs>(
    blobs: &B,
    run_id: &str,
    n: u32,
    result: &RecoveryResult,
    baseline_now: Option<&str>,
) -> RecoveryDecision {
    let key = s3::recovery_bundle_key(run_id, n);
    let exists = blobs.exists(&key).await.unwrap_or(false);
    let object_head = if exists {
        blobs
            .get_prefix(&key, ham_core::snapshot::HEADER_BYTES)
            .await
            .ok()
            .flatten()
            .and_then(|h| ham_core::snapshot::bundle_head(&h).ok())
    } else {
        None
    };
    use_recovery(
        result,
        &ReadNow {
            key: exists.then_some(key),
            object_head,
            baseline_head: baseline_now.map(str::to_string),
        },
    )
}

impl SliceEnd {
    /// The loop state the machine continues with.
    pub fn state(&self) -> LoopState {
        LoopState {
            slice: if self.cont {
                self.slice + 1
            } else {
                self.slice
            },
            lost: self.lost,
            empty: self.empty,
            cont: self.cont,
            end_reason: self.end_reason,
        }
    }
}

/// How long the next slice may run and whether it is the last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    pub engine_s: u32,
    pub last: bool,
}

/// The seconds slice n+1 may run, given the run's total, the seconds already used and the longest a slice may be. `None`: not enough is left for
/// a slice worth starting. `cap_s` is the microVM's limit less the reserve (or a test's shorter limit); the least worth starting is
/// [`MIN_SLICE_ENGINE_S`], or `cap_s` when that is shorter. The reserve is counted as spent by the end of every slice, so a slice
/// after this one needs [`SLICE_RESERVE_S`] and the least engine time left once this one has ended.
pub fn plan_timing(max_run_s: u32, elapsed_s: u64, cap_s: u32) -> Option<Timing> {
    let reserve = u64::from(SLICE_RESERVE_S);
    let floor = u64::from(MIN_SLICE_ENGINE_S.min(cap_s));
    let remaining = u64::from(max_run_s).saturating_sub(elapsed_s);
    let engine = remaining.saturating_sub(reserve).min(u64::from(cap_s));
    if engine < floor || engine == 0 {
        return None;
    }
    let after = remaining.saturating_sub(engine).saturating_sub(reserve);
    Some(Timing {
        engine_s: u32::try_from(engine).ok()?,
        last: after < reserve + floor,
    })
}

/// The longest a slice's prompt may run in this deployment: the microVM's lifetime less the reserve, at most the protocol maximum, and the
/// stage's shorter test limit when it sets one (`HAMH_SLICE_DEADLINE_S`).
pub fn slice_cap_s(max_lifetime_s: u64, test_limit_s: Option<u32>) -> u32 {
    let cap = crate::start_session::session_timeout_s(max_lifetime_s).min(MAX_DIRECT_TIMEOUT_S);
    match test_limit_s {
        Some(s) if s > 0 => cap.min(s),
        _ => cap,
    }
}

/// What the machine learned about how slice n ended.
pub fn classify(
    turn: Option<&TurnCompleteRequest>,
    outcome: Option<DirectOutcome>,
    slice: u32,
) -> EndKind {
    let Some(t) = turn else {
        return EndKind::Lost;
    };
    let outcome = outcome.or_else(|| outcome_of_turn(t));
    match outcome {
        Some(DirectOutcome::StoppedSetupFailed) if slice > 1 => EndKind::RestoreFailed,
        Some(DirectOutcome::StoppedTimeout) if t.handoff == Some(true) => EndKind::Handoff,
        Some(DirectOutcome::StoppedTimeout) => EndKind::Deadline,
        _ if t.handoff == Some(true) => EndKind::Handoff,
        _ => EndKind::Done,
    }
}

/// The direct outcome in the telemetry word of a turn-complete (`finishReason`), when summary.json is not there to say.
fn outcome_of_turn(t: &TurnCompleteRequest) -> Option<DirectOutcome> {
    DirectOutcome::ALL
        .into_iter()
        .find(|o| o.as_str() == t.finish_reason)
}

/// What the loop decides after slice n, before the timing and the restore are known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Start another slice.
    Next { lost: u32, empty: u32 },
    /// Deliver and end the run for this reason (`None`: Cline's own end).
    Deliver {
        lost: u32,
        empty: u32,
        reason: Option<EndReason>,
    },
}

/// The pure rule of the loop. `progress` is whether the slice left the work different from the one it restored (a head that differs).
pub fn decide(prev: &LoopState, kind: EndKind, progress: bool, time_left: bool) -> Verdict {
    let (mut lost, mut empty) = (prev.lost, prev.empty);
    match kind {
        EndKind::Done => Verdict::Deliver {
            lost: 0,
            empty: 0,
            reason: None,
        },
        EndKind::Deadline => Verdict::Deliver {
            lost: 0,
            empty: 0,
            reason: Some(EndReason::TotalTime),
        },
        EndKind::Handoff => {
            lost = 0;
            empty = if progress { 0 } else { empty + 1 };
            if empty >= MAX_EMPTY_SLICES {
                Verdict::Deliver {
                    lost,
                    empty,
                    reason: Some(EndReason::NoProgress),
                }
            } else if !time_left {
                Verdict::Deliver {
                    lost,
                    empty,
                    reason: Some(EndReason::TotalTime),
                }
            } else {
                Verdict::Next { lost, empty }
            }
        }
        EndKind::Lost | EndKind::RestoreFailed => {
            lost += 1;
            let reason = if kind == EndKind::RestoreFailed {
                EndReason::RestoreFailed
            } else {
                EndReason::SessionLost
            };
            if lost >= MAX_LOST_SLICES || !time_left {
                Verdict::Deliver {
                    lost,
                    empty,
                    reason: Some(reason),
                }
            } else {
                Verdict::Next { lost, empty }
            }
        }
    }
}

fn sha_ok(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn invalid(what: &str, e: impl std::fmt::Display) -> RunSessionError {
    RunSessionError::Invalid(format!("{what}: {e}"))
}

/// The record at `key`, when it exists and reads as `T`.
pub(crate) async fn read_record<B: Blobs, T: for<'de> Deserialize<'de>>(
    blobs: &B,
    key: &str,
) -> Res<Option<T>> {
    match blobs.get(key).await? {
        Some(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| invalid(key, e)),
        None => Ok(None),
    }
}

/// Writes `record` at `key` once and returns the record that stands: this one, or the one an earlier attempt wrote.
pub(crate) async fn write_once<B: Blobs, T: Serialize + for<'de> Deserialize<'de>>(
    blobs: &B,
    key: &str,
    record: T,
) -> Res<T> {
    let body = serde_json::to_vec_pretty(&record).map_err(|e| invalid(key, e))?;
    if blobs.put_if_absent(key, &body).await? {
        return Ok(record);
    }
    read_record(blobs, key).await?.ok_or_else(|| {
        RunSessionError::Unavailable(format!("{key} was written and is not readable"))
    })
}

pub async fn read_plan<B: Blobs>(blobs: &B, run_id: &str, n: u32) -> Res<Option<SlicePlan>> {
    read_record(blobs, &s3::slice_record_key(run_id, n)).await
}

pub(crate) async fn read_end<B: Blobs>(blobs: &B, run_id: &str, n: u32) -> Res<Option<SliceEnd>> {
    read_record(blobs, &end_key(run_id, n)).await
}

fn end_key(run_id: &str, n: u32) -> String {
    format!("runs/{run_id}/slices/{n}-end.json")
}

/// The run's total seconds: the request's, when it names a usable one.
pub fn total_s(requested: Option<u32>) -> u32 {
    requested
        .filter(|s| max_run_s_ok(*s))
        .unwrap_or(DEFAULT_MAX_RUN_S)
}

/// A retry reads the plan back.
pub async fn plan_first<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run_id: &str,
    max_run_s: Option<u32>,
    staged: &Value,
) -> Res<SlicePlan>
where
    B: Blobs,
{
    let cap = slice_cap_s(deps.cfg.max_lifetime_s, deps.cfg.slice_deadline_s);
    let timing = plan_timing(total_s(max_run_s), 0, cap).unwrap_or(Timing {
        engine_s: cap,
        last: true,
    });
    let plan = SlicePlan {
        slice: 1,
        engine_s: timing.engine_s,
        last: timing.last,
        restore: Restore {
            kind: RestoreKind::Base,
            key: s3::repo_bundle_key(run_id),
            head: None,
        },
        recovery: None,
        base_sha: staged
            .get("baseSha")
            .and_then(Value::as_str)
            .filter(|s| sha_ok(s))
            .map(str::to_string),
        sessions_before: 0,
        brief: None,
        planned_at_ms: deps.cfg.now_ms(),
    };
    write_once(&deps.blobs, &s3::slice_record_key(run_id, 1), plan).await
}

/// The brief a slice hands the next: the tail of Cline's last message that fits [`MAX_SLICE_BRIEF_BYTES`], cut at a character; `None` for a blank message.
pub fn brief_of(message: &str) -> Option<String> {
    let text = message.trim();
    if text.is_empty() {
        return None;
    }
    let mut start = text.len().saturating_sub(MAX_SLICE_BRIEF_BYTES);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    Some(text[start..].trim_start().to_string())
}

/// The last snapshot a slice saved: the commit its bundle names; the record's commit only for a bundle whose header cannot be read.
pub(crate) async fn snapshot_of<B: Blobs>(blobs: &B, run_id: &str, n: u32) -> Res<Option<Restore>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Json {
        head_sha: String,
    }
    let record = match blobs.get(&s3::snapshot_json_key_t(run_id, n)).await? {
        Some(bytes) => serde_json::from_slice::<Json>(&bytes)
            .ok()
            .map(|snap| snap.head_sha)
            .filter(|head| sha_ok(head)),
        None => None,
    };
    // The commit the bundle itself names. A session stopped between its two uploads leaves the record of the copy before, and a
    // repository restored from the bundle is never at that commit.
    let head = match crate::plan_delivery::bundle_says(blobs, run_id, n).await? {
        crate::plan_delivery::BundleSays::Nothing => None,
        crate::plan_delivery::BundleSays::Commit(head) => Some(head),
        crate::plan_delivery::BundleSays::Unread => record,
    };
    Ok(head.map(|head| Restore {
        kind: RestoreKind::Snapshot,
        key: s3::snapshot_bundle_key_t(run_id, n),
        head: Some(head),
    }))
}

/// The final bundle slice n reported, when it is the slice's own key and exists.
async fn bundle_of<B: Blobs>(
    blobs: &B,
    bucket: &str,
    run_id: &str,
    n: u32,
    turn: &TurnCompleteRequest,
) -> Res<Option<Restore>> {
    let key = s3::bundle_key_t(run_id, n);
    let (Some(uri), Some(sha)) = (turn.bundle_uri.as_deref(), turn.last_commit_sha.as_deref())
    else {
        return Ok(None);
    };
    if uri == s3::uri(bucket, &key) && sha_ok(sha) && blobs.exists(&key).await? {
        Ok(Some(Restore {
            kind: RestoreKind::Bundle,
            key,
            head: Some(sha.to_string()),
        }))
    } else {
        Ok(None)
    }
}

/// The best head slice n left, whatever way it ended: its final bundle, else its last snapshot. `None`: nothing of its own.
pub async fn best_head<B: Blobs>(
    blobs: &B,
    bucket: &str,
    run_id: &str,
    n: u32,
    turn: Option<&TurnCompleteRequest>,
) -> Res<Option<Restore>> {
    if let Some(t) = turn
        && let Some(r) = bundle_of(blobs, bucket, run_id, n, t).await?
    {
        return Ok(Some(r));
    }
    snapshot_of(blobs, run_id, n).await
}

/// The summary slice n wrote, when it is this run's and well formed.
async fn summary_of<B: Blobs>(blobs: &B, run_id: &str, n: u32) -> Res<Option<DirectSummary>> {
    let Some(bytes) = blobs.get(&s3::summary_key_t(run_id, n)).await? else {
        return Ok(None);
    };
    Ok(serde_json::from_slice::<DirectSummary>(&bytes)
        .ok()
        .filter(|s| s.validate().is_ok() && s.run_id == run_id))
}

/// The op `next-slice`: how did slice n end, and does the run go on?
pub async fn next_slice<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run_id: &str,
    staged: &Value,
    session: &SessionState,
    prev: &LoopState,
) -> Res<LoopState>
where
    S: Store,
    T: Tokens,
    M: Minter,
    R: Sessions,
    B: Blobs,
    P: PackageTokens,
{
    let n = prev.slice.max(1);
    // 1. a decision is made once; its word to the run's events is written again, the same
    if let Some(end) = read_end(&deps.blobs, run_id, n).await? {
        note_recovered(deps, run_id, &end).await;
        tell_lost(deps, run_id, &end).await;
        return Ok(end.state());
    }
    let now = deps.cfg.now_ms();
    let run = deps
        .store
        .get_run(run_id)
        .await?
        .ok_or_else(|| RunSessionError::Invalid(format!("run {run_id} has no Run item")))?;
    let plan = read_plan(&deps.blobs, run_id, n).await?;
    let turn: Option<TurnCompleteRequest> = session
        .turn
        .as_ref()
        .filter(|v| v.as_object().is_some_and(|o| !o.is_empty()))
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let summary = if turn.is_some() {
        summary_of(&deps.blobs, run_id, n).await?
    } else {
        None
    };
    let kind = classify(turn.as_ref(), summary.as_ref().map(|s| s.outcome), n);

    // 2. the slice's machine is stopped before anything is decided from what it left: nothing of this slice writes after the decision
    let mut last_heartbeat_at = None;
    let agent = deps.store.get_agent(run_id, DIRECT_AGENT_ID).await?;
    if let Some(agent) = &agent {
        // The last heartbeat of this slice's session, while the Agent item still names it.
        if kind == EndKind::Lost && agent.session_generation.is_none_or(|g| g == n) {
            last_heartbeat_at = agent.heartbeat_at.clone().filter(|t| is_utc_timestamp(t));
        }
        stop_agent_machine(deps, agent).await?;
    }
    // Why a lost slice was lost: its worker gave notice that the session is being ended, its heartbeat stopped, or it ended
    // without a result for another reason. A notice is no proof that the machine stopped, which is why the cause is not called a stop.
    let ended = session.ended().filter(|_| kind == EndKind::Lost);
    let mut lost_cause = if kind == EndKind::Lost {
        let cause = if ended.is_some() {
            CAUSE_END_REQUESTED
        } else {
            let age = crate::plan_delivery::session_age_s(deps, run_id, n, now).await?;
            if crate::plan_delivery::heartbeat_stopped(session, age) {
                CAUSE_NO_HEARTBEAT
            } else {
                CAUSE_NO_RESULT
            }
        };
        Some(cause.to_string())
    } else {
        None
    };
    let ended_at = ended
        .as_ref()
        .map(|e| e.at.clone())
        .filter(|t| is_utc_timestamp(t));
    // What came of the read of this slice's storage, after the stopped machine had its last word. The clock is read again: the
    // decision, the time the run has used and the plan of the next slice are of the moment after the wait.
    let read = if kind == EndKind::Lost {
        await_read(deps, run_id, n, agent.as_ref(), ended.as_ref(), now).await?
    } else {
        None
    };
    let now = deps.cfg.now_ms();

    // 3. what the slice left, and whether it moved the work
    let own = best_head(
        &deps.blobs,
        deps.cfg.bucket.as_str(),
        run_id,
        n,
        turn.as_ref(),
    )
    .await?;
    let restored = plan.as_ref().map(|p| p.restore.clone());
    // The read-back copy this slice was planned to start from, when its plan names one.
    let planned_recovery = plan.as_ref().and_then(|p| p.recovery.clone());
    // The head the slice started on: the read-back copy its plan names, else the commit it restored, or the staged base for a slice that
    // restored the base itself.
    let started_on = planned_recovery
        .as_ref()
        .map(|r| r.head.clone())
        .or_else(|| {
            plan.as_ref()
                .and_then(|p| p.restore.head.clone().or_else(|| p.base_sha.clone()))
        });
    let progress = own.as_ref().is_some_and(|o| o.head != started_on);
    // The decision about the copy read back, against the newest saved copy as it is now: the copy this slice saved (read after
    // the wait, so a copy that landed late is seen), else what the slice started from.
    let baseline_now = own
        .as_ref()
        .and_then(|o| o.head.clone())
        .or_else(|| started_on.clone());
    let decision = match &read {
        Some(result) => {
            Some(decide_read(&deps.blobs, run_id, n, result, baseline_now.as_deref()).await)
        }
        None => None,
    };
    // A slice that was planned to start from a read-back copy and provably never worked on it: its worker said it could not
    // restore the copy, its start was refused before any machine was called, or it was lost with no copy of its own and the read of its
    // storage found no marker that names the slice and the restored commit. Such a slice is not charged as a lost session, and the next
    // one starts from the saved copy: a read-back copy that cannot be started from costs one start and never the run.
    //
    // Only a storage that was read is proof. A read that missed, was abandoned or skipped, or that never began, says nothing about
    // what the slice did: whether it worked is then not known, and not known is never read as "never worked". That slice is a lost
    // session like any other, and the run goes on from the read-back copy the slice started from, which is the newest copy the service
    // found whole; an older saved copy never replaces it without proof.
    let proven = read
        .as_ref()
        .is_some_and(|r| restore_proven(r, n, planned_recovery.as_ref()));
    let storage_read = read.as_ref().is_some_and(|r| r.state == STATE_COPIED);
    let left_nothing = own.is_none();
    // Whether a record says that the machine of this slice was called. It is asked only for a lost slice that was planned to start
    // from a read-back copy and left nothing of its own; the rule below reads it once the Run item has been read again.
    let invoked = match (&planned_recovery, kind) {
        (Some(_), EndKind::Lost) if left_nothing => deps
            .store
            .get_turn(run_id, DIRECT_AGENT_ID, n)
            .await?
            .is_some_and(|t| t.invoked_at.is_some()),
        _ => true,
    };
    let (request, _) = read_request(&deps.store, &deps.blobs, run_id).await?;
    let total = total_s(request.max_run_s);
    let started_ms = deps
        .store
        .get_turn(run_id, DIRECT_AGENT_ID, 1)
        .await?
        .and_then(|t| t.started_at_ms)
        .unwrap_or(now);
    let elapsed_s = now.saturating_sub(started_ms) / 1000;
    let cap = slice_cap_s(deps.cfg.max_lifetime_s, deps.cfg.slice_deadline_s);
    let timing = if n < MAX_SLICES && run.status == GoalStatus::Active {
        plan_timing(total, elapsed_s, cap)
    } else {
        None
    };

    // 4. the decision. The Run item is read again here, after everything above that can take time (the stop of the machine, the
    // reads of the bucket), so a cancel or a discard that landed since the top of the op is seen before the decision is stored.
    let fresh = deps
        .store
        .get_run(run_id)
        .await?
        .ok_or_else(|| RunSessionError::Invalid(format!("run {run_id} has no Run item")))?;
    // With: a slice that was never invoked while the user's keep-the-work cancel is recorded did not fail on the copy. Its
    // start was refused for the cancel, or failed before the cancel landed, so the copy was never tried. The run ends here for the
    // cancel and delivers that copy, the newest one the service found whole: `plan-delivery` skips the read-back copy of a plan only
    // for a slice whose cause says that it could not start from it.
    let cancel_recorded = fresh.status == GoalStatus::Active && fresh.cancel_requested_at.is_some();
    let never_worked = match (&planned_recovery, kind) {
        (Some(_), EndKind::RestoreFailed) => true,
        (Some(_), EndKind::Lost) if left_nothing => {
            (!invoked && !cancel_recorded) || (storage_read && !proven)
        }
        _ => false,
    };
    if never_worked {
        lost_cause = Some(CAUSE_RESTORE_FAILED.to_string());
    }
    let mut verdict = decide(prev, kind, progress, timing.is_some());
    if never_worked {
        // The limit of lost sessions in a row counts sessions that worked. With time left the run goes on; without, it ends as it would.
        verdict = match verdict {
            Verdict::Deliver { empty, reason, .. } if timing.is_none() => Verdict::Deliver {
                lost: prev.lost,
                empty,
                reason,
            },
            Verdict::Next { empty, .. } | Verdict::Deliver { empty, .. } => Verdict::Next {
                lost: prev.lost,
                empty,
            },
        };
    }
    let mut cancelled_at = None;
    if fresh.status != GoalStatus::Active {
        verdict = Verdict::Deliver {
            lost: prev.lost,
            empty: prev.empty,
            reason: Some(EndReason::RunEnded),
        };
    } else if let Some(at) = fresh.cancel_requested_at.clone() {
        // The user's keep-the-work cancel is recorded: no further session. A verdict to go on, and an end the loop counts itself (the
        // lost-session limit, a failed restore, no progress, no time left), delivers what exists for the cancel. An end the engine made
        // and the run's total time keep their own reason.
        let cancelled = |lost, empty| Verdict::Deliver {
            lost,
            empty,
            reason: Some(EndReason::Cancelled),
        };
        verdict = match (verdict, kind) {
            (Verdict::Next { lost, empty }, _) => cancelled(lost, empty),
            (
                Verdict::Deliver { lost, empty, .. },
                EndKind::Lost | EndKind::RestoreFailed | EndKind::Handoff,
            ) => cancelled(lost, empty),
            (other, _) => other,
        };
        if matches!(
            verdict,
            Verdict::Deliver {
                reason: Some(EndReason::Cancelled),
                ..
            }
        ) {
            cancelled_at = Some(at);
        }
    }
    let head = turn.as_ref().and_then(|t| t.last_commit_sha.clone());
    let end = match verdict {
        Verdict::Deliver {
            lost,
            empty,
            reason,
        } => SliceEnd {
            slice: n,
            kind,
            cont: false,
            lost,
            empty,
            end_reason: reason,
            head,
            decided_at_ms: now,
            lost_cause,
            last_heartbeat_at,
            ended_at,
            recovery: decision,
            cancel_requested_at: cancelled_at,
        },
        Verdict::Next { lost, empty } => {
            // The commit every later slice is told as the run's base: the original base.
            // A run whose stage reported none (an empty repository) cannot
            // name the commit a new session must be at, so it delivers what it has instead of starting a session that could not be checked.
            let base_sha = plan.as_ref().and_then(|p| p.base_sha.clone()).or_else(|| {
                staged
                    .get("baseSha")
                    .and_then(Value::as_str)
                    .filter(|s| sha_ok(s))
                    .map(str::to_string)
            });
            // The read-back copy the next slice starts from. It is the one read from this slice's storage when the decision uses
            // it. Else it is the one this slice itself started from, when the slice was lost and left no copy of its own, unless the
            // slice provably never worked on it: the work is still only in that copy, whether the slice is known to have restored it
            // or its start is not known. A slice that never worked on its read-back copy hands none on. `restore` below stays what
            // the rule gave before the read existed, so a reader that does not know the member starts from the saved copy.
            let next_recovery = match (&decision, &planned_recovery) {
                (Some(d), _) if d.used => d
                    .key
                    .clone()
                    .zip(d.head.clone())
                    .map(|(key, head)| PlanRecovery { key, head }),
                (_, Some(carried)) if kind == EndKind::Lost && left_nothing && !never_worked => {
                    Some(carried.clone())
                }
                _ => None,
            };
            // The next slice starts from the best head there is: what this slice left, else what it restored (a replay), else the base.
            let restore = match (own, restored) {
                (Some(o), _) => o,
                (None, Some(r)) => r,
                (None, None) => Restore {
                    kind: RestoreKind::Base,
                    key: s3::repo_bundle_key(run_id),
                    head: None,
                },
            };
            // The base bundle is at the base commit: that is the head a replay from it must find.
            let restore = if restore.kind == RestoreKind::Base {
                Restore {
                    head: base_sha.clone(),
                    ..restore
                }
            } else {
                restore
            };
            if base_sha.is_none() || restore.head.is_none() {
                SliceEnd {
                    cancel_requested_at: None,
                    slice: n,
                    kind,
                    cont: false,
                    lost,
                    empty,
                    end_reason: Some(if kind == EndKind::RestoreFailed {
                        EndReason::RestoreFailed
                    } else {
                        EndReason::SessionLost
                    }),
                    head,
                    decided_at_ms: now,
                    lost_cause,
                    last_heartbeat_at,
                    ended_at,
                    recovery: decision,
                }
            } else {
                let timing = timing.ok_or_else(|| {
                    RunSessionError::Invalid("a next slice was decided with no time to plan".into())
                })?;
                let next = SlicePlan {
                    slice: n + 1,
                    engine_s: timing.engine_s,
                    last: timing.last,
                    restore,
                    recovery: next_recovery,
                    base_sha,
                    sessions_before: n,
                    brief: summary
                        .as_ref()
                        .and_then(|s| s.cline_message.as_deref())
                        .and_then(brief_of),
                    planned_at_ms: now,
                };
                write_once(&deps.blobs, &s3::slice_record_key(run_id, n + 1), next).await?;
                SliceEnd {
                    cancel_requested_at: None,
                    slice: n,
                    kind,
                    cont: true,
                    lost,
                    empty,
                    end_reason: None,
                    head,
                    decided_at_ms: now,
                    lost_cause,
                    last_heartbeat_at,
                    ended_at,
                    recovery: decision,
                }
            }
        }
    };
    let end = write_once(&deps.blobs, &end_key(run_id, n), end).await?;
    note_recovered(deps, run_id, &end).await;
    tell_lost(deps, run_id, &end).await;
    Ok(end.state())
}

/// Names the read-back copy a stored decision uses on the Run item (`recoveredCopy`), where the download route finds the copy the
/// run goes on from. Like the word about a lost session it never changes what the run does: a failure is tried once more and then
/// logged, and a repeated `next-slice` writes it again. A copy that is not used is not named there: it is found by its slice's key.
async fn note_recovered<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run_id: &str,
    end: &SliceEnd,
) where
    S: Store,
    B: Blobs,
{
    let Some((key, head)) = end
        .recovery
        .as_ref()
        .filter(|d| d.used)
        .and_then(|d| d.key.clone().zip(d.head.clone()))
    else {
        return;
    };
    let noted = crate::tag_run::retry_once_after(LOST_WORD_PAUSE.0, LOST_WORD_PAUSE.1, || async {
        let copy = recover::read_result(&deps.blobs, run_id, end.slice)
            .await?
            .and_then(|r| r.copy);
        let reference = RecoveredCopyRef {
            session: end.slice,
            key: key.clone(),
            version_id: copy.as_ref().and_then(|c| c.version_id.clone()),
            head_sha: head.clone(),
            at: dispatcher::items::rfc3339(end.decided_at_ms / 1000),
            newest_write_at: copy.as_ref().and_then(|c| c.newest_write_at.clone()),
            bundle_bytes: copy.as_ref().map_or(0, |c| c.bundle_bytes),
        };
        deps.store.set_recovered_copy(run_id, &reference).await
    })
    .await;
    if let Err(e) = noted {
        println!(
            "{}",
            serde_json::json!({ "warning": "recovered-copy-reference", "runId": run_id, "session": end.slice, "reason": e.to_string() })
        );
    }
}

/// The pause before the second try of the word about a lost session ([`tell_lost`]): the pause of the store calls of the tag branch.
const LOST_WORD_PAUSE: (std::time::Duration, std::time::Duration) = (
    std::time::Duration::from_millis(200),
    std::time::Duration::from_millis(400),
);

/// [`announce_lost`] for a decision that is stored. The word about a lost session never changes what the run does: a failure to
/// read its records or to write the event is logged, and the caller returns the decision. A repeated `next-slice` writes the event again.
/// A failure that a second try can change (the store was unavailable, a conditional write lost) gets that second try after a short
/// pause before it is logged: the state machine never sees this error, so nothing else tries again, and without the event the run's
/// screens never say that the session was lost and what the next one started from. An error that can never work is not tried again.
async fn tell_lost<S, T, M, R, B, P>(deps: &Deps<S, T, M, R, B, P>, run_id: &str, end: &SliceEnd)
where
    S: Store,
    B: Blobs,
{
    let told = crate::tag_run::retry_once_after(LOST_WORD_PAUSE.0, LOST_WORD_PAUSE.1, || {
        announce_lost(deps, run_id, end)
    })
    .await;
    if let Err(e) = told {
        println!(
            "{}",
            serde_json::json!({ "warning": "session-lost-event", "runId": run_id, "session": end.slice, "reason": e.to_string() })
        );
    }
}

/// The service's word to the run's events that the session of `end` was lost and what follows: the session that starts and what it
/// starts from, or the end of the run. It is built from the records of the decision alone (the end record, the plan of the next slice
/// and the record of the copy that plan restores), so a repeated `next-slice` writes the same event. A slice that ended any other way,
/// and a run that someone else ended, are not announced.
async fn announce_lost<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run_id: &str,
    end: &SliceEnd,
) -> Res<()>
where
    S: Store,
    B: Blobs,
{
    // A slice that could not start from the files read back is told too: the run goes on from the saved copy, and says so.
    let failed_start = end.lost_cause.as_deref() == Some(CAUSE_RESTORE_FAILED);
    if !(end.kind == EndKind::Lost || failed_start) || end.end_reason == Some(EndReason::RunEnded) {
        return Ok(());
    }
    // The session whose storage a read-back copy was read from: the slice that names its key, this one or an earlier one.
    let read_from = |key: &str| {
        (1..=end.slice)
            .rev()
            .find(|k| s3::recovery_bundle_key(run_id, *k) == key)
    };
    // A run that ends for the user's cancel announces the lost session only when a record says that the session was invoked. A
    // slice whose start was refused for the cancel never ran and was not lost. The Turn item is a stored record, so a repeat says the same.
    if end.end_reason == Some(EndReason::Cancelled)
        && !deps
            .store
            .get_turn(run_id, DIRECT_AGENT_ID, end.slice)
            .await?
            .is_some_and(|t| t.invoked_at.is_some())
    {
        return Ok(());
    }
    let (from, saved_at, mut read_back_session) = if end.cont {
        match read_plan(&deps.blobs, run_id, end.slice + 1).await? {
            // The next slice starts from files the service read back: from this slice's storage, or from an earlier slice's
            // when this slice had started from those and left no copy of its own.
            Some(plan) if plan.recovery.is_some() => (
                Some(FROM_RECOVERED),
                None,
                plan.recovery.as_ref().and_then(|r| read_from(&r.key)),
            ),
            Some(plan) => match plan.restore.kind {
                RestoreKind::Base => (Some(FROM_START), None, None),
                RestoreKind::Bundle => (Some(FROM_COPY), None, None),
                RestoreKind::Snapshot => (
                    Some(FROM_COPY),
                    copy_time(&deps.blobs, run_id, end.slice, &plan.restore).await?,
                    None,
                ),
            },
            None => (None, None, None),
        }
    } else {
        (None, None, None)
    };
    // What the run delivers when it ends here, in the order `plan-delivery` takes it: the copy read back from this slice's storage
    // when the decision uses it, else the copy this slice saved, else the work it restored (the read-back copy its plan named, when
    // the slice did not provably fail to start from it, else the saved copy).
    let delivers = if end.cont {
        None
    } else if let Some(read_back) =
        crate::plan_delivery::read_back_head(&deps.blobs, run_id, end.slice).await?
    {
        read_back_session = read_from(&read_back.key);
        Some(ham_core::lost_session::DELIVERS_COPY.to_string())
    } else if crate::plan_delivery::snapshot_head(&deps.blobs, run_id, end.slice)
        .await?
        .is_some()
    {
        Some(ham_core::lost_session::DELIVERS_COPY.to_string())
    } else if let Some(read_back) =
        crate::plan_delivery::restored_read_back(&deps.blobs, run_id, end.slice).await?
    {
        read_back_session = read_from(&read_back.key);
        Some(ham_core::lost_session::DELIVERS_COPY.to_string())
    } else if crate::plan_delivery::restored_head(&deps.blobs, run_id, end.slice)
        .await?
        .is_some()
    {
        Some(ham_core::lost_session::DELIVERS_COPY.to_string())
    } else {
        Some(ham_core::lost_session::DELIVERS_NOTHING.to_string())
    };
    let payload = SessionLost {
        kind: KIND.to_string(),
        session: end.slice,
        decided_at: dispatcher::items::rfc3339(end.decided_at_ms / 1000),
        last_heartbeat_at: end.last_heartbeat_at.clone(),
        cause: end
            .lost_cause
            .clone()
            .unwrap_or_else(|| CAUSE_NO_RESULT.to_string()),
        silence_s: crate::plan_delivery::SESSION_HEARTBEAT_S,
        lost_in_a_row: end.lost,
        limit: MAX_LOST_SLICES,
        then: if end.cont { THEN_RESTART } else { THEN_END }.to_string(),
        next_session: end.cont.then_some(end.slice + 1),
        from: from.map(str::to_string),
        saved_at,
        delivers,
        ended_at: end.ended_at.clone(),
        recovery: end.recovery.as_ref().map(|d| LostRecovery {
            tried: d.tried,
            used: d.used,
            reason: d.reason.clone(),
            newest_write_at: d.newest_write_at.clone(),
            regressed: d.regressed,
            regressed_path: d.regressed_path.clone(),
            // The files read back are in the bucket: the client names the command that downloads them when they are not used.
            kept: d.key.is_some(),
        }),
        read_back_session,
        // The run is named by the event's envelope, where the client reads it.
        run_id: String::new(),
        // The run ends because the user had stopped it.
        cancelled_at: end.cancel_requested_at.clone(),
    };
    deps.store
        .put_event(&crate::events::session_lost(
            run_id,
            end.decided_at_ms,
            end.slice,
            &payload,
        ))
        .await
}

/// The time the record of the snapshot at `restore.key` names, when the record is that copy's: the session that saved it is the newest
/// one (`newest` down to 1) whose snapshot key is the restore's. `plan-delivery` asks it too, for the work the last slice restored.
pub(crate) async fn copy_time<B: Blobs>(
    blobs: &B,
    run_id: &str,
    newest: u32,
    restore: &Restore,
) -> Res<Option<String>> {
    for k in (1..=newest).rev() {
        if s3::snapshot_bundle_key_t(run_id, k) == restore.key {
            return Ok(crate::plan_delivery::snapshot_head(blobs, run_id, k)
                .await?
                .filter(|s| Some(s.head_sha.as_str()) == restore.head.as_deref())
                .and_then(|s| s.at)
                .filter(|t| is_utc_timestamp(t)));
        }
    }
    Ok(None)
}
