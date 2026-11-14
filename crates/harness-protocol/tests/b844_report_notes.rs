//! The report names the command a failed install stopped, and says which commands of the lifecycle log its plan left out.
//!
//! Now:
//!
//! - the sentence of an install that stopped every test names the command, as the sentence does when other commands ran;
//! - a step that agentd chose from the layout of the tree to run in place of logged commands carries the head line
//!   ``PLAN NOTE: in place of `<command>` `` and its detail line says so;
//! - the first step of a report whose plan left logged commands out with nothing in their place carries ``PLAN NOTE: left out `<command>` ``
//!   and the details get a line for it.
//!
//! No member of the report or the summary is new: the facts ride at the head of a step's output, where agentd alone writes.
#![allow(clippy::unwrap_used)]

use harness_protocol::direct::{
    IN_PLACE_PREFIX, LEFT_OUT_PREFIX, MAX_SUMMARY_DETAIL_CHARS, MAX_SUMMARY_DETAILS,
    TestCommandSource, TestCounts, TestReport, TestReportStatus, TestStep, TestStepRole,
    in_place_line, left_out_line, plan_note_line,
};

const RUN: &str = "01M4XTESTRUNDIRECT00000001";
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
const LOGGED: &str = "python3 -m pytest tests/test_textstats.py -q";

fn ran(
    role: TestStepRole,
    source: TestCommandSource,
    command: &str,
    exit: i32,
    counts: Option<(u32, u32)>,
) -> TestStep {
    TestStep {
        role,
        source,
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
        counts_from: counts.map(|_| "pytest".to_string()),
        output_tail: String::new(),
        output_truncated: false,
        logged: None,
    }
}

fn setup(command: &str, exit: i32) -> TestStep {
    ran(
        TestStepRole::Setup,
        TestCommandSource::Detected,
        command,
        exit,
        None,
    )
}

fn logged(command: &str, exit: i32, counts: Option<(u32, u32)>) -> TestStep {
    ran(
        TestStepRole::Test,
        TestCommandSource::LifecycleLog,
        command,
        exit,
        counts,
    )
}

fn detected(command: &str, exit: i32, counts: Option<(u32, u32)>) -> TestStep {
    ran(
        TestStepRole::Test,
        TestCommandSource::Detected,
        command,
        exit,
        counts,
    )
}

fn with_head(mut step: TestStep, head: &str) -> TestStep {
    step.output_tail = format!("{head}\n{}", step.output_tail);
    step
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

// ---- the sentence of an install that stopped every test -------------------------------------------------------------------------------

#[test]
fn b844_an_install_that_stopped_every_test_names_the_command_it_stopped() {
    // The steps of the live report: `npm install` exited 1 and `node --test` was not started.
    let s = report(vec![
        setup("npm install", 1),
        skipped("node --test", "npm install"),
    ])
    .summary();
    assert!(s.setup_failed, "{s:?}");
    assert_eq!(s.command, None, "{s:?}");
    assert_eq!(s.claim(), None);
    assert_eq!(
        s.sentence(),
        "Setup `npm install` exited 1 in a clean checkout, so `node --test` did not run."
    );
}

#[test]
fn b844_more_than_one_command_stopped_by_the_install_is_counted_as_in_the_other_sentence() {
    let s = report(vec![
        setup("npm ci", 1),
        skipped("npm test", "npm ci"),
        skipped("npm run test:e2e", "npm ci"),
    ])
    .summary();
    assert_eq!(
        s.sentence(),
        "Setup `npm ci` exited 1 in a clean checkout, so `npm test` and 1 more did not run."
    );
    // Each command that did not run has its own line under the sentence.
    assert_eq!(
        s.details,
        [
            "`npm test`: not run (skipped: the setup step `npm ci` failed).",
            "`npm run test:e2e`: not run (skipped: the setup step `npm ci` failed).",
        ]
    );
}

#[test]
fn b844_a_report_that_lists_no_stopped_command_keeps_the_sentence_of_before() {
    // A report of an agentd that did not list the commands behind a failed setup.
    let alone = report(vec![setup("npm ci", 1)]).summary();
    assert_eq!(
        alone.sentence(),
        "Setup `npm ci` exited 1 in a clean checkout, so the test command did not run."
    );
    // The command that was not run has another reason than the setup: the sentence does not say the setup stopped it.
    let other = report(vec![
        setup("npm ci", 1),
        TestStep::not_run(
            TestStepRole::Test,
            TestCommandSource::LifecycleLog,
            "docker compose run tests",
            "the session has no container runtime",
            None,
        ),
    ])
    .summary();
    assert_eq!(
        other.sentence(),
        "Setup `npm ci` exited 1 in a clean checkout, so the test command did not run."
    );
}

// ---- a step that ran in place of logged commands --------------------------------------------------------------------------------------

#[test]
fn b844_the_lines_are_plan_notes_with_the_command_in_backticks() {
    assert_eq!(
        in_place_line(LOGGED, 0),
        "PLAN NOTE: in place of `python3 -m pytest tests/test_textstats.py -q`"
    );
    assert_eq!(
        left_out_line("npx jest src/a.test.ts", 2),
        "PLAN NOTE: left out `npx jest src/a.test.ts` and 2 more"
    );
    assert!(in_place_line("x", 0).starts_with(IN_PLACE_PREFIX));
    assert!(left_out_line("x", 0).starts_with(LEFT_OUT_PREFIX));
    // A backtick of the command's own cannot end the quote, and a long command is cut.
    assert_eq!(
        in_place_line("pytest `x`.py", 0),
        "PLAN NOTE: in place of `pytest 'x'.py`"
    );
    let long = in_place_line(&format!("pytest {}", "a".repeat(400)), 3);
    assert!(long.chars().count() < 120, "{long}");
    assert!(long.ends_with("` and 3 more"), "{long}");
}

#[test]
fn b844_a_step_that_ran_in_place_of_a_logged_command_says_so_in_its_detail_line() {
    // What the live run gives now: the Python layout runs beside the Node command that its install stopped.
    let r = report(vec![
        setup("npm install", 1),
        skipped("node --test", "npm install"),
        with_head(
            detected("python3 -m pytest -q", 0, Some((6, 0))),
            &in_place_line(LOGGED, 0),
        ),
    ]);
    assert_eq!(r.steps[2].in_place_of(), Some((LOGGED, 0)));
    let s = r.summary();
    assert_eq!(
        s.sentence(),
        "Setup `npm install` exited 1 in a clean checkout, so `node --test` did not run; for the test commands that ran, the runner reported 6 passed, 0 failed. The report chose a whole-suite command in place of 1 command of the lifecycle log that names part of a suite; details are in EVIDENCE.md."
    );
    assert_eq!(
        s.details,
        [
            "`node --test`: not run (skipped: the setup step `npm install` failed).",
            "`python3 -m pytest -q`: exit 0, 6 passed, 0 failed; it ran in place of `python3 -m pytest tests/test_textstats.py -q` of the lifecycle log, which names a test file, a test or a filter.",
        ]
    );
}

#[test]
fn b844_more_logged_commands_than_one_are_counted_and_the_claim_stays() {
    let r = report(vec![with_head(
        detected("python3 -m pytest -q", 0, Some((6, 0))),
        &in_place_line("pytest tests/test_a.py::test_x", 2),
    )]);
    let s = r.summary();
    assert_eq!(
        s.claim().as_deref(),
        Some("repository tests pass in a clean checkout: 6 of 6")
    );
    assert_eq!(
        s.sentence(),
        "repository tests pass in a clean checkout: 6 of 6. The report chose a whole-suite command in place of 3 commands of the lifecycle log that name parts of a suite; details are in EVIDENCE.md."
    );
    assert_eq!(
        s.details,
        [
            "`python3 -m pytest -q`: exit 0, 6 passed, 0 failed; it ran in place of `pytest tests/test_a.py::test_x` and 2 more of the lifecycle log, which name test files, tests or filters."
        ]
    );
}

#[test]
fn b844_the_line_is_read_only_where_agentd_writes_it() {
    let line = in_place_line(LOGGED, 0);
    // A command of the log or the goal card is the model's own: no step of the report stands in its place.
    let own = with_head(logged("python3 -m pytest -q", 0, Some((6, 0))), &line);
    assert_eq!(own.in_place_of(), None);
    // A setup step runs no test.
    let install = with_head(setup("npm ci", 0), &line);
    assert_eq!(install.in_place_of(), None);
    // The line in the middle of the output is output.
    let mut later = detected("python3 -m pytest -q", 0, Some((6, 0)));
    later.output_tail = format!("......\n{line}\n6 passed in 0.01s\n");
    assert_eq!(later.in_place_of(), None);
    assert_eq!(
        report(vec![later]).summary().details,
        ["`python3 -m pytest -q`: exit 0, 6 passed, 0 failed."]
    );
    // After the reason of a step that was not run, the line is read.
    let mut not_run = TestStep::not_run(
        TestStepRole::Test,
        TestCommandSource::Detected,
        ".ham-venv/bin/python -m pytest -q",
        "skipped: the setup step `pip` failed",
        None,
    );
    not_run.output_tail = format!("{}\n{line}", not_run.output_tail);
    assert_eq!(not_run.in_place_of(), Some((LOGGED, 0)));
    assert_eq!(
        not_run.not_run_reason(),
        Some("skipped: the setup step `pip` failed")
    );
}

#[test]
fn b844_a_line_that_is_not_in_the_form_agentd_writes_gives_nothing() {
    for head in [
        "PLAN NOTE: in place of pytest -q",
        "PLAN NOTE: in place of ``",
        "PLAN NOTE: in place of `pytest tests/a.py` and more",
        "PLAN NOTE: in place of `pytest tests/a.py` and 0 more",
        "PLAN NOTE: in place of `pytest tests/a.py` and 2 more, all tests were checked",
        "PLAN NOTE: in place of `pytest tests/a.py` checked",
        "PLAN NOTE: in place of",
        "plan note: in place of `pytest tests/a.py`",
    ] {
        let step = with_head(detected("python3 -m pytest -q", 0, Some((6, 0))), head);
        assert_eq!(step.in_place_of(), None, "{head}");
        assert_eq!(
            report(vec![step]).summary().details,
            ["`python3 -m pytest -q`: exit 0, 6 passed, 0 failed."],
            "{head}"
        );
    }
}

#[test]
fn b844_the_line_follows_the_note_of_a_cargo_workspace_and_both_are_read() {
    let mut step = detected("cargo test", 0, Some((40, 0)));
    step.output_tail = format!(
        "{}\n{}\ntest result: ok. 40 passed\n",
        plan_note_line(14),
        in_place_line("cargo test --test api", 0)
    );
    assert_eq!(step.plan_note_members(), Some(14));
    assert_eq!(step.in_place_of(), Some(("cargo test --test api", 0)));
}

#[test]
fn b844_a_service_a_step_ran_without_is_still_read_behind_the_line() {
    let mut step = detected("python3 -m pytest -q", 1, Some((2, 4)));
    step.output_tail = format!(
        "{}\nNEEDS: postgres: the server did not start\nE   connection refused\n",
        in_place_line(LOGGED, 0)
    );
    assert_eq!(step.needs(), [("postgres", "the server did not start")]);
    let s = report(vec![step]).summary();
    assert_eq!(
        s.details,
        [
            "`python3 -m pytest -q`: exit 1, 2 passed, 4 failed; it ran without postgres (the server did not start); it ran in place of `python3 -m pytest tests/test_textstats.py -q` of the lifecycle log, which names a test file, a test or a filter."
        ]
    );
}

// ---- commands left out with nothing in their place ------------------------------------------------------------------------------------

#[test]
fn b844_commands_left_out_with_nothing_in_their_place_get_a_line_and_leave_the_claim_as_it_is() {
    let r = report(vec![with_head(
        logged("go test -v ./...", 0, Some((4, 0))),
        &left_out_line("npx jest src/a.test.ts", 0),
    )]);
    assert_eq!(r.steps[0].left_out(), Some(("npx jest src/a.test.ts", 0)));
    let s = r.summary();
    assert_eq!(
        s.claim().as_deref(),
        Some("repository tests pass in a clean checkout: 4 of 4")
    );
    assert_eq!(
        s.sentence(),
        "repository tests pass in a clean checkout: 4 of 4. The report left out 1 command of the lifecycle log that names part of a suite; details are in EVIDENCE.md."
    );
    assert_eq!(
        s.details,
        [
            "`go test -v ./...`: exit 0, 4 passed, 0 failed.",
            "`npx jest src/a.test.ts` of the lifecycle log was left out: it names a test file, a test or a filter, and the report found no command for the whole suite.",
        ]
    );
    let more = report(vec![with_head(
        logged("go test -v ./...", 0, Some((4, 0))),
        &left_out_line("npx jest src/a.test.ts", 2),
    )])
    .summary();
    assert_eq!(
        more.details[1],
        "`npx jest src/a.test.ts` and 2 more of the lifecycle log were left out: they name test files, tests or filters, and the report found no command for the whole suite."
    );
}

#[test]
fn b844_the_left_out_line_rides_on_a_setup_step_and_behind_the_other_line() {
    // The first step of the report is an install: it has no detail line of its own, and the line about the plan still shows.
    let s = report(vec![
        with_head(
            setup("npm ci", 0),
            &left_out_line("go test -run TestA ./...", 0),
        ),
        logged("npm test", 0, Some((3, 0))),
    ])
    .summary();
    assert_eq!(
        s.details,
        [
            "`go test -run TestA ./...` of the lifecycle log was left out: it names a test file, a test or a filter, and the report found no command for the whole suite.",
            "`npm test`: exit 0, 3 passed, 0 failed.",
        ]
    );
    // One step may carry both lines: the stand-in first.
    let mut both = detected("python3 -m pytest -q", 0, Some((6, 0)));
    both.output_tail = format!(
        "{}\n{}\n......\n",
        in_place_line(LOGGED, 0),
        left_out_line("cd web && npx jest src/a.test.ts", 0)
    );
    assert_eq!(both.in_place_of(), Some((LOGGED, 0)));
    assert_eq!(
        both.left_out(),
        Some(("cd web && npx jest src/a.test.ts", 0))
    );
    // A service step is the report's own helper: nothing about the plan is read from it.
    let service = with_head(
        setup("ham-services postgres", 0),
        &left_out_line("pytest -k x", 0),
    );
    assert_eq!(service.left_out(), None);
}

// ---- the bounds ------------------------------------------------------------------------------------------------------------------------

#[test]
fn b844_the_lines_stay_inside_the_bounds_of_the_details() {
    // A long command on the step and in the line: the detail line is cut to its limit.
    let long = format!("python3 -m pytest {}", "tests/very_long_name ".repeat(40));
    let s = report(vec![with_head(
        detected(long.trim(), 0, Some((6, 0))),
        &in_place_line(&format!("pytest {}", "a".repeat(400)), 0),
    )])
    .summary();
    assert!(s.details[0].chars().count() <= MAX_SUMMARY_DETAIL_CHARS);
    // More steps than the details hold: the line about the plan sits with the first step, so it is among the lines kept.
    let mut steps = vec![with_head(
        logged("go test -v ./...", 0, Some((4, 0))),
        &left_out_line("npx jest src/a.test.ts", 0),
    )];
    steps.extend((0..20).map(|i| logged(&format!("go test ./pkg{i}/..."), 0, Some((1, 0)))));
    let many = report(steps).summary();
    assert_eq!(many.details.len(), MAX_SUMMARY_DETAILS);
    assert!(
        many.details[1].starts_with("`npx jest src/a.test.ts` of the lifecycle log was left out"),
        "{:?}",
        many.details
    );
}
