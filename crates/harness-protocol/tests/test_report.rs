//! The clean-checkout test report: the wire shapes, the summary a reader gets from the steps, the one
//! allowed wording, and the bounds run-session checks before it copies a worker's report into the evidence.
#![allow(clippy::unwrap_used)]

use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{
    DirectEvidence, MAX_REPORT_BYTES, MAX_REPORT_COMMAND_CHARS, MAX_REPORT_REASON_CHARS,
    MAX_REPORT_STEPS, MAX_REPORT_TAIL_BYTES, TestCommandSource, TestCounts, TestReport,
    TestReportStatus, TestReportSummary, TestStep, TestStepRole, banned_claim_word, plan_note_line,
    s3,
};
use serde_json::json;

const RUN: &str = "01M4XTESTRUNDIRECT00000001";
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

fn step(
    role: TestStepRole,
    command: &str,
    exit: Option<i32>,
    counts: Option<(u32, u32, u32)>,
) -> TestStep {
    TestStep {
        role,
        source: TestCommandSource::Detected,
        command: command.to_string(),
        exit_code: exit,
        timed_out: exit.is_none(),
        duration_ms: 1200,
        limit_s: None,
        counts: counts.map(|(passed, failed, skipped)| TestCounts {
            passed,
            failed,
            skipped,
        }),
        counts_from: counts.map(|_| "pytest".to_string()),
        output_tail: "12 passed in 0.31s\n".to_string(),
        output_truncated: false,
        logged: None,
    }
}

fn test_step(exit: i32, counts: Option<(u32, u32, u32)>) -> TestStep {
    step(
        TestStepRole::Test,
        "python3 -m pytest -q",
        Some(exit),
        counts,
    )
}

fn report(steps: Vec<TestStep>) -> TestReport {
    TestReport {
        schema_version: 1,
        run_id: RUN.to_string(),
        head_sha: HEAD.to_string(),
        status: TestReportStatus::Ran,
        reason: None,
        started_at: Some("2026-10-16T21:00:00Z".to_string()),
        duration_ms: 4_200,
        steps,
        summary: None,
    }
}

// ---- the summary ------------------------------------------------------------------------------------------------------------

#[test]
fn one_green_test_step_gives_counts_and_the_claim() {
    let s = report(vec![test_step(0, Some((12, 0, 0)))]).summary();
    assert_eq!(s.status, TestReportStatus::Ran);
    assert_eq!(s.steps_run, 1);
    assert!(s.all_exit_zero);
    assert!(!s.setup_failed);
    assert_eq!(s.exit_code, Some(0));
    assert_eq!(
        s.counts,
        Some(TestCounts {
            passed: 12,
            failed: 0,
            skipped: 0
        })
    );
    assert_eq!(s.command.as_deref(), Some("python3 -m pytest -q"));
    assert_eq!(s.duration_ms, 4_200);
    assert_eq!(
        s.claim().as_deref(),
        Some("repository tests pass in a clean checkout: 12 of 12")
    );
    assert_eq!(
        s.sentence(),
        "repository tests pass in a clean checkout: 12 of 12."
    );
}

#[test]
fn skipped_tests_stay_out_of_n_and_are_named_apart() {
    let s = report(vec![test_step(0, Some((10, 0, 3)))]).summary();
    assert_eq!(
        s.claim().as_deref(),
        Some("repository tests pass in a clean checkout: 10 of 10")
    );
    assert_eq!(
        s.sentence(),
        "repository tests pass in a clean checkout: 10 of 10 (3 skipped)."
    );
}

#[test]
fn a_nonzero_exit_with_counts_gives_no_claim_and_says_what_the_runner_reported() {
    let s = report(vec![test_step(1, Some((18, 2, 0)))]).summary();
    assert!(!s.all_exit_zero);
    assert_eq!(s.exit_code, Some(1));
    assert_eq!(s.failed_command.as_deref(), Some("python3 -m pytest -q"));
    assert_eq!(s.claim(), None);
    assert_eq!(
        s.sentence(),
        "`python3 -m pytest -q` exited 1 in a clean checkout; the runner reported 18 passed, 2 failed."
    );
}

#[test]
fn exit_zero_with_failures_in_the_counts_gives_no_claim() {
    let s = report(vec![test_step(0, Some((9, 1, 0)))]).summary();
    assert_eq!(s.claim(), None);
}

#[test]
fn exit_zero_without_counts_gives_no_claim_and_says_the_counts_could_not_be_read() {
    let s = report(vec![test_step(0, None)]).summary();
    assert!(s.all_exit_zero);
    assert_eq!(s.counts, None);
    assert_eq!(s.claim(), None);
    assert_eq!(
        s.sentence(),
        "`python3 -m pytest -q` exited 0 in a clean checkout; the runner's test counts could not be read."
    );
}

#[test]
fn exit_zero_with_zero_tests_gives_no_claim() {
    let s = report(vec![test_step(0, Some((0, 0, 0)))]).summary();
    assert_eq!(s.claim(), None);
    assert_eq!(
        s.sentence(),
        "`python3 -m pytest -q` exited 0 in a clean checkout and the runner reported 0 tests."
    );
}

#[test]
fn a_nonzero_exit_without_counts_names_the_exit_and_the_missing_counts() {
    let s = report(vec![test_step(2, None)]).summary();
    assert_eq!(
        s.sentence(),
        "`python3 -m pytest -q` exited 2 in a clean checkout; the runner's test counts could not be read."
    );
}

#[test]
fn a_failed_setup_step_blocks_the_claim_and_is_named() {
    let setup = step(TestStepRole::Setup, "npm ci", Some(1), None);
    let s = report(vec![setup]).summary();
    assert!(s.setup_failed);
    assert_eq!(s.failed_command.as_deref(), Some("npm ci"));
    assert_eq!(s.exit_code, Some(1));
    assert_eq!(s.claim(), None);
    assert_eq!(
        s.sentence(),
        "Setup `npm ci` exited 1 in a clean checkout, so the test command did not run."
    );
}

#[test]
fn a_green_setup_and_a_green_test_step_give_the_claim_from_the_test_step_only() {
    let setup = step(TestStepRole::Setup, "npm ci", Some(0), None);
    let s = report(vec![
        setup,
        step(TestStepRole::Test, "npm test", Some(0), Some((7, 0, 0))),
    ])
    .summary();
    assert_eq!(s.steps_run, 2);
    assert_eq!(s.command.as_deref(), Some("npm test"));
    assert_eq!(
        s.claim().as_deref(),
        Some("repository tests pass in a clean checkout: 7 of 7")
    );
}

/// A test step of `runner` that ran `command` and printed these counts.
fn runner_step(command: &str, runner: &str, counts: (u32, u32, u32)) -> TestStep {
    let mut s = step(TestStepRole::Test, command, Some(0), Some(counts));
    s.counts_from = Some(runner.to_string());
    s
}

fn counts(passed: u32, failed: u32, skipped: u32) -> Option<TestCounts> {
    Some(TestCounts {
        passed,
        failed,
        skipped,
    })
}

#[test]
fn counts_exist_only_when_every_test_step_has_them() {
    let one_missing = report(vec![test_step(0, Some((3, 0, 0))), test_step(0, None)]).summary();
    assert_eq!(one_missing.counts, None);
    assert_eq!(one_missing.claim(), None);
    let setup_only = report(vec![step(TestStepRole::Setup, "npm ci", Some(0), None)]).summary();
    assert_eq!(setup_only.counts, None);
    assert_eq!(setup_only.claim(), None);
    let no_steps = report(vec![]).summary();
    assert_eq!(no_steps.counts, None);
    assert_eq!(no_steps.claim(), None);
    assert_eq!(no_steps.steps_run, 0);
}

/// The clean-account run of a stranger: the lifecycle log named two commands over one suite of 13 tests, and the card said "26 of 26".
#[test]
fn two_commands_over_the_same_tests_are_not_added_twice() {
    let s = report(vec![
        runner_step(
            "python3 -m unittest -v test_slugify",
            "unittest",
            (13, 0, 0),
        ),
        runner_step(
            "python3 -m unittest discover -s . -p 'test_*.py'",
            "unittest",
            (13, 0, 0),
        ),
    ])
    .summary();
    assert_eq!(s.counts, counts(13, 0, 0));
    assert_eq!(
        s.claim().as_deref(),
        Some("repository tests pass in a clean checkout: 13 of 13")
    );
    assert_eq!(
        s.sentence(),
        "repository tests pass in a clean checkout: 13 of 13."
    );
}

#[test]
fn two_steps_of_one_runner_give_the_larger_counts_never_more_than_either() {
    // A command that selects part of a suite adds nothing to the command that runs all of it.
    let whole_then_part = report(vec![
        runner_step("python3 -m pytest -q", "pytest", (13, 0, 0)),
        runner_step("python3 -m pytest -q tests/unit", "pytest", (3, 0, 0)),
    ])
    .summary();
    assert_eq!(whole_then_part.counts, counts(13, 0, 0));
    let part_then_whole = report(vec![
        runner_step("python3 -m pytest -q tests/unit", "pytest", (3, 0, 0)),
        runner_step("python3 -m pytest -q", "pytest", (13, 0, 0)),
    ])
    .summary();
    assert_eq!(part_then_whole.counts, counts(13, 0, 0));
    // The failures of the part are failures of the whole: they are not counted twice either.
    let mut failing = report(vec![
        runner_step("python3 -m pytest -q", "pytest", (11, 2, 1)),
        runner_step("python3 -m pytest -q tests/unit", "pytest", (1, 2, 0)),
    ]);
    for s in &mut failing.steps {
        s.exit_code = Some(1);
    }
    let failing = failing.summary();
    assert_eq!(failing.counts, counts(11, 2, 1));
    assert_eq!(failing.claim(), None);
    assert_eq!(
        failing.sentence(),
        "`python3 -m pytest -q` exited 1 in a clean checkout; the runner reported 11 passed, 2 failed."
    );
}

#[test]
fn different_runners_are_different_suites_and_add_up() {
    let s = report(vec![
        runner_step("cargo test", "cargo", (120, 0, 0)),
        runner_step("python3 -m pytest -q", "pytest", (40, 0, 3)),
    ])
    .summary();
    assert_eq!(s.counts, counts(160, 0, 3));
    assert_eq!(
        s.sentence(),
        "repository tests pass in a clean checkout: 160 of 160 (3 skipped)."
    );
    // Two commands of each runner, as the lifecycle log of a run in a repository with a backend and a frontend would name them.
    let four = report(vec![
        runner_step("cargo test", "cargo", (120, 0, 0)),
        runner_step("cargo test -p core", "cargo", (30, 0, 0)),
        runner_step("npm test", "jest", (40, 0, 0)),
        runner_step("npm test -- --runInBand", "jest", (40, 0, 0)),
    ])
    .summary();
    assert_eq!(four.counts, counts(160, 0, 0));
}

#[test]
fn two_runners_that_report_the_same_counts_ran_the_same_tests() {
    // pytest runs the tests of a unittest suite: the same 13 tests under two runner names are 13 tests.
    let s = report(vec![
        runner_step("python3 -m unittest -v", "unittest", (13, 0, 0)),
        runner_step("python3 -m pytest -q", "pytest", (13, 0, 0)),
    ])
    .summary();
    assert_eq!(s.counts, counts(13, 0, 0));
    // The same counts with a different skip count are not the same tests.
    let s = report(vec![
        runner_step("cargo test", "cargo", (13, 0, 0)),
        runner_step("python3 -m pytest -q", "pytest", (13, 0, 1)),
    ])
    .summary();
    assert_eq!(s.counts, counts(26, 0, 1));
}

#[test]
fn steps_without_a_runner_name_form_one_group_and_the_order_does_not_matter() {
    let mut a = runner_step("make test", "x", (5, 0, 0));
    a.counts_from = None;
    let mut b = runner_step("make check", "x", (8, 0, 0));
    b.counts_from = None;
    let c = runner_step("go test ./...", "go", (4, 0, 0));
    let forward = report(vec![a.clone(), b.clone(), c.clone()]).summary();
    let backward = report(vec![c, b, a]).summary();
    assert_eq!(forward.counts, counts(12, 0, 0));
    assert_eq!(forward.counts, backward.counts);
}

/// Whatever the steps are, the counts are never more than all the steps added and never less than the largest step: a count never exceeds the tests that exist.
#[test]
fn the_counts_stay_between_the_largest_step_and_the_sum_of_the_steps() {
    let runners = ["pytest", "unittest", "cargo", "jest"];
    let mut seed = 7_u32;
    let mut next = move |m: u32| {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) % m
    };
    for _ in 0..200 {
        let n = 1 + next(5) as usize;
        let steps: Vec<TestStep> = (0..n)
            .map(|i| {
                runner_step(
                    &format!("cmd {i}"),
                    runners[next(4) as usize],
                    (next(30), next(3), next(4)),
                )
            })
            .collect();
        let got = report(steps.clone()).summary().counts.unwrap();
        let pass = |f: fn(&TestCounts) -> u32| -> (u32, u32) {
            let all: Vec<u32> = steps.iter().map(|s| f(&s.counts.unwrap())).collect();
            (*all.iter().max().unwrap(), all.iter().sum())
        };
        for (value, (low, high)) in [
            (got.passed, pass(|c| c.passed)),
            (got.failed, pass(|c| c.failed)),
            (got.skipped, pass(|c| c.skipped)),
        ] {
            assert!(
                (low..=high).contains(&value),
                "{value} outside {low}..={high}"
            );
        }
    }
}

#[test]
fn a_step_stopped_at_the_time_limit_blocks_the_claim() {
    let mut r = report(vec![
        test_step(0, Some((3, 0, 0))),
        step(TestStepRole::Test, "sleep 600", None, None),
    ]);
    r.status = TestReportStatus::TimedOut;
    let s = r.summary();
    assert!(!s.all_exit_zero);
    assert_eq!(s.steps_run, 1);
    assert_eq!(s.failed_command.as_deref(), Some("sleep 600"));
    assert_eq!(s.claim(), None);
    assert_eq!(
        s.sentence(),
        "`sleep 600` timed out after 1 s; the commands that finished are listed in the report."
    );
}

#[test]
fn a_report_that_made_no_attempt_says_why_in_one_sentence() {
    let none = TestReport::without_steps(
        RUN,
        HEAD,
        TestReportStatus::NoCommand,
        Some("no test command found".into()),
    )
    .summary();
    assert_eq!(none.claim(), None);
    assert_eq!(
        none.sentence(),
        "No test command was found, so no clean-checkout test report was made."
    );
    let not_run = TestReport::without_steps(
        RUN,
        HEAD,
        TestReportStatus::NotRun,
        Some("the checkout failed".into()),
    )
    .summary();
    assert_eq!(not_run.reason.as_deref(), Some("the checkout failed"));
    assert_eq!(
        not_run.sentence(),
        "The clean-checkout test report was not made: the checkout failed."
    );
    assert_eq!(
        TestReportSummary::off().sentence(),
        "The clean-checkout test report was switched off for this run."
    );
    assert_eq!(TestReportSummary::off().status, TestReportStatus::Off);
    assert_eq!(TestReportSummary::off().claim(), None);
}

#[test]
fn the_summary_is_pure_and_ignores_a_summary_the_worker_wrote() {
    let honest = report(vec![test_step(1, Some((1, 1, 0)))]);
    let mut lying = honest.clone();
    lying.summary = Some(report(vec![test_step(0, Some((99, 0, 0)))]).summary());
    assert_eq!(lying.summary(), honest.summary());
    assert_eq!(honest.summary().claim(), None);
}

#[test]
fn no_sentence_uses_a_word_the_product_may_not_claim() {
    let cases = vec![
        report(vec![test_step(0, Some((12, 0, 0)))]),
        report(vec![test_step(0, Some((12, 0, 4)))]),
        report(vec![test_step(1, Some((18, 2, 0)))]),
        report(vec![test_step(0, None)]),
        report(vec![test_step(0, Some((0, 0, 0)))]),
        report(vec![step(TestStepRole::Setup, "npm ci", Some(1), None)]),
        report(vec![step(TestStepRole::Test, "sleep 600", None, None)]),
        TestReport::without_steps(RUN, HEAD, TestReportStatus::NoCommand, None),
        TestReport::without_steps(
            RUN,
            HEAD,
            TestReportStatus::NotRun,
            Some("the checkout failed".into()),
        ),
    ];
    for c in cases {
        let s = c.summary();
        let text = format!("{} {}", s.sentence(), s.claim().unwrap_or_default());
        assert_eq!(banned_claim_word(&text), None, "{text}");
    }
    assert_eq!(
        banned_claim_word(&TestReportSummary::off().sentence()),
        None
    );
}

#[test]
fn a_command_with_backticks_cannot_break_out_of_the_sentence() {
    let s = report(vec![step(TestStepRole::Test, "echo `date`", Some(3), None)]).summary();
    assert!(!s.sentence().contains("`date`"), "{}", s.sentence());
    assert!(
        s.sentence().starts_with("`echo 'date'` exited 3"),
        "{}",
        s.sentence()
    );
}

// ---- the wire ---------------------------------------------------------------------------------------------------------------

#[test]
fn the_report_round_trips_and_leaves_unset_members_out() {
    let r = report(vec![test_step(0, Some((12, 0, 0)))]);
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["schemaVersion"], 1);
    assert_eq!(v["status"], "ran");
    assert_eq!(v["steps"][0]["role"], "test");
    assert_eq!(v["steps"][0]["source"], "detected");
    assert_eq!(v["steps"][0]["countsFrom"], "pytest");
    assert_eq!(
        v["steps"][0]["counts"],
        json!({"passed": 12, "failed": 0, "skipped": 0})
    );
    assert!(v.get("summary").is_none(), "{v}");
    assert!(v.get("reason").is_none());
    assert!(v["steps"][0].get("timedOut").is_none());
    assert!(v["steps"][0].get("logged").is_none());
    assert_eq!(serde_json::from_value::<TestReport>(v).unwrap(), r);
}

#[test]
fn a_reader_ignores_an_unknown_member() {
    let mut v = serde_json::to_value(report(vec![test_step(0, Some((1, 0, 0)))])).unwrap();
    v["somethingNew"] = json!(1);
    v["steps"][0]["alsoNew"] = json!("x");
    assert!(serde_json::from_value::<TestReport>(v).is_ok());
}

#[test]
fn the_evidence_member_is_left_out_when_unset_and_round_trips_when_set() {
    let base = json!({
        "outcome": "reported_done",
        "label": "Cline reported done",
        "modelId": "us.anthropic.claude-sonnet-5-5",
        "rules": [],
        "headSource": "final"
    });
    let plain: DirectEvidence = serde_json::from_value(base.clone()).unwrap();
    assert_eq!(plain.test_report, None);
    assert!(
        serde_json::to_value(&plain)
            .unwrap()
            .get("testReport")
            .is_none()
    );
    let mut with = plain.clone();
    with.test_report = Some(report(vec![test_step(0, Some((12, 0, 0)))]).summary());
    let v = serde_json::to_value(&with).unwrap();
    assert_eq!(v["testReport"]["counts"]["passed"], 12);
    assert_eq!(serde_json::from_value::<DirectEvidence>(v).unwrap(), with);
}

#[test]
fn the_request_member_is_sent_only_when_it_switches_the_report_off() {
    let base = json!({
        "prompt": "Add a function.",
        "target": {"mode": "s3-only"},
        "model": "us.anthropic.claude-sonnet-5-5",
        "rules": []
    });
    let on: StartDirectRunRequest = serde_json::from_value(base.clone()).unwrap();
    assert_eq!(on.report_tests, None);
    assert!(
        serde_json::to_value(&on)
            .unwrap()
            .get("reportTests")
            .is_none()
    );
    let mut off_v = base.clone();
    off_v["reportTests"] = json!(false);
    let off: StartDirectRunRequest = serde_json::from_value(off_v.clone()).unwrap();
    assert_eq!(off.report_tests, Some(false));
    assert_eq!(serde_json::to_value(&off).unwrap(), off_v);
    // Requests stay strict: a member nobody defined is still refused.
    let mut bad = base;
    bad["reportTest"] = json!(false);
    assert!(serde_json::from_value::<StartDirectRunRequest>(bad).is_err());
}

#[test]
fn the_object_keys_are_the_worker_prefix_and_the_evidence_prefix() {
    assert_eq!(
        s3::test_report_key(RUN),
        format!("runs/{RUN}/agents/direct/turns/1/test-report.json")
    );
    assert_eq!(
        s3::evidence_test_report_key(RUN),
        format!("runs/{RUN}/evidence/test-report.json")
    );
}

// ---- validate and bounded -----------------------------------------------------------------------------------------------------

#[test]
fn a_report_about_this_run_and_head_within_bounds_is_valid() {
    assert_eq!(
        report(vec![test_step(0, Some((12, 0, 0)))]).validate(RUN, HEAD),
        Ok(())
    );
    let none = TestReport::without_steps(RUN, HEAD, TestReportStatus::NoCommand, None);
    assert_eq!(none.validate(RUN, HEAD), Ok(()));
}

#[test]
fn a_report_about_another_run_or_head_or_version_is_refused() {
    let r = report(vec![test_step(0, Some((1, 0, 0)))]);
    assert!(
        r.validate("01M4XOTHERRUN000000000001", HEAD)
            .unwrap_err()
            .contains("run")
    );
    assert!(
        r.validate(RUN, &"f".repeat(40))
            .unwrap_err()
            .contains("head")
    );
    let mut v2 = r;
    v2.schema_version = 2;
    assert!(
        v2.validate(RUN, HEAD)
            .unwrap_err()
            .contains("schemaVersion")
    );
}

#[test]
fn too_many_steps_a_long_tail_a_long_command_or_a_big_report_is_refused() {
    let many = report(
        (0..=MAX_REPORT_STEPS)
            .map(|_| test_step(0, Some((1, 0, 0))))
            .collect(),
    );
    assert!(many.validate(RUN, HEAD).unwrap_err().contains("steps"));
    let mut tail = report(vec![test_step(0, Some((1, 0, 0)))]);
    tail.steps[0].output_tail = "x".repeat(MAX_REPORT_TAIL_BYTES + 1);
    assert!(tail.validate(RUN, HEAD).unwrap_err().contains("outputTail"));
    let mut cmd = report(vec![test_step(0, Some((1, 0, 0)))]);
    cmd.steps[0].command = "c".repeat(MAX_REPORT_COMMAND_CHARS + 1);
    assert!(cmd.validate(RUN, HEAD).unwrap_err().contains("command"));
    let mut reason = TestReport::without_steps(
        RUN,
        HEAD,
        TestReportStatus::NotRun,
        Some("r".repeat(MAX_REPORT_REASON_CHARS + 1)),
    );
    assert!(reason.validate(RUN, HEAD).unwrap_err().contains("reason"));
    reason.reason = Some("r".repeat(MAX_REPORT_REASON_CHARS));
    assert_eq!(reason.validate(RUN, HEAD), Ok(()));
}

#[test]
fn a_control_character_in_a_command_or_a_reason_is_refused_but_newlines_in_the_tail_are_not() {
    let mut r = report(vec![test_step(0, Some((1, 0, 0)))]);
    r.steps[0].output_tail = "line one\nline two\ttabbed\n".into();
    assert_eq!(r.validate(RUN, HEAD), Ok(()));
    r.steps[0].command = "pytest\u{7}".into();
    assert!(r.validate(RUN, HEAD).unwrap_err().contains("control"));
    let mut tail = report(vec![test_step(0, Some((1, 0, 0)))]);
    tail.steps[0].output_tail = "bell \u{7} here".into();
    assert!(tail.validate(RUN, HEAD).unwrap_err().contains("control"));
}

#[test]
fn a_worker_cannot_write_the_off_status_and_the_status_must_fit_the_steps() {
    let off = TestReport::without_steps(RUN, HEAD, TestReportStatus::Off, None);
    assert!(off.validate(RUN, HEAD).unwrap_err().contains("off"));
    let mut none_with_steps = report(vec![test_step(0, Some((1, 0, 0)))]);
    none_with_steps.status = TestReportStatus::NoCommand;
    assert!(
        none_with_steps
            .validate(RUN, HEAD)
            .unwrap_err()
            .contains("status")
    );
    let ran_without_steps = TestReport::without_steps(RUN, HEAD, TestReportStatus::Ran, None);
    assert!(
        ran_without_steps
            .validate(RUN, HEAD)
            .unwrap_err()
            .contains("status")
    );
}

#[test]
fn a_step_cannot_be_both_timed_out_and_have_an_exit_code() {
    let mut r = report(vec![test_step(0, Some((1, 0, 0)))]);
    r.steps[0].timed_out = true;
    assert!(r.validate(RUN, HEAD).unwrap_err().contains("timedOut"));
}

#[test]
fn bounded_cuts_the_report_to_its_limits_and_the_result_is_valid() {
    let mut r = report(
        (0..MAX_REPORT_STEPS + 8)
            .map(|_| test_step(0, Some((1, 0, 0))))
            .collect(),
    );
    r.steps[0].command = format!("{}é", "c".repeat(MAX_REPORT_COMMAND_CHARS + 50));
    r.steps[1].output_tail = format!(
        "{}é\n{}",
        "a".repeat(MAX_REPORT_TAIL_BYTES + 5000),
        "tail end"
    );
    r.steps[2].command = "pytest\u{7} -q".into();
    r.reason = Some("r".repeat(MAX_REPORT_REASON_CHARS + 600));
    let b = r.bounded();
    assert_eq!(b.steps.len(), MAX_REPORT_STEPS);
    assert_eq!(b.steps[0].command.chars().count(), MAX_REPORT_COMMAND_CHARS);
    assert!(b.steps[1].output_tail.len() <= MAX_REPORT_TAIL_BYTES);
    assert!(
        b.steps[1].output_tail.ends_with("tail end"),
        "the end of the output is kept"
    );
    assert!(b.steps[1].output_truncated);
    assert_eq!(b.steps[2].command, "pytest -q");
    assert_eq!(
        b.reason.as_ref().unwrap().chars().count(),
        MAX_REPORT_REASON_CHARS
    );
    assert_eq!(b.validate(RUN, HEAD), Ok(()));
}

// ---- the byte limit is enforced by `bounded`, not only checked by `validate` ----------------------------

/// The bytes of output of a step of [`escape_heavy`]: more than the default tail, so that the most steps together are over the byte limit.
const HEAVY_TAIL: usize = 400 * 1024;

/// A report within every field limit that, serialized, is over `MAX_REPORT_BYTES`: the most steps, each with output that is all newlines,
/// quotes and backslashes, each of which JSON writes in two bytes.
fn escape_heavy(steps: usize) -> TestReport {
    report(
        (0..steps)
            .map(|i| {
                let mut s = step(
                    TestStepRole::Test,
                    &format!("python3 -m pytest -q part{i}"),
                    Some(0),
                    Some((3, 0, 0)),
                );
                let end = format!("END{i}");
                let body = "\n\"\\".repeat((HEAVY_TAIL - end.len()) / 3);
                s.output_tail = format!("{body}{end}");
                s
            })
            .collect(),
    )
}

#[test]
fn a_report_within_every_field_limit_can_still_be_over_the_byte_limit() {
    // TRIGGER PRESENT: the input is the case the review found, so the test fails if the helper stops building it.
    let r = escape_heavy(MAX_REPORT_STEPS);
    assert!(
        r.steps
            .iter()
            .all(|s| s.output_tail.len() <= MAX_REPORT_TAIL_BYTES)
    );
    assert!(serde_json::to_vec(&r).unwrap().len() > MAX_REPORT_BYTES);
    assert!(r.validate(RUN, HEAD).unwrap_err().contains("larger than"));
}

#[test]
fn bounded_enforces_the_byte_limit_so_validate_passes_on_its_output() {
    let before = escape_heavy(MAX_REPORT_STEPS);
    let b = before.clone().bounded();
    assert_eq!(b.validate(RUN, HEAD), Ok(()));
    assert!(serde_json::to_vec(&b).unwrap().len() <= MAX_REPORT_BYTES);
    // The steps, their commands and their counts survive; only the outputs got shorter, the end of each is the part kept, and a step that
    // lost output says so.
    assert_eq!(b.steps.len(), MAX_REPORT_STEPS);
    let mut cut = 0;
    for (i, s) in b.steps.iter().enumerate() {
        assert_eq!(s.command, format!("python3 -m pytest -q part{i}"));
        assert_eq!(
            s.counts,
            Some(TestCounts {
                passed: 3,
                failed: 0,
                skipped: 0
            })
        );
        assert!(
            s.output_tail.ends_with(&format!("END{i}")),
            "step {i}: the end of the output is the part kept"
        );
        if s.output_tail.len() < before.steps[i].output_tail.len() {
            cut += 1;
            assert!(
                s.output_truncated,
                "step {i} lost output and did not say so"
            );
        } else {
            assert!(
                !s.output_truncated,
                "step {i} kept its output and says it was cut"
            );
        }
    }
    assert!(
        cut > 0,
        "nothing was cut, yet the report was over the limit"
    );
    // The last step, the test run, keeps more than the first one: the earliest of equal tails is cut first.
    assert!(b.steps[MAX_REPORT_STEPS - 1].output_tail.len() >= b.steps[0].output_tail.len());
}

#[test]
fn bounded_leaves_a_report_that_fits_exactly_as_it_was() {
    let r = escape_heavy(2);
    let b = r.clone().bounded();
    assert_eq!(b.steps, r.steps, "two steps are well inside the limit");
    assert!(b.steps.iter().all(|s| !s.output_truncated));
}

// ---- a step that was not run is not a failure ---------------------------------------------------------------
//
// agentd marks a command it did not run, or whose result its environment decided (a command that needs a container runtime the session does
// not have), with `countsFrom` "not run" and `NOT RUN: <reason>` at the start of the output. It is neither a pass nor a failure: it keeps the
// claim away, and it is not red.

fn not_run_step(role: TestStepRole, command: &str, reason: &str) -> TestStep {
    TestStep {
        role,
        source: TestCommandSource::TestCommandFile,
        command: command.to_string(),
        exit_code: None,
        timed_out: false,
        duration_ms: 0,
        limit_s: None,
        counts: None,
        counts_from: Some("not run".to_string()),
        output_tail: format!("NOT RUN: {reason}"),
        output_truncated: false,
        logged: None,
    }
}

#[test]
fn a_command_that_was_not_run_is_not_a_failing_command_and_not_a_pass() {
    let r = report(vec![
        not_run_step(
            TestStepRole::Test,
            "docker compose up",
            "the session has no container runtime",
        ),
        test_step(0, Some((12, 0, 0))),
    ]);
    let s = r.summary();
    assert_eq!(s.failed_command, None, "{s:?}");
    assert_eq!(s.exit_code, Some(0));
    assert!(!s.setup_failed);
    assert_eq!(s.steps_run, 1, "only the command that ran counts as run");
    assert_eq!(
        s.claim(),
        None,
        "a command did not run, so the tests are not all shown to pass"
    );
    assert_eq!(
        s.sentence(),
        "NOT RUN: `docker compose up` (the session has no container runtime)."
    );
    assert_eq!(banned_claim_word(&s.sentence()), None);
}

#[test]
fn a_report_of_only_commands_not_run_says_so_and_is_not_red() {
    let r = report(vec![not_run_step(
        TestStepRole::Test,
        "./mvnw verify",
        "the session has no container runtime",
    )]);
    let s = r.summary();
    assert_eq!(s.steps_run, 0);
    assert_eq!(s.exit_code, None);
    assert_eq!(s.failed_command, None);
    assert!(!s.all_exit_zero);
    assert_eq!(
        s.sentence(),
        "NOT RUN: `./mvnw verify` (the session has no container runtime)."
    );
}

#[test]
fn a_real_failure_beside_a_command_not_run_is_still_red() {
    let r = report(vec![
        not_run_step(
            TestStepRole::Test,
            "docker compose up",
            "the session has no container runtime",
        ),
        test_step(1, Some((10, 2, 0))),
    ]);
    let s = r.summary();
    assert_eq!(s.exit_code, Some(1));
    assert_eq!(s.failed_command.as_deref(), Some("python3 -m pytest -q"));
    assert!(s.sentence().contains("exited 1"), "{}", s.sentence());
}

#[test]
fn a_setup_not_run_does_not_stop_the_tests_from_counting() {
    let r = report(vec![
        not_run_step(
            TestStepRole::Setup,
            "docker compose pull",
            "the session has no container runtime",
        ),
        test_step(0, Some((4, 0, 0))),
    ]);
    let s = r.summary();
    assert!(!s.setup_failed, "{s:?}");
    assert_eq!(s.failed_command, None);
}

#[test]
fn the_output_of_a_command_cannot_make_its_step_not_run() {
    // The words in the output are the repository's; the marker is `countsFrom`, which only agentd sets.
    let mut s = test_step(1, Some((0, 1, 0)));
    s.output_tail = "NOT RUN: the session has no container runtime".to_string();
    let sum = report(vec![s]).summary();
    assert_eq!(sum.exit_code, Some(1));
    assert!(sum.failed_command.is_some());
    assert!(sum.sentence().contains("exited 1"), "{}", sum.sentence());
}

#[test]
fn a_report_with_commands_not_run_passes_its_own_bounds() {
    let r = report(vec![
        not_run_step(
            TestStepRole::Test,
            "docker compose up",
            "the session has no container runtime",
        ),
        test_step(0, Some((12, 0, 0))),
    ]);
    assert_eq!(r.clone().bounded().validate(RUN, HEAD), Ok(()));
}

// ---- the note on a command stopped at its limit --------------------------------------------------------------------------------
//
// EVIDENCE.md says which command was stopped, at which limit, and which settings change that limit. The sentence stays what it was (`ham` adds its own
// advice to it, and the same words would show twice), so the note is a second text that the packager adds.

const LIMIT_NOTE: &str = "`cargo test` was stopped at the limit of 300 s. The limit for one command is the client setting `run.report_command_minutes` (`ham config set run.report_command_minutes <minutes>`). When a run sets none, the stage's own limit applies; the stage variable `AGENTD_REPORT_STEP_S` sets it.";

fn timed_out_report(command: &str, limit_s: Option<u32>) -> TestReport {
    let mut stopped = step(TestStepRole::Test, command, None, None);
    stopped.limit_s = limit_s;
    let mut r = report(vec![stopped]);
    r.status = TestReportStatus::TimedOut;
    r.reason = Some("a command was stopped at the report time limit".to_string());
    r
}

#[test]
fn a_stopped_command_gets_a_note_with_its_limit_and_the_names_of_the_settings() {
    let s = timed_out_report("cargo test", Some(300)).summary();
    assert_eq!(s.limit_note().as_deref(), Some(LIMIT_NOTE));
    assert_eq!(banned_claim_word(LIMIT_NOTE), None);
}

#[test]
fn the_note_names_the_limit_the_command_was_given_and_quotes_the_command() {
    let s = timed_out_report("sleep 600 && echo `date`", Some(60)).summary();
    let note = s.limit_note().unwrap();
    assert!(
        note.starts_with("`sleep 600 && echo 'date'` was stopped at the limit of 60 s. "),
        "{note}"
    );
    // A report an older worker wrote has no limit: the time the command ran stands in, as in the sentence.
    let older = timed_out_report("cargo test", None).summary();
    assert!(
        older
            .limit_note()
            .unwrap()
            .starts_with("`cargo test` was stopped at the limit of 1 s. "),
        "{:?}",
        older.limit_note()
    );
}

#[test]
fn there_is_no_note_when_no_command_was_stopped_at_its_limit() {
    assert_eq!(
        report(vec![test_step(0, Some((12, 0, 0)))])
            .summary()
            .limit_note(),
        None
    );
    assert_eq!(
        report(vec![test_step(1, Some((18, 2, 0)))])
            .summary()
            .limit_note(),
        None
    );
    // The whole report's time ran out between two commands: no command was stopped, and the per-command setting is not the cause.
    let mut r = report(vec![test_step(0, Some((3, 0, 0)))]);
    r.status = TestReportStatus::TimedOut;
    assert_eq!(r.summary().limit_note(), None);
    assert_eq!(TestReportSummary::off().limit_note(), None);
}

#[test]
fn the_sentence_does_not_carry_the_note_because_ham_adds_its_own_advice_to_it() {
    let s = timed_out_report("cargo test", Some(300)).summary();
    let sentence = s.sentence();
    assert_eq!(
        sentence,
        "`cargo test` timed out after 300 s; the commands that finished are listed in the report."
    );
    assert!(
        !sentence.contains("run.report_command_minutes"),
        "{sentence}"
    );
    assert!(!sentence.contains("AGENTD_REPORT_STEP_S"), "{sentence}");
}

// ---- how the command was chosen ------------------------------------------------------------------------------------------------
//
// When the repository names no test command, the harness reads the manifests at the tree root and picks one. For a Cargo workspace that is
// `cargo test`, which runs every member and can fill the disk or run past its limit. agentd puts one line at the head of the output of that step
// (after the NOT RUN line, when there is one); the summary turns it into a note that EVIDENCE.md prints, and only for a step of that kind.

const NOTE_LINE: &str = "PLAN NOTE: cargo workspace, 14 members";
const PLAN_NOTE: &str = "The repository names no test command, so the harness chose `cargo test` from the manifest (Cargo.toml). It runs the tests of the whole workspace, 14 members. A `.ham/test-command` file in the repository names a narrower command.";

fn cargo_step(source: TestCommandSource, exit: Option<i32>, tail: &str) -> TestStep {
    let mut s = step(TestStepRole::Test, "cargo test", exit, None);
    s.source = source;
    s.output_tail = tail.to_string();
    s
}

#[test]
fn the_line_agentd_writes_has_one_fixed_shape() {
    assert_eq!(plan_note_line(14), NOTE_LINE);
    assert_eq!(plan_note_line(2), "PLAN NOTE: cargo workspace, 2 members");
}

#[test]
fn only_a_cargo_test_step_chosen_from_the_manifest_carries_the_note() {
    let tail = format!("{NOTE_LINE}\nrunning 3 tests\n");
    let ok = cargo_step(TestCommandSource::Detected, Some(0), &tail);
    assert_eq!(ok.plan_note_members(), Some(14));
    // After the NOT RUN line agentd wrote for a step it marked as not run.
    let mut not_run = cargo_step(
        TestCommandSource::Detected,
        Some(127),
        &format!("NOT RUN: cargo is not installed here\n{NOTE_LINE}\nsh: 1: cargo: not found\n"),
    );
    not_run.counts_from = Some("not run".to_string());
    assert_eq!(not_run.plan_note_members(), Some(14));
    // A command the repository named, another command, a setup step and a line anywhere else say nothing.
    assert_eq!(
        cargo_step(TestCommandSource::TestCommandFile, Some(0), &tail).plan_note_members(),
        None
    );
    assert_eq!(
        cargo_step(TestCommandSource::LifecycleLog, Some(0), &tail).plan_note_members(),
        None
    );
    let mut other = cargo_step(TestCommandSource::Detected, Some(0), &tail);
    other.command = "cargo test --workspace".to_string();
    assert_eq!(other.plan_note_members(), None);
    let mut setup = cargo_step(TestCommandSource::Detected, Some(0), &tail);
    setup.role = TestStepRole::Setup;
    assert_eq!(setup.plan_note_members(), None);
    for later in [
        format!("compiling\n{NOTE_LINE}\n"),
        format!("\n{NOTE_LINE}\n"),
        format!("NOT RUN: x\ny\n{NOTE_LINE}\n"),
    ] {
        let s = cargo_step(TestCommandSource::Detected, Some(0), &later);
        assert_eq!(s.plan_note_members(), None, "{later:?}");
    }
    // A step that merely begins with the words "NOT RUN:" in its own output is not one agentd marked.
    let forged = cargo_step(
        TestCommandSource::Detected,
        Some(1),
        &format!("NOT RUN: forged\n{NOTE_LINE}\n"),
    );
    assert_eq!(forged.plan_note_members(), None);
}

#[test]
fn the_count_must_be_a_plain_number_of_at_least_two() {
    for bad in [
        "PLAN NOTE: cargo workspace, 1 members",
        "PLAN NOTE: cargo workspace, 0 members",
        "PLAN NOTE: cargo workspace, -3 members",
        "PLAN NOTE: cargo workspace, 14 members and more",
        "PLAN NOTE: cargo workspace, fourteen members",
        "PLAN NOTE: cargo workspace, 99999999999 members",
        "PLAN NOTE: cargo workspace,  14 members",
        "plan note: cargo workspace, 14 members",
        "PLAN NOTE: cargo workspace, 14 member",
    ] {
        let s = cargo_step(TestCommandSource::Detected, Some(0), &format!("{bad}\n"));
        assert_eq!(s.plan_note_members(), None, "{bad}");
    }
    let s = cargo_step(
        TestCommandSource::Detected,
        Some(0),
        "PLAN NOTE: cargo workspace, 2 members",
    );
    assert_eq!(
        s.plan_note_members(),
        Some(2),
        "the line may be the whole tail"
    );
}

#[test]
fn the_summary_carries_the_note_and_the_sentence_stays_what_it_was() {
    let plain = report(vec![cargo_step(
        TestCommandSource::Detected,
        Some(1),
        "boom\n",
    )])
    .summary();
    let noted = report(vec![cargo_step(
        TestCommandSource::Detected,
        Some(1),
        &format!("{NOTE_LINE}\nboom\n"),
    )])
    .summary();
    assert_eq!(plain.plan_note(), None);
    assert_eq!(noted.plan_note(), Some(PLAN_NOTE));
    assert_eq!(noted.sentence(), plain.sentence());
    assert_eq!(
        noted.sentence(),
        "`cargo test` exited 1 in a clean checkout; the runner's test counts could not be read."
    );
    assert_eq!(banned_claim_word(PLAN_NOTE), None);
}

#[test]
fn the_note_does_not_hide_the_reason_of_a_step_that_was_not_run_or_a_stopped_command() {
    // The disk filled: the NOT RUN sentence is the same with and without the note.
    let reason =
        "ran out of disk: couldn't create a temp dir: No space left on device (os error 28)";
    let mut full = cargo_step(
        TestCommandSource::Detected,
        Some(101),
        &format!("NOT RUN: {reason}\n{NOTE_LINE}\nerror: ...\n"),
    );
    full.counts_from = Some("not run".to_string());
    let s = report(vec![full]).summary();
    assert_eq!(s.sentence(), format!("NOT RUN: `cargo test` ({reason})."));
    assert_eq!(s.plan_note(), Some(PLAN_NOTE));
    assert_eq!(s.claim(), None);
    // A stopped command: the sentence and the limit note are the same, and the plan note comes beside them.
    let mut stopped = report(vec![cargo_step(
        TestCommandSource::Detected,
        None,
        &format!("{NOTE_LINE}\n"),
    )]);
    stopped.status = TestReportStatus::TimedOut;
    stopped.reason = Some("a command was stopped at the report time limit".to_string());
    let s = stopped.summary();
    assert_eq!(s.plan_note(), Some(PLAN_NOTE));
    assert!(s.limit_note().is_some());
    assert_eq!(
        s.sentence(),
        "`cargo test` timed out after 1 s; the commands that finished are listed in the report."
    );
}

#[test]
fn a_report_that_ran_clean_keeps_its_claim_and_gets_the_note_beside_it() {
    let mut green = cargo_step(
        TestCommandSource::Detected,
        Some(0),
        &format!("{NOTE_LINE}\nok\n"),
    );
    green.counts = Some(TestCounts {
        passed: 9,
        failed: 0,
        skipped: 0,
    });
    green.counts_from = Some("cargo".to_string());
    let s = report(vec![green]).summary();
    assert_eq!(
        s.sentence(),
        "repository tests pass in a clean checkout: 9 of 9."
    );
    assert_eq!(s.plan_note(), Some(PLAN_NOTE));
}

#[test]
fn a_summary_from_the_wire_reads_the_same_two_lines() {
    let s = report(vec![cargo_step(
        TestCommandSource::Detected,
        Some(1),
        &format!("{NOTE_LINE}\nboom\n"),
    )])
    .summary();
    let back: TestReportSummary =
        serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
    assert_eq!(back, s);
    assert_eq!(back.plan_note(), Some(PLAN_NOTE));
    // A summary an older build wrote has one line of reason, or none.
    let mut old = back.clone();
    old.reason =
        Some("NOT RUN: `docker compose up` (the session has no container runtime)".to_string());
    assert_eq!(old.plan_note(), None);
    old.reason = None;
    assert_eq!(old.plan_note(), None);
}
