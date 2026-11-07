//! The stop classifier. ACP's raw end of the one prompt goes in; the outcome, its frozen label
//! (`DirectOutcome::label`), the telemetry outcome and the specifics come out. It is pure: one unit test per row of the table.
//!
//! The harness records how the engine stopped and says only that. Row 2 is the one row that says the engine reported done, and
//! it does so only when the engine itself ended its turn with a message and left at least one changed file ([`refine`] reads the
//! tree after the end sequence has committed it). Row 15 (`stopped_session_lost`) is set by run-session when no turn-complete
//! arrived, never here.

use crate::acp::{AcpError, Watched};
use crate::direct::engine_stop::{Shape, StopReport};
use harness_protocol::api::internal::FaultHint;
use harness_protocol::common::Usage;
use harness_protocol::direct::{
    DIRECT_TURN, DirectOutcome, HARNESS_STOP_PREFIX, MAX_DIRECT_DETAIL_CHARS, head_chars,
};
use harness_protocol::runtime::TurnOutcome;

/// A prompt the engine answered: its raw stop reason and the usage its response reported, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptDone {
    pub stop_reason: String,
    pub usage: Option<Usage>,
}

/// How the one prompt ended, as the driver saw it.
#[derive(Debug)]
pub struct RawEnd {
    pub watched: Watched<Result<PromptDone, AcpError>>,
    /// agentd wrote `session/cancel` for the prompt (`Activity::cancel_sent`).
    pub cancel_sent: bool,
    /// The final message, control characters removed ([`clean_message`]).
    pub message: String,
    /// A failure before the prompt was sent (`initialize`, `session/new`, the model pin, the rules preflight, the startup).
    pub setup_error: Option<String>,
}

/// What the end means: the outcome and its words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Classified {
    pub outcome: DirectOutcome,
    /// Telemetry only; the machine never branches on it.
    pub turn_outcome: TurnOutcome,
    /// At most 300 characters, no control character, no secret.
    pub detail: Option<String>,
    pub fault_hint: Option<FaultHint>,
}

/// The text with control characters removed; newlines and tabs stay.
pub fn clean_message(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

/// `text` with every one of `secrets` (exact values; those under 8 characters are ignored, they would eat ordinary words)
/// replaced by `<redacted>`. The engine is given session credentials, so a model can print them; none may leave in a
/// message, an error or an event.
///
/// The longest value is replaced first: a secret that contains another one would otherwise leave its tail behind once the shorter value
/// had been replaced inside it.
pub fn redact_known(text: &str, secrets: &[String]) -> String {
    let mut ordered: Vec<&String> = secrets.iter().filter(|s| s.chars().count() >= 8).collect();
    ordered.sort_by_key(|s| std::cmp::Reverse(s.len()));
    ordered.into_iter().fold(text.to_string(), |acc, s| {
        acc.replace(s.as_str(), "<redacted>")
    })
}

/// `text` with every AWS key ID (`ASIA` or `AKIA` followed by 12 or more capitals and digits) replaced by `<redacted>`. Newlines and
/// the length of everything else stay as they are.
pub fn redact_key_ids(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let key = ["ASIA", "AKIA"].iter().filter_map(|p| rest.find(p)).min();
        match key {
            Some(i) => {
                let tail = &rest[i..];
                let len = tail
                    .bytes()
                    .take_while(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
                    .count();
                if len >= 16 {
                    out.push_str(&rest[..i]);
                    out.push_str("<redacted>");
                    rest = &tail[len..];
                } else {
                    out.push_str(&rest[..i + 4]);
                    rest = &tail[4..];
                }
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}

/// A detail line: on one line, control characters replaced by spaces, key IDs redacted, at most 300 characters.
pub fn scrub_detail(text: &str) -> String {
    let one_line: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let out = redact_key_ids(&one_line);
    head_chars(out.trim(), MAX_DIRECT_DETAIL_CHARS).to_string()
}

/// The raw ACP stop reason, for a prompt the engine answered.
pub fn stop_reason(raw: &RawEnd) -> Option<String> {
    match &raw.watched {
        Watched::Done(Ok(done)) => Some(done.stop_reason.clone()),
        _ => None,
    }
}

fn classified(
    outcome: DirectOutcome,
    turn_outcome: TurnOutcome,
    detail: Option<String>,
    fault_hint: Option<FaultHint>,
) -> Classified {
    Classified {
        outcome,
        turn_outcome,
        detail: detail.map(|d| scrub_detail(&d)).filter(|d| !d.is_empty()),
        fault_hint,
    }
}

/// What the end sequence measured about the delivered tree. It is the one fact [`refine`] reads besides the classified end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeFacts {
    /// How many files differ between the base commit and the delivered head (what `git.filesChanged` records); `None` when the
    /// harness could not measure it (no base, no worktree, the commit or the diff failed). Unknown is never read as zero.
    pub files_changed: Option<u32>,
}

/// Row 16: a stop the engine called done (row 2) is `stopped_no_changes` when the delivered tree equals the base. ACP has no
/// completion call and a give-up ends its turn exactly like a finished task, so the harness does not read the message. It counts
/// files, and a done claim is one it makes only for a run that left a trace. Every other outcome, and a tree the
/// harness could not measure, passes through unchanged. Pure.
pub fn refine(classified: Classified, tree: TreeFacts) -> Classified {
    if classified.outcome == DirectOutcome::ReportedDone && tree.files_changed == Some(0) {
        return Classified {
            outcome: DirectOutcome::StoppedNoChanges,
            ..classified
        };
    }
    classified
}

/// A provider-error end whose stage found the session disk full is the harness's own stop, and it says so. The prompt ended on
/// error -32603 and the harness measured the disk afterwards (`StageFacts::disk_full`): the checkout mount holds 1 GiB and reports no use,
/// so a full mount looks like a provider fault. The outcome is row 14 (`stopped_other`) with the detail of a stop by the harness, so the run's
/// label says that the harness ended the run and names the disk (`DirectOutcome::label_for`); the telemetry outcome is `failed` and there is no
/// fault hint (no provider is at fault). Every other end, and a provider error with no disk cause, passes through. This is the one place
/// that maps the cause: the stage classifier and the end sequence both call it. Pure.
pub fn with_disk(
    classified: Classified,
    disk: Option<&crate::direct::disk::DiskFull>,
) -> Classified {
    match disk {
        Some(cause) if classified.outcome == DirectOutcome::StoppedProviderError => {
            self::classified(
                DirectOutcome::StoppedOther,
                TurnOutcome::Failed,
                Some(cause.detail()),
                None,
            )
        }
        _ => classified,
    }
}

/// Row 8, read with the facts of the attempt: a prompt the engine ended `cancelled` with no cancel from agentd is its own brake,
/// so the fault hint carries the shape of [`StopReport`] (`loop-guard`, `mistake-limit` or `unexplained`),
/// no transport, and the detail names what the last calls looked like. Every other outcome passes through unchanged. Pure.
pub fn refine_engine_stop(classified: Classified, report: &StopReport) -> Classified {
    if classified.outcome != DirectOutcome::StoppedEngineCancelled {
        return classified;
    }
    Classified {
        detail: Some(scrub_detail(&engine_stop_detail(Some(&report.describe())))),
        fault_hint: Some(engine_stop_hint(report.shape)),
        ..classified
    }
}

/// The detail of row 8: what the harness saw, never a cause it did not see.
fn engine_stop_detail(shape: Option<&str>) -> String {
    match shape {
        Some(s) => format!(
            "the engine ended the prompt cancelled with no cancel request ({s}); agentd did not change the model because of it"
        ),
        None => "the engine ended the prompt cancelled with no cancel request; agentd did not change the model because of it".into(),
    }
}

/// The hint of row 8: the shape word of `engine_stop` and no transport fault (a cut model stream never ends `cancelled`).
fn engine_stop_hint(shape: Shape) -> FaultHint {
    FaultHint {
        code: Some(shape.word().into()),
        ..FaultHint::default()
    }
}

/// A stop a module of the seam asked for (`TurnVote::Stop`, `crate::direct::hooks`): the run ends here as a "stopped" row, with
/// the module's words as the detail (one scrubbed line, at most 300 characters). It uses row 14 (`stopped_other`): the harness records
/// that it ended the run and why, and says nothing about the work. Pure.
pub fn stopped_by_module(detail: &str) -> Classified {
    classified(
        DirectOutcome::StoppedOther,
        TurnOutcome::Completed,
        Some(detail.to_string()),
        None,
    )
}

/// agentd could not write `summary.json` for a run the engine ended with a message.
/// The harness owns the end state of the run, and a done it could not record is not one it can claim: the run ends on row 14
/// (`stopped_other`) with the harness's own stop sentence, and `DirectOutcome::label_for` reads "<engine> stopped: the harness ended the
/// run (agentd could not write summary.json: <why>)". The telemetry outcome is `failed`. No new outcome and no new label text. Pure.
pub fn summary_not_written(why: &str) -> Classified {
    classified(
        DirectOutcome::StoppedOther,
        TurnOutcome::Failed,
        Some(format!(
            "{HARNESS_STOP_PREFIX} agentd could not write summary.json: {}. The end of the run is not recorded; what the engine did is in the repository.",
            scrub_detail(why).trim_end_matches('.')
        )),
        None,
    )
}

/// agentd could not commit or bundle the checkout of a run the engine ended with a message, because the repository in it is no longer the one
/// the run started with. The engine owns the checkout for
/// the whole run, and a program its settings name would start in agentd's own `git add`, so agentd runs no git there. The harness owns the end
/// state of the run, and a done it cannot deliver is not one it claims: the run ends on row 14 (`stopped_other`) with the harness's own stop
/// sentence, as [`summary_not_written`] does. The telemetry outcome is `failed`. No new outcome and no new label text. Pure.
pub fn repository_not_trusted(why: &str) -> Classified {
    classified(
        DirectOutcome::StoppedOther,
        TurnOutcome::Failed,
        Some(format!(
            "{HARNESS_STOP_PREFIX} agentd did not commit or bundle the checkout: {}. What the engine did is in the checkout and in the last snapshot, if one was taken.",
            scrub_detail(why).trim_end_matches('.')
        )),
        None,
    )
}

/// The one sentence of a conflict on an end-of-run object: `the object <object> already existed before agentd wrote it`. `object` is
/// the key below `runs/<run>/agents/direct/` (`turns/1/bundle`, `turns/1/summary.json`, `turns/1/test-report.json`).
pub fn upload_conflict_text(object: &str) -> String {
    format!("the object {object} already existed before agentd wrote it")
}

/// An object the end sequence writes once (`If-None-Match: *`) was already in the bucket when agentd came to write it (S3's 412, and
/// for a retry after a lost answer the HEAD showed that it is not agentd's own bytes). What a reader finds under that key is not
/// agentd's record until agentd has written its own object over it, which the end sequence does after it recorded the conflict. The run
/// ends on row 14 (`stopped_other`) with the harness's own stop sentence, and `DirectOutcome::label_for` reads "<engine> stopped: the
/// harness ended the run (the object <object> already existed before agentd wrote it)". The telemetry outcome is `failed`. No new
/// outcome and no new label text. Pure.
pub fn upload_conflict(object: &str) -> Classified {
    classified(
        DirectOutcome::StoppedOther,
        TurnOutcome::Failed,
        Some(format!(
            "{HARNESS_STOP_PREFIX} {}. This process did not write it.",
            upload_conflict_text(object)
        )),
        None,
    )
}

/// The sentence a final error carries when agentd's own object could not be written over the foreign one at an end-of-run key:
/// `agentd's own object could not be written over it: <why>`. `why` is scrubbed.
pub fn not_replaced_text(why: &str) -> String {
    format!(
        "agentd's own object could not be written over it: {}",
        scrub_detail(why)
    )
}

/// The `turn.progress` event of a conflict (kind `upload-conflict`): the object and one plain sentence, never the body of either
/// object. It is posted before agentd writes its own object over the foreign one.
pub fn upload_conflict_payload(object: &str) -> serde_json::Value {
    serde_json::json!({
        "turn": DIRECT_TURN,
        "kind": "upload-conflict",
        "object": object,
        "text": format!(
            "The object {object} already existed in the run's bucket before agentd wrote it. agentd records this and then writes its own object over it; the bucket keeps the earlier one as an earlier version."
        ),
    })
}

/// The one rule that turns a raw end into an outcome. Pure.
pub fn classify(raw: &RawEnd) -> Classified {
    use DirectOutcome as O;
    use TurnOutcome as T;
    // Row 1: nothing was asked of the model.
    if let Some(e) = &raw.setup_error {
        return classified(O::StoppedSetupFailed, T::Failed, Some(e.clone()), None);
    }
    match &raw.watched {
        Watched::Done(Err(e @ AcpError::ModelMismatch { .. })) => {
            classified(O::StoppedSetupFailed, T::Failed, Some(e.to_string()), None)
        }
        // Row 9: the engine did not answer the cancel the harness wrote.
        Watched::Done(Err(AcpError::CancelIgnored)) => classified(
            O::StoppedByUser,
            T::Cancelled,
            Some("the engine did not answer session/cancel within the grace".into()),
            None,
        ),
        // Row 10: a JSON-RPC error answering the prompt, after the engine's own retries.
        Watched::Done(Err(e @ AcpError::Rpc { .. })) => classified(
            O::StoppedProviderError,
            T::Failed,
            Some(e.to_string()),
            e.fault_hint(),
        ),
        // Row 11: the process closed its output or broke the protocol.
        Watched::Done(Err(e)) => classified(
            O::StoppedEngineDied,
            T::Failed,
            Some(e.to_string()),
            e.fault_hint(),
        ),
        // Row 12: no output for the silence limit.
        Watched::Stalled { silent } => {
            let secs = u32::try_from(silent.as_secs()).unwrap_or(u32::MAX);
            classified(
                O::StoppedSilence,
                T::Failed,
                Some(format!("provider stall: no engine output for {secs} s")),
                Some(FaultHint {
                    code: Some("stall".into()),
                    silent_s: Some(secs),
                    ..FaultHint::default()
                }),
            )
        }
        // Row 13: the deadline.
        Watched::TimedOut => classified(O::StoppedTimeout, T::TimedOut, None, None),
        Watched::Done(Ok(done)) => match done.stop_reason.as_str() {
            // Rows 2 and 3.
            "end_turn" if raw.message.trim().is_empty() => {
                classified(O::StoppedNoMessage, T::Completed, None, None)
            }
            "end_turn" => classified(O::ReportedDone, T::Completed, None, None),
            // Rows 4 to 6.
            "max_tokens" => classified(O::StoppedMaxTokens, T::Completed, None, None),
            "max_turn_requests" => classified(O::StoppedMaxRequests, T::Completed, None, None),
            "refusal" => classified(O::StoppedRefusal, T::Completed, None, None),
            // Row 7: the harness asked for the cancel.
            "cancelled" if raw.cancel_sent => {
                classified(O::StoppedByUser, T::Cancelled, None, None)
            }
            // Row 8: nobody asked. On the pinned engine it is the engine's own brake (the loop guard after five identical calls in a
            // row, the mistake limit after six failed calls in a row) and never a cut model stream: a transport fault is not
            // claimed, and the shape is named only when the facts of the attempt are known ([`refine_engine_stop`]). It moves no
            // route and no model.
            "cancelled" => classified(
                O::StoppedEngineCancelled,
                T::Failed,
                Some(engine_stop_detail(None)),
                Some(engine_stop_hint(Shape::Unexplained)),
            ),
            // Row 14.
            other => classified(
                O::StoppedOther,
                T::Completed,
                Some(format!("stop reason {other:?}")),
                None,
            ),
        },
    }
}
