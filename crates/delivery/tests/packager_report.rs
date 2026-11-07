//! `packager` and the clean-checkout test report: EVIDENCE.md carries one section and one sentence about the report when
//! `PackageRequest.direct.testReport` is set, and the zip's `evidence/` carries `test-report.json` like any other evidence object. A run
//! with no report renders exactly as before. No text says the harness checked, tested, gated or evaluated the work.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use delivery::packager::{PackageRequest, render_evidence};
use harness_protocol::direct::{
    DirectEvidence, DirectOutcome, DirectRuleRef, HeadSource, TestCommandSource, TestCounts,
    TestReport, TestReportStatus, TestReportSummary, TestStep, TestStepRole, banned_claim_word,
};

const RUN: &str = "01M4X3Q7R9S0V1W2X3Y4Z5A6B7";
const HEAD: &str = "cccccccccccccccccccccccccccccccccccccccc";

fn evidence() -> DirectEvidence {
    DirectEvidence {
        outcome: DirectOutcome::ReportedDone,
        label: DirectOutcome::ReportedDone.label().into(),
        detail: None,
        stop_reason: Some("end_turn".into()),
        cline_message: Some("Added pct(a, b).".into()),
        model_id: "us.anthropic.claude-opus-5-5".into(),
        engine_version: None,
        rules: vec![DirectRuleRef {
            name: "00-my-rules.md".into(),
            sha256: "ab".repeat(32),
            bytes: Some(22),
        }],
        usage: None,
        wall_s: Some(3731),
        head_source: HeadSource::Final,
        final_model_id: None,
        model_switches: 0,
        test_report: None,
        review: None,
        flow: None,
    }
}

fn request(d: DirectEvidence) -> PackageRequest {
    PackageRequest {
        run_id: RUN.into(),
        repo: "hamh-e2e-python-calc-20260925".into(),
        base_sha: None,
        head_sha: HEAD.into(),
        objective: "Add percent_change(a, b)".into(),
        pr_url: None,
        direct: d,
    }
}

fn step(exit: i32, counts: Option<(u32, u32, u32)>) -> TestStep {
    TestStep {
        role: TestStepRole::Test,
        source: TestCommandSource::TestCommandFile,
        command: "python3 -m pytest -q".into(),
        exit_code: Some(exit),
        timed_out: false,
        duration_ms: 900,
        limit_s: None,
        counts: counts.map(|(passed, failed, skipped)| TestCounts {
            passed,
            failed,
            skipped,
        }),
        counts_from: Some("pytest".into()),
        output_tail: String::new(),
        output_truncated: false,
        logged: None,
    }
}

fn summary_of(
    status: TestReportStatus,
    steps: Vec<TestStep>,
    reason: Option<&str>,
) -> TestReportSummary {
    TestReport {
        schema_version: 1,
        run_id: RUN.into(),
        head_sha: HEAD.into(),
        status,
        reason: reason.map(str::to_string),
        started_at: None,
        duration_ms: 1_000,
        steps,
        summary: None,
    }
    .summary()
}

fn md_with(summary: Option<TestReportSummary>) -> String {
    let mut d = evidence();
    d.test_report = summary;
    render_evidence(&request(d))
}

const WITHOUT: &str = "The harness hosted the session, committed what Cline left in the tree, scanned the commits for secrets and packaged this zip. It ran no test and made no judgment on the work.";

#[test]
fn a_run_without_a_report_renders_exactly_as_before() {
    let md = md_with(None);
    assert!(md.contains(WITHOUT), "{md}");
    assert!(!md.contains("Clean-checkout"), "{md}");
    assert!(!md.contains("test-report.json"), "{md}");
}

#[test]
fn a_report_that_ran_adds_a_section_with_the_one_sentence_and_says_what_the_harness_did() {
    let md = md_with(Some(summary_of(
        TestReportStatus::Ran,
        vec![step(0, Some((12, 0, 0)))],
        None,
    )));
    assert!(md.contains("## Clean-checkout test report"), "{md}");
    assert!(
        md.contains("repository tests pass in a clean checkout: 12 of 12."),
        "{md}"
    );
    assert!(md.contains("`evidence/test-report.json`"), "{md}");
    assert!(
        md.contains("It does not say the tests are meaningful."),
        "{md}"
    );
    assert!(
        !md.contains("It ran no test"),
        "the harness ran the repository's test commands, so the old sentence would be wrong: {md}"
    );
    assert!(
        md.contains("It made no judgment on the work. It also ran the repository's own test commands in a clean checkout of the delivered head and recorded what they did in the section above."),
        "{md}"
    );
    // The section sits between Cline's message and what the harness did, and the file is listed with the other contents.
    let (a, b, c) = (
        md.find("## Cline's last message").unwrap(),
        md.find("## Clean-checkout test report").unwrap(),
        md.find("## What the harness did").unwrap(),
    );
    assert!(a < b && b < c, "{md}");
    assert!(
        md.contains("- `evidence/test-report.json`: the clean-checkout test report, step by step"),
        "{md}"
    );
    // The label row is untouched.
    assert!(md.contains("| Outcome | Cline reported done |"), "{md}");
}

#[test]
fn a_failing_report_says_what_happened_and_makes_no_claim() {
    let md = md_with(Some(summary_of(
        TestReportStatus::Ran,
        vec![step(1, Some((18, 2, 0)))],
        None,
    )));
    assert!(
        md.contains("`python3 -m pytest -q` exited 1 in a clean checkout; the runner reported 18 passed, 2 failed."),
        "{md}"
    );
    assert!(!md.contains("repository tests pass"), "{md}");
    assert!(
        md.contains("| Outcome | Cline reported done |"),
        "the label stays what Cline reported: {md}"
    );
}

#[test]
fn a_report_that_ran_nothing_keeps_the_old_sentence_and_still_gets_its_section() {
    for (summary, sentence) in [
        (
            summary_of(TestReportStatus::NoCommand, vec![], Some("none")),
            "No test command was found, so no clean-checkout test report was made.",
        ),
        (
            summary_of(
                TestReportStatus::NotRun,
                vec![],
                Some("the clean checkout failed"),
            ),
            "The clean-checkout test report was not made: the clean checkout failed.",
        ),
    ] {
        let md = md_with(Some(summary));
        assert!(md.contains(sentence), "{md}");
        assert!(
            md.contains(WITHOUT),
            "no test ran, so the old sentence holds: {md}"
        );
    }
}

#[test]
fn a_report_that_was_switched_off_says_so_and_lists_no_file() {
    let md = md_with(Some(TestReportSummary::off()));
    assert!(
        md.contains("The clean-checkout test report was switched off for this run."),
        "{md}"
    );
    assert!(!md.contains("test-report.json"), "{md}");
    assert!(md.contains(WITHOUT), "{md}");
}

#[test]
fn no_status_makes_evidence_md_say_the_harness_checked_the_work() {
    let all = [
        summary_of(TestReportStatus::Ran, vec![step(0, Some((12, 0, 4)))], None),
        summary_of(TestReportStatus::Ran, vec![step(1, Some((1, 1, 0)))], None),
        summary_of(TestReportStatus::Ran, vec![step(0, None)], None),
        summary_of(
            TestReportStatus::TimedOut,
            vec![step(0, Some((3, 0, 0)))],
            Some("a command was stopped at the report time limit"),
        ),
        summary_of(TestReportStatus::NoCommand, vec![], None),
        summary_of(
            TestReportStatus::NotRun,
            vec![],
            Some("the report directory could not be made"),
        ),
        TestReportSummary::off(),
    ];
    for s in all {
        let md = md_with(Some(s.clone()));
        assert_eq!(banned_claim_word(&md), None, "{s:?}\n{md}");
    }
}

#[test]
fn a_sentence_with_a_pipe_or_a_line_break_in_its_reason_cannot_break_the_tables() {
    let md = md_with(Some(summary_of(
        TestReportStatus::NotRun,
        vec![],
        Some("the clean checkout failed: git clone | exit"),
    )));
    // The reason is a paragraph, not a cell, and the table of fields above it is intact.
    assert!(md.contains("| Outcome | Cline reported done |"), "{md}");
    assert!(md.contains("git clone | exit."), "{md}");
}

// ---- a command stopped at its limit --------------------------------------------------------------------------------------------

fn stopped_summary() -> TestReportSummary {
    let mut stopped = step(0, None);
    stopped.command = "cargo test".into();
    stopped.exit_code = None;
    stopped.timed_out = true;
    stopped.limit_s = Some(300);
    summary_of(
        TestReportStatus::TimedOut,
        vec![step(0, Some((3, 0, 0))), stopped],
        Some("a command was stopped at the report time limit"),
    )
}

#[test]
fn a_stopped_command_is_named_with_its_limit_and_the_settings_that_change_it() {
    let md = md_with(Some(stopped_summary()));
    assert!(
        md.contains("`cargo test` timed out after 300 s; the commands that finished are listed in the report."),
        "{md}"
    );
    assert!(
        md.contains("`cargo test` was stopped at the limit of 300 s. The limit for one command is the client setting `run.report_command_minutes` (`ham config set run.report_command_minutes <minutes>`). When a run sets none, the stage's own limit applies; the stage variable `AGENTD_REPORT_STEP_S` sets it."),
        "{md}"
    );
    // The note is its own paragraph between the sentence and the line that says what the report records.
    let (a, b, c) = (
        md.find("timed out after 300 s").unwrap(),
        md.find("was stopped at the limit of 300 s").unwrap(),
        md.find("It records what the repository's own test commands did")
            .unwrap(),
    );
    assert!(a < b && b < c, "{md}");
    assert_eq!(md.matches("was stopped at the limit").count(), 1, "{md}");
    assert_eq!(banned_claim_word(&md), None, "{md}");
}

#[test]
fn a_report_with_no_stopped_command_gets_no_note_about_the_limit() {
    for s in [
        summary_of(TestReportStatus::Ran, vec![step(0, Some((12, 0, 0)))], None),
        summary_of(TestReportStatus::Ran, vec![step(1, Some((18, 2, 0)))], None),
        summary_of(
            TestReportStatus::TimedOut,
            vec![step(0, Some((3, 0, 0)))],
            Some("the report time limit ended before every command ran"),
        ),
        TestReportSummary::off(),
    ] {
        let md = md_with(Some(s.clone()));
        assert!(!md.contains("run.report_command_minutes"), "{s:?}\n{md}");
        assert!(!md.contains("AGENTD_REPORT_STEP_S"), "{s:?}\n{md}");
    }
}

// ---- a command the harness chose from the manifest -----------------------------------------------------------------------------

const PLAN_NOTE: &str = "The repository names no test command, so the harness chose `cargo test` from the manifest (Cargo.toml). It runs the tests of the whole workspace, 14 members. A `.ham/test-command` file in the repository names a narrower command.";

fn chosen_from_manifest(exit: Option<i32>) -> TestStep {
    let mut s = step(exit.unwrap_or(0), None);
    s.source = TestCommandSource::Detected;
    s.command = "cargo test".into();
    s.counts_from = None;
    s.output_tail = "PLAN NOTE: cargo workspace, 14 members\nerror: boom\n".into();
    if exit.is_none() {
        s.exit_code = None;
        s.timed_out = true;
        s.limit_s = Some(300);
    }
    s
}

#[test]
fn a_command_chosen_from_the_manifest_is_said_so_once_and_the_machine_line_stays_out() {
    let md = md_with(Some(summary_of(
        TestReportStatus::Ran,
        vec![chosen_from_manifest(Some(101))],
        None,
    )));
    assert!(md.contains(PLAN_NOTE), "{md}");
    assert_eq!(md.matches("chose `cargo test`").count(), 1, "{md}");
    assert!(!md.contains("PLAN NOTE"), "{md}");
    // Its own paragraph, between the sentence and the line that says what the report records.
    let (a, b, c) = (
        md.find("`cargo test` exited 101 in a clean checkout")
            .unwrap(),
        md.find("The repository names no test command").unwrap(),
        md.find("It records what the repository's own test commands did")
            .unwrap(),
    );
    assert!(a < b && b < c, "{md}");
    assert_eq!(banned_claim_word(&md), None, "{md}");
}

#[test]
fn a_stopped_command_chosen_from_the_manifest_gets_both_notes() {
    let md = md_with(Some(summary_of(
        TestReportStatus::TimedOut,
        vec![chosen_from_manifest(None)],
        Some("a command was stopped at the report time limit"),
    )));
    assert!(md.contains(PLAN_NOTE), "{md}");
    assert!(
        md.contains("`cargo test` was stopped at the limit of 300 s."),
        "{md}"
    );
    assert_eq!(md.matches("chose `cargo test`").count(), 1, "{md}");
    assert_eq!(md.matches("was stopped at the limit").count(), 1, "{md}");
    assert_eq!(banned_claim_word(&md), None, "{md}");
}

#[test]
fn a_command_the_repository_named_gets_no_note_about_its_choice() {
    let mut named = chosen_from_manifest(Some(101));
    named.source = TestCommandSource::TestCommandFile;
    let md = md_with(Some(summary_of(TestReportStatus::Ran, vec![named], None)));
    assert!(!md.contains("The repository names no test command"), "{md}");
    let plain = md_with(Some(summary_of(
        TestReportStatus::Ran,
        vec![step(1, Some((18, 2, 0)))],
        None,
    )));
    assert!(!plain.contains("chose `"), "{plain}");
}
