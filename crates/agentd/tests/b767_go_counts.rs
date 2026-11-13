//! The clean-checkout test report reads the count line of gotestsum (`DONE 80 tests in 1.065s`).
//!
//! Go repositories run their tests through `go run gotest.tools/gotestsum@v1.13.0` (the three events runs of a release gate did), and gotestsum prints no
//! `--- PASS:` line in its default format, so the `go test -v` reader found nothing and the report said "counts not read". The samples here are the end of
//! the output of the real steps (the `go: downloading` lines trimmed to two). The grammar is gotestsum v1.13.0's `PrintSummary`:
//! `"\n%s %d tests%s%s%s in %s\n"` with `DONE` (and ` N runs,` after `--rerun-fails`), `, N skipped`, `, N failure(s)`, `, N error(s)`.
//! Nothing here starts a process.
#![allow(clippy::unwrap_used)]

use agentd::direct::report::parse::{OutputScan, count_in};
use harness_protocol::direct::{TestCounts, TestReport, TestStepRole};

const A_UNIT: &str = include_str!("b767_fixtures/events_a_unit.txt");
const A_INTEGRATION: &str = include_str!("b767_fixtures/events_a_integration.txt");
const A_E2E: &str = include_str!("b767_fixtures/events_a_e2e.txt");
const B_UNIT: &str = include_str!("b767_fixtures/events_b_unit.txt");
const B_INTEGRATION: &str = include_str!("b767_fixtures/events_b_integration.txt");
const B_E2E: &str = include_str!("b767_fixtures/events_b_e2e.txt");

const SCRIPT: &str = "bash scripts/test-unit.sh";

fn counts(p: u32, f: u32, s: u32) -> TestCounts {
    TestCounts {
        passed: p,
        failed: f,
        skipped: s,
    }
}

fn gts(p: u32, f: u32, s: u32) -> Option<(&'static str, TestCounts)> {
    Some(("gotestsum", counts(p, f, s)))
}

/// The output of a step: some package lines, a blank line and the summary line, as gotestsum prints them.
fn run_with(done: &str) -> String {
    format!("✓  internal/api (6ms)\n∅  internal/app\n\n{done}\n")
}

#[test]
fn b767_the_six_real_step_outputs_of_the_events_runs_give_their_test_counts() {
    for (name, out, want) in [
        ("a unit", A_UNIT, 80),
        ("a integration", A_INTEGRATION, 8),
        ("a e2e", A_E2E, 1),
        ("b unit", B_UNIT, 69),
        ("b integration", B_INTEGRATION, 11),
        ("b e2e", B_E2E, 1),
    ] {
        assert_eq!(count_in(SCRIPT, out), gts(want, 0, 0), "{name}");
    }
}

#[test]
fn b767_a_command_that_names_gotestsum_reads_its_summary_and_no_other() {
    let out = format!(
        "12 passed in 0.31s\n{}",
        run_with("DONE 80 tests in 1.065s")
    );
    for command in [
        "go run gotest.tools/gotestsum@v1.13.0 --junitfile r.xml -- -count=1 ./...",
        "go run gotest.tools/gotestsum --format testname -- ./...",
        "go run -mod=mod gotest.tools/gotestsum@latest -- ./...",
        "go tool gotestsum -- ./...",
        "gotestsum --format pkgname -- ./...",
        "/home/agent/go/bin/gotestsum -- ./...",
    ] {
        assert_eq!(count_in(command, &out), gts(80, 0, 0), "{command}");
    }
    // A command that names another runner does not read gotestsum's line, and a wrapper that ran two kinds of runner chooses neither.
    assert_eq!(count_in("python3 -m pytest", &out).unwrap().0, "pytest");
    assert_eq!(count_in(SCRIPT, &out), None);
}

#[test]
fn b767_skipped_tests_and_failures_follow_the_summary_line() {
    for (done, want) in [
        ("DONE 80 tests, 2 skipped, 1 failure in 1.065s", (77, 1, 2)),
        ("DONE 80 tests, 3 failures in 1.065s", (77, 3, 0)),
        ("DONE 80 tests, 2 skipped in 1.065s", (78, 0, 2)),
        ("DONE 1 tests, 1 skipped in 0.001s", (0, 0, 1)),
        ("DONE 5 tests, 1 skipped, 2 failures in 0.5s", (2, 2, 1)),
        ("DONE 0 tests in 0.000s", (0, 0, 0)),
        // More failures than tests: a package that failed before it ran a test is a failure with no test behind it.
        ("DONE 0 tests, 1 failure, 2 errors in 0.400s", (0, 1, 0)),
        ("DONE 2 tests, 5 failures in 1.0s", (0, 5, 0)),
    ] {
        assert_eq!(
            count_in(SCRIPT, &run_with(done)),
            gts(want.0, want.1, want.2),
            "{done}"
        );
    }
}

#[test]
fn b767_a_go_step_with_a_failing_test_still_reads_as_failed() {
    let out = format!(
        "✖  internal/api (4ms)\n\n=== Failed\n=== FAIL: internal/api TestRetry (0.00s)\n    api_test.go:44: got 500\n\n{}",
        run_with("DONE 80 tests, 1 failure in 1.065s")
    );
    let (runner, c) = count_in(SCRIPT, &out).unwrap();
    assert_eq!(runner, "gotestsum");
    assert_eq!(c, counts(79, 1, 0));
    assert!(c.failed >= 1);
    // A build failure: gotestsum counts the package as one failure and the compiler's lines as errors.
    let build = "FAIL\texample.com/x [build failed]\n\nDONE 0 tests, 1 failure, 3 errors in 0.5s\n";
    assert_eq!(count_in(SCRIPT, build), gts(0, 1, 0));
}

#[test]
fn b767_errors_with_no_failure_give_no_counts_and_do_not_fall_back_to_the_verbose_go_lines() {
    let done = "DONE 5 tests, 2 errors in 1.0s";
    assert_eq!(count_in(SCRIPT, &run_with(done)), None);
    // Errors are lines on standard error, not tests: they are never added to the failures, and a summary that contradicts a clean run
    // makes the whole gotestsum result unreadable so that another reader cannot bring a pass count back.
    let verbose = format!(
        "--- PASS: TestA (0.00s)\n--- PASS: TestB (0.00s)\n{}",
        run_with(done)
    );
    assert_eq!(count_in("sh run-tests.sh -v", &verbose), None);
    assert_eq!(
        count_in("gotestsum --format standard-verbose -- -v ./...", &verbose),
        None
    );
    // One good summary and one that is not readable: the result is not readable.
    let both = format!("{}{}", run_with("DONE 80 tests in 1.0s"), run_with(done));
    assert_eq!(count_in(SCRIPT, &both), None);
}

#[test]
fn b767_a_rerun_summary_gives_no_counts() {
    // `--rerun-fails` adds the retried tests to the total again, so the numbers are attempts and not tests.
    let out = run_with("DONE 2 runs, 80 tests, 1 failure in 3.2s");
    assert_eq!(count_in(SCRIPT, &out), None);
    let ok = run_with("DONE 3 runs, 84 tests in 3.2s");
    assert_eq!(count_in(SCRIPT, &ok), None);
}

#[test]
fn b767_text_that_only_looks_like_the_summary_line_is_not_read() {
    for line in [
        "DONE with the setup",
        "DONE 5 items in 1.0s",
        "DONE 5 tests",
        "DONE 5 tests in",
        "DONE 5 tests in fast",
        "DONE five tests in 1.0s",
        "DONE 5 tests, 1 failure, 2 skipped in 1.0s",
        "DONE 5 test in 1.0s",
        "DONE 5 tests, 1 failed in 1.0s",
        "DONE 5 tests, 2 skipped, 1 skipped in 1.0s",
        "DONE -5 tests in 1.0s",
        "done 5 tests in 1.0s",
        "  DONE 5 tests in 1.0s",
        "5 tests in 1.0s",
        " 5 tests in 1.0s",
        "DONE 5 tests in 1.0s and then some",
        "DONE  5 tests in 1.0s",
        "DONE 5 tests in 1.0 s",
        "DONE 5 tests in -1.0s",
        "DONE 5 tests in 3s",
        "DONE 5 tests in .5s",
        "### DONE 5 tests in 1.0s",
    ] {
        assert_eq!(count_in(SCRIPT, &run_with(line)), None, "{line:?}");
    }
    // The output of a repository's own script can say DONE without a summary behind it.
    assert_eq!(count_in(SCRIPT, "DONE\n"), None);
    assert_eq!(count_in(SCRIPT, ""), None);
}

#[test]
fn b767_the_duration_is_any_seconds_value_or_unknown() {
    for done in [
        "DONE 5 tests in 0.000s",
        "DONE 5 tests in 125.500s",
        "DONE 5 tests in unknown",
    ] {
        assert_eq!(count_in(SCRIPT, &run_with(done)), gts(5, 0, 0), "{done}");
    }
}

#[test]
fn b767_a_count_beyond_what_the_report_holds_is_clamped_and_not_wrapped() {
    assert_eq!(
        count_in(SCRIPT, &run_with("DONE 99999999999 tests in 1.0s")),
        gts(u32::MAX, 0, 0)
    );
    assert_eq!(
        count_in(
            SCRIPT,
            &run_with("DONE 99999999999999999999999 tests in 1.0s")
        ),
        None,
        "a number that does not fit a 64-bit count is not a count"
    );
}

#[test]
fn b767_the_summaries_of_several_runs_in_one_output_are_added() {
    // A script that runs gotestsum for the unit packages and then for the integration packages prints one summary for each.
    let out = format!("{A_UNIT}{A_INTEGRATION}{A_E2E}");
    assert_eq!(count_in("bash scripts/test-all.sh", &out), gts(89, 0, 0));
    let mixed = format!(
        "{}{}",
        run_with("DONE 10 tests, 1 skipped, 1 failure in 1.0s"),
        run_with("DONE 5 tests, 1 skipped in 1.0s")
    );
    assert_eq!(count_in(SCRIPT, &mixed), gts(12, 1, 2));
}

#[test]
fn b767_gotestsum_wins_over_the_verbose_go_lines_of_the_same_output() {
    // `gotestsum --format standard-verbose -- -v` prints the lines of `go test -v` and the summary; the summary counts subtests, the lines do not.
    let out = format!(
        "=== RUN   TestA\n--- PASS: TestA (0.00s)\n=== RUN   TestA/sub\n    --- PASS: TestA/sub (0.00s)\n--- PASS: TestB (0.00s)\n{}",
        run_with("DONE 3 tests in 0.5s")
    );
    assert_eq!(
        count_in("gotestsum --format standard-verbose -- -v ./...", &out),
        gts(3, 0, 0)
    );
    assert_eq!(count_in("sh run-tests.sh -v", &out), gts(3, 0, 0));
    // The verbose reader still works alone.
    let plain = "--- PASS: TestA (0.00s)\n--- FAIL: TestB (0.00s)\nFAIL\n";
    assert_eq!(
        count_in("go test -v ./...", plain),
        Some(("go", counts(1, 1, 0)))
    );
    // And a command that runs `go test` and names no gotestsum does not read a DONE line some other tool printed.
    assert_eq!(
        count_in("go test ./...", &run_with("DONE 3 tests in 0.5s")),
        None
    );
}

#[test]
fn b767_colour_codes_and_reads_that_split_the_line_do_not_hide_the_summary() {
    let out = "\u{1b}[32m✓\u{1b}[0m  internal/api (6ms)\n\nDONE 80 tests in 1.065s\n";
    assert_eq!(count_in(SCRIPT, out), gts(80, 0, 0));
    let mut scan = OutputScan::new(4096);
    for b in A_UNIT.as_bytes() {
        scan.feed(std::slice::from_ref(b));
    }
    let done = scan.finish(SCRIPT);
    assert_eq!(done.counts, gts(80, 0, 0));
    assert!(done.tail.ends_with("DONE 80 tests in 1.065s\n"));
    // A summary with no newline after it, at the end of the stream.
    assert_eq!(count_in(SCRIPT, "\nDONE 80 tests in 1.065s"), gts(80, 0, 0));
}

/// The report the release gate delivered for an events run (Go, three test scripts that run gotestsum), as it was written: no step has counts.
const DELIVERED_EVENTS_REPORT: &str = include_str!("b767_fixtures/events_report.json");

#[test]
fn b767_the_delivered_events_report_states_its_go_counts_when_its_outputs_are_read_again() {
    let report: TestReport = serde_json::from_str(DELIVERED_EVENTS_REPORT).unwrap();
    report
        .validate(&report.run_id, &report.head_sha)
        .expect("the delivered report is valid");
    let tests: Vec<_> = report
        .steps
        .iter()
        .filter(|s| s.role == TestStepRole::Test)
        .collect();
    assert_eq!(tests.len(), 3);
    assert!(
        tests.iter().all(|s| s.counts.is_none()),
        "as delivered, no step has counts"
    );
    assert!(
        report
            .summary()
            .details
            .iter()
            .any(|d| d.contains("counts not read")),
        "as delivered, the report says so"
    );
    // The same steps read by the reader of today: each output tail ends with the DONE line of its script.
    let read: Vec<_> = tests
        .iter()
        .map(|s| count_in(&s.command, &s.output_tail))
        .collect();
    assert_eq!(read, [gts(80, 0, 0), gts(8, 0, 0), gts(1, 0, 0)]);
    // And the report made from them states the counts and the claim, as a web or a Java report does.
    let mut steps = report.steps.clone();
    for s in steps.iter_mut().filter(|s| s.role == TestStepRole::Test) {
        let (runner, counts) = count_in(&s.command, &s.output_tail).unwrap();
        s.counts = Some(counts);
        s.counts_from = Some(runner.to_string());
    }
    let now = TestReport {
        steps,
        summary: None,
        ..report
    };
    now.validate(&now.run_id, &now.head_sha).unwrap();
    let summary = now.summary();
    assert!(
        summary
            .details
            .iter()
            .all(|d| !d.contains("counts not read"))
    );
    assert_eq!(summary.counts, Some(counts(80, 0, 0)));
    assert_eq!(
        summary.claim().as_deref(),
        Some("repository tests pass in a clean checkout: 80 of 80")
    );
    let json = serde_json::to_value(&now).unwrap();
    assert_eq!(json["steps"][2]["countsFrom"], "gotestsum");
    assert_eq!(json["steps"][2]["counts"]["passed"], 80);
}

#[test]
fn b767_a_failed_test_line_the_summary_does_not_count_makes_the_result_unreadable() {
    // A wrapper that runs `go test -v` for one package list and gotestsum for another: the summary says nothing of the first, and the command
    // exited with the second one's code.
    let out = format!(
        "--- FAIL: TestBroken (0.00s)\nFAIL\n{}",
        run_with("DONE 10 tests in 0.5s")
    );
    assert_eq!(count_in("sh run-tests.sh", &out), None);
    assert_eq!(count_in("go test -v ./a; gotestsum ./b", &out), None);
    // The failed test of a gotestsum run in its verbose format is in the summary too: both agree and the count is read.
    let same = format!(
        "--- PASS: TestA (0.00s)\n--- FAIL: TestB (0.00s)\n{}",
        run_with("DONE 2 tests, 1 failure in 0.5s")
    );
    assert_eq!(count_in("sh run-tests.sh -v", &same), gts(1, 1, 0));
    // A failing subtest is indented and is not a top-level line: the summary counts it and the lines do not.
    let sub = format!(
        "--- FAIL: TestB (0.00s)\n    --- FAIL: TestB/sub (0.00s)\n{}",
        run_with("DONE 2 tests, 2 failures in 0.5s")
    );
    assert_eq!(count_in("sh run-tests.sh -v", &sub), gts(0, 2, 0));
}

#[test]
fn b767_a_summary_without_done_is_what_gotestsum_prints_when_it_did_not_finish() {
    // gotestsum leaves out `DONE` when the run was not completed (the line then starts with a space). A readable summary before it cannot
    // stand for the run that did not finish.
    let out = format!(
        "{}\n 5 tests, 1 failure in 1.000s\n",
        run_with("DONE 80 tests in 1.0s")
    );
    assert_eq!(count_in(SCRIPT, &out), None);
    assert_eq!(count_in(SCRIPT, "\n 5 tests in 1.000s\n"), None);
    // The same text deeper in or without the one space is no summary.
    assert_eq!(
        count_in(
            SCRIPT,
            &format!(
                "{}\n  5 tests in 1.000s\n",
                run_with("DONE 80 tests in 1.0s")
            )
        ),
        gts(80, 0, 0)
    );
}
