//! The last entry of the run log as a fact of the summary, whole agent against the fake engine: a run that ends with a
//! red last log entry records it beside its label, and the label is still the engine's. A green last entry, a `NOT RUN` last entry and a
//! missing log record nothing. The scenarios `writes_red_log`, `writes_green_log` and `writes_unrun_log` write `.ham/lifecycle/LOG.md`, the place
//! of the records of a repository without `.ham/records`.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use direct_support::{Direct, Opts};
use harness_protocol::direct::{DirectObservationKind, DirectOutcome, banned_claim_word};

async fn run(scenario: &'static str) -> Direct {
    let d = Direct::start(Opts {
        scenario,
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    d.wait_healthy().await;
    d
}

fn log_of(d: &Direct) -> String {
    std::fs::read_to_string(d.worktree().join(".ham/lifecycle/LOG.md")).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_ends_with_a_red_last_log_entry_records_it_and_keeps_the_engines_label() {
    let d = run("writes_red_log").await;
    let s = d.summary();
    // TRIGGER PRESENT: the work tree holds a log whose last entry is red (and an earlier one that is green)
    let log = log_of(&d);
    assert!(log.trim_end().ends_with("two tests fail"), "{log}");
    assert!(log.contains("exit 0"), "{log}");
    // A run on a repository without `.ham/records` leaves nothing under docs/
    assert!(
        !d.worktree().join("docs").exists(),
        "the run wrote under docs/"
    );
    assert!(!d.worktree().join(".ham/records").exists());
    assert_eq!(
        s.outcome,
        DirectOutcome::ReportedDone,
        "{}",
        d.engine_trace()
    );
    assert_eq!(s.label, "Cline reported done");
    assert_eq!(s.observations.len(), 1, "{:?}", s.observations);
    let o = &s.observations[0];
    assert_eq!(o.kind, DirectObservationKind::LastLogEntryRed);
    assert_eq!(o.command.as_deref(), Some("pytest -q"));
    assert_eq!(
        (o.exit.as_deref(), o.failed.as_deref()),
        (Some("1"), Some("2"))
    );
    assert_eq!(
        o.text,
        "The last entry of the run log is a red run: pytest -q exited 1 and 2 failed."
    );
    // the summary stays valid and no word of it claims the work
    assert_eq!(s.validate(), Ok(()));
    let text = serde_json::to_string(&s.observations).unwrap();
    assert_eq!(banned_claim_word(&text), None, "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_green_or_not_run_last_entry_and_a_missing_log_record_nothing() {
    for (scenario, trigger) in [
        ("writes_green_log", "exit 1"),
        ("writes_unrun_log", "exit 1"),
    ] {
        let d = run(scenario).await;
        // TRIGGER PRESENT: an earlier entry of the log is red, so a reading of "any red entry" would record it
        assert!(log_of(&d).contains(trigger), "{scenario}: {}", log_of(&d));
        let s = d.summary();
        assert_eq!(s.outcome, DirectOutcome::ReportedDone, "{scenario}");
        assert!(
            s.observations.is_empty(),
            "{scenario}: {:?}",
            s.observations
        );
    }
    let d = run("writes_files").await;
    assert!(log_of(&d).is_empty());
    assert!(d.summary().observations.is_empty());
}

// ---- where the log is, whole agent against the fake engine ----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_red_last_entry_of_a_moved_log_is_recorded_like_one_in_the_default_place() {
    let d = run("writes_moved_red_log").await;
    // TRIGGER PRESENT: the engine named the folder docs in .ham/records (it takes the place of .ham) and wrote its log there, and no log sits in .ham/lifecycle
    let moved = std::fs::read_to_string(d.worktree().join("docs/lifecycle/LOG.md")).unwrap();
    assert!(moved.trim_end().ends_with("two tests fail"), "{moved}");
    assert_eq!(
        std::fs::read_to_string(d.worktree().join(".ham/records")).unwrap(),
        "docs\n"
    );
    assert!(log_of(&d).is_empty(), "no log in the default place");
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::ReportedDone,
        "{}",
        d.engine_trace()
    );
    assert_eq!(s.observations.len(), 1, "{:?}", s.observations);
    let o = &s.observations[0];
    assert_eq!(o.kind, DirectObservationKind::LastLogEntryRed);
    assert_eq!(o.command.as_deref(), Some("pytest -q"));
    assert_eq!(s.validate(), Ok(()));
}

#[tokio::test(flavor = "multi_thread")]
async fn records_off_in_the_repository_records_no_observation_even_with_a_red_log() {
    let d = run("writes_off_red_log").await;
    // TRIGGER PRESENT: the log in .ham/lifecycle ends red, and the repository says the records are off
    assert!(
        log_of(&d).trim_end().ends_with("two tests fail"),
        "{}",
        log_of(&d)
    );
    assert_eq!(
        std::fs::read_to_string(d.worktree().join(".ham/records")).unwrap(),
        "off\n"
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::ReportedDone,
        "{}",
        d.engine_trace()
    );
    assert!(s.observations.is_empty(), "{:?}", s.observations);
}
