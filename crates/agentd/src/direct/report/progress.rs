//! What the report tells the event stream while it runs.
//!
//! The report takes minutes after the engine has ended, and the run's stored phase stays `green` until delivery starts. The module therefore writes
//! `turn.progress` events of kind `report` (`harness_protocol::direct::ReportProgress`): one when the clean checkout begins, one at the start of
//! each command (which command, how many, the time limit it was given) and one when the commands are over. They are ordinary agent events: they
//! change no status and no outcome, are written urgently so the screen moves at once, and carry plain words about what the harness is doing, never
//! about how the tests went. The command is a line of the repository's own files, so it passes the secret redaction the report applies to its
//! reasons before it is cut.

use crate::events::EventRecorder;
use harness_core::refusal::redact;
use harness_protocol::direct::{MAX_REPORT_STEP_S, ReportProgress, TestReportStatus};
use harness_protocol::envelope::EventType;
use std::sync::Arc;
use std::time::Duration;

/// Writes the report's progress events.
#[derive(Clone, Debug)]
pub struct Progress {
    rec: Arc<EventRecorder>,
}

/// A time limit in whole seconds, rounded up, from 1 to the longest limit the report accepts: the number a command records as `limitS`.
pub fn limit_seconds(limit: Duration) -> u32 {
    let whole = limit
        .as_secs()
        .saturating_add(u64::from(limit.subsec_nanos() > 0));
    u32::try_from(whole)
        .unwrap_or(MAX_REPORT_STEP_S)
        .clamp(1, MAX_REPORT_STEP_S)
}

/// A limit as a screen says it: `5 min` for a whole number of minutes, else `143 s`.
pub fn limit_words(secs: u32) -> String {
    if secs >= 60 && secs.is_multiple_of(60) {
        format!("{} min", secs / 60)
    } else {
        format!("{secs} s")
    }
}

impl Progress {
    pub fn new(rec: Arc<EventRecorder>) -> Self {
        Self { rec }
    }

    fn post(&self, p: &ReportProgress) {
        self.rec
            .record_urgent(EventType::TurnProgress, p.to_value());
    }

    /// The report began: the clean checkout is being made.
    pub fn checkout(&self) {
        self.post(&ReportProgress::checkout(
            "Making a clean checkout to run the repository's tests.",
        ));
    }

    /// Command `step` of `of` starts with `limit` to run in; returns the whole seconds the command records as its limit.
    pub fn running(&self, step: usize, of: usize, command: &str, limit: Duration) -> u32 {
        let secs = limit_seconds(limit);
        let text = format!(
            "Running the repository's tests: command {step} of {of} (up to {}).",
            limit_words(secs)
        );
        self.post(&ReportProgress::running(
            u32::try_from(step).unwrap_or(u32::MAX),
            u32::try_from(of).unwrap_or(u32::MAX),
            &redact(command),
            secs,
            &text,
        ));
        secs
    }

    /// The commands are over, as `status` says.
    pub fn ended(&self, status: TestReportStatus) {
        let text = match status {
            TestReportStatus::Ran => {
                "The repository's test commands ended; the report is in the zip."
            }
            TestReportStatus::TimedOut => {
                "The repository's test commands ended: a command timed out or the report's time ran out."
            }
            TestReportStatus::NoCommand => "No test command was found, so the report ran none.",
            TestReportStatus::NotRun | TestReportStatus::Off => {
                "The clean-checkout test report could not be made."
            }
        };
        self.post(&ReportProgress::ended(status, text));
    }
}
