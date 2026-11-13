//! The resume of paused and stalled runs as the client words it.
//!
//! A run that stopped delivered, `paused` at the total time it was given or `stalled` after it lost its progress, keeps its status and its
//! delivered code. What continues is a new run (`POST /v1/runs/{runId}/resume`, the follow-up path). This module holds what the key `R` and
//! `ham resume` share: which runs of a list they take ([`plan`]), the one confirmation line ([`confirm_line`]), the Idempotency-Key, the words for
//! a refusal, and the report of what each run did. The client keeps no state: the plan is read from the list it was given, and the service
//! decides every run again when the call arrives (a run that ended `active` since, or was resumed since, is refused with its own words).

use crate::sanitize::sanitize_line;
use harness_protocol::api::v1::RunSummary;
use harness_protocol::goal::{GoalStatus, ResumeMode};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The start of the Idempotency-Key of a resume.
pub const KEY_PREFIX: &str = "resume-";

/// The most resumes in flight at once: each starts a billable run.
pub const MAX_AT_ONCE: usize = 4;

/// The Idempotency-Key of the resume of `stopped_run`: the same for every press, so the service returns the run the first one started.
pub fn start_key(stopped_run: &str) -> String {
    format!("{KEY_PREFIX}{stopped_run}")
}

/// A paused or stalled run the plan leaves, and the plain reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub run_id: String,
    pub why: String,
}

/// What a resume of a list of runs would do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResumePlan {
    /// The runs that start a follow-up, in the order of the list.
    pub resume: Vec<String>,
    /// The paused and stalled runs the plan leaves, in the order of the list.
    pub skipped: Vec<Skipped>,
}

/// The plan for `runs`. Only paused and stalled runs take part (a running, complete, blocked or cleared run is nothing to resume and is not
/// listed). A run with no code to continue from is skipped, and so is a run that a run of the list already continues: the newest run of a line
/// is the one that goes on, so a second press never starts a second continuation of the same run.
pub fn plan(runs: &[RunSummary]) -> ResumePlan {
    let continued: BTreeMap<&str, &str> = runs
        .iter()
        .filter_map(|r| Some((r.continue_from.as_deref()?, r.run_id.as_str())))
        .collect();
    let mut plan = ResumePlan::default();
    for r in runs
        .iter()
        .filter(|r| matches!(r.status, GoalStatus::Paused | GoalStatus::Stalled))
    {
        let id = sanitize_line(&r.run_id);
        if r.resume_mode == Some(ResumeMode::None) {
            plan.skipped.push(Skipped {
                run_id: id,
                why: "nothing was delivered to continue from".to_string(),
            });
        } else if let Some(child) = continued.get(r.run_id.as_str()) {
            plan.skipped.push(Skipped {
                run_id: id,
                why: format!("already continued by run {}", sanitize_line(child)),
            });
        } else {
            plan.resume.push(id);
        }
    }
    plan
}

/// The one line that asks before anything starts: the count, what each does and what is left out. `None` when the plan holds nothing at all.
pub fn confirm_line(plan: &ResumePlan) -> Option<String> {
    if plan.resume.is_empty() && plan.skipped.is_empty() {
        return None;
    }
    let n = plan.resume.len();
    let runs = if n == 1 { "run" } else { "runs" };
    let skipped = match plan.skipped.len() {
        0 => String::new(),
        k => format!(", {k} skipped"),
    };
    if n == 0 {
        return Some(format!(
            "Resume 0 runs{skipped}: nothing here can be resumed. Esc closes"
        ));
    }
    Some(format!(
        "Resume {n} {runs}: each starts a new billable run from the code it delivered{skipped}. Enter confirms · Esc cancels"
    ))
}

/// The words for a refusal of the resume route, in one line: the service's own message made safe, with what the person can do next. `status` is 0
/// when there was no answer.
pub fn explain(status: u16, code: &str, message: &str) -> String {
    let m = sanitize_line(message);
    let code_word = sanitize_line(code);
    match (status, code) {
        (404, "not-found") if m.contains("no such route") => {
            "this stage does not offer resume yet: it needs a newer backend. Use ham continue instead".to_string()
        }
        (404, _)
        | (409, "run-active" | "run-not-resumable")
        | (422, "stored-request-unreadable") => m,
        (409, "no-delivered-head" | "head-mismatch") => {
            format!("{m}; start a new run instead")
        }
        (422, "unknown-model") => format!("{m}; start a new run and pick another model"),
        (422, "idempotency-key-reused") => {
            "the key of this resume was already used for another request; start a new run instead".to_string()
        }
        (500..=599, _) => "the service did not answer: press the key again in a moment (a resume never starts a second run for the same run); if it keeps failing, use ham continue".to_string(),
        (0, _) => format!(
            "{m}; the resume may not have reached the service: press the key again (a resume never starts a second run for the same run)"
        ),
        _ => format!("{status} {code_word}: {m}"),
    }
}

/// What one resume call did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub run_id: String,
    /// The id of the run that started, or the plain words for why none did.
    pub result: Result<String, String>,
}

/// One line per run: what it did, in the order the plan lists them (the runs that started or failed first, then the skipped).
pub fn report_lines(plan: &ResumePlan, outcomes: &[Outcome]) -> Vec<String> {
    let mut lines: Vec<String> = outcomes
        .iter()
        .map(|o| match &o.result {
            Ok(new) => format!(
                "run {}: follow-up run {} started from the code it delivered",
                sanitize_line(&o.run_id),
                sanitize_line(new)
            ),
            Err(why) => format!(
                "run {}: not started: {}",
                sanitize_line(&o.run_id),
                sanitize_line(why)
            ),
        })
        .collect();
    lines.extend(plan.skipped.iter().map(|s| {
        format!(
            "run {}: skipped: {}",
            sanitize_line(&s.run_id),
            sanitize_line(&s.why)
        )
    }));
    lines
}

/// The same report as one JSON object per run (`ham resume --json`).
pub fn report_json(plan: &ResumePlan, outcomes: &[Outcome]) -> Vec<Value> {
    let mut out: Vec<Value> = outcomes
        .iter()
        .map(|o| match &o.result {
            Ok(new) => json!({
                "runId": o.run_id, "result": "resumed", "mode": "follow_up", "newRunId": new
            }),
            Err(why) => json!({"runId": o.run_id, "result": "failed", "reason": why}),
        })
        .collect();
    out.extend(
        plan.skipped
            .iter()
            .map(|s| json!({"runId": s.run_id, "result": "skipped", "reason": s.why})),
    );
    out
}
