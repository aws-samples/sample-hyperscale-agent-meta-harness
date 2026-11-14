//! EVIDENCE.md says which test commands of the lifecycle log the report's plan left out.
//!
//! The report carries the fact at the head of a step's output (`PLAN NOTE: in place of ...`, `PLAN NOTE: left out ...`); the summary turns it
//! into detail lines, and the packager prints the detail lines under the sentence. The machine lines themselves stay out of EVIDENCE.md.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use delivery::packager::{PackageRequest, render_evidence};
use harness_protocol::direct::{
    DirectEvidence, DirectOutcome, HeadSource, TestCommandSource, TestCounts, TestReport,
    TestReportStatus, TestStep, TestStepRole, banned_claim_word, in_place_line, left_out_line,
};

const RUN: &str = "01M4X3Q7R9S0V1W2X3Y4Z5A6B7";
const HEAD: &str = "cccccccccccccccccccccccccccccccccccccccc";

fn step(
    role: TestStepRole,
    source: TestCommandSource,
    command: &str,
    exit: i32,
    passed: Option<u32>,
    head: Option<String>,
) -> TestStep {
    TestStep {
        role,
        source,
        command: command.into(),
        exit_code: Some(exit),
        timed_out: false,
        duration_ms: 900,
        limit_s: Some(300),
        counts: passed.map(|passed| TestCounts {
            passed,
            failed: 0,
            skipped: 0,
        }),
        counts_from: passed.map(|_| "pytest".to_string()),
        output_tail: head.map(|h| format!("{h}\n......\n")).unwrap_or_default(),
        output_truncated: false,
        logged: None,
    }
}

fn md_of(steps: Vec<TestStep>) -> String {
    let report = TestReport {
        schema_version: 1,
        run_id: RUN.into(),
        head_sha: HEAD.into(),
        status: TestReportStatus::Ran,
        reason: None,
        started_at: None,
        duration_ms: 1_000,
        steps,
        summary: None,
    };
    report.validate(RUN, HEAD).unwrap();
    render_evidence(&PackageRequest {
        run_id: RUN.into(),
        repo: "hamh-e2e-textstats-20261114".into(),
        base_sha: None,
        head_sha: HEAD.into(),
        objective: "Build a small repository with two parts".into(),
        pr_url: None,
        direct: DirectEvidence {
            outcome: DirectOutcome::ReportedDone,
            label: DirectOutcome::ReportedDone.label().into(),
            detail: None,
            stop_reason: Some("end_turn".into()),
            cline_message: Some("Both suites pass.".into()),
            model_id: "us.anthropic.claude-sonnet-5-5".into(),
            engine_version: None,
            rules: Vec::new(),
            usage: None,
            wall_s: Some(600),
            head_source: HeadSource::Final,
            final_model_id: None,
            model_switches: 0,
            test_report: Some(report.summary()),
            review: None,
            flow: None,
            engine: None,
        },
    })
}

#[test]
fn b844_evidence_names_the_logged_command_a_step_ran_in_place_of() {
    // The steps of the delivered run as the report makes them now.
    let md = md_of(vec![
        step(
            TestStepRole::Setup,
            TestCommandSource::Detected,
            "npm install",
            1,
            None,
            None,
        ),
        TestStep::not_run(
            TestStepRole::Test,
            TestCommandSource::LifecycleLog,
            "node --test",
            "skipped: the setup step `npm install` failed",
            None,
        ),
        step(
            TestStepRole::Test,
            TestCommandSource::Detected,
            "python3 -m pytest -q",
            0,
            Some(6),
            Some(in_place_line(
                "python3 -m pytest tests/test_textstats.py -q",
                0,
            )),
        ),
    ]);
    assert!(
        md.contains("Setup `npm install` exited 1 in a clean checkout, so `node --test` did not run; for the test commands that ran, the runner reported 6 passed, 0 failed. The report chose a whole-suite command in place of 1 command of the lifecycle log that names part of a suite; details are in EVIDENCE.md."),
        "{md}"
    );
    assert!(
        md.contains("What the report did, step by step:\n\n- `node --test`: not run (skipped: the setup step `npm install` failed).\n- `python3 -m pytest -q`: exit 0, 6 passed, 0 failed; it ran in place of `python3 -m pytest tests/test_textstats.py -q` of the lifecycle log, which names a test file, a test or a filter.\n"),
        "{md}"
    );
    assert!(!md.contains("PLAN NOTE"), "{md}");
    assert_eq!(banned_claim_word(&md), None, "{md}");
}

#[test]
fn b844_evidence_names_the_logged_command_that_was_left_out_with_nothing_in_its_place() {
    let md = md_of(vec![step(
        TestStepRole::Test,
        TestCommandSource::LifecycleLog,
        "go test -v ./...",
        0,
        Some(4),
        Some(left_out_line("npx jest src/a.test.ts", 0)),
    )]);
    // The claim is about the commands that ran; the sentence after it says that a logged command was left out.
    assert!(
        md.contains("repository tests pass in a clean checkout: 4 of 4. The report left out 1 command of the lifecycle log that names part of a suite; details are in EVIDENCE.md."),
        "{md}"
    );
    assert!(
        md.contains("- `go test -v ./...`: exit 0, 4 passed, 0 failed.\n- `npx jest src/a.test.ts` of the lifecycle log was left out: it names a test file, a test or a filter, and the report found no command for the whole suite.\n"),
        "{md}"
    );
    assert!(!md.contains("PLAN NOTE"), "{md}");
    assert_eq!(banned_claim_word(&md), None, "{md}");
}
