//! `plan-delivery`: after the session state ended (a result, a lost heartbeat or the
//! timeout), decide what is delivered and in what words, and build the payloads the unchanged delivery Lambdas take.
//!
//! The head comes from the session's final bundle, else from its last periodic snapshot; with neither nothing is
//! delivered and the machine ends blocked. The outcome and label are agentd's, read from `summary.json`; when the
//! session never wrote one they follow from how the state ended. No label says "verified" or "tested": the harness
//! delivers what Cline left and says how Cline stopped.
//!
//! The decision is written once to `runs/<run>/evidence/direct.json`, which the packager zips; a retry reads it back
//! and keeps its `plannedAtMs`, so the ids of the delivery events never change.

use crate::error::RunSessionError;
use crate::events;
use crate::ops::{Deps, SessionState, StatesFailure};
use crate::ports::{Blobs, PackageTokens, Res, Sessions, Store, Tokens};
use crate::prepare::{read_request, repo_name};
use crate::slices::{self, EndReason, LoopState};
use crate::text::{self, Continues, OBJECTIVE_MAX_CHARS, PrFacts};
use harness_protocol::api::internal::TurnCompleteRequest;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{
    DIRECT_AGENT_ID, DirectEvidence, DirectObservation, DirectOutcome, DirectRuleRef,
    DirectSummary, HeadSource, MAX_DIRECT_DETAIL_CHARS, MAX_REPORT_BYTES, MAX_STATE_MESSAGE_BYTES,
    REPORT_SKIPPED_NO_TIME, TestReport, TestReportStatus, TestReportSummary, head_chars,
    message_preview, s3,
};
use harness_protocol::goal::GoalPhase;
use harness_protocol::research::ThirdPartyMode;
use harness_protocol::target::RunTarget;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker_api::handler::Minter;

/// The 40 lowercase hex characters of a git object id.
fn sha_ok(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `TimeoutSeconds` of the `StartSession` task in `statemachines/direct.asl.json`: the longest the machine waits for the session.
/// `tests/session_lost.rs` holds it to the file.
pub const SESSION_TASK_TIMEOUT_S: u64 = 28_500;
/// `HeartbeatSeconds` of the same task: agentd's flusher sends one every 60 s, and a session silent this long is gone.
pub const SESSION_HEARTBEAT_S: u64 = 600;
/// The clock between the task's entry and the Turn item's start time, and between the limit and the plan: a `States.Timeout` this
/// close to [`SESSION_TASK_TIMEOUT_S`] is the limit.
const TIMEOUT_SLACK_S: u64 = 60;

/// The remark in the marker of a message preview: the package holds a start and an end, and the evidence file the whole text.
const STATE_PREVIEW_NOTE: &str = " (the whole text is in evidence/direct.json)";

/// What `plan-delivery` answers; every `Option` is left out when absent, so the machine's `IsPresent` tests work.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedDelivery {
    /// A head exists: validate, open the pull request, package, finalize. Otherwise the machine ends blocked.
    pub deliver: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub outcome: DirectOutcome,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The facts agentd recorded beside the label (its summary's `observations`); they go to `run.completed` as they are.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observations: Vec<DirectObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_source: Option<HeadSource>,
    /// The bundle-validator payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validate: Option<Value>,
    /// The repo-gateway `push-pr` payload; absent for `--s3-only` and for a head equal to the base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_pr: Option<Value>,
    /// The packager payload without `prUrl`; the machine merges it in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<Value>,
    /// Why no pull request is opened, when a repository target has none (`Cline left the repository unchanged`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_pr_reason: Option<String>,
    /// A fault of the test report's own storage (the copy could not be saved, the worker's report could not be read), in plain words
    /// without the store's error text. `finalize` puts it in the detail of `run.completed`, so a missing grant shows in the run's events
    /// and not only in a log. It is beside the outcome: the label, the head and the delivery do not depend on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_note: Option<String>,
    pub planned_at_ms: u64,
    /// The run's last slice (the session the delivered work comes from); 1 for a run that never renewed its session.
    #[serde(default = "first_slice")]
    pub slice: u32,
    /// Why the slice loop ended, when it ended for a reason of its own (total time, lost sessions, no progress). `finalize` turns it into
    /// the status, the reason and the resume mode of the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<EndReason>,
}

fn first_slice() -> u32 {
    1
}

/// `runs/<run>/evidence/direct.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceFile {
    pub schema_version: u32,
    pub run_id: String,
    pub outcome: DirectOutcome,
    pub label: String,
    pub detail: Option<String>,
    pub head_source: Option<HeadSource>,
    pub head_sha: Option<String>,
    pub base_sha: Option<String>,
    pub planned_at: String,
    pub planned_at_ms: u64,
    pub failure: Option<StatesFailure>,
    pub summary: Option<DirectSummary>,
    /// When the snapshot that was delivered was taken (RFC 3339, from `snapshot.json`); only for a delivery from a snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_at: Option<String>,
    /// The bucket key the delivered head is read from when it is neither this slice's bundle nor its snapshot (a slice that left nothing:
    /// the work it restored is delivered). Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    /// The run's last slice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slice: Option<u32>,
}

/// `snapshot.json`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotJson {
    head_sha: String,
    #[serde(default)]
    at: Option<String>,
}

/// The snapshot agentd saved last: the head its bundle holds and when it was taken.
struct Snapshot {
    head_sha: String,
    at: Option<String>,
}

/// `2026-10-13T18:50:00Z` as `2026-10-13 18:50 UTC`; `None` for anything of another shape.
fn utc_words(at: &str) -> Option<String> {
    let b = at.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    let ok = b.len() == 20
        && digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        && b[10] == b'T'
        && digits(11..13)
        && b[13] == b':'
        && digits(14..16)
        && b[16] == b':'
        && digits(17..19)
        && b[19] == b'Z';
    ok.then(|| format!("{} {} UTC", &at[..10], &at[11..16]))
}

/// What the detail says when the delivered work is the last snapshot and not the session's final bundle.
fn snapshot_sentence(at: Option<&str>) -> String {
    match at.and_then(utc_words) {
        Some(t) => format!("The work is the last saved snapshot, taken at {t}."),
        None => "The work is the last saved snapshot.".to_string(),
    }
}

/// `detail` (the cause) cut so that the snapshot sentence fits whole in [`MAX_DIRECT_DETAIL_CHARS`], then the sentence.
fn with_snapshot_sentence(detail: Option<&str>, sentence: &str) -> String {
    let room = MAX_DIRECT_DETAIL_CHARS.saturating_sub(sentence.chars().count() + 2);
    match detail.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => format!(
            "{}. {sentence}",
            head_chars(d, room).trim_end_matches(['.', ' '])
        ),
        None => sentence.to_string(),
    }
}

/// The facts agentd recorded beside the label, when there is a summary.
fn observations_of(summary: Option<&DirectSummary>) -> Vec<DirectObservation> {
    summary.map(|s| s.observations.clone()).unwrap_or_default()
}

fn cut(s: &str) -> String {
    head_chars(s, MAX_DIRECT_DETAIL_CHARS).to_string()
}

/// How the session state ended, when `summary.json` says nothing.
///
/// Step Functions reports a missed heartbeat as `States.Timeout`, with no cause, the name it also gives the task's own limit (for
/// example `TaskTimedOut` 662 s after the task started). `session_age_s`, the seconds from the Turn item's start to now, tells
/// them apart: a name that comes well before [`SESSION_TASK_TIMEOUT_S`] is a session that stopped answering. An unknown age keeps the
/// name's old meaning, the time limit.
fn outcome_without_summary(
    session: &SessionState,
    turn: Option<&TurnCompleteRequest>,
    session_age_s: Option<u64>,
) -> (DirectOutcome, Option<String>) {
    let lost = || {
        (
            DirectOutcome::StoppedSessionLost,
            Some(format!("no heartbeat for {SESSION_HEARTBEAT_S} s")),
        )
    };
    match session.failure.as_ref().and_then(|f| f.error.as_deref()) {
        Some("States.HeartbeatTimeout") => lost(),
        Some("States.Timeout")
            if session_age_s.is_some_and(|a| a + TIMEOUT_SLACK_S < SESSION_TASK_TIMEOUT_S) =>
        {
            lost()
        }
        Some("States.Timeout") => (
            DirectOutcome::StoppedTimeout,
            Some("the session state reached its time limit".into()),
        ),
        Some(other) => (DirectOutcome::StoppedSessionLost, Some(cut(other))),
        None => (
            DirectOutcome::StoppedSessionLost,
            Some(match turn {
                Some(t) => cut(&format!(
                    "the session wrote no summary ({})",
                    t.error.as_deref().unwrap_or(&t.finish_reason)
                )),
                None => "the session ended without a result".into(),
            }),
        ),
    }
}

async fn read_summary<B: Blobs>(blobs: &B, run_id: &str, slice: u32) -> Res<Option<DirectSummary>> {
    let Some(bytes) = blobs.get(&s3::summary_key_t(run_id, slice)).await? else {
        return Ok(None);
    };
    Ok(serde_json::from_slice::<DirectSummary>(&bytes)
        .ok()
        .filter(|s| s.validate().is_ok() && s.run_id == run_id))
}

/// The final bundle's key and head, when the session reported one that is the pinned key and exists.
async fn final_head<B: Blobs>(
    blobs: &B,
    bucket: &str,
    run_id: &str,
    slice: u32,
    turn: Option<&TurnCompleteRequest>,
    summary: Option<&DirectSummary>,
) -> Res<Option<String>> {
    let want_uri = s3::uri(bucket, &s3::bundle_key_t(run_id, slice));
    let from_turn = turn.and_then(|t| Some((t.bundle_uri.clone()?, t.last_commit_sha.clone()?)));
    let from_summary =
        summary.and_then(|s| Some((s.git.bundle_uri.clone()?, s.git.head_sha.clone()?)));
    for (uri, sha) in [from_turn, from_summary].into_iter().flatten() {
        if uri == want_uri && sha_ok(&sha) && blobs.exists(&s3::bundle_key_t(run_id, slice)).await?
        {
            return Ok(Some(sha));
        }
    }
    Ok(None)
}

/// Said in the detail of `run.completed` when the checked copy could not be written.
const NOTE_COPY: &str = "the test report could not be saved to the run's evidence";
/// Said when the worker's own report could not be read (an error, not an absent object).
const NOTE_READ: &str = "the test report could not be read from storage";

/// A fault of the report's storage is logged with the store's own words (the Lambda log) and told to the run's events in a fixed sentence
/// that carries none of them. The line is a JSON object, like the other warnings of this crate.
fn report_fault(run_id: &str, step: &str, why: &RunSessionError, note: &'static str) -> String {
    println!(
        "{}",
        json!({ "warning": "test-report", "runId": run_id, "step": step, "reason": why.to_string() })
    );
    note.to_string()
}

/// The clean-checkout test report of the run: what the package and the pull request say about it, and the checked copy in the
/// evidence. It never changes the label, the outcome or whether the run is delivered: a report that is missing, about another run or head,
/// or over a bound gives `None` and the run goes on as it would without one. So does a report whose storage failed (a copy that cannot be
/// written, a worker object that cannot be read), and then the second answer is a fault note: `finalize` puts it in the detail of
/// `run.completed`, so a missing grant is visible in the run's events. The checked copy once failed silently in a deployed stage.
/// The summary is computed here from the steps; a summary the worker wrote is not read.
async fn test_report_summary<B: Blobs>(
    blobs: &B,
    run_id: &str,
    slice: u32,
    head_sha: &str,
    request: &StartDirectRunRequest,
) -> (Option<TestReportSummary>, Option<String>) {
    if request.report_tests == Some(false) {
        return (Some(TestReportSummary::off()), None);
    }
    let bytes = match blobs.get(&s3::test_report_key_t(run_id, slice)).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return (None, None),
        Err(e) => {
            return (
                None,
                Some(report_fault(run_id, "read-report", &e, NOTE_READ)),
            );
        }
    };
    if bytes.len() > MAX_REPORT_BYTES {
        return (None, None);
    }
    let Ok(report) = serde_json::from_slice::<TestReport>(&bytes) else {
        return (None, None);
    };
    if report.validate(run_id, head_sha).is_err() {
        return (None, None);
    }
    let mut report = report.bounded();
    // A report agentd skipped for lack of time before the run's deadline is said in the run's detail, word for word.
    let skipped = (report.status == TestReportStatus::NotRun
        && report.reason.as_deref() == Some(REPORT_SKIPPED_NO_TIME))
    .then(|| REPORT_SKIPPED_NO_TIME.to_string());
    let summary = report.summary();
    report.summary = Some(summary.clone());
    let Ok(body) = serde_json::to_vec_pretty(&report) else {
        return (None, None);
    };
    match blobs
        .put_if_absent(&s3::evidence_test_report_key(run_id), &body)
        .await
    {
        Ok(_) => (Some(summary), skipped),
        Err(e) => (None, Some(report_fault(run_id, "save-copy", &e, NOTE_COPY))),
    }
}

async fn snapshot_head<B: Blobs>(blobs: &B, run_id: &str, slice: u32) -> Res<Option<Snapshot>> {
    let Some(bytes) = blobs.get(&s3::snapshot_json_key_t(run_id, slice)).await? else {
        return Ok(None);
    };
    let Ok(snap) = serde_json::from_slice::<SnapshotJson>(&bytes) else {
        return Ok(None);
    };
    if sha_ok(&snap.head_sha)
        && blobs
            .exists(&s3::snapshot_bundle_key_t(run_id, slice))
            .await?
    {
        Ok(Some(Snapshot {
            head_sha: snap.head_sha,
            at: snap.at,
        }))
    } else {
        Ok(None)
    }
}

/// Seconds from the start of the session task (the Turn item `start-session` wrote) to `now_ms`; `None` when no Turn or no start time exists.
async fn session_age_s<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run_id: &str,
    slice: u32,
    now_ms: u64,
) -> Res<Option<u64>>
where
    S: Store,
{
    let turn = deps.store.get_turn(run_id, DIRECT_AGENT_ID, slice).await?;
    Ok(turn
        .and_then(|t| t.started_at_ms)
        .map(|started| now_ms.saturating_sub(started) / 1000))
}

fn rule_refs(request: &StartDirectRunRequest) -> Vec<DirectRuleRef> {
    request
        .rules
        .iter()
        .map(|f| DirectRuleRef {
            name: f.path.clone(),
            sha256: f.sha256.clone(),
            bytes: Some(f.content.len() as u64),
        })
        .collect()
}

/// What the packager renders: agentd's summary when there is one, else what the state and the request tell.
fn evidence(
    summary: Option<&DirectSummary>,
    head_source: HeadSource,
    outcome: DirectOutcome,
    detail: Option<&String>,
    request: &StartDirectRunRequest,
    turn: Option<&TurnCompleteRequest>,
) -> DirectEvidence {
    match summary {
        Some(s) => s.to_evidence(head_source),
        None => DirectEvidence {
            outcome,
            label: outcome.label_for(detail.map(String::as_str)),
            detail: detail.cloned(),
            stop_reason: turn.map(|t| t.finish_reason.clone()),
            cline_message: turn.and_then(|t| t.summary.clone()),
            model_id: request.model.clone(),
            engine_version: None,
            rules: rule_refs(request),
            usage: turn.and_then(|t| t.usage),
            wall_s: None,
            head_source,
            final_model_id: None,
            model_switches: 0,
            test_report: None,
            review: None,
            flow: None,
        },
    }
}

/// What `plan-delivery` decided, as `evidence/direct.json` stores it.
struct Decision {
    outcome: DirectOutcome,
    detail: Option<String>,
    head_sha: Option<String>,
    head_source: Option<HeadSource>,
    summary: Option<DirectSummary>,
    snapshot_at: Option<String>,
    planned_at_ms: u64,
    source_key: Option<String>,
}

impl From<EvidenceFile> for Decision {
    fn from(e: EvidenceFile) -> Self {
        Decision {
            outcome: e.outcome,
            detail: e.detail,
            head_sha: e.head_sha,
            head_source: e.head_source,
            summary: e.summary,
            snapshot_at: e.snapshot_at,
            planned_at_ms: e.planned_at_ms,
            source_key: e.source_key,
        }
    }
}

pub async fn plan_delivery<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run_id: &str,
    staged: &Value,
    session: &SessionState,
    loop_state: &LoopState,
) -> Res<PlannedDelivery>
where
    S: Store,
    T: Tokens,
    M: Minter,
    R: Sessions,
    B: Blobs,
    P: PackageTokens,
{
    let bucket = deps.cfg.bucket.as_str();
    let slice = loop_state.slice.max(1);
    let end_reason = loop_state.end_reason;
    let run = deps
        .store
        .get_run(run_id)
        .await?
        .ok_or_else(|| RunSessionError::Invalid(format!("run {run_id} has no Run item")))?;
    let (request, _) = read_request(&deps.store, &deps.blobs, run_id).await?;
    let base_sha = staged
        .get("baseSha")
        .and_then(Value::as_str)
        .filter(|s| sha_ok(s))
        .map(str::to_string);
    let turn: Option<TurnCompleteRequest> = session
        .turn
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    // A session whose state failed (a lost heartbeat, the time limit) may still be running Cline: stop it before its
    // bucket is read, so the head chosen below is the last one it will ever write and the validator reads the same
    // bundle (review S2). A stop that fails fails the op; the machine retries it.
    if session.failure.as_ref().is_some_and(StatesFailure::is_set) {
        crate::finalize::end_session(deps, run_id).await?;
    }

    // The decision is made once and written once: a retry (or a second attempt that raced this one) returns the stored
    // head, outcome, label and time, whatever the session wrote to the bucket in between.
    let key = s3::evidence_key(run_id);
    let stored = |bytes: Vec<u8>| {
        serde_json::from_slice::<EvidenceFile>(&bytes)
            .ok()
            .filter(|e| e.run_id == run_id)
            .map(Decision::from)
    };
    let existing = deps.blobs.get(&key).await?.and_then(stored);
    let Decision {
        outcome,
        detail,
        head_sha,
        head_source,
        summary,
        snapshot_at,
        planned_at_ms,
        source_key,
    } = match existing {
        Some(d) => d,
        None => {
            let summary = read_summary(&deps.blobs, run_id, slice).await?;
            // The head: the final bundle, else the last snapshot, else (a slice after the first that left nothing of its own) the work it
            // restored from an earlier slice.
            let (head_sha, head_source, snapshot_at, source_key) = match final_head(
                &deps.blobs,
                bucket,
                run_id,
                slice,
                turn.as_ref(),
                summary.as_ref(),
            )
            .await?
            {
                Some(sha) => (Some(sha), Some(HeadSource::Final), None, None),
                None => match snapshot_head(&deps.blobs, run_id, slice).await? {
                    Some(s) => (Some(s.head_sha), Some(HeadSource::Snapshot), s.at, None),
                    None => match restored_head(&deps.blobs, run_id, slice).await? {
                        Some(r) => (r.head, Some(HeadSource::Snapshot), None, Some(r.key)),
                        None => (None, None, None, None),
                    },
                },
            };
            let ms = deps.cfg.now_ms();
            // The outcome and label: agentd's, else from how the state ended. The age of the session is read only when the name of
            // the failure needs it.
            let timed_out = session
                .failure
                .as_ref()
                .and_then(|f| f.error.as_deref())
                .is_some_and(|e| e == "States.Timeout");
            let age_s = if summary.is_none() && timed_out {
                session_age_s(deps, run_id, slice, ms).await?
            } else {
                None
            };
            let (outcome, detail) = match &summary {
                Some(s) => (s.outcome, s.detail.clone()),
                None => outcome_without_summary(session, turn.as_ref(), age_s),
            };
            // A delivery from a snapshot says so in the detail, which the card, `run.completed` and EVIDENCE.md carry.
            let detail = match head_source {
                Some(HeadSource::Snapshot) => Some(with_snapshot_sentence(
                    detail.as_deref(),
                    &snapshot_sentence(snapshot_at.as_deref()),
                )),
                _ => detail,
            };
            let file = EvidenceFile {
                schema_version: 1,
                run_id: run_id.to_string(),
                outcome,
                label: outcome.label_for(detail.as_deref()),
                detail: detail.clone(),
                head_source,
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                planned_at: dispatcher::items::rfc3339(ms / 1000),
                planned_at_ms: ms,
                failure: session.failure.clone().filter(StatesFailure::is_set),
                summary: summary.clone(),
                snapshot_at: snapshot_at.clone(),
                source_key: source_key.clone(),
                slice: Some(slice),
            };
            let body = serde_json::to_vec_pretty(&file)
                .map_err(|e| RunSessionError::Invalid(format!("evidence/direct.json: {e}")))?;
            if deps.blobs.put_if_absent(&key, &body).await? {
                Decision::from(file)
            } else {
                // Another attempt wrote it between our read and our write: its decision stands.
                match deps.blobs.get(&key).await?.and_then(stored) {
                    Some(winner) => winner,
                    None => Decision::from(file),
                }
            }
        }
    };

    let (Some(head_sha), Some(head_source)) = (head_sha, head_source) else {
        return Ok(PlannedDelivery {
            deliver: false,
            reason: Some("the session ended before any work was saved".into()),
            outcome,
            label: outcome.label_for(detail.as_deref()),
            detail,
            observations: observations_of(summary.as_ref()),
            head_sha: None,
            head_source: None,
            validate: None,
            push_pr: None,
            package: None,
            no_pr_reason: None,
            report_note: None,
            planned_at_ms,
            slice,
            end_reason,
        });
    };

    // From here the machine delivers.
    deps.store
        .set_run_phase(run_id, GoalPhase::Delivering)
        .await?;
    deps.store
        .put_event(&events::goal_delivering(run_id, planned_at_ms))
        .await?;

    let source_bundle = match (head_source, source_key) {
        (_, Some(key)) => key,
        (HeadSource::Final, None) => s3::bundle_key_t(run_id, slice),
        (HeadSource::Snapshot, None) => s3::snapshot_bundle_key_t(run_id, slice),
    };
    let mut validate = json!({
        "runId": run_id, "headSha": head_sha, "allowedGlobs": ["**", "!.git/**"],
        "sourceBundleUri": s3::uri(bucket, &source_bundle),
        "engineConfig": "executable-only",
    });
    if let Some(b) = &base_sha {
        validate["baseSha"] = json!(b);
    }
    // The user's per-run setting: a delivered copy of a stored third-party file is reported as
    // a finding and the run passes. The default (`block`, or no member) stays off the payload.
    if request.third_party == Some(ThirdPartyMode::Allow) {
        validate["thirdParty"] = json!("allow");
    }

    let mut ev = evidence(
        summary.as_ref(),
        head_source,
        outcome,
        detail.as_ref(),
        &request,
        turn.as_ref(),
    );
    if head_source == HeadSource::Snapshot {
        // agentd's own detail, with the sentence about the snapshot after it.
        ev.detail = detail.clone();
    }
    // The message sits in the state twice (the turn result and the evidence the packager gets), so the package carries a preview inside the state
    // budget. The whole message is in `evidence/direct.json`, which the zip holds.
    ev.cline_message = ev
        .cline_message
        .take()
        .map(|m| message_preview(&m, MAX_STATE_MESSAGE_BYTES, STATE_PREVIEW_NOTE));
    // The clean-checkout test report: a summary beside the label, never a part of it.
    let (test_report, report_note) =
        test_report_summary(&deps.blobs, run_id, slice, &head_sha, &request).await;
    ev.test_report = test_report;
    let report_sentence = ev
        .test_report
        .as_ref()
        .filter(|s| s.status != TestReportStatus::Off)
        .map(TestReportSummary::sentence);
    let mut package = json!({
        "runId": run_id, "repo": repo_name(&run.target), "headSha": head_sha,
        "objective": head_chars(&request.prompt, OBJECTIVE_MAX_CHARS),
        "direct": ev,
    });
    if let Some(b) = &base_sha {
        package["baseSha"] = json!(b);
    }

    // Agentd measures a run from the DOX starter commit it made on the head the repository had, and records that commit as `summary.git.baseSha`.
    // A run that ended on it changed nothing of its own, so it opens no pull request, as a run that ended on the original head never did. The record of
    // the delivery (`validate.baseSha`, `package.baseSha`) keeps naming the original head: the zip and a pull request hold the starter files against it.
    let measured_from = summary
        .as_ref()
        .and_then(|s| s.git.base_sha.as_deref())
        .filter(|b| sha_ok(b));
    let (push_pr, no_pr_reason) = match &run.target {
        RunTarget::S3Only => (None, None),
        RunTarget::Repo { repo } | RunTarget::NewRepo { repo } => {
            if base_sha.as_deref() == Some(head_sha.as_str())
                || measured_from == Some(head_sha.as_str())
            {
                (
                    None,
                    Some("Cline left the repository unchanged".to_string()),
                )
            } else {
                let message = ev.cline_message.as_deref();
                let snapshot_words = snapshot_at.as_deref().and_then(utc_words);
                // A follow-up run hands the gateway the run it continues, the commit that run delivered and the link of
                // its pull request, read from its `delivery.pr` event. A store that cannot answer fails the op, which the machine retries:
                // a missing link is never guessed.
                let follow_up = match &request.continue_from {
                    Some(c) if c.head_sha.is_some() => {
                        let old_url = deps.store.delivery_pr_url(&c.run_id).await?;
                        Some((c, old_url))
                    }
                    _ => None,
                };
                let facts = PrFacts {
                    run_id,
                    outcome,
                    model_id: &request.model,
                    wall_s: ev.wall_s,
                    message,
                    detail: detail.as_deref(),
                    from_snapshot: head_source == HeadSource::Snapshot,
                    snapshot_at: snapshot_words.as_deref(),
                    continues: follow_up.as_ref().map(|(c, url)| Continues {
                        run_id: &c.run_id,
                        pull_request_url: url.as_deref(),
                    }),
                };
                let target = serde_json::to_value(repo)
                    .map_err(|e| RunSessionError::Invalid(e.to_string()))?;
                let mut push = json!({
                    "operation": "push-pr", "runId": run_id, "target": target, "headSha": head_sha,
                    "title": text::title(&request.prompt),
                    "description": text::description_with(&facts, report_sentence.is_some()),
                    "comments": text::comments_with_report(message, report_sentence.as_deref()),
                });
                if let Some((c, old_url)) = &follow_up {
                    let mut member = json!({"runId": c.run_id, "headSha": c.head_sha});
                    if let Some(u) = old_url {
                        member["pullRequestUrl"] = json!(u);
                    }
                    push["followUp"] = member;
                }
                (Some(push), None)
            }
        }
    };

    Ok(PlannedDelivery {
        deliver: true,
        reason: None,
        outcome,
        label: outcome.label_for(detail.as_deref()),
        detail,
        observations: observations_of(summary.as_ref()),
        head_sha: Some(head_sha),
        head_source: Some(head_source),
        validate: Some(validate),
        push_pr,
        package: Some(package),
        no_pr_reason,
        report_note,
        planned_at_ms,
        slice,
        end_reason,
    })
}

/// The work slice `slice` restored, when that is all there is to deliver: the bundle or snapshot of an earlier slice that its plan names, if it
/// still exists. The staged base is no work.
async fn restored_head<B: Blobs>(
    blobs: &B,
    run_id: &str,
    slice: u32,
) -> Res<Option<slices::Restore>> {
    let Some(plan) = slices::read_plan(blobs, run_id, slice).await? else {
        return Ok(None);
    };
    if plan.restore.kind == slices::RestoreKind::Base
        || !plan.restore.head.as_deref().is_some_and(sha_ok)
        || !blobs.exists(&plan.restore.key).await?
    {
        return Ok(None);
    }
    Ok(Some(plan.restore))
}
