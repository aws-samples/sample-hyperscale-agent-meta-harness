//! An install step that fails stops only the test commands that need it, and the sentence of the report says what ran.
//!
//! Since 5.15 the report put every install step in front of the test commands of the lifecycle log and stopped at the first that failed: a failed
//! `npm ci` left `go test ./...` not run. Now the steps that do not need the install run, and the sentence names the install that failed, the
//! command it kept from running and what the runner reported for the commands that ran. The claim stays away while a setup step failed.
#![allow(clippy::unwrap_used)]

use harness_protocol::direct::{
    TestCommandSource, TestCounts, TestReport, TestReportStatus, TestStep, TestStepRole,
};

const RUN: &str = "01M4XTESTRUNDIRECT00000001";
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
const VENV_INSTALL: &str = "python3 -m venv --system-site-packages .ham-venv && .ham-venv/bin/python -m pip install -q -r requirements.txt";

fn ran(role: TestStepRole, command: &str, exit: i32, counts: Option<(u32, u32)>) -> TestStep {
    TestStep {
        role,
        source: if role == TestStepRole::Setup {
            TestCommandSource::Detected
        } else {
            TestCommandSource::LifecycleLog
        },
        command: command.to_string(),
        exit_code: Some(exit),
        timed_out: false,
        duration_ms: 900,
        limit_s: Some(300),
        counts: counts.map(|(passed, failed)| TestCounts {
            passed,
            failed,
            skipped: 0,
        }),
        counts_from: counts.map(|_| "go".to_string()),
        output_tail: String::new(),
        output_truncated: false,
        logged: None,
    }
}

fn skipped(command: &str, setup: &str) -> TestStep {
    TestStep::not_run(
        TestStepRole::Test,
        TestCommandSource::LifecycleLog,
        command,
        &format!("skipped: the setup step `{setup}` failed"),
        None,
    )
}

fn report(steps: Vec<TestStep>) -> TestReport {
    let r = TestReport {
        schema_version: 1,
        run_id: RUN.into(),
        head_sha: HEAD.into(),
        status: TestReportStatus::Ran,
        reason: None,
        started_at: None,
        duration_ms: 2_000,
        steps,
        summary: None,
    };
    r.validate(RUN, HEAD).unwrap();
    r
}

#[test]
fn b831_a_failed_install_names_the_command_it_stopped_and_what_the_others_reported() {
    let s = report(vec![
        ran(TestStepRole::Test, "go test -v ./...", 0, Some((12, 0))),
        ran(TestStepRole::Setup, "npm ci", 1, None),
        skipped("npm test", "npm ci"),
    ])
    .summary();
    assert!(s.setup_failed, "{s:?}");
    assert_eq!(s.failed_command.as_deref(), Some("npm ci"));
    assert_eq!(s.exit_code, Some(1));
    assert_eq!(s.claim(), None);
    assert_eq!(
        s.sentence(),
        "Setup `npm ci` exited 1 in a clean checkout, so `npm test` did not run; for the test commands that ran, the runner reported 12 passed, 0 failed."
    );
}

#[test]
fn b831_more_than_one_command_stopped_by_the_install_is_counted() {
    let s = report(vec![
        ran(TestStepRole::Setup, "npm ci", 1, None),
        skipped("npm test", "npm ci"),
        skipped("npm run test:e2e", "npm ci"),
        ran(TestStepRole::Test, "go test -v ./...", 0, None),
    ])
    .summary();
    assert_eq!(
        s.sentence(),
        "Setup `npm ci` exited 1 in a clean checkout, so `npm test` and 1 more did not run; for the test commands that ran, the runner's test counts could not be read."
    );
}

#[test]
fn b831_tests_that_ran_without_a_failed_install_say_so() {
    // The report's virtual environment could not be made, so the logged Python command ran as the log wrote it, with the image's own Python.
    let s = report(vec![
        ran(TestStepRole::Setup, VENV_INSTALL, 1, None),
        ran(TestStepRole::Test, "python3 -m pytest -q", 0, Some((5, 0))),
    ])
    .summary();
    assert!(s.setup_failed);
    assert_eq!(s.claim(), None);
    assert_eq!(
        s.sentence(),
        format!(
            "Setup `{VENV_INSTALL}` exited 1 in a clean checkout, so the test commands ran without it; the runner reported 5 passed, 0 failed."
        )
    );
}

#[test]
fn b831_a_test_that_failed_before_the_install_failed_leads_the_sentence() {
    let s = report(vec![
        ran(TestStepRole::Test, "go test -v ./...", 1, Some((3, 1))),
        ran(TestStepRole::Setup, "npm ci", 1, None),
        skipped("npm test", "npm ci"),
    ])
    .summary();
    assert!(!s.setup_failed, "the first failure is a test: {s:?}");
    assert_eq!(s.failed_command.as_deref(), Some("go test -v ./..."));
    assert_eq!(
        s.sentence(),
        "`go test -v ./...` exited 1 in a clean checkout; the runner reported 3 passed, 1 failed."
    );
}

#[test]
fn b831_an_install_that_stopped_every_test_keeps_the_sentence_of_before() {
    let s = report(vec![
        ran(TestStepRole::Setup, "npm ci", 1, None),
        skipped("npm test", "npm ci"),
    ])
    .summary();
    assert!(s.setup_failed);
    assert_eq!(
        s.sentence(),
        "Setup `npm ci` exited 1 in a clean checkout, so the test command did not run."
    );
}
