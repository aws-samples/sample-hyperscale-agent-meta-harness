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
    md_with(steps, None)
}

/// The page for a run whose engine was told `note` about the delivery, or nothing.
fn md_with(steps: Vec<TestStep>, note: Option<&str>) -> String {
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
        judged_base_sha: None,
        head_sha: HEAD.into(),
        objective: "Build a small repository with two parts".into(),
        target: Default::default(),
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
            delivery_note: note.map(str::to_string),
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

// ---- the lanes of release 5.22 together: a command that was not run again, a left-out command and the block on the delivery ---------

/// The line agentd writes behind the notes of the plan for a whole-suite command it did not run again:
/// the log recorded the command as failed and a later run of the same suite passed.
const NOT_AGAIN: &str = "PLAN NOTE: not run again `python3 -m pytest`; logged exit 1, 2 failed; passed later as `python3 -m pytest -q`";
const TOLD: &str = "How this run delivers (written by the harness): this run delivers only a zip.";

/// A step whose output begins with the note of a left-out command and, behind it, the line of a command that was not run again.
fn step_with_both_lines(exit: i32, passed: Option<u32>) -> TestStep {
    step(
        TestStepRole::Test,
        TestCommandSource::LifecycleLog,
        "python3 -m pytest -q",
        exit,
        passed,
        Some(format!(
            "{}\n{NOT_AGAIN}",
            left_out_line("npx jest src/a.test.ts", 0)
        )),
    )
}

/// The page keeps the detail of the left-out command beside the line of the command that was not run again, prints neither machine
/// line, and quotes the block on the delivery in its own section: the three additions of one release do not displace each other. The
/// command that was not run again is named in `evidence/test-report.json`; the page gives the count of the command that ran.
#[test]
fn b851_b850_evidence_keeps_the_left_out_detail_beside_a_command_that_was_not_run_again_and_the_delivery_block()
 {
    let md = md_with(vec![step_with_both_lines(0, Some(45))], Some(TOLD));
    println!(
        "B851-B850 TRIGGER PRESENT: the step's output holds both machine lines and the run was told how it delivers: {}",
        md.contains("about the delivery")
    );
    assert!(
        md.contains("repository tests pass in a clean checkout: 45 of 45. The report left out 1 command of the lifecycle log that names part of a suite; details are in EVIDENCE.md."),
        "{md}"
    );
    assert!(
        md.contains("- `python3 -m pytest -q`: exit 0, 45 passed, 0 failed.\n- `npx jest src/a.test.ts` of the lifecycle log was left out: it names a test file, a test or a filter, and the report found no command for the whole suite.\n"),
        "{md}"
    );
    assert!(
        !md.contains("PLAN NOTE"),
        "no machine line on the page: {md}"
    );
    assert!(
        md.contains(&format!(
            "## What the harness told Cline about the delivery\n\n> {TOLD}\n"
        )),
        "{md}"
    );
    let at = |needle: &str| {
        md.find(needle)
            .unwrap_or_else(|| panic!("{needle} in {md}"))
    };
    assert!(
        at("## Objective") < at("## What the harness told Cline about the delivery")
            && at("## What the harness told Cline about the delivery")
                < at("## Cline's last message"),
        "the block stands after the objective and before the last message: {md}"
    );
    assert_eq!(banned_claim_word(&md), None, "{md}");
}

/// The same step when its command failed: the page shows the end of the output of a failed command, and the machine lines at the
/// head of that output still stay off the page.
#[test]
fn b851_b850_a_failed_step_shows_no_machine_line_either() {
    let md = md_with(vec![step_with_both_lines(1, None)], Some(TOLD));
    assert!(
        md.contains("`python3 -m pytest -q`"),
        "TRIGGER PRESENT: the failed command is on the page: {md}"
    );
    assert!(
        !md.contains("PLAN NOTE"),
        "no machine line on the page: {md}"
    );
    assert!(
        md.contains("## What the harness told Cline about the delivery"),
        "{md}"
    );
    assert_eq!(banned_claim_word(&md), None, "{md}");
}
