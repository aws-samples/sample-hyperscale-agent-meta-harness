//! `finalize` and `mark-blocked`: the two ends of a run.
//!
//! "Complete" on a direct run means the harness delivered what the session left; what Cline said about itself is the
//! `outcome` and `label` of `run.completed`. A run that Cline stopped halfway (silence, timeout, the mistake limit)
//! still ends `complete` with a label that says so, because the zip and the pull request exist. A run is `blocked`
//! only when nothing could be delivered.

use crate::compute::Compute;
use crate::error::RunSessionError;
use crate::events;
use crate::ops::{Deps, StatesFailure, failure_reason};
use crate::plan_delivery::{PlannedDelivery, Snapshot, newest_snapshot, utc_words};
use crate::ports::{Blobs, PackageTokens, Res, Sessions, Store, Tokens};
use crate::slices::EndReason;
use dispatcher::endpoints::session_qualifier;
use harness_protocol::common::Engine;
use harness_protocol::direct::{
    DIRECT_AGENT_ID, DirectOutcome, MAX_DIRECT_DETAIL_CHARS, head_chars,
};
use harness_protocol::goal::{GoalStatus, ResumeMode};
use harness_protocol::orchestration::DeliveryReadyPayload;
use harness_protocol::target::RunTarget;
use serde::Serialize;
use serde_json::Value;
use worker_api::handler::Minter;

#[derive(Debug)]
pub struct FinalizeInput<'a> {
    pub run_id: &'a str,
    pub plan: &'a PlannedDelivery,
    pub validated: &'a Value,
    pub packaged: &'a Value,
    pub pr_link: &'a Value,
    pub pr_failure: Option<&'a StatesFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finalized {
    pub status: GoalStatus,
    pub outcome: DirectOutcome,
    pub label: String,
}

/// A delivered run that stopped for a reason of its own. `paused` is a stop at the total time the run was given; `stalled` is a stop
/// because the run lost its progress. Both name the reason as `code: sentence` and can go on as a follow-up from the code they delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunStop {
    pub status: GoalStatus,
    pub reason: String,
    pub mode: ResumeMode,
}

fn stop_reason(code: &str, sentence: &str) -> String {
    format!("{code}: {sentence}")
}

/// The status a delivered run ends in. The loop's own end reason decides first; without one the outcome does: a run that stopped at the
/// time limit is paused, a run whose session was lost or whose Cline went silent is stalled, and every other end (Cline's own, a refusal, a
/// provider error, the mistake limit) stays `complete` with its label, as before.
pub fn run_stop(plan: &PlannedDelivery, engine_name: &str) -> Option<RunStop> {
    let paused = |code: &str, sentence: &str| RunStop {
        status: GoalStatus::Paused,
        reason: stop_reason(code, sentence),
        mode: ResumeMode::FollowUp,
    };
    let stalled = |code: &str, sentence: &str| RunStop {
        status: GoalStatus::Stalled,
        reason: stop_reason(code, sentence),
        mode: ResumeMode::FollowUp,
    };
    match (plan.end_reason, plan.outcome) {
        (Some(EndReason::NoProgress), _) => Some(stalled(
            "no_progress",
            "several sessions in a row ran to their time limit and changed nothing",
        )),
        (Some(EndReason::RestoreFailed), _) => Some(stalled(
            "restore_failed",
            "a new session could not restore the work of the one before it",
        )),
        (Some(EndReason::SessionLost), _) | (_, DirectOutcome::StoppedSessionLost) => {
            Some(stalled(
                "session_lost",
                "the session stopped answering before it finished",
            ))
        }
        (_, DirectOutcome::StoppedSilence) => Some(stalled(
            "silence",
            &format!("{engine_name} sent no output for the silence limit"),
        )),
        (Some(EndReason::TotalTime), _) | (_, DirectOutcome::StoppedTimeout) => Some(paused(
            "total_time",
            "the run used the total time it was given",
        )),
        (Some(EndReason::RunEnded), _) | (None, _) => None,
    }
}

fn status_word(s: GoalStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Stops the machine of the Agent item's current session and nothing else: `StopRuntimeSession` (a session that is already gone is fine; any
/// other failure is an error, because a session that could not be stopped is still running Cline with live credentials), and for an Instances
/// session `DeleteCapacityProviderSession`, which ends the EC2 machine and its billing. Both steps are safe to repeat. The Agent item stays as
/// it is: `end_session` revokes it at the end of the run, `start-session` moves it to the next slice.
pub(crate) async fn stop_agent_machine<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    agent: &harness_protocol::store::AgentItem,
) -> Res<()>
where
    R: Sessions,
{
    if !agent.runtime_arn.is_empty() && !agent.runtime_session_id.is_empty() {
        deps.sessions
            .stop(
                &agent.runtime_arn,
                session_qualifier(agent.runtime_endpoint.as_deref()),
                &agent.runtime_session_id,
            )
            .await?;
    }
    if agent.compute.as_deref() == Some(Compute::Instances.word())
        && let Some(provider) = agent.capacity_provider_id.as_deref()
        && !agent.runtime_session_id.is_empty()
    {
        deps.sessions
            .delete_provider_session(provider, &agent.runtime_session_id)
            .await?;
    }
    Ok(())
}

/// Ends the run's session: first the run's messages are closed (`Store::close_messages`), then `StopRuntimeSession` (a session that is already gone is fine; any other failure is an error,
/// because a session that could not be stopped is still running Cline with live credentials), then the Agent item is
/// marked `stopped` with its credential binding revoked. Both steps are safe to repeat, so the op that calls this is too.
/// Returns whether the run has an Agent item at all.
pub(crate) async fn end_session<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run_id: &str,
) -> Res<bool>
where
    S: Store,
    R: Sessions,
{
    // The run takes no more messages from here on, and what is open ends. Before the session stops, so none arrives while
    // it does; a message agentd holds in `dispatching` becomes `delivery-unknown`.
    deps.store.close_messages(run_id).await?;
    let Some(agent) = deps.store.get_agent(run_id, DIRECT_AGENT_ID).await? else {
        // A run that took an Instances slot and failed before its Agent item was written still holds the slot (the claim comes first).
        deps.store.release_instances_slot(run_id).await?;
        return Ok(false);
    };
    // An Instances session is its own EC2 machine in the stage's account. `StopRuntimeSession` stops only the agent inside it; the machine and its
    // disks (and their billing) end with `DeleteCapacityProviderSession`, so that call is part of ending the session. A session that is
    // already gone is fine; any other failure leaves the Agent item open and fails the op, so the machine's retry (or the reaper) repeats it.
    stop_agent_machine(deps, &agent).await?;
    // The run's id leaves the stage's set of active Instances sessions whatever the cap says now and whichever machine the Agent item records: the
    // claim was made under the cap of an earlier attempt or deploy, and an attempt that claimed and failed before its Agent item was written may have
    // been followed by one that chose the microVM. It is one DELETE of the run's own id, so a run that holds nothing changes nothing.
    deps.store.release_instances_slot(run_id).await?;
    deps.store.end_agent(run_id, DIRECT_AGENT_ID).await?;
    Ok(true)
}

/// Records the end of a run on its search item: the outcome word and the time. The index is derived data, so a failure is one
/// log line and never fails the end of a run.
pub(crate) async fn touch_index<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run: &harness_protocol::store::RunItem,
    outcome: Option<&str>,
    at_ms: u64,
) where
    S: Store,
{
    let at = dispatcher::items::rfc3339(at_ms / 1000);
    if let Err(e) = deps.store.touch_run_index(run, outcome, &at).await {
        println!(
            "{}",
            serde_json::json!({ "warning": "run-index", "runId": run.run_id, "reason": e.to_string() })
        );
    }
}

/// The longest path [`zip_notice`] names; a longer one is cut and ends with three dots.
const NOTICE_PATH_CHARS: usize = 120;

/// A path as the notice shows it: the engine chose the file name, so a control character becomes a space and a long path is cut.
fn shown_path(path: &str) -> String {
    let clean: String = path
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if clean.chars().count() > NOTICE_PATH_CHARS {
        format!("{}...", head_chars(&clean, NOTICE_PATH_CHARS))
    } else {
        clean
    }
}

/// What a delivered zip holds that a repository would have refused, in the words the user reads on the run card. The bundle
/// validator reports a workflow file and a root build specification under `findings` when the run delivers a zip alone; one sentence for
/// each kind says that the zip holds it and when it would run. One path is named and the rest are counted. `None` without such a finding
/// (an answer of an earlier validator has none).
fn zip_notice(validated: &Value) -> Option<String> {
    let findings = validated.get("findings").and_then(Value::as_array)?;
    let paths = |rule: &str| -> Vec<String> {
        findings
            .iter()
            .filter(|f| f.get("rule").and_then(Value::as_str) == Some(rule))
            .filter_map(|f| f.get("path").and_then(Value::as_str))
            .map(shown_path)
            .collect()
    };
    let mut sentences: Vec<String> = Vec::new();
    let workflows = paths("workflow-file");
    if let Some(first) = workflows.first() {
        sentences.push(match workflows.len() {
            1 => format!(
                "The zip holds a workflow file ({first}); it runs only if the code is pushed to a git host that runs such files."
            ),
            n => format!(
                "The zip holds {n} workflow files ({first} and {} more); they run only if the code is pushed to a git host that runs such files.",
                n - 1
            ),
        });
    }
    if let Some(first) = paths("buildspec").first() {
        sentences.push(format!(
            "The zip holds a build specification ({first}); a build service runs it only if a build project builds this code."
        ));
    }
    (!sentences.is_empty()).then(|| sentences.join(" "))
}

/// The detail of `run.completed`: a fault of the test report's storage, Cline's specifics, then why no pull request was opened, then what
/// the zip holds that a repository would have refused.
fn completed_detail(input: &FinalizeInput<'_>) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    // A fault of the test report's storage comes first: a harness fault must not be cut off by a long detail of Cline's.
    if let Some(n) = &input.plan.report_note {
        parts.push(n.clone());
    }
    if let Some(d) = &input.plan.detail {
        parts.push(d.clone());
    }
    // A run that renewed its session says how many sessions it used.
    if input.plan.slice > 1 {
        parts.push(format!("The run used {} sessions.", input.plan.slice));
    }
    if let Some(f) = input.pr_failure {
        parts.push(format!("pull request not opened: {}", failure_reason(f)));
    } else if let Some(r) = &input.plan.no_pr_reason {
        parts.push(format!("no pull request: {r}"));
    }
    let lead = (!parts.is_empty()).then(|| parts.join("; "));
    let Some(notice) = zip_notice(input.validated) else {
        return lead.map(|l| head_chars(&l, MAX_DIRECT_DETAIL_CHARS).to_string());
    };
    // The notice is the harness's own sentence about the delivery and is kept whole: the text before it gives way. It comes last, because
    // the label reads the start of the detail.
    let room = MAX_DIRECT_DETAIL_CHARS.saturating_sub(notice.chars().count() + 2);
    let lead = lead
        .map(|l| head_chars(&l, room).trim_end().to_string())
        .filter(|l| !l.is_empty());
    let whole = match lead {
        Some(l) if l.ends_with(['.', '!', '?']) => format!("{l} {notice}"),
        Some(l) => format!("{l}. {notice}"),
        None => notice,
    };
    Some(head_chars(&whole, MAX_DIRECT_DETAIL_CHARS).to_string())
}

pub async fn finalize<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    input: &FinalizeInput<'_>,
) -> Res<Finalized>
where
    S: Store,
    T: Tokens,
    M: Minter,
    R: Sessions,
    B: Blobs,
    P: PackageTokens,
{
    let run_id = input.run_id;
    let plan = input.plan;
    let mut run = deps
        .store
        .get_run(run_id)
        .await?
        .ok_or_else(|| RunSessionError::Invalid(format!("run {run_id} has no Run item")))?;
    // The engine of the run: the Run item's, Cline when the request named none.
    let engine = Engine::or_default(run.engine);
    let stop = run_stop(plan, engine.display_name());
    let to = stop.as_ref().map_or(GoalStatus::Complete, |s| s.status);
    let done = |status| Finalized {
        status,
        outcome: plan.outcome,
        label: plan.label.clone(),
    };
    // A run the owner discarded stays cleared: the work is not published under a run that was thrown away.
    if run.status == GoalStatus::Cleared {
        end_session(deps, run_id).await?;
        return Ok(done(GoalStatus::Cleared));
    }
    let mut ready: DeliveryReadyPayload = serde_json::from_value(input.packaged.clone())
        .map_err(|e| RunSessionError::Invalid(format!("the packager's answer: {e}")))?;
    let pr_url = input
        .pr_link
        .get("prUrl")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(ready.pr_url.clone());
    ready.pr_url.clone_from(&pr_url);
    let ms = plan.planned_at_ms;
    if let Some(url) = &pr_url {
        deps.store
            .put_event(&events::delivery_pr(run_id, ms, url))
            .await?;
    }
    deps.store
        .put_event(&events::delivery_ready(run_id, ms, &ready))
        .await?;
    if run.status != to {
        let event = match &stop {
            Some(s) => events::goal_stopped(run_id, ms, s.status),
            None => events::goal_complete(run_id, ms),
        };
        let moved = match &stop {
            Some(s) => {
                deps.store
                    .set_run_stop(&run, to, &event, &s.reason, s.mode)
                    .await?
            }
            None => deps.store.set_run_status(&run, to, &event).await?,
        };
        if !moved {
            // Lost a race, or this is a retry that already moved it: only a run in the status it ends in is a success.
            run =
                deps.store.get_run(run_id).await?.ok_or_else(|| {
                    RunSessionError::Invalid(format!("run {run_id} has no Run item"))
                })?;
            if run.status != to {
                let what = if to == GoalStatus::Complete {
                    "completed".to_string()
                } else {
                    format!("moved to {}", status_word(to))
                };
                return Err(RunSessionError::Conflict(format!(
                    "run {run_id} is {} and could not be {what}",
                    status_word(run.status)
                )));
            }
        }
    }
    deps.store
        .put_event(&events::run_completed_as(
            run_id,
            ms,
            to,
            engine,
            plan.outcome,
            completed_detail(input),
            plan.observations.clone(),
        ))
        .await?;
    touch_index(deps, &run, Some(plan.label.as_str()), ms).await;
    end_session(deps, run_id).await?;
    // The run has its final status and its session is stopped: index its chat for search. Never an error.
    crate::index::best_effort(deps, run_id).await;
    Ok(done(to))
}

/// What a blocked run says when EC2 had no machine for its session: the cause in plain words and the two ways out. The service's own words
/// follow it ([`blocked_reason`]).
pub const CAPACITY_REASON: &str = "EC2 had no free capacity for the machine this --compute instances run asked for. Start the run again later, or start it without --compute instances to run in the standard session.";

/// Whether `reason` is the answer of a start that found no EC2 capacity: AgentCore's `Insufficient EC2 capacity` or EC2's own error code.
fn no_ec2_capacity(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    lower.contains("insufficient ec2 capacity") || lower.contains("insufficientinstancecapacity")
}

/// The reason a run ended blocked: the failed step's own error, else the bundle validation's violations, else why
/// nothing could be delivered, else how the session state failed. A start that found no EC2 capacity reads [`CAPACITY_REASON`] first, then the
/// service's words.
pub fn blocked_reason(state: &Value) -> String {
    let reason = blocked_reason_raw(state);
    if no_ec2_capacity(&reason) {
        format!("{CAPACITY_REASON} The service said: {reason}")
    } else {
        reason
    }
}

/// The reason a Step Functions failure object gives, when it names one.
fn stated_failure(v: Option<&Value>) -> Option<String> {
    v.and_then(|v| serde_json::from_value::<StatesFailure>(v.clone()).ok())
        .filter(|f| f.error.is_some() || f.cause.is_some())
        .map(|f| failure_reason(&f))
        .filter(|r| !r.is_empty())
}

/// The bundle validator's answer when the validation failed and that is the reason the run is blocked (no step failed with an error of
/// its own).
fn failed_validation(state: &Value) -> Option<&Value> {
    if stated_failure(state.get("failure")).is_some() {
        return None;
    }
    state
        .get("validated")
        .and_then(|v| v.get("result"))
        .filter(|v| v.get("pass").and_then(Value::as_bool) == Some(false))
}

/// The last sentence of a blocked reason when `ham pull --snapshot` has nothing to download and no follow-up brings the work
/// back. True whether the run saved no snapshot or its newest snapshot record has no bundle that can be read.
const NO_SNAPSHOT_NOTE: &str = "The run has no snapshot to download.";

/// What a refused secret means for a follow-up.
const SECRET_FOLLOW_UP_NOTE: &str = "A follow-up of this run is refused for the same secret, which stays in its commits. Start a new run instead.";

/// The rule a violation of the bundle validator names.
fn rule_of(violation: &Value) -> Option<&str> {
    violation.get("rule").and_then(Value::as_str)
}

/// Which ways to get the work back the violations of a refused delivery leave.
struct WayBack {
    /// A follow-up with the zip target delivers all of the work.
    zip_follow_up: bool,
    /// The run allowed third-party copies and holds some (they are findings, not violations). A follow-up is judged with this run's
    /// commits and reads the items it stored, so it needs the same option or it is refused for the copies.
    allow_third_party: bool,
    /// A follow-up is refused for the same secret.
    secret: bool,
}

/// How the user gets the work back when the delivery was refused for a protected path (a workflow file, the build specification,
/// the engine's executable configuration) or a secret; `None` for any other reason. Each sentence of [`way_back_note`] names a route that
/// exists for this run:
///
/// - A repository run refused for nothing but a workflow file or the build specification: the follow-up with the zip target. It starts
///   on the refused head (the validator wrote the bundle before it refused the head), `plan-delivery` has it judged from this run's base
///   under the zip rule, and the zip holds all of the work with the file. Any other violation beside it (the engine's configuration, a
///   secret, a size, a copy) would refuse that follow-up too, so it is not named then. A zip run needs no such sentence. When this run
///   allowed third-party copies and holds some, the command carries `--allow-third-party`: the follow-up reads this run's stored items.
/// - A secret: a follow-up is judged from this run's base and the scan reads every commit of that range, so it is refused again.
/// - The snapshot command, only when the run saved a snapshot that the command finds, with the time it was saved (a short run has none,
///   and the snapshot of a longer run is older than the refused head). Without one, and without the follow-up, the reason says so.
fn way_back(state: &Value, target: &RunTarget) -> Option<WayBack> {
    let validation = failed_validation(state)?;
    let violations = validation.get("violations").and_then(Value::as_array)?;
    let zip_takes = |x: &Value| matches!(rule_of(x), Some("workflow-file" | "buildspec"));
    let protected_path = violations
        .iter()
        .any(|x| zip_takes(x) || rule_of(x) == Some("engine-config"));
    let secret = violations.iter().any(|x| rule_of(x) == Some("secret"));
    if !protected_path && !secret {
        return None;
    }
    let allow_third_party = validation
        .get("findings")
        .and_then(Value::as_array)
        .is_some_and(|found| found.iter().any(|x| rule_of(x) == Some("third-party-copy")));
    Some(WayBack {
        zip_follow_up: !matches!(target, RunTarget::S3Only) && violations.iter().all(zip_takes),
        allow_third_party,
        secret,
    })
}

/// What the bucket said about the run's snapshot when the blocked reason was written.
enum SnapshotLookup {
    /// The snapshot `ham pull <run> --snapshot` downloads.
    Found(Snapshot),
    /// The command has nothing to download.
    None,
    /// The bucket could not answer.
    Unread,
}

/// The sentences of [`way_back`], in the order a reader needs them: the follow-up that brings all of the work, what a secret means,
/// then the snapshot `ham pull` finds, the fact that there is none, or that the bucket could not say.
fn way_back_note(way: &WayBack, run_id: &str, snapshot: &SnapshotLookup) -> String {
    let mut parts: Vec<String> = Vec::new();
    if way.zip_follow_up {
        let allow = if way.allow_third_party {
            " --allow-third-party"
        } else {
            ""
        };
        parts.push(format!(
            "To get all of this work as a zip, start a follow-up run with `ham continue {run_id} --s3-only{allow} '<prompt>'`. A zip run delivers such a file."
        ));
    }
    if way.secret {
        parts.push(SECRET_FOLLOW_UP_NOTE.to_string());
    }
    match snapshot {
        SnapshotLookup::Found(s) => parts.push(match s.at.as_deref().and_then(utc_words) {
            Some(at) => format!(
                "The run's last snapshot was saved at {at}. Download it with `ham pull {run_id} --snapshot`; it does not hold work done after that time."
            ),
            None => format!(
                "Download the run's last snapshot with `ham pull {run_id} --snapshot`; it does not hold work done after it was saved."
            ),
        }),
        SnapshotLookup::None if !way.zip_follow_up => parts.push(NO_SNAPSHOT_NOTE.to_string()),
        SnapshotLookup::None => {}
        SnapshotLookup::Unread => parts.push(format!(
            "Whether the run saved a snapshot could not be read; `ham pull {run_id} --snapshot` downloads it if it did."
        )),
    }
    parts.join(" ")
}

fn blocked_reason_raw(state: &Value) -> String {
    let failure = stated_failure;
    if let Some(r) = failure(state.get("failure")) {
        return r;
    }
    if let Some(v) = failed_validation(state) {
        let parts: Vec<String> = v
            .get("violations")
            .and_then(Value::as_array)
            .map(|a| a.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|x| {
                let rule = x.get("rule").and_then(Value::as_str).unwrap_or("rule");
                let detail = x.get("detail").and_then(Value::as_str).unwrap_or_default();
                match x.get("path").and_then(Value::as_str) {
                    Some(p) => format!("{rule} {p}: {detail}"),
                    None => format!("{rule}: {detail}"),
                }
            })
            .collect();
        return format!("bundle validation failed: {}", parts.join("; "));
    }
    let session_failure = state.get("session").and_then(|s| s.get("failure"));
    let session = failure(session_failure);
    let plan = state.get("plan").and_then(|p| p.get("result"));
    if let Some(r) = plan.and_then(|p| p.get("reason")).and_then(Value::as_str) {
        // A failure that names only an error, or whose cause is Step Functions' own word for a timeout (a missed heartbeat reaches here as
        // `States.Timeout`), is told in the plan's own words.
        let failed = session_failure
            .and_then(|v| serde_json::from_value::<StatesFailure>(v.clone()).ok())
            .unwrap_or_default();
        let timeout_name = matches!(
            failed.error.as_deref(),
            Some("States.Timeout" | "States.HeartbeatTimeout")
        );
        let plain = plan
            .and_then(|p| p.get("detail"))
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty() && (failed.cause.is_none() || timeout_name))
            .map(str::to_string);
        return match plain.or(session) {
            Some(s) => format!("{r} ({s})"),
            None => r.to_string(),
        };
    }
    session.unwrap_or_else(|| "the run could not be delivered".to_string())
}

/// `plannedAtMs` of an `evidence/direct.json` (the plan-delivery decision, or the marker [`blocked_base_ms`] wrote).
fn planned_at_ms_of(bytes: &[u8]) -> Option<u64> {
    serde_json::from_slice::<Value>(bytes)
        .ok()?
        .get("plannedAtMs")?
        .as_u64()
}

/// The time the blocked events are dated from, the same on every attempt so their ids repeat: the delivery plan's, else
/// the one `evidence/direct.json` holds (a plan that wrote it and then failed, or an earlier attempt's marker), else the
/// clock of this attempt, which is the first, stored there as a marker. Dating a run that was never planned by its creation would put the blocked events hours
/// before the events agentd wrote, and a client following events by "id greater than the last one seen" would never
/// show them (review S4).
async fn blocked_base_ms<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run_id: &str,
    state: &Value,
) -> Res<u64>
where
    B: Blobs,
{
    if let Some(ms) = state
        .get("plan")
        .and_then(|p| p.get("result"))
        .and_then(|p| p.get("plannedAtMs"))
        .and_then(Value::as_u64)
    {
        return Ok(ms);
    }
    // The create-only write is the whole protocol: it wins on the first attempt, and on any later one (or after a plan that wrote the
    // file and then failed) it loses and the stored time is read back.
    let key = harness_protocol::direct::s3::evidence_key(run_id);
    let now = deps.cfg.now_ms();
    let marker = serde_json::json!({
        "schemaVersion": 1, "runId": run_id, "blocked": true,
        "plannedAt": dispatcher::items::rfc3339(now / 1000), "plannedAtMs": now,
    });
    let body = serde_json::to_vec_pretty(&marker)
        .map_err(|e| RunSessionError::Invalid(format!("evidence/direct.json: {e}")))?;
    if deps.blobs.put_if_absent(&key, &body).await? {
        return Ok(now);
    }
    // The file exists (a plan's, or an earlier attempt's marker, or one that raced this write): its time stands.
    Ok(deps
        .blobs
        .get(&key)
        .await?
        .as_deref()
        .and_then(planned_at_ms_of)
        .unwrap_or(now))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Blocked {
    pub status: GoalStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

pub async fn mark_blocked<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run_id: &str,
    state: &Value,
) -> Res<Blocked>
where
    S: Store,
    T: Tokens,
    M: Minter,
    R: Sessions,
    B: Blobs,
    P: PackageTokens,
{
    let Some(mut run) = deps.store.get_run(run_id).await? else {
        return Err(RunSessionError::Invalid(format!(
            "run {run_id} has no Run item"
        )));
    };
    match run.status {
        // A run that finished or was discarded is not turned into a blocked one by a late failure.
        GoalStatus::Complete => {
            // Its own `finalize` may have failed at the stop; the session ends here either way.
            end_session(deps, run_id).await?;
            return Ok(Blocked {
                status: run.status,
                reason: None,
            });
        }
        GoalStatus::Cleared => {
            end_session(deps, run_id).await?;
            return Ok(Blocked {
                status: run.status,
                reason: None,
            });
        }
        _ => {}
    }
    let mut reason = blocked_reason(state);
    // A delivery refused for a protected path or a secret names the ways to get the work back that exist for this run. The
    // sentence about the snapshot is advice: a bucket that cannot answer does not keep the run from being recorded as blocked, and
    // the reason then says that the snapshot could not be read.
    if let Some(way) = way_back(state, &run.target) {
        let snapshot = match newest_snapshot(&deps.blobs, run_id, run.slice.unwrap_or(1)).await {
            Ok(Some(found)) => SnapshotLookup::Found(found),
            Ok(None) => SnapshotLookup::None,
            Err(_) => SnapshotLookup::Unread,
        };
        if !reason.ends_with(['.', '!', '?']) {
            reason.push('.');
        }
        reason.push(' ');
        reason.push_str(&way_back_note(&way, run_id, &snapshot));
    }
    let ms = blocked_base_ms(deps, run_id, state).await?;
    // The engine of the run: the Run item's, Cline when the request named none.
    let engine = Engine::or_default(run.engine);
    let plan = state.get("plan").and_then(|p| p.get("result"));
    let outcome: Option<DirectOutcome> = plan
        .and_then(|p| p.get("outcome"))
        .and_then(|o| serde_json::from_value(o.clone()).ok());
    let detail = plan
        .and_then(|p| p.get("detail"))
        .and_then(Value::as_str)
        .map(|d| head_chars(d, MAX_DIRECT_DETAIL_CHARS).to_string());
    deps.store
        .put_event(&events::goal_blocked(run_id, ms, &reason))
        .await?;
    if run.status != GoalStatus::Blocked {
        let event = events::goal_blocked_status(run_id, ms);
        if !deps
            .store
            .set_run_status(&run, GoalStatus::Blocked, &event)
            .await?
        {
            run =
                deps.store.get_run(run_id).await?.ok_or_else(|| {
                    RunSessionError::Invalid(format!("run {run_id} has no Run item"))
                })?;
            if !matches!(
                run.status,
                GoalStatus::Blocked | GoalStatus::Complete | GoalStatus::Cleared
            ) {
                return Err(RunSessionError::Conflict(format!(
                    "run {run_id} is {} and could not be blocked",
                    status_word(run.status)
                )));
            }
        }
    }
    deps.store
        .put_event(&events::run_blocked(run_id, ms, engine, outcome, detail))
        .await?;
    touch_index(
        deps,
        &run,
        outcome.map(|o| o.label_text(engine)).as_deref(),
        ms,
    )
    .await;
    end_session(deps, run_id).await?;
    // A blocked run's conversation is searchable too; never an error.
    crate::index::best_effort(deps, run_id).await;
    Ok(Blocked {
        status: GoalStatus::Blocked,
        reason: Some(head_chars(&reason, 1_000).to_string()),
    })
}
