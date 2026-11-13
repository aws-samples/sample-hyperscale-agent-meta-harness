//! The clean-checkout test report in a pulled zip.
//!
//! The harness puts the report in the zip as `<top>/evidence/test-report.json` (run-session copies the worker's checked report there).
//! The client reads that one entry, recomputes the summary from its steps (a summary the file carries is not read) and shows one `Tests`
//! line beside the label: `ham pull` and `ham watch` print it, and the delivery card of the terminal app has it once the zip is downloaded
//! ([`with_tests`] puts it on the card; the terminal app reads the zip once, when the download ends, and keeps the line). The line never
//! changes the label or the exit code. A zip without the entry, an entry that is too large or not a report, or a zip that cannot be read
//! gives nothing. When a command was stopped at its time limit the line also says which setting raises the limit.

use crate::pull::Pulled;
use ham_core::render::{FinalCard, TESTS_LABEL};
use ham_core::sanitize::sanitize_line;
use harness_protocol::direct::{
    DEFAULT_REPORT_STEP_S, MAX_REPORT_BYTES, MAX_REPORT_STEP_S, TestReport, TestReportSummary,
};
use harness_protocol::store::ArtifactKind;
use std::io::Read;
use std::path::Path;

/// The name of the report inside a delivered zip: one top directory, then `evidence/test-report.json`.
fn is_report_entry(name: &str) -> bool {
    let parts: Vec<&str> = name.split('/').collect();
    parts.len() == 3
        && !parts[0].is_empty()
        && parts[1] == "evidence"
        && parts[2] == "test-report.json"
}

/// The summary of `evidence/test-report.json` in the delivered zip at `path`; `None` when the zip has none or it cannot be read.
pub fn summary_of_zip(path: &Path) -> Option<TestReportSummary> {
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let name = (0..zip.len()).find_map(|i| {
        let entry = zip.by_index_raw(i).ok()?;
        let name = entry.name().ok()?;
        is_report_entry(&name).then(|| name.into_owned())
    })?;
    let entry = zip.by_name(&name).ok()?;
    let limit = u64::try_from(MAX_REPORT_BYTES).ok()?;
    if entry.size() > limit {
        return None;
    }
    let mut body = Vec::new();
    entry.take(limit + 1).read_to_end(&mut body).ok()?;
    if u64::try_from(body.len()).ok()? > limit {
        return None;
    }
    let report: TestReport = serde_json::from_slice(&body).ok()?;
    Some(report.summary())
}

/// The setting that raises the time one command may run.
pub const LIMIT_KEY: &str = "run.report_command_minutes";

/// The words after the label: the sentence of the summary, and for a command that was stopped at its time limit the setting that raises it,
/// with twice the larger of the limit the command had and the stage's own limit ([`DEFAULT_REPORT_STEP_S`]), in whole minutes, at most the
/// largest the setting takes. A command is given the step limit or what is left of the report's budget, whichever is less, so one stopped
/// when the budget ran out records only that rest (6 s, 65 s and 143 s were seen); a number based on it would lower the step limit and the
/// same stop would come again. A limit already at the largest gets no advice.
pub fn tests_text(summary: &TestReportSummary) -> String {
    let sentence = summary.sentence();
    let Some(t) = &summary.timed_out else {
        return sentence;
    };
    if t.after_s >= MAX_REPORT_STEP_S {
        return sentence;
    }
    let basis = t.after_s.max(DEFAULT_REPORT_STEP_S);
    let suggested = basis
        .saturating_mul(2)
        .div_ceil(60)
        .min(MAX_REPORT_STEP_S / 60);
    format!("{sentence} Raise the limit with `ham config set {LIMIT_KEY} {suggested}`.")
}

/// The `Tests` line of the card: a fixed label and the words of [`tests_text`].
pub fn card_line(summary: &TestReportSummary) -> String {
    format!("{TESTS_LABEL}{}", tests_text(summary))
}

/// The words of the `Tests` line for `summary`, safe for a screen.
fn screen_text(summary: &TestReportSummary) -> String {
    sanitize_line(&tests_text(summary))
}

/// The words of the `Tests` line for the zip at `path`, safe for a screen; `None` when the zip has no report or cannot be read.
pub fn tests_of_zip(path: &Path) -> Option<String> {
    summary_of_zip(path).map(|s| screen_text(&s))
}

/// The summary of the report in the zip `pulled` names, when it is the delivered zip and holds one: the one read of the zip for a caller
/// that needs the summary itself (`ham pull` adds it to `--json`).
pub fn summary_of_pulled(pulled: &Pulled) -> Option<TestReportSummary> {
    (pulled.artifact.kind == ArtifactKind::Zip)
        .then(|| summary_of_zip(&pulled.path))
        .flatten()
}

/// `card` with the `Tests` line of `summary`, the report of a zip that was already read; no summary, no line.
pub fn with_summary(card: FinalCard, summary: Option<&TestReportSummary>) -> FinalCard {
    FinalCard {
        tests: summary.map(screen_text),
        ..card
    }
}

/// `card` with the `Tests` line of the zip `pulled` names, when it is the delivered zip and holds a report. Reads the zip once: a caller that
/// draws the card again and again keeps the line instead (the terminal app).
pub fn with_tests(card: FinalCard, pulled: Option<&Pulled>) -> FinalCard {
    with_summary(card, pulled.and_then(summary_of_pulled).as_ref())
}
