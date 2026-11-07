//! Protocol 1.41.0: the time limit of the clean-checkout test report is a setting, the report says how long a stopped command
//! ran, and the worker tells the event stream what the report is doing. These tests pin the request member `reportStepS`, the limit
//! each command was given (`TestStep.limitS`), the command that was stopped (`TestReportSummary.timedOut`), the sentences for a stop,
//! and the progress payload of the `turn.progress` events of kind `report`.
//!
#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{
    DEFAULT_REPORT_STEP_S, MAX_REPORT_STEP_S, PROGRESS_REPORT, ReportProgress, ReportProgressState,
    TestCommandSource, TestReport, TestReportStatus, TestStep, TestStepRole, TimedOutCommand,
    banned_claim_word, report_step_s_ok,
};
use serde_json::{Value, json};

const T3: &str = include_str!("fixtures/dogfood-t3-test-report.json");
const T3_SLOW: &str = "cd scripts/tests && python3 -m unittest test_docs_set test_docs_setup test_dox_lint test_internal_names test_password_scan test_lifecycle_goal_card test_lifecycle_pack test_lifecycle_lock test_lifecycle_check test_remote_job test_aws_target";

fn step(command: &str, exit: Option<i32>, duration_ms: u64, limit_s: Option<u32>) -> TestStep {
    TestStep {
        role: TestStepRole::Test,
        source: TestCommandSource::Detected,
        command: command.to_string(),
        exit_code: exit,
        timed_out: exit.is_none(),
        duration_ms,
        limit_s,
        counts: None,
        counts_from: None,
        output_tail: String::new(),
        output_truncated: false,
        logged: None,
    }
}

fn report(status: TestReportStatus, steps: Vec<TestStep>) -> TestReport {
    TestReport {
        schema_version: 1,
        run_id: "01M4XTESTRUNDIRECT00000001".into(),
        head_sha: "0123456789abcdef0123456789abcdef01234567".into(),
        status,
        reason: None,
        started_at: None,
        duration_ms: 1_000,
        steps,
        summary: None,
    }
}

fn request_json() -> Value {
    json!({"prompt": "p", "target": {"mode": "s3-only"}, "model": "global.anthropic.claude-sonnet-5-5"})
}

// ---- the version ------------------------------------------------------------------------------------------------------------

#[test]
fn this_change_is_in_the_protocol_line_of_1_42_0() {
    // The report card came in with 1.41.0; the stall limits of the goal loop (1.42.0) follow it on the same line, so the test names the
    // version the crate has now. A bump changes this literal like the one of every other wire test.
    assert_eq!(PROTOCOL_VERSION, "1.56.0");
}

// ---- the request member -----------------------------------------------------------------------------------------------------

#[test]
fn a_request_without_report_step_s_has_the_bytes_it_had_before() {
    let r: StartDirectRunRequest = serde_json::from_value(request_json()).unwrap();
    assert_eq!(r.report_step_s, None);
    let v = serde_json::to_value(&r).unwrap();
    assert!(v.get("reportStepS").is_none(), "{v}");
}

#[test]
fn report_step_s_round_trips_as_seconds() {
    let mut j = request_json();
    j["reportStepS"] = json!(360);
    let r: StartDirectRunRequest = serde_json::from_value(j).unwrap();
    assert_eq!(r.report_step_s, Some(360));
    let back = serde_json::to_value(&r).unwrap();
    assert_eq!(back["reportStepS"], 360);
    assert_eq!(
        serde_json::from_value::<StartDirectRunRequest>(back).unwrap(),
        r
    );
}

#[test]
fn the_step_limit_is_one_second_to_the_session_bound_and_the_default_is_five_minutes() {
    assert_eq!(DEFAULT_REPORT_STEP_S, 300);
    assert_eq!(MAX_REPORT_STEP_S, 27_900);
    assert!(!report_step_s_ok(0));
    assert!(report_step_s_ok(1));
    assert!(report_step_s_ok(300));
    assert!(report_step_s_ok(3_601));
    assert!(report_step_s_ok(27_900));
    assert!(!report_step_s_ok(27_901));
}

// ---- the limit of a command -------------------------------------------------------------------------------------------------

#[test]
fn a_command_carries_the_limit_it_was_given_and_an_old_report_has_none() {
    let with = step("make test", Some(0), 900, Some(300));
    let v = serde_json::to_value(&with).unwrap();
    assert_eq!(v["limitS"], 300);
    let back: TestStep = serde_json::from_value(v).unwrap();
    assert_eq!(back, with);
    let without = serde_json::to_value(step("make test", Some(0), 900, None)).unwrap();
    assert!(without.get("limitS").is_none(), "{without}");
    let t3: TestReport = serde_json::from_str(T3).unwrap();
    assert!(t3.steps.iter().all(|s| s.limit_s.is_none()));
}

#[test]
fn a_command_that_was_not_run_has_no_limit() {
    let s = TestStep::not_run(
        TestStepRole::Test,
        TestCommandSource::Detected,
        "docker compose up",
        "no container runtime",
        None,
    );
    assert_eq!(s.limit_s, None);
}

// ---- the command that was stopped -------------------------------------------------------------------------------------------

#[test]
fn the_dogfood_report_says_which_command_timed_out_and_after_how_many_seconds() {
    let t3: TestReport = serde_json::from_str(T3).unwrap();
    let s = t3.summary();
    assert_eq!(s.status, TestReportStatus::TimedOut);
    assert_eq!(
        s.timed_out,
        Some(TimedOutCommand {
            command: T3_SLOW.to_string(),
            after_s: 300
        })
    );
    assert_eq!(
        s.sentence(),
        format!(
            "`{T3_SLOW}` timed out after 300 s; the commands that finished are listed in the report."
        )
    );
    assert_eq!(banned_claim_word(&s.sentence()), None);
}

#[test]
fn the_limit_the_worker_recorded_wins_over_the_time_the_command_took() {
    let mut r = report(
        TestReportStatus::TimedOut,
        vec![step("make slow", None, 303_400, Some(300))],
    );
    assert_eq!(r.summary().timed_out.unwrap().after_s, 300);
    // No limit recorded (an older worker): the time the command ran, to the nearest second.
    r.steps[0].limit_s = None;
    r.steps[0].duration_ms = 299_600;
    assert_eq!(r.summary().timed_out.unwrap().after_s, 300);
    r.steps[0].duration_ms = 61_400;
    assert_eq!(r.summary().timed_out.unwrap().after_s, 61);
    // A stop is at least one second, never "after 0 s".
    r.steps[0].duration_ms = 120;
    assert_eq!(r.summary().timed_out.unwrap().after_s, 1);
}

#[test]
fn the_stopped_command_is_named_even_when_an_earlier_command_failed() {
    let r = report(
        TestReportStatus::TimedOut,
        vec![
            step("make unit", Some(1), 5_000, Some(300)),
            step("make slow", None, 300_010, Some(300)),
        ],
    );
    let s = r.summary();
    assert_eq!(s.failed_command.as_deref(), Some("make unit"));
    assert_eq!(s.timed_out.as_ref().unwrap().command, "make slow");
    assert_eq!(
        s.sentence(),
        "`make slow` timed out after 300 s; the commands that finished are listed in the report."
    );
}

#[test]
fn a_report_whose_time_ran_out_between_commands_says_so_and_names_no_command() {
    let r = report(
        TestReportStatus::TimedOut,
        vec![step("make unit", Some(0), 5_000, Some(300))],
    );
    let s = r.summary();
    assert_eq!(s.timed_out, None);
    assert_eq!(
        s.sentence(),
        "The report's time ran out before every command ran; the commands that finished are listed in the report."
    );
}

#[test]
fn a_report_that_ran_to_its_end_has_no_stopped_command() {
    let r = report(
        TestReportStatus::Ran,
        vec![step("make unit", Some(1), 5_000, Some(300))],
    );
    assert_eq!(r.summary().timed_out, None);
    let v = serde_json::to_value(r.summary()).unwrap();
    assert!(v.get("timedOut").is_none(), "{v}");
}

#[test]
fn the_stopped_command_is_on_the_wire_as_timed_out_with_after_s() {
    let t3: TestReport = serde_json::from_str(T3).unwrap();
    let v = serde_json::to_value(t3.summary()).unwrap();
    assert_eq!(v["timedOut"]["afterS"], 300);
    assert_eq!(v["timedOut"]["command"], T3_SLOW);
}

// ---- the progress of the report ---------------------------------------------------------------------------------------------

#[test]
fn the_progress_event_is_a_turn_progress_payload_of_kind_report() {
    assert_eq!(PROGRESS_REPORT, "report");
    let p = ReportProgress::running(2, 3, "make slow", 300, "Running the repository's tests");
    let v = p.to_value();
    assert_eq!(v["kind"], "report");
    assert_eq!(v["state"], "running");
    assert_eq!(v["step"], 2);
    assert_eq!(v["of"], 3);
    assert_eq!(v["command"], "make slow");
    assert_eq!(v["limitS"], 300);
    assert_eq!(v["text"], "Running the repository's tests");
    assert!(v.get("status").is_none(), "{v}");
    assert_eq!(ReportProgress::from_value(&v), Some(p));
}

#[test]
fn the_checkout_and_end_payloads_leave_out_what_they_do_not_have() {
    let c = ReportProgress::checkout("Making a clean checkout").to_value();
    assert_eq!(c["state"], "checkout");
    for k in ["step", "of", "command", "limitS", "status"] {
        assert!(c.get(k).is_none(), "{k}: {c}");
    }
    let e = ReportProgress::ended(TestReportStatus::TimedOut, "The tests ended").to_value();
    assert_eq!(e["state"], "ended");
    assert_eq!(e["status"], "timed_out");
    assert!(e.get("step").is_none(), "{e}");
}

#[test]
fn a_reader_ignores_a_payload_it_does_not_know() {
    // Another kind, a state from a later version and a payload with no state are not this client's to show.
    assert_eq!(
        ReportProgress::from_value(&json!({"kind": "skills", "state": "running"})),
        None
    );
    assert_eq!(
        ReportProgress::from_value(&json!({"kind": "report", "state": "paused"})),
        None
    );
    assert_eq!(ReportProgress::from_value(&json!({"kind": "report"})), None);
    // An unknown extra member is ignored, and the text may be missing.
    let p = ReportProgress::from_value(&json!({"kind": "report", "state": "ended", "later": 1}))
        .unwrap();
    assert_eq!(p.state, ReportProgressState::Ended);
    assert_eq!(p.text, "");
}

#[test]
fn the_command_and_the_text_of_a_progress_payload_are_cut() {
    let long = "x".repeat(900);
    let p = ReportProgress::running(1, 1, &long, 300, &long);
    assert!(p.command.as_deref().unwrap().chars().count() <= 120);
    assert!(p.text.chars().count() <= 200);
}
