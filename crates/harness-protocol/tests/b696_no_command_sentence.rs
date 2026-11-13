//! When the clean-checkout report finds no test command, the card says what it looked for: the sentence was "No test command was found, so
//! no clean-checkout test report was made." with nothing about where it looked or how a repository names its command.

#![allow(clippy::unwrap_used)]

use harness_protocol::direct::{TestReport, TestReportStatus};

fn no_command(reason: Option<&str>) -> TestReport {
    TestReport {
        schema_version: 1,
        run_id: "R1".into(),
        head_sha: "a".repeat(40),
        status: TestReportStatus::NoCommand,
        reason: reason.map(str::to_string),
        started_at: None,
        duration_ms: 0,
        steps: Vec::new(),
        summary: None,
    }
}

#[test]
fn b696_the_sentence_carries_what_the_report_looked_for() {
    let why = "it looked for .ham/test-command, the lifecycle log, the goal card and the test layouts it knows; a .ham/test-command file names the command";
    assert_eq!(
        no_command(Some(why)).summary().sentence(),
        format!("No test command was found, so no clean-checkout test report was made: {why}.")
    );
}

#[test]
fn b696_a_report_without_a_reason_reads_as_before() {
    assert_eq!(
        no_command(None).summary().sentence(),
        "No test command was found, so no clean-checkout test report was made."
    );
}

/// Review of the 5.15 integration head: a plain Python layout whose packages the report has no install step for is found and not run. The
/// reason starts with `it found `, and the sentence says that the tests were not run and why, in one sentence.
#[test]
fn fix515_b696_a_reason_that_says_test_files_were_found_gives_the_sentence_that_they_were_not_run()
{
    let why = "it found Python test files (tests/test_app.py), and pyproject.toml names packages that the report does not install (it installs only the packages of requirements.txt); a .ham/test-command file in the repository names the install and test commands";
    assert!(why.starts_with(harness_protocol::direct::NO_COMMAND_FOUND_TESTS));
    let s = no_command(Some(why)).summary();
    assert_eq!(
        s.sentence(),
        format!("The clean-checkout test report did not run the tests: {why}.")
    );
    assert_eq!(s.claim(), None);
    assert_eq!(
        harness_protocol::direct::banned_claim_word(&s.sentence()),
        None
    );
    // Any other reason reads as before.
    assert_eq!(
        no_command(Some("the tree is empty")).summary().sentence(),
        "No test command was found, so no clean-checkout test report was made."
    );
}
