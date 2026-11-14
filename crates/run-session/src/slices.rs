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
//! What the loop guarantees: the service writes the plan of slice n+1 only after it stopped the session of slice n and read how slice n ended;
//! the Agent item moves to slice n+1 (new session id, new refresh nonce) before the new session is invoked, so the old session can neither refresh
//! its credentials nor pass worker-api's generation check; work that was in the tree at the deadline is committed and bundled by agentd before it
//! says `handoff`; work since the last periodic snapshot of a session that vanished is lost, which is the recovery interval of the snapshot.

use crate::error::RunSessionError;
use crate::finalize::stop_agent_machine;
use crate::ops::{Deps, SessionState};
use crate::ports::{Blobs, PackageTokens, Res, Sessions, Store, Tokens};
use crate::prepare::read_request;
use harness_protocol::api::internal::TurnCompleteRequest;
use harness_protocol::direct::{
    DEFAULT_MAX_RUN_S, DIRECT_AGENT_ID, DirectOutcome, DirectSummary, MAX_DIRECT_TIMEOUT_S,
    MAX_EMPTY_SLICES, MAX_LOST_SLICES, MAX_SLICE_BRIEF_BYTES, MAX_SLICES, MIN_SLICE_ENGINE_S,
    SLICE_RESERVE_S, max_run_s_ok, s3,
};
use harness_protocol::goal::GoalStatus;
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
    /// The run's original base commit (`staged.baseSha`), when the stage reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// Engine sessions that earlier slices used (the slices before this one).
    pub sessions_before: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief: Option<String>,
    pub planned_at_ms: u64,
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
async fn read_record<B: Blobs, T: for<'de> Deserialize<'de>>(
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
async fn write_once<B: Blobs, T: Serialize + for<'de> Deserialize<'de>>(
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

async fn read_end<B: Blobs>(blobs: &B, run_id: &str, n: u32) -> Res<Option<SliceEnd>> {
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
async fn snapshot_of<B: Blobs>(blobs: &B, run_id: &str, n: u32) -> Res<Option<Restore>> {
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
    // 1. a decision is made once
    if let Some(end) = read_end(&deps.blobs, run_id, n).await? {
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
    if let Some(agent) = deps.store.get_agent(run_id, DIRECT_AGENT_ID).await? {
        stop_agent_machine(deps, &agent).await?;
    }

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
    // The head the slice started on: the commit it restored, or the staged base for a slice that restored the base itself.
    let started_on = plan
        .as_ref()
        .and_then(|p| p.restore.head.clone().or_else(|| p.base_sha.clone()));
    let progress = own.as_ref().is_some_and(|o| o.head != started_on);
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

    // 4. the decision
    let mut verdict = decide(prev, kind, progress, timing.is_some());
    if run.status != GoalStatus::Active {
        verdict = Verdict::Deliver {
            lost: prev.lost,
            empty: prev.empty,
            reason: Some(EndReason::RunEnded),
        };
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
                    slice: n,
                    kind,
                    cont: true,
                    lost,
                    empty,
                    end_reason: None,
                    head,
                    decided_at_ms: now,
                }
            }
        }
    };
    let end = write_once(&deps.blobs, &end_key(run_id, n), end).await?;
    Ok(end.state())
}
