//! A test command of the lifecycle log that could not run as written does not lead the sentence of the report.
//!
//! The owner's run of 2026-11-12 logged `pytest -q at BASE worktree` (the engine had run pytest in a scratch copy of the base commit). The report
//! ran those words in the clean checkout, pytest answered "ERROR: file or directory not found: at" with exit 4, and the card of a correct delivery
//! read "`pytest -q at BASE worktree` exited 4 in a clean checkout; the runner reported 6 passed, 0 failed." while its three real commands passed.
//! agentd now marks such a step not run with a reason that starts `could not run as written` (it checks that the path is a word of the command and is
//! missing in the clean checkout). Such a step is no failure, and the sentence leads with the commands that ran.
#![allow(clippy::unwrap_used)]

use harness_protocol::direct::{
    NOT_RUN_MARK, TestCommandSource, TestCounts, TestReport, TestReportStatus, TestStep,
    TestStepRole,
};

const RUN: &str = "01M794ST5PZ5HD8EJ5DYS3X5FD";
const HEAD: &str = "7e0f7ba11ebb63729848815177725d231327b29f";

fn green(command: &str, passed: u32) -> TestStep {
    TestStep {
        role: TestStepRole::Test,
        source: TestCommandSource::LifecycleLog,
        command: command.to_string(),
        exit_code: Some(0),
        timed_out: false,
        duration_ms: 190,
        limit_s: Some(300),
        counts: Some(TestCounts {
            passed,
            failed: 0,
            skipped: 0,
        }),
        counts_from: Some("pytest".into()),
        output_tail: format!("{passed} passed in 0.01s\n"),
        output_truncated: false,
        logged: None,
    }
}

/// A step agentd marked: it ran, pytest exited 4 because a word of the command names nothing in the clean checkout.
fn as_written(command: &str, word: &str) -> TestStep {
    TestStep {
        role: TestStepRole::Test,
        source: TestCommandSource::LifecycleLog,
        command: command.to_string(),
        exit_code: Some(4),
        timed_out: false,
        duration_ms: 178,
        limit_s: Some(300),
        counts: None,
        counts_from: Some(NOT_RUN_MARK.into()),
        output_tail: format!(
            "NOT RUN: could not run as written: the clean checkout has no file or directory `{word}`\nERROR: file or directory not found: {word}\n\nno tests ran in 0.00s\n"
        ),
        output_truncated: false,
        logged: None,
    }
}

fn report(steps: Vec<TestStep>) -> TestReport {
    TestReport {
        schema_version: 1,
        run_id: RUN.into(),
        head_sha: HEAD.into(),
        status: TestReportStatus::Ran,
        reason: None,
        started_at: None,
        duration_ms: 763,
        steps,
        summary: None,
    }
}

#[test]
fn b834_a_command_that_could_not_run_as_written_is_no_failure_and_does_not_lead() {
    let r = report(vec![
        green("python3 -m pytest -q", 6),
        green("python3 -m pytest -q tests", 6),
        as_written("pytest -q tests/integration", "tests/integration"),
        green("pytest -q", 6),
    ]);
    r.validate(RUN, HEAD).unwrap();
    let s = r.summary();
    assert_eq!(s.failed_command, None, "{s:?}");
    assert_eq!(s.exit_code, Some(0), "{s:?}");
    assert_eq!(s.steps_run, 3);
    // A command that did not run as the log wrote it shows nothing about the suite: the claim stays away.
    assert_eq!(s.claim(), None);
    assert_eq!(
        s.sentence(),
        "The test commands that ran exited 0 in a clean checkout; the runner reported 6 passed, 0 failed. `pytest -q tests/integration` could not run as written: the clean checkout has no file or directory `tests/integration`."
    );
    assert!(
        s.details.contains(
            &"`pytest -q tests/integration`: not run (could not run as written: the clean checkout has no file or directory `tests/integration`).".to_string()
        ),
        "{:?}",
        s.details
    );
}

#[test]
fn b834_when_no_other_command_ran_the_sentence_says_only_that() {
    let s = report(vec![as_written("pytest -q at BASE worktree", "at")]).summary();
    assert_eq!(s.failed_command, None);
    assert_eq!(s.claim(), None);
    assert_eq!(
        s.sentence(),
        "`pytest -q at BASE worktree` could not run as written: the clean checkout has no file or directory `at`."
    );
}

#[test]
fn b834_two_such_commands_are_named_once_with_the_count_of_the_others() {
    let s = report(vec![
        green("pytest -q", 4),
        as_written("pytest -q tests/a", "tests/a"),
        as_written("pytest -q tests/b", "tests/b"),
    ])
    .summary();
    assert_eq!(
        s.sentence(),
        "The test commands that ran exited 0 in a clean checkout; the runner reported 4 passed, 0 failed. `pytest -q tests/a` and 1 more could not run as written: the clean checkout has no file or directory that each of them names."
    );
    // The path of the first command is no path of the others: it is not named once there is more than one command.
    assert!(!s.sentence().contains("tests/b"), "{}", s.sentence());
}

#[test]
fn b834_three_such_commands_are_named_once_with_the_count_of_the_others() {
    let s = report(vec![
        green("pytest -q", 4),
        as_written("pytest -q tests/a", "tests/a"),
        as_written("pytest -q tests/b", "tests/b"),
        as_written("pytest -q tests/c", "tests/c"),
    ])
    .summary();
    assert_eq!(
        s.sentence(),
        "The test commands that ran exited 0 in a clean checkout; the runner reported 4 passed, 0 failed. `pytest -q tests/a` and 2 more could not run as written: the clean checkout has no file or directory that each of them names."
    );
}

#[test]
fn b834_counts_that_could_not_be_read_are_said_so() {
    let mut plain = green("bash scripts/test-all.sh", 0);
    plain.counts = None;
    plain.counts_from = None;
    let s = report(vec![plain, as_written("pytest -q tests/x", "tests/x")]).summary();
    assert_eq!(
        s.sentence(),
        "The test commands that ran exited 0 in a clean checkout; the runner's test counts could not be read. `pytest -q tests/x` could not run as written: the clean checkout has no file or directory `tests/x`."
    );
}

#[test]
fn b834_another_reason_not_to_run_keeps_its_sentence_and_comes_first() {
    let docker = TestStep::not_run(
        TestStepRole::Test,
        TestCommandSource::LifecycleLog,
        "docker compose run tests",
        "the session has no container runtime",
        None,
    );
    let s = report(vec![
        green("pytest -q", 4),
        as_written("pytest -q tests/x", "tests/x"),
        docker,
    ])
    .summary();
    assert_eq!(
        s.sentence(),
        "NOT RUN: `docker compose run tests` (the session has no container runtime)."
    );
}

#[test]
fn b834_a_real_failure_still_leads() {
    let mut red = green("pytest -q", 5);
    red.exit_code = Some(1);
    red.counts = Some(TestCounts {
        passed: 5,
        failed: 1,
        skipped: 0,
    });
    let s = report(vec![as_written("pytest -q tests/x", "tests/x"), red]).summary();
    assert_eq!(s.failed_command.as_deref(), Some("pytest -q"));
    assert_eq!(
        s.sentence(),
        "`pytest -q` exited 1 in a clean checkout; the runner reported 5 passed, 1 failed."
    );
}
