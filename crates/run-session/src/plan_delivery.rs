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
//!
//! A run whose keep-the-work cancel is recorded (`cancelRequestedAt` of the Run item) and whose session left no result ends in the
//! words of a run the harness ended: the outcome is `stopped_other`, the detail begins with [`HARNESS_STOP_PREFIX`] and says when the user
//! stopped the run, and the label carries that sentence as its reason. No engine stop was observed, so the label claims none. The op reads
//! the record itself, so a run whose `next-slice` failed ends the same way, and it stores the time with its decision
//! (`cancelRequestedAt` of the evidence file): a retry and `finalize` read what was decided. A refusal of the run's final files and a
//! session that left a summary keep their own outcome and detail.

use crate::error::RunSessionError;
use crate::events;
use crate::ops::{Deps, SessionState, StatesFailure};
use crate::pinned::{self, Read, Trust};
use crate::ports::{Blobs, PackageTokens, Res, Sessions, Store, Tokens};
use crate::prepare::{read_request, repo_name};
use crate::slices::{self, EndReason, LoopState};
use crate::text::{self, Continues, OBJECTIVE_MAX_CHARS, PrFacts};
use harness_protocol::api::internal::TurnCompleteRequest;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::common::{Engine, parse_utc_unix};
use harness_protocol::direct::{
    DIRECT_AGENT_ID, DirectEvidence, DirectObservation, DirectOutcome, DirectRuleRef,
    DirectSummary, HARNESS_STOP_PREFIX, HeadSource, MAX_DIRECT_DETAIL_CHARS,
    MAX_DIRECT_SUMMARY_BYTES, MAX_REPORT_BYTES, MAX_STATE_MESSAGE_BYTES, REPORT_SKIPPED_NO_TIME,
    TestReport, TestReportStatus, TestReportSummary, head_chars, message_preview, s3,
};
use harness_protocol::goal::GoalPhase;
use harness_protocol::output_pins::OutputPins;
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
pub const SESSION_HEARTBEAT_S: u64 = harness_protocol::direct::SESSION_LOST_AFTER_S;
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
    /// The sessions of the run that were invoked: `slice`, or one less when the last slice was planned and its start refused (a
    /// cancel recorded between the decision to go on and the start). `finalize` counts these in the card. Absent in a plan made before
    /// the member existed: `slice` then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions: Option<u32>,
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
    /// The version records of the end-of-run objects this decision read (agentd's SHA-256 and version id for the bundle, `summary.json`
    /// and `test-report.json`), for a run that requires them. Absent for a run that does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_pins: Option<OutputPins>,
    /// The end-of-run object that did not match the harness's record (`bundle` or `summary.json`), when one did not. Nothing was
    /// delivered from that run's final files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity_refusal: Option<String>,
    /// When the user's keep-the-work cancel was recorded, for a run that ended because of it: no session answered the cancel (the
    /// detail then says so), or the slice loop ended with the reason `cancelled`. Absent for every other run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_requested_at: Option<String>,
}

/// `snapshot.json`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotJson {
    head_sha: String,
    #[serde(default)]
    at: Option<String>,
    /// The head of the base the run started from.
    #[serde(default)]
    base_sha: Option<String>,
    /// `true` on the copy agentd saves when the engine has stopped; absent on a copy made while the run worked.
    #[serde(default, rename = "final")]
    final_copy: bool,
}

/// The snapshot agentd saved last: the head its bundle holds and when it was taken.
pub(crate) struct Snapshot {
    pub(crate) head_sha: String,
    pub(crate) at: Option<String>,
    /// The copy of the session's end: it holds the work as the engine left it.
    pub(crate) final_copy: bool,
    /// The copy's head is the run's base: no file changed.
    pub(crate) unchanged: bool,
}

/// `2026-10-13T18:50:00Z` as `2026-10-13 18:50 UTC`; `None` for anything of another shape.
pub(crate) fn utc_words(at: &str) -> Option<String> {
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

/// What the detail says when the delivered work is the copy the service read back from a stopped session's storage.
pub const READ_BACK_SENTENCE: &str =
    "The work is what the service read back from the session's storage after the session ended.";

/// What the detail says of a session whose wait ended on the worker's notice that the session was being ended.
pub const ENDED_FROM_OUTSIDE: &str = "the session was asked to end from outside";

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

/// How the detail of a run begins that the harness ended for a recorded cancel that no session answered. It begins with
/// [`HARNESS_STOP_PREFIX`], which makes its first sentence the reason in the label.
const CANCEL_DETAIL_HEAD: &str = "The harness ended the run: you stopped it";

/// The detail of a run whose keep-the-work cancel was recorded at `at` and whose session left no result. The second sentence is
/// worded from the records: `started_after` names the run's last invoked session and whether it was invoked after the cancel was
/// recorded (the invoke was in flight, and that session never answered); `None` drops the sentence.
fn cancel_detail(at: Option<&str>, started_after: Option<(u32, bool)>) -> String {
    let first = match at.and_then(utc_words) {
        Some(t) => format!("{CANCEL_DETAIL_HEAD} at {t} while its session was not answering."),
        None => format!("{CANCEL_DETAIL_HEAD} while its session was not answering."),
    };
    match started_after {
        Some((n, true)) => format!(
            "{first} Session {n} was already starting when you stopped the run, and it did not answer."
        ),
        Some((_, false)) => format!("{first} No further session was started."),
        None => first,
    }
}

/// Whether `detail` is one [`cancel_detail`] made: how a decision that is read back says that no session answered the cancel.
fn is_cancel_detail(detail: Option<&str>) -> bool {
    detail.is_some_and(|d| d.starts_with(CANCEL_DETAIL_HEAD))
}

/// `detail` of a session that left a summary and got no successor because the cancel was recorded at `at`, with the sentence
/// that says so after it.
fn with_cancel_sentence(detail: Option<&str>, at: Option<&str>) -> String {
    let sentence = match at.and_then(utc_words) {
        Some(t) => format!("You stopped the run at {t}, so no further session was started."),
        None => "You stopped the run, so no further session was started.".to_string(),
    };
    with_snapshot_sentence(detail, &sentence)
}

/// Why nothing is delivered for a run whose cancel no session answered and that had saved no copy.
fn cancel_no_copy_reason(at: Option<&str>) -> String {
    let what = "while its session was not answering; no copy of its work had been saved, so there is nothing to deliver";
    match at.and_then(utc_words) {
        Some(t) => format!("you stopped the run at {t} {what}"),
        None => format!("you stopped the run {what}"),
    }
}

/// The run's last invoked session, of `slice` or of the slice before it, and whether it was invoked after the cancel was recorded
/// at `cancel_at`. Both times are compared as UTC seconds. `None` when no session was invoked or a time is missing or does not read.
async fn started_after_cancel<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run_id: &str,
    slice: u32,
    cancel_at: Option<&str>,
) -> Res<Option<(u32, bool)>>
where
    S: Store,
{
    let Some(cancelled) = cancel_at.and_then(parse_utc_unix) else {
        return Ok(None);
    };
    for n in [slice, slice.saturating_sub(1)] {
        if n == 0 {
            continue;
        }
        let invoked = deps
            .store
            .get_turn(run_id, DIRECT_AGENT_ID, n)
            .await?
            .and_then(|t| t.invoked_at);
        if let Some(at) = invoked {
            return Ok(parse_utc_unix(&at).map(|at| (n, at > cancelled)));
        }
    }
    Ok(None)
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
    // The wait for the session ended on the worker's notice. That is no result of the session: it is a lost session, and the
    // detail says what was known.
    if session.ended().is_some() {
        return (
            DirectOutcome::StoppedSessionLost,
            Some(ENDED_FROM_OUTSIDE.to_string()),
        );
    }
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

/// The summary of the slice. A run that requires version records reads the version agentd recorded and checks its digest; the object at
/// the key is never read for such a run, so a newer version written with the session's credentials changes nothing.
async fn read_summary<B: Blobs>(
    blobs: &B,
    run_id: &str,
    slice: u32,
    trust: &Trust,
) -> Res<Read<DirectSummary>> {
    let key = s3::summary_key_t(run_id, slice);
    let bytes = match trust {
        Trust::Legacy => match blobs.get(&key).await? {
            Some(b) => b,
            None => return Ok(Read::Absent),
        },
        Trust::Unattested => return Ok(Read::Absent),
        Trust::Pinned(p) => {
            match pinned::read(
                blobs,
                &key,
                p.summary.as_ref(),
                "summary.json",
                MAX_DIRECT_SUMMARY_BYTES,
            )
            .await?
            {
                Read::Found(b) => b,
                Read::Absent => return Ok(Read::Absent),
                Read::Refused(o) => return Ok(Read::Refused(o)),
            }
        }
    };
    Ok(
        match serde_json::from_slice::<DirectSummary>(&bytes)
            .ok()
            .filter(|s| s.validate().is_ok() && s.run_id == run_id)
        {
            Some(s) => Read::Found(s),
            None => Read::Absent,
        },
    )
}

/// The final bundle's key and head, when the session reported one that is the pinned key and exists.
async fn final_head<B: Blobs>(
    blobs: &B,
    bucket: &str,
    run_id: &str,
    slice: u32,
    turn: Option<&TurnCompleteRequest>,
    summary: Option<&DirectSummary>,
    trust: &Trust,
) -> Res<Read<String>> {
    let key = s3::bundle_key_t(run_id, slice);
    let want_uri = s3::uri(bucket, &key);
    let from_turn = turn.and_then(|t| Some((t.bundle_uri.clone()?, t.last_commit_sha.clone()?)));
    let from_summary =
        summary.and_then(|s| Some((s.git.bundle_uri.clone()?, s.git.head_sha.clone()?)));
    for (uri, sha) in [from_turn, from_summary].into_iter().flatten() {
        if uri != want_uri || !sha_ok(&sha) {
            continue;
        }
        // A run that requires version records finds the bundle by its record: the version agentd wrote, with the digest S3 stored for it.
        let there = match trust {
            Trust::Legacy => blobs.exists(&key).await?,
            Trust::Unattested => false,
            Trust::Pinned(p) => {
                match pinned::check(blobs, &key, p.bundle.as_ref(), "bundle").await? {
                    Read::Found(()) => true,
                    Read::Absent => false,
                    Read::Refused(o) => return Ok(Read::Refused(o)),
                }
            }
        };
        if there {
            return Ok(Read::Found(sha));
        }
    }
    Ok(Read::Absent)
}

/// Said in the detail of `run.completed` when the checked copy could not be written.
const NOTE_COPY: &str = "the test report could not be saved to the run's evidence";
/// Said when the worker's own report could not be read (an error, not an absent object).
const NOTE_READ: &str = "the test report could not be read from storage";
/// Said when the report object is not the one agentd recorded: it was not used.
const NOTE_UNMATCHED: &str =
    "the test report did not match the harness's record of it and was not used";
/// Said in the detail of a run whose final files do not match the harness's record; `HARNESS_STOP_PREFIX` makes it the reason in the label.
fn unmatched_detail(object: &str) -> String {
    format!(
        "{HARNESS_STOP_PREFIX} the {object} saved at the end of the run does not match the harness's record of it. It was not used"
    )
}
/// Said when a run that requires version records sent none with its end.
const NOTE_NO_RECORD: &str = "The harness ended the run: the session sent no record of the files it saved at its end. They were not used";

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
    trust: &Trust,
) -> (Option<TestReportSummary>, Option<String>) {
    if request.report_tests == Some(false) {
        return (Some(TestReportSummary::off()), None);
    }
    let key = s3::test_report_key_t(run_id, slice);
    let bytes = match trust {
        Trust::Legacy => match blobs.get(&key).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return (None, None),
            Err(e) => {
                return (
                    None,
                    Some(report_fault(run_id, "read-report", &e, NOTE_READ)),
                );
            }
        },
        Trust::Unattested => return (None, None),
        Trust::Pinned(p) => {
            match pinned::read(
                blobs,
                &key,
                p.test_report.as_ref(),
                "test-report.json",
                MAX_REPORT_BYTES,
            )
            .await
            {
                Ok(Read::Found(bytes)) => bytes,
                Ok(Read::Absent) => return (None, None),
                Ok(Read::Refused(_)) => return (None, Some(NOTE_UNMATCHED.to_string())),
                Err(e) => {
                    return (
                        None,
                        Some(report_fault(run_id, "read-report", &e, NOTE_READ)),
                    );
                }
            }
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

/// What the snapshot bundle of a session says of itself.
pub(crate) enum BundleSays {
    /// No bundle is at the key: the session has no copy.
    Nothing,
    /// The commit its header names for `refs/heads/snapshot`: the work the bundle holds.
    Commit(String),
    /// An object whose header cannot be read as a bundle's: the record names its commit and the validator judges it, as before.
    Unread,
}

/// Reads the header of the snapshot bundle of session `slice` (its first 8 KiB). agentd stores the bundle and then the record, so a
/// session that is stopped between the two leaves the bundle of its newest copy beside the record of the copy before it, or beside no
/// record. The bundle is the work, and it names its own commit: every reader takes that commit, as the download route does, and the
/// record says what the bundle is only when it names the same one.
pub(crate) async fn bundle_says<B: Blobs>(blobs: &B, run_id: &str, slice: u32) -> Res<BundleSays> {
    let key = s3::snapshot_bundle_key_t(run_id, slice);
    if !blobs.exists(&key).await? {
        return Ok(BundleSays::Nothing);
    }
    Ok(
        match blobs
            .get_prefix(&key, ham_core::snapshot::HEADER_BYTES)
            .await?
        {
            Some(prefix) => match ham_core::snapshot::bundle_head(&prefix) {
                Ok(commit) => BundleSays::Commit(commit),
                Err(_) => BundleSays::Unread,
            },
            None => BundleSays::Nothing,
        },
    )
}

pub(crate) async fn snapshot_head<B: Blobs>(
    blobs: &B,
    run_id: &str,
    slice: u32,
) -> Res<Option<Snapshot>> {
    let record = match blobs.get(&s3::snapshot_json_key_t(run_id, slice)).await? {
        Some(bytes) => serde_json::from_slice::<SnapshotJson>(&bytes)
            .ok()
            .filter(|snap| sha_ok(&snap.head_sha)),
        None => None,
    };
    let of_record = |snap: SnapshotJson| Snapshot {
        unchanged: snap.base_sha.as_deref() == Some(snap.head_sha.as_str()),
        head_sha: snap.head_sha,
        at: snap.at,
        final_copy: snap.final_copy,
    };
    Ok(match bundle_says(blobs, run_id, slice).await? {
        BundleSays::Nothing => None,
        BundleSays::Commit(head) => Some(match record.filter(|snap| snap.head_sha == head) {
            Some(snap) => of_record(snap),
            // The record is missing or names the copy before this one: its time and its `final` are not this bundle's.
            None => Snapshot {
                head_sha: head,
                at: None,
                final_copy: false,
                unchanged: false,
            },
        }),
        BundleSays::Unread => record.map(of_record),
    })
}

/// The snapshot that `ham pull <run> --snapshot` downloads, when there is one. The route of control-api takes the newest session
/// of the run that has a bundle, the current one first, and the commit that bundle names; a record without its bundle is no
/// snapshot to download, and the session before it is asked. `slices` is the number of the run's current session. Only
/// [`finalize`](crate::finalize) calls this, for the sentence of a blocked reason; a delivery is built from `snapshot_head` too.
///
/// Agentd saves a copy when the engine stops (the record's `final`). Only the one of the run's current session is the work as the
/// engine left it; the copy of the end of an earlier session is the newest copy there is and no more, because the run worked on after it.
pub(crate) async fn newest_snapshot<B: Blobs>(
    blobs: &B,
    run_id: &str,
    slices: u32,
) -> Res<Option<Snapshot>> {
    let newest = slices.max(1);
    for slice in (1..=newest).rev() {
        if let Some(mut found) = snapshot_head(blobs, run_id, slice).await? {
            found.final_copy = found.final_copy && slice == newest;
            return Ok(Some(found));
        }
        // The route stops at the newest session that has a bundle. A bundle whose header cannot be read and that has no record is no copy
        // it serves (it answers that the snapshot cannot be read), and no session before it is asked.
        if blobs
            .exists(&s3::snapshot_bundle_key_t(run_id, slice))
            .await?
        {
            return Ok(None);
        }
    }
    Ok(None)
}

/// Whether a session that ended with no result ended because its heartbeat stopped, read as [`outcome_without_summary`] reads it:
/// `States.HeartbeatTimeout`, or `States.Timeout` well before the limit of the task. Every other end without a result (a start that
/// failed, a state that failed) is no silence.
pub(crate) fn heartbeat_stopped(session: &SessionState, session_age_s: Option<u64>) -> bool {
    match session.failure.as_ref().and_then(|f| f.error.as_deref()) {
        Some("States.HeartbeatTimeout") => true,
        Some("States.Timeout") => {
            session_age_s.is_some_and(|a| a + TIMEOUT_SLACK_S < SESSION_TASK_TIMEOUT_S)
        }
        _ => false,
    }
}

/// Seconds from the start of the session task (the Turn item `start-session` wrote) to `now_ms`; `None` when no Turn or no start time exists.
pub(crate) async fn session_age_s<S, T, M, R, B, P>(
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
    // The engine of the run is the request's; a run that named none is a Cline run.
    let engine = Engine::or_default(request.engine);
    match summary {
        Some(s) => s.to_evidence(head_source),
        None => DirectEvidence {
            outcome,
            label: outcome.label_for_engine(engine, detail.map(String::as_str)),
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
            engine: request.engine.filter(|e| *e != Engine::DEFAULT),
            // What the engine was told about the delivery is the worker's record; a run with no summary has none.
            delivery_note: None,
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
    integrity_refusal: Option<String>,
    cancel_requested_at: Option<String>,
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
            integrity_refusal: e.integrity_refusal,
            cancel_requested_at: e.cancel_requested_at,
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
    let run = deps
        .store
        .get_run(run_id)
        .await?
        .ok_or_else(|| RunSessionError::Invalid(format!("run {run_id} has no Run item")))?;
    // The record of the user's keep-the-work cancel. It is read here and not taken from the loop state, so a run whose `next-slice`
    // failed (the machine then comes here with the loop state of a slice that was never decided) still ends as a cancelled run.
    let cancel_at = run.cancel_requested_at.clone();
    let (request, _) = read_request(&deps.store, &deps.blobs, run_id).await?;
    // The engine of the run is the request's; a request that named none is a Cline run.
    let engine = Engine::or_default(request.engine);
    let base_sha = staged
        .get("baseSha")
        .and_then(Value::as_str)
        .filter(|s| sha_ok(s))
        .map(str::to_string);
    let turn: Option<TurnCompleteRequest> = session
        .turn
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    // How far the end-of-run objects of the slice can be trusted. A run that requires version records reads only the versions agentd wrote.
    let trust = pinned::trust_of(&deps.store, run_id, turn.as_ref()).await?;

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
        integrity_refusal,
        cancel_requested_at,
    } = match existing {
        Some(d) => d,
        None => {
            // An end-of-run object that is not the one agentd recorded is not used, and the run's final files are not delivered.
            let mut refusal: Option<&'static str> = None;
            let summary = match read_summary(&deps.blobs, run_id, slice, &trust).await? {
                Read::Found(s) => Some(s),
                Read::Absent => None,
                Read::Refused(o) => {
                    refusal = Some(o);
                    None
                }
            };
            let found = if refusal.is_some() {
                Read::Absent
            } else {
                final_head(
                    &deps.blobs,
                    bucket,
                    run_id,
                    slice,
                    turn.as_ref(),
                    summary.as_ref(),
                    &trust,
                )
                .await?
            };
            let found = match found {
                Read::Found(sha) => Some(sha),
                Read::Absent => None,
                Read::Refused(o) => {
                    refusal = Some(o);
                    None
                }
            };
            // The head: the final bundle, else the copy the service read back from this slice's storage when next-slice chose it,
            // else the last snapshot, else (a slice after the first that left nothing of its own) the work it restored from an earlier
            // slice: the read-back copy its plan named, or the bundle or snapshot. `read_back` says that the work is a copy read back.
            let (head_sha, head_source, snapshot_at, source_key, read_back) = if refusal.is_some() {
                (None, None, None, None, false)
            } else {
                match found {
                    Some(sha) => (Some(sha), Some(HeadSource::Final), None, None, false),
                    None => match read_back_head(&deps.blobs, run_id, slice).await? {
                        Some(r) => (
                            Some(r.head),
                            Some(HeadSource::Snapshot),
                            None,
                            Some(r.key),
                            true,
                        ),
                        None => match snapshot_head(&deps.blobs, run_id, slice).await? {
                            Some(s) => (
                                Some(s.head_sha),
                                Some(HeadSource::Snapshot),
                                s.at,
                                None,
                                false,
                            ),
                            None => match restored_read_back(&deps.blobs, run_id, slice).await? {
                                Some(r) => (
                                    Some(r.head),
                                    Some(HeadSource::Snapshot),
                                    None,
                                    Some(r.key),
                                    true,
                                ),
                                None => match restored_head(&deps.blobs, run_id, slice).await? {
                                    Some(r) => {
                                        // When the restored work is the snapshot of an earlier session, its record names the
                                        // time, and the detail says it as for a snapshot of the last session.
                                        let at = slices::copy_time(
                                            &deps.blobs,
                                            run_id,
                                            slice.saturating_sub(1),
                                            &r,
                                        )
                                        .await?;
                                        (r.head, Some(HeadSource::Snapshot), at, Some(r.key), false)
                                    }
                                    None => (None, None, None, None, false),
                                },
                            },
                        },
                    },
                }
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
            let by_loop = loop_state.end_reason == Some(EndReason::Cancelled);
            let mut unanswered_cancel = false;
            let (outcome, detail) = match (&summary, refusal) {
                (_, Some(object)) => (DirectOutcome::StoppedOther, Some(unmatched_detail(object))),
                // A session that left a summary and got no successor because the cancel was recorded keeps its outcome and
                // detail, and one sentence says why no further session was started.
                (Some(s), None) if by_loop => (
                    s.outcome,
                    Some(with_cancel_sentence(
                        s.detail.as_deref(),
                        cancel_at.as_deref(),
                    )),
                ),
                (Some(s), None) => (s.outcome, s.detail.clone()),
                // The session ended with a turn-complete, but a run that requires version records got none: its files are not the harness's.
                (None, None) if turn.is_some() && trust == Trust::Unattested => (
                    DirectOutcome::StoppedOther,
                    Some(NOTE_NO_RECORD.to_string()),
                ),
                // The cancel is recorded and the session left no result. No engine stop was observed, so the outcome is the
                // harness's own end of the run, and the detail says what the user did and what became of the sessions.
                (None, None) if cancel_at.is_some() => {
                    unanswered_cancel = true;
                    let started_after =
                        started_after_cancel(deps, run_id, slice, cancel_at.as_deref()).await?;
                    (
                        DirectOutcome::StoppedOther,
                        Some(cancel_detail(cancel_at.as_deref(), started_after)),
                    )
                }
                (None, None) => outcome_without_summary(session, turn.as_ref(), age_s),
            };
            // A delivery from a snapshot says so in the detail, which the card, `run.completed` and EVIDENCE.md carry.
            let detail = match head_source {
                // A copy read back is not worded as a saved snapshot.
                Some(HeadSource::Snapshot) if read_back => Some(with_snapshot_sentence(
                    detail.as_deref(),
                    READ_BACK_SENTENCE,
                )),
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
                label: outcome.label_for_engine(engine, detail.as_deref()),
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
                output_pins: match &trust {
                    Trust::Pinned(p) => Some(p.clone()),
                    Trust::Legacy | Trust::Unattested => None,
                },
                integrity_refusal: refusal.map(str::to_string),
                // The time of the cancel is part of the decision when the run ends because of it.
                cancel_requested_at: cancel_at.clone().filter(|_| unanswered_cancel || by_loop),
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

    // Why the loop ended, as `finalize` reads it. A decision (new or read back) that holds the time of a cancel ends the run for
    // the cancel, whatever the loop state says: the state of a failed `next-slice` names no reason. The run's total time and a run that
    // someone else ended keep their own reason.
    let end_reason = match loop_state.end_reason {
        Some(EndReason::TotalTime | EndReason::RunEnded) => loop_state.end_reason,
        _ if cancel_requested_at.is_some() => Some(EndReason::Cancelled),
        other => other,
    };
    // The sessions that were invoked. A slice whose start was refused never ran and is not counted.
    let sessions = if deps
        .store
        .get_turn(run_id, DIRECT_AGENT_ID, slice)
        .await?
        .is_some_and(|t| t.invoked_at.is_some())
    {
        slice
    } else {
        slice.saturating_sub(1)
    };

    let (Some(head_sha), Some(head_source)) = (head_sha, head_source) else {
        return Ok(PlannedDelivery {
            sessions: Some(sessions),
            deliver: false,
            reason: Some(match &integrity_refusal {
                Some(object) => format!(
                    "the {object} saved at the end of the run does not match the harness's record of it; nothing was delivered"
                ),
                // No session answered the cancel and no copy had been saved.
                None if cancel_requested_at.is_some() && is_cancel_detail(detail.as_deref()) => {
                    cancel_no_copy_reason(cancel_requested_at.as_deref())
                }
                None => "the session ended before any work was saved".into(),
            }),
            outcome,
            label: outcome.label_for_engine(engine, detail.as_deref()),
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
    // The commit the validator judges from. For a follow-up of a run whose head no delivery check passed, that is further back
    // than the staged base; without one the validator judges every file of the head.
    let judged = judged_base(deps, &run.target, &request, base_sha.as_deref()).await?;
    if let Some(b) = &judged.base {
        validate["baseSha"] = json!(b);
    }
    // The runs whose commits are judged with this run's. The validator reads the items they stored for the copy rule; a run judged
    // from its own base names none, and the member stays off the payload.
    if !judged.sources.is_empty() {
        validate["sourceRunIds"] = json!(judged.sources);
    }
    // A run that delivers a zip alone says so, and the validator then delivers a workflow file or a root build specification
    // and reports it. A repository run sends no member, which the validator reads as a pull request, as it reads an earlier release.
    if matches!(run.target, RunTarget::S3Only) {
        validate["target"] = json!("zip");
    }
    // The validator reads the version agentd recorded for the bundle and checks its digest, not the object at the key.
    if head_source == HeadSource::Final
        && let Trust::Pinned(p) = &trust
        && let Some(pin) = &p.bundle
    {
        validate["sourceBundleSha256"] = json!(pin.sha256);
        if let Some(v) = &pin.version_id {
            validate["sourceBundleVersionId"] = json!(v);
        }
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
        test_report_summary(&deps.blobs, run_id, slice, &head_sha, &request, &trust).await;
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
    // A run that delivers a zip alone says so to the packager as it does to the validator. The packager then names in EVIDENCE.md
    // a workflow file or a root build specification the zip holds, in the words the run card shows. It reads the changes from the commit
    // the validator judges from, so both name
    // the same files; without one both read every file of the head. A repository run sends neither member.
    if matches!(run.target, RunTarget::S3Only) {
        package["target"] = json!("zip");
        if let Some(b) = &judged.base {
            package["judgedBaseSha"] = json!(b);
        }
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
                    Some(format!(
                        "{} left the repository unchanged",
                        engine.display_name()
                    )),
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
                    engine,
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
                    "comments": text::comments_for(engine, message, report_sentence.as_deref()),
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
        sessions: Some(sessions),
        deliver: true,
        reason: None,
        outcome,
        label: outcome.label_for_engine(engine, detail.as_deref()),
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

/// The most runs [`judged_base`] follows back from a follow-up run. A chain that is longer is judged from the empty tree. It is also the
/// most run ids the validator takes as `sourceRunIds` (`delivery::validator::MAX_SOURCE_RUNS`; `tests/b821_zip_delivery.rs` holds the two
/// equal).
pub const MAX_SOURCE_RUNS: usize = 32;

/// What the bundle validator judges for this run.
struct Judged {
    /// The commit the validator judges from; `None` for the empty tree (every file of the head).
    base: Option<String>,
    /// The runs this run continues whose commits are judged with its own, the nearest first. The validator reads the items they stored
    /// for the copy rule (`sourceRunIds`): a follow-up inherits no stored item, so without them a copy that an earlier run committed
    /// would pass unseen.
    sources: Vec<String>,
}

/// The commit the bundle validator judges this run's delivery from, and the runs whose commits that range holds.
///
/// The validator judges `base..head`. A run that continues nothing is judged from its staged base. A follow-up run was staged on the head
/// of the run it continues, and that head is only a safe place to start judging from when a delivery check passed it under the rules this
/// run is held to:
///
/// - the source run was delivered (`GoalStatus::is_delivered`: `complete`, `paused` or `stalled`, which only `finalize` sets, after the
///   validation passed), and
/// - for a repository run, the source was a repository run too (what a zip run was allowed as a zip was never judged as a repository
///   delivery; control-api refuses such a follow-up today, and this does not rely on it).
///
/// Otherwise the source's commits are judged with this run's: the base moves to the commit the source itself started from (the `baseSha`
/// of its `evidence/direct.json`), the source is named in [`Judged::sources`], and the same is asked of the run the source continued. A
/// run that ended blocked keeps the bundle the validator wrote before it refused the head, and a follow-up starts from it; judged from
/// that head, the follow-up would deliver what the blocked run was refused for. A source whose records cannot be read, or a chain longer
/// than [`MAX_SOURCE_RUNS`], is judged from the empty tree; such a source is still named, because items may be stored under its id. A
/// store that cannot answer fails the op, which the machine retries: a base is never guessed.
async fn judged_base<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    target: &RunTarget,
    request: &StartDirectRunRequest,
    staged_base: Option<&str>,
) -> Res<Judged>
where
    S: Store,
    B: Blobs,
{
    let repository_run = !matches!(target, RunTarget::S3Only);
    let mut base = staged_base.map(str::to_string);
    let mut sources: Vec<String> = Vec::new();
    let mut next = request.continue_from.as_ref().map(|c| c.run_id.clone());
    let empty_tree = |sources: Vec<String>| Judged {
        base: None,
        sources,
    };
    for _ in 0..MAX_SOURCE_RUNS {
        let Some(source) = next.take() else {
            return Ok(Judged { base, sources });
        };
        let Some(run) = deps.store.get_run(&source).await? else {
            sources.push(source);
            return Ok(empty_tree(sources));
        };
        let delivered = run.status.is_delivered();
        let same_rules = !repository_run || !matches!(run.target, RunTarget::S3Only);
        if delivered && same_rules {
            return Ok(Judged { base, sources });
        }
        sources.push(source.clone());
        let Some(source_base) = source_base(&deps.blobs, &source).await? else {
            return Ok(empty_tree(sources));
        };
        base = Some(source_base);
        next = match read_request(&deps.store, &deps.blobs, &source).await {
            Ok((r, _)) => r.continue_from.map(|c| c.run_id),
            // The stored request is gone or is not the one the run was created with: whether the source continued a run is not known.
            Err(RunSessionError::Invalid(_)) => return Ok(empty_tree(sources)),
            Err(e) => return Err(e),
        };
    }
    Ok(if next.is_none() {
        Judged { base, sources }
    } else {
        empty_tree(sources)
    })
}

/// The commit run `source` started from, as its `plan-delivery` recorded it in `evidence/direct.json`; `None` when the file is absent,
/// is not that run's or names no commit.
async fn source_base<B: Blobs>(blobs: &B, source: &str) -> Res<Option<String>> {
    let Some(bytes) = blobs.get(&s3::evidence_key(source)).await? else {
        return Ok(None);
    };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok(None);
    };
    if v.get("runId").and_then(Value::as_str) != Some(source) {
        return Ok(None);
    }
    Ok(v.get("baseSha")
        .and_then(Value::as_str)
        .filter(|s| sha_ok(s))
        .map(str::to_string))
}

/// The copy the service read back from the storage of slice `slice`, when `next-slice` chose to go on from it (the end record
/// says `used`) and the object is still in the bucket. A copy that is missing at delivery is no refusal: the delivery takes what it
/// would have taken without it.
pub(crate) async fn read_back_head<B: Blobs>(
    blobs: &B,
    run_id: &str,
    slice: u32,
) -> Res<Option<slices::PlanRecovery>> {
    let Some(decision) = slices::read_end(blobs, run_id, slice)
        .await?
        .and_then(|end| end.recovery)
        .filter(|d| d.used)
    else {
        return Ok(None);
    };
    let (Some(key), Some(head)) = (decision.key, decision.head) else {
        return Ok(None);
    };
    if !sha_ok(&head) || !blobs.exists(&key).await? {
        return Ok(None);
    }
    Ok(Some(slices::PlanRecovery { key, head }))
}

/// The read-back copy slice `slice` restored, when that is all there is to deliver: its plan names one, the slice worked on it
/// (it did not end as a start that failed on that copy) and the object is still there. Otherwise the delivery takes the saved copy the
/// plan names ([`restored_head`]).
pub(crate) async fn restored_read_back<B: Blobs>(
    blobs: &B,
    run_id: &str,
    slice: u32,
) -> Res<Option<slices::PlanRecovery>> {
    let Some(named) = slices::read_plan(blobs, run_id, slice)
        .await?
        .and_then(|plan| plan.recovery)
    else {
        return Ok(None);
    };
    let never_worked = slices::read_end(blobs, run_id, slice)
        .await?
        .is_some_and(|end| {
            end.lost_cause.as_deref() == Some(ham_core::lost_session::CAUSE_RESTORE_FAILED)
        });
    if never_worked || !sha_ok(&named.head) || !blobs.exists(&named.key).await? {
        return Ok(None);
    }
    Ok(Some(named))
}

/// The work slice `slice` restored, when that is all there is to deliver: the bundle or snapshot of an earlier slice that its plan names, if it
/// still exists. The staged base is no work.
pub(crate) async fn restored_head<B: Blobs>(
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
