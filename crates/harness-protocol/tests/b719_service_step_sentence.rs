//! The card sentence of a step that needed a service the clean checkout did not have names that step and the service:
//! the web sample's card read ``Tests `bash scripts/test-integration.sh` exited 1 in a clean checkout; the runner reported 74
//! passed, 4 failed.``, with nothing about the PostgreSQL service of its Compose file. Since the report starts a local server in place
//! of the Compose service and a step that still could not reach it carries a NEEDS head line; this test pins that the sentence names the step and the
//! service.
#![allow(clippy::unwrap_used)]

use harness_protocol::direct::{
    NEEDS_PREFIX, STAND_IN_PREFIX, TestCommandSource, TestCounts, TestReport, TestReportStatus,
    TestStep, TestStepRole,
};

fn test_step(command: &str, exit: i32, counts: Option<(u32, u32)>, tail: &str) -> TestStep {
    TestStep {
        role: TestStepRole::Test,
        source: TestCommandSource::Detected,
        command: command.into(),
        exit_code: Some(exit),
        timed_out: false,
        duration_ms: 10,
        limit_s: Some(300),
        counts: counts.map(|(p, f)| TestCounts {
            passed: p,
            failed: f,
            skipped: 0,
        }),
        counts_from: counts.map(|_| "pytest".to_string()),
        output_tail: tail.into(),
        output_truncated: false,
        logged: None,
    }
}

fn service_step(name: &str, started: bool) -> TestStep {
    let sentence = format!(
        "compose service db (postgres:16 in compose.yaml) has a local {name} server in its place; the Compose container does not run"
    );
    let mut s = TestStep {
        role: TestStepRole::Setup,
        source: TestCommandSource::Detected,
        command: format!("ham-services {name}"),
        exit_code: Some(0),
        timed_out: false,
        duration_ms: 900,
        limit_s: Some(150),
        counts: None,
        counts_from: None,
        output_tail: format!(
            "{STAND_IN_PREFIX}{sentence}\nham-services: postgres ready on 127.0.0.1:5432"
        ),
        output_truncated: false,
        logged: None,
    };
    if !started {
        s = TestStep::not_run(
            s.role,
            s.source,
            &s.command,
            "ham-services is not installed here",
            None,
        );
        s.output_tail =
            format!("NOT RUN: ham-services is not installed here\n{STAND_IN_PREFIX}{sentence}");
    }
    s
}

fn report(steps: Vec<TestStep>) -> TestReport {
    TestReport {
        schema_version: 1,
        run_id: "R1".into(),
        head_sha: "a".repeat(40),
        status: TestReportStatus::Ran,
        reason: None,
        started_at: None,
        duration_ms: 1000,
        steps,
        summary: None,
    }
}

#[test]
fn b719_the_card_names_the_integration_step_that_ran_without_postgres() {
    let failing = test_step(
        "bash scripts/test-integration.sh",
        1,
        Some((74, 4)),
        &format!("{NEEDS_PREFIX}postgres: it could not be started\nConnection refused"),
    );
    let s = report(vec![service_step("postgres", false), failing]).summary();
    assert_eq!(
        s.sentence(),
        "`bash scripts/test-integration.sh` exited 1 in a clean checkout while postgres was not available; the runner reported 74 passed, 4 failed."
    );
}

#[test]
fn b719_the_card_of_a_step_that_got_its_service_claims_with_the_service_named() {
    let ok = test_step(
        "bash scripts/test-integration.sh",
        0,
        Some((78, 0)),
        "78 passed",
    );
    let s = report(vec![service_step("postgres", true), ok]).summary();
    assert_eq!(
        s.sentence(),
        "repository tests pass in a clean checkout: 78 of 78. The report started a local postgres server in place of the Compose service the repository declares."
    );
}
