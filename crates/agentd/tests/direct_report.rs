//! Module `report`, whole: the module is registered through `Deps.hooks`, and the whole agent runs in process against the fake AWS
//! and the fake ACP engine. The fake engine leaves a `.ham/test-command` of harmless commands (`echo` of a pytest summary line, an exit
//! code, `sleep`). The tests pin what the report says and, as much, that an ordinary report changes nothing else: the label, outcome,
//! head, `turn-complete` request and bundle upload are what they are without the module. A refused create-only report write is the one
//! exception: it preserves the head and becomes a hard upload-conflict result.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::classify::upload_conflict_text;
use agentd::direct::hooks::{DirectHook, Hooks, Limits};
use agentd::direct::report::{ReportHook, Settings};
use agentd::direct::{Deps, TEST_REPORT_OBJECT};
use direct_support::{Direct, Opts, PutFailure, RUN};
use harness_protocol::direct::{
    DirectOutcome, TestCommandSource, TestCounts, TestReport, TestReportStatus, TestStepRole,
    s3 as keys,
};
use harness_protocol::envelope::EventType;
use harness_protocol::runtime::{InvocationResponse, TurnOutcome};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn settings() -> Settings {
    Settings {
        enabled: true,
        budget: Duration::from_secs(60),
        step: Duration::from_secs(30),
        tail: harness_protocol::direct::DEFAULT_REPORT_TAIL_BYTES,
    }
}

async fn run_with(d: &Direct, s: Settings) -> InvocationResponse {
    // The driver's own limits, as the product runs them: the module works in the driver's `on_report` step (`Limits::report`, 600 s), not in
    // the 15-second finalize hook.
    let hooks = Hooks::new(vec![Arc::new(ReportHook::new(s)) as Arc<dyn DirectHook>]);
    let deps = Deps {
        hooks: Some(hooks),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await
}

fn report_of(d: &Direct) -> Option<TestReport> {
    let bytes = d.aws.get(&keys::test_report_key(RUN))?;
    Some(serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_green_test_command_of_the_delivered_tree_is_run_in_a_clean_checkout_and_reported() {
    let d = Direct::start(Opts {
        scenario: "report_green",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    let resp = run_with(&d, settings()).await;
    let r = report_of(&d).expect("a report was uploaded");
    assert_eq!(r.schema_version, 1);
    assert_eq!(r.run_id, RUN);
    assert_eq!(
        Some(r.head_sha.clone()),
        resp.last_commit_sha,
        "the report is about the delivered head"
    );
    assert_eq!(r.status, TestReportStatus::Ran);
    assert_eq!(r.steps.len(), 1, "{r:?}");
    let s = &r.steps[0];
    assert_eq!(s.role, TestStepRole::Test);
    assert_eq!(s.source, TestCommandSource::TestCommandFile);
    assert_eq!(s.command, "echo \"4 passed in 0.02s\"");
    assert_eq!(s.exit_code, Some(0));
    assert_eq!(
        s.counts,
        Some(TestCounts {
            passed: 4,
            failed: 0,
            skipped: 0
        })
    );
    assert!(s.output_tail.contains("4 passed in 0.02s"));
    assert_eq!(
        r.summary, None,
        "the worker leaves the summary to run-session"
    );
    assert_eq!(
        r.summary().claim().as_deref(),
        Some("repository tests pass in a clean checkout: 4 of 4")
    );
}

/// `test-report.json` is written once (`If-None-Match: *`) like the bundle and `summary.json`: an object that was there before
/// agentd's write is a conflict, recorded once, and agentd then writes its own report over it (the versioned bucket keeps the foreign
/// object as a noncurrent version), so the report run-session reads is the one agentd made. The single conflict event is followed by
/// the hard `stopped_other` result; the delivered head stays the one agentd stored before the report step.
#[tokio::test(flavor = "multi_thread")]
async fn a_test_report_object_that_already_exists_is_a_hard_conflict_and_is_replaced_by_agentds_own()
 {
    let d = Direct::start(Opts {
        scenario: "report_green",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    let marker = b"{\"forged\":true}".to_vec();
    d.aws.put(&keys::test_report_key(RUN), marker.clone());
    let resp = run_with(&d, settings()).await;
    assert_eq!(
        d.aws.put_attempts(&keys::test_report_key(RUN)),
        2,
        "the refused create-only put, then the replacing put"
    );
    assert_eq!(
        d.aws.head_attempts(&keys::test_report_key(RUN)),
        0,
        "nothing failed before the 412, so no HEAD"
    );
    assert_ne!(
        d.aws.get(&keys::test_report_key(RUN)),
        Some(marker),
        "the foreign object was written over"
    );
    let own = report_of(&d).expect("agentd's own report is in the bucket");
    assert_eq!(own.run_id, RUN);
    assert_eq!(Some(own.head_sha.clone()), resp.last_commit_sha);
    assert_eq!(own.status, TestReportStatus::Ran);
    assert_eq!(resp.outcome, TurnOutcome::Failed);
    assert!(resp.bundle_uri.is_some());
    assert!(resp.last_commit_sha.is_some());
    let ended = d.events_of(EventType::TurnEnded);
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(ended[0]["directOutcome"], "stopped_other");
    let label = ended[0]["label"].as_str().unwrap();
    assert!(
        label.starts_with("Cline stopped: the harness ended the run (")
            && label.contains("turns/1/test-report.json"),
        "{label}"
    );
    let complete = d.aws.complete();
    assert_eq!(complete.len(), 1, "{complete:?}");
    assert_eq!(complete[0].outcome, Some(TurnOutcome::Failed));
    let error = complete[0].error.as_deref().unwrap();
    let conflict_text = upload_conflict_text(TEST_REPORT_OBJECT);
    assert_eq!(
        error.matches(conflict_text.as_str()).count(),
        1,
        "the report conflict appears once: {error}"
    );
    assert_eq!(resp.error.as_deref(), Some(error));
    let persisted: InvocationResponse =
        serde_json::from_slice(&std::fs::read(d.result_path()).unwrap()).unwrap();
    assert_eq!(persisted.outcome, TurnOutcome::Failed);
    assert_eq!(persisted.error.as_deref(), Some(error));
    // `summary.json` was stored before the report step and remains the record agentd wrote at that point.
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    assert_eq!(s.label, "Cline reported done");
    let conflicts = d.aws.progress("upload-conflict");
    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    assert_eq!(conflicts[0].1["object"], "turns/1/test-report.json");
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.starts_with("direct report not uploaded:") && l.contains("already existed")),
        "{:?}",
        d.log.lines()
    );
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.starts_with("direct report replaced:")),
        "{:?}",
        d.log.lines()
    );
    let everything = format!(
        "{}\n{}\n{}\n{}\n{}",
        serde_json::to_string(&d.aws.events()).unwrap(),
        serde_json::to_string(&complete).unwrap(),
        serde_json::to_string(&resp).unwrap(),
        serde_json::to_string(&persisted).unwrap(),
        d.log.lines().join("\n"),
    );
    direct_support::no_secrets(&everything);
}

/// If agentd cannot write its report over the foreign object, the foreign object stays, the conflict is recorded once and the log says
/// agentd's own report could not be written over it.
#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_test_report_that_cannot_be_written_over_stays_and_the_log_says_so() {
    let d = Direct::start(Opts {
        scenario: "report_green",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    let marker = b"{\"forged\":true}".to_vec();
    d.aws.put(&keys::test_report_key(RUN), marker.clone());
    d.aws.fail_overwrites(&keys::test_report_key(RUN), u32::MAX);
    let resp = run_with(&d, settings()).await;
    // The refused create-only put, then the replacing put and its two more tries.
    assert_eq!(d.aws.put_attempts(&keys::test_report_key(RUN)), 4);
    assert_eq!(d.aws.get(&keys::test_report_key(RUN)), Some(marker));
    assert_eq!(resp.outcome, TurnOutcome::Failed);
    assert_eq!(d.aws.progress("upload-conflict").len(), 1);
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.starts_with("direct report not replaced:")
                && l.contains("could not be written over")),
        "{:?}",
        d.log.lines()
    );
}

/// A report put that reached S3 and whose answer was lost is found again by its checksum on the retry's 412: it counts as stored, with no
/// conflict, and the run ends as Cline left it.
#[tokio::test(flavor = "multi_thread")]
async fn a_test_report_put_whose_answer_was_lost_counts_as_stored_because_the_object_is_agentds_own()
 {
    for how in [PutFailure::Status500, PutFailure::ResetBody] {
        let d = Direct::start(Opts {
            scenario: "report_green",
            timeout_s: 3600,
            ..Opts::default()
        })
        .await;
        d.aws
            .fail_puts_after_store(&keys::test_report_key(RUN), 1, how);
        let resp = run_with(&d, settings()).await;
        // TRIGGER PRESENT: the first put was committed and its answer lost, so the retry met the stored object.
        assert_eq!(
            d.aws.put_attempts(&keys::test_report_key(RUN)),
            2,
            "{how:?}"
        );
        assert_eq!(
            d.aws.head_attempts(&keys::test_report_key(RUN)),
            1,
            "{how:?}"
        );
        assert_eq!(d.aws.progress("upload-conflict").len(), 0, "{how:?}");
        assert_eq!(resp.outcome, TurnOutcome::Completed, "{how:?}: {resp:?}");
        assert_eq!(resp.error, None, "{how:?}");
        let r = report_of(&d).expect("the report is in the bucket");
        assert_eq!(r.status, TestReportStatus::Ran, "{how:?}");
        assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone, "{how:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_report_never_changes_the_label_the_head_or_the_turn_complete_request() {
    let with = Direct::start(Opts {
        scenario: "report_red",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    let without = Direct::start(Opts {
        scenario: "report_red",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    let a = run_with(&with, settings()).await;
    let b = agentd::direct::run_with(
        &without.app,
        without.req.clone(),
        &Deps {
            hooks: Some(Hooks::none()),
            ..Deps::default()
        },
    )
    .await;
    // A red report: the run still reads as Cline left it.
    let r = report_of(&with).unwrap();
    assert_eq!(r.steps[0].exit_code, Some(1));
    assert_eq!(r.summary().claim(), None);
    assert!(report_of(&without).is_none());
    assert_eq!(with.summary().outcome, DirectOutcome::ReportedDone);
    assert_eq!(with.summary().outcome, without.summary().outcome);
    assert_eq!(with.summary().label, without.summary().label);
    assert_eq!(a.outcome, b.outcome);
    assert_eq!(a.stop_reason, b.stop_reason);
    assert_eq!(a.error, b.error);
    assert_eq!(a.summary, b.summary);
    assert_eq!(a.bundle_uri, b.bundle_uri);
    assert_eq!(
        with.summary().git.files_changed,
        without.summary().git.files_changed
    );
    let (ca, cb) = (with.aws.complete(), without.aws.complete());
    assert_eq!(
        (ca.len(), cb.len()),
        (1, 1),
        "turn-complete was posted once in each"
    );
    assert_eq!(ca[0].finish_reason, cb[0].finish_reason);
    assert_eq!(ca[0].outcome, cb[0].outcome);
    assert_eq!(ca[0].error, cb[0].error);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_command_is_reported_with_its_exit_code_and_counts() {
    let d = Direct::start(Opts {
        scenario: "report_red",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    run_with(&d, settings()).await;
    let r = report_of(&d).unwrap();
    assert_eq!(r.status, TestReportStatus::Ran);
    let s = &r.steps[0];
    assert_eq!(s.exit_code, Some(1));
    assert_eq!(
        s.counts,
        Some(TestCounts {
            passed: 3,
            failed: 1,
            skipped: 0
        })
    );
    assert!(
        r.summary().sentence().contains("exited 1"),
        "{}",
        r.summary().sentence()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_whose_program_the_worker_lacks_is_not_run_and_the_sentence_says_so() {
    // The worker has no Rust toolchain, so the `cargo test` that a Cargo.toml gives a repository exits 127. The stand-in program does not
    // exist on any host. The step is NOT RUN with the program named, and the sentence of the card does not say that a test exited 127.
    let d = Direct::start(Opts {
        scenario: "report_missing_tool",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    run_with(&d, settings()).await;
    let r = report_of(&d).expect("a report was uploaded");
    assert_eq!(r.status, TestReportStatus::Ran);
    let s = &r.steps[0];
    assert!(s.is_not_run(), "{s:?}");
    assert_eq!(
        s.exit_code,
        Some(127),
        "the attempt's code stays on the step"
    );
    assert_eq!(
        s.not_run_reason(),
        Some("no-such-tool-for-the-report is not installed here")
    );
    let summary = r.summary();
    assert_eq!(summary.claim(), None);
    assert_eq!(
        summary.sentence(),
        "NOT RUN: `no-such-tool-for-the-report test --workspace` (no-such-tool-for-the-report is not installed here)."
    );
    assert!(!summary.sentence().contains("127"));
    assert_eq!(summary.failed_command, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_that_runs_past_the_time_limit_is_stopped_and_the_run_still_ends_as_it_did() {
    let d = Direct::start(Opts {
        scenario: "report_slow",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    let t = Instant::now();
    let resp = run_with(
        &d,
        Settings {
            enabled: true,
            budget: Duration::from_secs(4),
            step: Duration::from_secs(2),
            tail: harness_protocol::direct::DEFAULT_REPORT_TAIL_BYTES,
        },
    )
    .await;
    assert!(t.elapsed() < Duration::from_secs(60), "{:?}", t.elapsed());
    let r = report_of(&d).unwrap();
    assert_eq!(r.status, TestReportStatus::TimedOut);
    assert!(r.steps[0].timed_out);
    assert_eq!(r.steps[0].exit_code, None);
    assert_eq!(r.summary().claim(), None);
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    assert!(resp.bundle_uri.is_some());
}

// ---- the placement: the module works in the driver's `on_report` step, not in the 15-second finalize hook --------

/// Runs the module with the driver's own limits, as the product does: `on_finalize` is cut off after 15 s.
async fn run_with_product_limits(d: &Direct, s: Settings) -> (InvocationResponse, Duration) {
    assert_eq!(Limits::default().finalize, Duration::from_secs(15));
    let hooks = Hooks::new(vec![Arc::new(ReportHook::new(s)) as Arc<dyn DirectHook>]);
    let deps = Deps {
        hooks: Some(hooks),
        ..Deps::default()
    };
    let t = Instant::now();
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    (resp, t.elapsed())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_step_that_takes_longer_than_the_finalize_limit_still_produces_a_report() {
    // `report_slow` leaves `sleep 120`; the step is stopped at 18 s, three seconds past the finalize hook's 15 s ceiling.
    let d = Direct::start(Opts {
        scenario: "report_slow",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    let (resp, took) = run_with_product_limits(
        &d,
        Settings {
            enabled: true,
            budget: Duration::from_secs(40),
            step: Duration::from_secs(18),
            tail: harness_protocol::direct::DEFAULT_REPORT_TAIL_BYTES,
        },
    )
    .await;
    // TRIGGER PRESENT: the report step lasted longer than the 15 s a finalize hook gets.
    let r = report_of(&d).expect("a report was uploaded although the step took longer than 15 s");
    assert!(took >= Duration::from_secs(17), "{took:?}");
    assert_eq!(r.status, TestReportStatus::TimedOut);
    assert!(r.steps[0].timed_out, "{r:?}");
    assert!(
        r.steps[0].duration_ms >= 15_000,
        "{}",
        r.steps[0].duration_ms
    );
    // Whatever the report took, the run ended as it did and the head was delivered.
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    assert!(resp.bundle_uri.is_some());
    assert_eq!(
        d.aws.complete().len(),
        1,
        "turn-complete is posted once, after the report"
    );
}

// ---- the budget follows the run's deadline -------------------------------------------------------------

const SKIPPED: &str = "test report skipped: not enough time before the run deadline";

#[tokio::test(flavor = "multi_thread")]
async fn a_run_100_seconds_from_its_deadline_gets_a_report_that_says_it_was_skipped() {
    // The run deadline is 100 s after the start, so the report step begins about 95 s before it: less than the 180 s margin plus the 60 s
    // floor.
    let d = Direct::start(Opts {
        scenario: "report_green",
        timeout_s: 100,
        ..Opts::default()
    })
    .await;
    let t = Instant::now();
    let resp = run_with(&d, settings()).await;
    // TRIGGER PRESENT: the run ended on its own (not at the deadline) and a head was delivered, so the step was asked.
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    assert!(resp.bundle_uri.is_some());
    let r = report_of(&d).expect("a report that says it was skipped is uploaded");
    assert_eq!(r.status, TestReportStatus::NotRun, "{r:?}");
    assert!(r.steps.is_empty(), "no command ran: {r:?}");
    assert_eq!(r.reason.as_deref(), Some(SKIPPED));
    assert!(t.elapsed() < Duration::from_secs(60), "{:?}", t.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_1000_seconds_from_its_deadline_gets_its_report() {
    let d = Direct::start(Opts {
        scenario: "report_green",
        timeout_s: 1000,
        ..Opts::default()
    })
    .await;
    run_with(&d, settings()).await;
    let r = report_of(&d).expect("a report was uploaded");
    assert_eq!(r.status, TestReportStatus::Ran, "{r:?}");
    assert_eq!(r.steps.len(), 1, "{r:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_step_is_bounded_by_what_is_left_of_the_overall_budget() {
    // `report_slow` leaves `sleep 120`. The step limit is 300 s, the overall budget 6 s: the step is stopped at the budget, not at 300 s.
    let d = Direct::start(Opts {
        scenario: "report_slow",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    run_with(
        &d,
        Settings {
            enabled: true,
            budget: Duration::from_secs(6),
            step: Duration::from_secs(300),
            tail: harness_protocol::direct::DEFAULT_REPORT_TAIL_BYTES,
        },
    )
    .await;
    let r = report_of(&d).expect("a report was uploaded");
    assert_eq!(r.status, TestReportStatus::TimedOut, "{r:?}");
    assert!(r.steps[0].timed_out);
    assert!(
        r.steps[0].duration_ms <= 7_000,
        "{}",
        r.steps[0].duration_ms
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tree_that_names_no_test_command_gets_a_report_that_says_so() {
    // The base tree's plain pytest file is a layout the report runs, so this tree has it under another name.
    let d = Direct::start(Opts {
        scenario: "report_none",
        timeout_s: 3600,
        repo_moved: vec![("tests/test_calc.py", "tests/calc_check.py")],
        ..Opts::default()
    })
    .await;
    run_with(&d, settings()).await;
    let r = report_of(&d).unwrap();
    assert_eq!(r.status, TestReportStatus::NoCommand);
    assert!(r.steps.is_empty());
    assert!(
        r.reason
            .unwrap()
            .starts_with("it looked for .ham/test-command")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_request_member_and_the_stage_switch_each_turn_the_report_off() {
    let off_by_request = Direct::start(Opts {
        scenario: "report_green",
        report_tests: Some(false),
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    run_with(&off_by_request, settings()).await;
    assert!(
        report_of(&off_by_request).is_none(),
        "reportTests false: nothing is made"
    );
    assert_eq!(
        off_by_request.summary().outcome,
        DirectOutcome::ReportedDone
    );

    let off_by_stage = Direct::start(Opts {
        scenario: "report_green",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    run_with(
        &off_by_stage,
        Settings {
            enabled: false,
            ..settings()
        },
    )
    .await;
    assert!(
        report_of(&off_by_stage).is_none(),
        "AGENTD_REPORT_TESTS=off: nothing is made"
    );

    let on = Direct::start(Opts {
        scenario: "report_green",
        report_tests: Some(true),
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    run_with(&on, settings()).await;
    assert!(report_of(&on).is_some(), "reportTests true is the default");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_never_started_or_was_cancelled_makes_no_report() {
    let no_bundle = Direct::start(Opts {
        scenario: "report_green",
        no_bundle: true,
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    run_with(&no_bundle, settings()).await;
    assert_eq!(
        no_bundle.summary().outcome,
        DirectOutcome::StoppedSetupFailed
    );
    assert!(report_of(&no_bundle).is_none());

    let cancelled = Direct::start(Opts {
        scenario: "report_green",
        engine_env: vec![("FAKE_ACP_HOLD_PROMPTS", "1".into())],
        timeout_s: 60,
        ..Opts::default()
    })
    .await;
    cancelled.aws.cancel_on(EventType::TurnStarted);
    run_with(&cancelled, settings()).await;
    assert_eq!(cancelled.summary().outcome, DirectOutcome::StoppedByUser);
    assert!(
        report_of(&cancelled).is_none(),
        "the user stopped the run: no report"
    );
}

/// Puts an object under the report key of a fresh run, so a test can tell whether agentd wrote there; returns its bytes.
fn put_report_first(d: &Direct) -> Vec<u8> {
    let marker = b"{\"forged\":true}".to_vec();
    d.aws.put(&keys::test_report_key(RUN), marker.clone());
    marker
}

fn assert_report_object_untouched(d: &Direct, marker: &[u8], case: &str) {
    assert_eq!(
        d.aws.put_attempts(&keys::test_report_key(RUN)),
        0,
        "{case}: agentd wrote nothing under the report key"
    );
    assert_eq!(
        d.aws.get(&keys::test_report_key(RUN)).as_deref(),
        Some(marker),
        "{case}: the object that was there first is still the current one"
    );
}

/// The report step is the only writer of `test-report.json`, so an object that was under the key before the end sequence stays the current
/// one whenever the step does not run, and run-session reads that key whoever wrote it (`crates/agentd/AGENTS.md`, the end sequence;
/// `crates/run-session/AGENTS.md`). The step does not run after a bundle or summary conflict (agentd records it and writes its own object
/// over the foreign one, and the run already ends as a conflict), with the stage switch off, for a run that never got a head, and after the
/// user's cancel. A run that ran into its deadline is `direct_report_seam.rs`.
#[tokio::test(flavor = "multi_thread")]
async fn a_report_object_that_was_there_first_stays_current_when_the_report_step_does_not_run() {
    for (taken, object) in [
        (keys::bundle_key(RUN), "turns/1/bundle"),
        (keys::summary_key(RUN), "turns/1/summary.json"),
    ] {
        let d = Direct::start(Opts {
            scenario: "report_green",
            timeout_s: 3600,
            ..Opts::default()
        })
        .await;
        d.aws.put(&taken, b"taken first".to_vec());
        let marker = put_report_first(&d);
        let resp = run_with(&d, settings()).await;
        // TRIGGER PRESENT: the conflict happened and agentd wrote its own object over it, so only the skipped step left the report alone.
        assert_eq!(
            d.aws.put_attempts(&taken),
            2,
            "{object}: the refused create-only put, then the replacing put"
        );
        let conflicts = d.aws.progress("upload-conflict");
        assert_eq!(conflicts.len(), 1, "{object}: {conflicts:?}");
        assert_eq!(conflicts[0].1["object"], object);
        assert_eq!(resp.outcome, TurnOutcome::Failed, "{object}: {resp:?}");
        assert!(
            d.log.lines().iter().any(|l| {
                l == "direct turn report step skipped: an object of the run already existed"
            }),
            "{object}: {:?}",
            d.log.lines()
        );
        assert_report_object_untouched(&d, &marker, object);
    }

    let off = Direct::start(Opts {
        scenario: "report_green",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    let marker = put_report_first(&off);
    run_with(
        &off,
        Settings {
            enabled: false,
            ..settings()
        },
    )
    .await;
    assert_eq!(off.summary().outcome, DirectOutcome::ReportedDone);
    assert!(off.aws.progress("upload-conflict").is_empty());
    assert_report_object_untouched(&off, &marker, "AGENTD_REPORT_TESTS=off");

    let no_bundle = Direct::start(Opts {
        scenario: "report_green",
        no_bundle: true,
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    let marker = put_report_first(&no_bundle);
    run_with(&no_bundle, settings()).await;
    assert_eq!(
        no_bundle.summary().outcome,
        DirectOutcome::StoppedSetupFailed
    );
    assert_report_object_untouched(&no_bundle, &marker, "a run with no head");

    let cancelled = Direct::start(Opts {
        scenario: "report_green",
        engine_env: vec![("FAKE_ACP_HOLD_PROMPTS", "1".into())],
        timeout_s: 60,
        ..Opts::default()
    })
    .await;
    cancelled.aws.cancel_on(EventType::TurnStarted);
    let marker = put_report_first(&cancelled);
    run_with(&cancelled, settings()).await;
    assert_eq!(cancelled.summary().outcome, DirectOutcome::StoppedByUser);
    assert_report_object_untouched(&cancelled, &marker, "the user's cancel");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_upload_that_fails_loses_the_report_and_nothing_else() {
    let d = Direct::start(Opts {
        scenario: "report_green",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    d.aws.fail_puts("test-report.json", 20);
    let resp = run_with(&d, settings()).await;
    assert!(report_of(&d).is_none());
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    assert!(resp.bundle_uri.is_some());
    assert_eq!(d.aws.complete().len(), 1, "turn-complete was posted");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_report_names_no_secret_of_the_session() {
    let d = Direct::start(Opts {
        scenario: "report_green",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    run_with(&d, settings()).await;
    let text = String::from_utf8(d.aws.get(&keys::test_report_key(RUN)).unwrap()).unwrap();
    direct_support::no_secrets(&text);
}

#[test]
fn the_settings_come_from_the_environment_with_the_documented_defaults() {
    let none = |_: &str| None;
    assert_eq!(
        Settings::from_vars(&none),
        Settings {
            enabled: true,
            budget: Duration::from_secs(480),
            step: Duration::from_secs(300),
            tail: harness_protocol::direct::DEFAULT_REPORT_TAIL_BYTES,
        }
    );
    let get = |k: &str| match k {
        "AGENTD_REPORT_TESTS" => Some("off".to_string()),
        "AGENTD_REPORT_BUDGET_S" => Some("90".to_string()),
        "AGENTD_REPORT_STEP_S" => Some("45".to_string()),
        _ => None,
    };
    assert_eq!(
        Settings::from_vars(&get),
        Settings {
            enabled: false,
            budget: Duration::from_secs(90),
            step: Duration::from_secs(45),
            tail: harness_protocol::direct::DEFAULT_REPORT_TAIL_BYTES,
        }
    );
    for off in ["0", "off", "false", "no", "OFF", "False"] {
        let g = |k: &str| (k == "AGENTD_REPORT_TESTS").then(|| off.to_string());
        assert!(!Settings::from_vars(&g).enabled, "{off}");
    }
    for bad in ["", "0", "abc", "-5", "99999"] {
        let g = |k: &str| {
            (k == "AGENTD_REPORT_BUDGET_S" || k == "AGENTD_REPORT_STEP_S").then(|| bad.to_string())
        };
        let s = Settings::from_vars(&g);
        assert_eq!(
            (s.budget, s.step),
            (Duration::from_secs(480), Duration::from_secs(300)),
            "{bad:?} is the default"
        );
    }
}

// ---- a full disk ---------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_step_that_runs_out_of_disk_is_named_with_its_line_and_the_steps_after_it_are_skipped() {
    // The first command of `.ham/test-command` ends like a cargo build on a full volume (the words and exit 101); the second would pass. The
    // report names the disk, does not say that a test exited 101, and does not run the second command on a disk that is still full.
    let d = Direct::start(Opts {
        scenario: "report_disk_full",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    run_with(&d, settings()).await;
    let r = report_of(&d).expect("a report was uploaded");
    assert_eq!(r.status, TestReportStatus::Ran);
    assert_eq!(r.steps.len(), 2, "{r:?}");
    let first = &r.steps[0];
    assert!(first.is_not_run(), "{first:?}");
    assert_eq!(first.exit_code, Some(101), "the attempt's code stays");
    assert_eq!(
        first.not_run_reason(),
        Some("ran out of disk: couldn't create a temp dir: No space left on device (os error 28)")
    );
    let second = &r.steps[1];
    assert!(second.is_not_run(), "{second:?}");
    assert_eq!(second.exit_code, None);
    assert_eq!(second.duration_ms, 0);
    assert_eq!(second.command, "echo \"4 passed in 0.02s\"");
    assert_eq!(
        second.not_run_reason(),
        Some("skipped: the session ran out of disk in an earlier command")
    );
    let summary = r.summary();
    assert_eq!(summary.claim(), None);
    assert_eq!(summary.failed_command, None);
    let sentence = summary.sentence();
    assert!(sentence.starts_with("NOT RUN: `echo "), "{sentence}");
    assert!(
        sentence.ends_with(
            "(ran out of disk: couldn't create a temp dir: No space left on device (os error 28)), and 1 more."
        ),
        "{sentence}"
    );
    assert!(!sentence.contains("exited"), "{sentence}");
}

// ---- a command chosen from the manifest ----------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_cargo_workspace_without_a_test_command_gets_a_step_that_says_how_it_was_chosen() {
    // The fake engine leaves a Cargo workspace of two members and no `.ham/test-command`, so the report reads the manifest and detects
    // `cargo test`. Whether the worker has a Rust toolchain or not (the direct tool set has none) is not what is pinned here: the step carries the
    // line that says it was chosen from the manifest and how many members it runs, and the summary turns it into the note EVIDENCE.md prints.
    let d = Direct::start(Opts {
        scenario: "report_cargo_workspace",
        timeout_s: 3600,
        repo_moved: vec![("tests/test_calc.py", "tests/calc_check.py")],
        ..Opts::default()
    })
    .await;
    run_with(&d, settings()).await;
    let r = report_of(&d).expect("a report was uploaded");
    assert_eq!(r.steps.len(), 1, "{r:?}");
    let s = &r.steps[0];
    assert_eq!(s.source, TestCommandSource::Detected);
    assert_eq!(s.command, "cargo test");
    assert_eq!(s.plan_note_members(), Some(2), "{s:?}");
    assert!(
        s.output_tail
            .lines()
            .any(|l| l == "PLAN NOTE: cargo workspace, 2 members"),
        "{}",
        s.output_tail
    );
    let summary = r.summary();
    assert_eq!(summary.claim(), None);
    let note = summary.plan_note().expect("the summary has the note");
    assert!(note.contains("2 members"), "{note}");
    assert!(
        !summary.sentence().contains("workspace"),
        "the sentence stays what it was: {}",
        summary.sentence()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_the_repository_names_gets_no_line_about_how_it_was_chosen() {
    let d = Direct::start(Opts {
        scenario: "report_green",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    run_with(&d, settings()).await;
    let r = report_of(&d).expect("a report was uploaded");
    assert_eq!(r.steps[0].plan_note_members(), None);
    assert!(!r.steps[0].output_tail.contains("PLAN NOTE"));
    assert_eq!(r.summary().plan_note(), None);
}
