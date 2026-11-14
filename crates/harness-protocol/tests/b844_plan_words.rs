//! What the summary says of test commands of the lifecycle log that the plan left out, beyond the two lines of
//! `b844_report_notes.rs`.
//!
//! - a line about commands left out may carry the reason, and the detail line says it in words;
//! - the sentence of the card says that the report chose a command in place of logged commands or left some out;
//! - the detail line of a step that stands in place of logged commands and did not run does not say that it ran, and keeps its end;
//! - a failed install is named with the commands it stopped, and a second failed install with its own;
//! - the note of a Cargo workspace opens with why the report chose the command;
//! - a line of the forms agentd writes that a cut of the output brings to the head of the output is not read.
//!
//! No member of the report or the summary is new: the facts ride at the head of a step's output, where agentd alone writes, and the sentence
//! reads them from the detail lines of the summary.
#![allow(clippy::unwrap_used)]

use harness_protocol::direct::{
    MAX_REPORT_TAIL_BYTES, MAX_SUMMARY_DETAIL_CHARS, MAX_SUMMARY_DETAILS, TestCommandSource,
    TestCounts, TestReport, TestReportStatus, TestStep, TestStepRole, in_place_line, left_out_line,
    plan_note_line,
};

const RUN: &str = "01M4XTESTRUNDIRECT00000001";
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
/// What a detail line says of one command that names a part of a suite.
const ONE_PART: &str = "names a test file, a test or a filter";

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

fn logged(command: &str, exit: i32, counts: Option<(u32, u32)>) -> TestStep {
    ran(
        TestStepRole::Test,
        TestCommandSource::LifecycleLog,
        command,
        exit,
        counts,
    )
}

fn with_head(mut step: TestStep, head: &str) -> TestStep {
    step.output_tail = format!("{head}\n{}", step.output_tail);
    step
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

/// The detail lines of a report whose one step, `go test -v ./...` with 4 passed, carries `head` at the head of its output.
fn details_under(head: &str) -> Vec<String> {
    report(vec![with_head(
        logged("go test -v ./...", 0, Some((4, 0))),
        head,
    )])
    .summary()
    .details
}

const GO_LINE: &str = "`go test -v ./...`: exit 0, 4 passed, 0 failed.";

// ---- the reason of a line about commands left out ---------------------------------------------------------------------------------------

#[test]
fn b844_a_left_out_line_says_which_file_names_packages_the_report_does_not_install() {
    assert_eq!(
        details_under(
            "PLAN NOTE: left out `python3 -m pytest tests/test_api.py -q`; packages in pyproject.toml"
        ),
        [
            GO_LINE.to_string(),
            format!(
                "`python3 -m pytest tests/test_api.py -q` of the lifecycle log was left out: it {ONE_PART}, and pyproject.toml names packages that the report does not install."
            ),
        ]
    );
    // More commands than one, and a file in a folder.
    assert_eq!(
        details_under(
            "PLAN NOTE: left out `pytest -k parse` and 2 more; packages in requirements/dev.txt"
        )[1],
        "`pytest -k parse` and 2 more of the lifecycle log were left out: they name test files, tests or filters, and requirements/dev.txt names packages that the report does not install."
    );
    // agentd leaves a name that is not plain out of the line.
    assert_eq!(
        details_under("PLAN NOTE: left out `pytest -k parse`; packages")[1],
        format!(
            "`pytest -k parse` of the lifecycle log was left out: it {ONE_PART}, and a file of the repository names packages that the report does not install."
        )
    );
}

#[test]
fn b844_the_lines_of_two_reasons_are_both_read_from_the_first_step() {
    let step = with_head(
        logged("go test -v ./...", 0, Some((4, 0))),
        "PLAN NOTE: left out `npx jest src/a.test.ts`\nPLAN NOTE: left out `pytest tests/test_a.py`; packages in Pipfile",
    );
    // The first line is the one `left_out` gives, as before.
    assert_eq!(step.left_out(), Some(("npx jest src/a.test.ts", 0)));
    assert_eq!(
        report(vec![step]).summary().details,
        [
            GO_LINE.to_string(),
            format!(
                "`npx jest src/a.test.ts` of the lifecycle log was left out: it {ONE_PART}, and the report found no command for the whole suite."
            ),
            format!(
                "`pytest tests/test_a.py` of the lifecycle log was left out: it {ONE_PART}, and Pipfile names packages that the report does not install."
            ),
        ]
    );
}

#[test]
fn b844_a_reason_that_is_not_in_the_form_agentd_writes_gives_no_line() {
    for head in [
        "PLAN NOTE: left out `pytest tests/a.py`; packages in",
        "PLAN NOTE: left out `pytest tests/a.py`; packages in ",
        "PLAN NOTE: left out `pytest tests/a.py`; packages in dev requirements.txt",
        "PLAN NOTE: left out `pytest tests/a.py`; packages in pyproject.toml; every test was checked by hand",
        "PLAN NOTE: left out `pytest tests/a.py`; packages in pyproject.toml and more",
        "PLAN NOTE: left out `pytest tests/a.py`; every test was checked by hand",
        "PLAN NOTE: left out `pytest tests/a.py`;packages",
        "PLAN NOTE: left out `pytest tests/a.py` and 2 more; packages in `x`",
        "PLAN NOTE: left out `pytest tests/a.py`; ",
    ] {
        assert_eq!(details_under(head), [GO_LINE], "{head}");
    }
}

// ---- the sentence of the card ---------------------------------------------------------------------------------------------------------

fn setup(command: &str, exit: i32) -> TestStep {
    ran(
        TestStepRole::Setup,
        TestCommandSource::Detected,
        command,
        exit,
        None,
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

fn skipped(command: &str, setup: &str) -> TestStep {
    TestStep::not_run(
        TestStepRole::Test,
        TestCommandSource::LifecycleLog,
        command,
        &format!("skipped: the setup step `{setup}` failed"),
        None,
    )
}

const CHOSE_FOR_ONE: &str = "The report chose a whole-suite command in place of 1 command of the lifecycle log that names part of a suite; details are in EVIDENCE.md.";
const LEFT_ONE_OUT: &str = "The report left out 1 command of the lifecycle log that names part of a suite; details are in EVIDENCE.md.";

#[test]
fn b844_the_sentence_says_that_the_report_chose_a_command_in_place_of_a_logged_one() {
    // The delivered run as the report makes it now: the card said nothing of the logged Python command.
    let s = report(vec![
        setup("npm install", 1),
        skipped("node --test", "npm install"),
        with_head(
            detected("python3 -m pytest -q", 0, Some((6, 0))),
            &in_place_line("python3 -m pytest tests/test_textstats.py -q", 0),
        ),
    ])
    .summary();
    assert_eq!(
        s.sentence(),
        format!(
            "Setup `npm install` exited 1 in a clean checkout, so `node --test` did not run; for the test commands that ran, the runner reported 6 passed, 0 failed. {CHOSE_FOR_ONE}"
        )
    );
}

#[test]
fn b844_the_sentence_says_that_logged_commands_were_left_out_and_the_claim_keeps_its_words() {
    let s = report(vec![with_head(
        logged("go test -v ./...", 0, Some((4, 0))),
        &left_out_line("npx jest src/a.test.ts", 0),
    )])
    .summary();
    // The claim is the frozen wording, about the commands that ran; what the plan left out follows it as a sentence of its own.
    assert_eq!(
        s.claim().as_deref(),
        Some("repository tests pass in a clean checkout: 4 of 4")
    );
    assert_eq!(
        s.sentence(),
        format!("repository tests pass in a clean checkout: 4 of 4. {LEFT_ONE_OUT}")
    );
}

#[test]
fn b844_the_sentence_counts_the_commands_of_every_line() {
    // One step in place of three commands; two lines of commands left out, one and three.
    let both = report(vec![with_head(
        detected("python3 -m pytest -q", 0, Some((6, 0))),
        &format!(
            "{}\n{}\nPLAN NOTE: left out `pytest -k parse` and 2 more; packages in Pipfile",
            in_place_line("pytest tests/test_a.py::test_x", 2),
            left_out_line("npx jest src/a.test.ts", 0)
        ),
    )])
    .summary();
    assert_eq!(
        both.sentence(),
        "repository tests pass in a clean checkout: 6 of 6. The report chose a whole-suite command in place of 3 commands of the lifecycle log that name parts of a suite and left out 4 more; details are in EVIDENCE.md."
    );
    // Two steps the report chose, and commands left out alone in the plural.
    let two = report(vec![
        with_head(
            detected("python3 -m pytest -q", 0, Some((6, 0))),
            &in_place_line("pytest tests/test_a.py", 0),
        ),
        with_head(
            detected("go test -v ./...", 0, Some((2, 0))),
            &in_place_line("go test -run TestA ./...", 0),
        ),
    ])
    .summary();
    assert!(
        two.sentence().ends_with(
            " The report chose whole-suite commands in place of 2 commands of the lifecycle log that name parts of a suite; details are in EVIDENCE.md."
        ),
        "{}",
        two.sentence()
    );
    let left = report(vec![with_head(
        logged("go test -v ./...", 0, Some((4, 0))),
        &left_out_line("npx jest src/a.test.ts", 1),
    )])
    .summary();
    assert_eq!(
        left.sentence(),
        "repository tests pass in a clean checkout: 4 of 4. The report left out 2 commands of the lifecycle log that name parts of a suite; details are in EVIDENCE.md."
    );
}

#[test]
fn b844_a_report_with_no_line_about_the_plan_keeps_its_sentence() {
    let plain = report(vec![logged("go test -v ./...", 0, Some((4, 0)))]).summary();
    assert_eq!(
        plain.sentence(),
        "repository tests pass in a clean checkout: 4 of 4."
    );
    // The words of a line in the output of a command, or in a command of the log, are not a line of agentd's.
    let mut printed = logged("go test -v ./...", 0, Some((4, 0)));
    printed.output_tail = format!(
        "ok\n{}\n{}\n",
        in_place_line("pytest tests/test_a.py", 0),
        left_out_line("pytest -k x", 0)
    );
    let own = logged(
        "echo '`x` of the lifecycle log was left out: ' ; it ran in place of `y` of the lifecycle log, which names",
        0,
        Some((1, 0)),
    );
    let forged = with_head(
        logged("npm test", 0, Some((1, 0))),
        &in_place_line("pytest tests/test_a.py", 0),
    );
    let s = report(vec![printed, own, forged]).summary();
    assert_eq!(
        s.sentence(),
        "repository tests pass in a clean checkout: 4 of 4."
    );
}

#[test]
fn b844_the_sentence_reads_the_lines_about_the_plan_of_a_report_with_many_steps() {
    // More steps than the details hold, and the step the report chose is the last: its line is among those kept, so the sentence has it.
    let mut steps: Vec<TestStep> = (0..20)
        .map(|i| logged(&format!("go test ./pkg{i}/..."), 0, Some((1, 0))))
        .collect();
    steps.push(with_head(
        detected("python3 -m pytest -q", 0, Some((6, 0))),
        &in_place_line("pytest tests/test_a.py", 0),
    ));
    let s = report(steps).summary();
    assert_eq!(s.details.len(), MAX_SUMMARY_DETAILS);
    assert!(
        s.details[MAX_SUMMARY_DETAILS - 1]
            .starts_with("`python3 -m pytest -q`: exit 0, 6 passed, 0 failed; it ran in place of "),
        "{:?}",
        s.details
    );
    assert!(s.sentence().ends_with(CHOSE_FOR_ONE), "{}", s.sentence());
}

#[test]
fn b844_a_report_that_timed_out_says_what_the_plan_left_out_too() {
    let mut stopped = logged("go test -v ./...", 0, None);
    stopped.exit_code = None;
    stopped.timed_out = true;
    let mut r = report(vec![with_head(
        stopped,
        &left_out_line("npx jest src/a.test.ts", 0),
    )]);
    r.status = TestReportStatus::TimedOut;
    assert_eq!(
        r.summary().sentence(),
        format!(
            "`go test -v ./...` timed out after 300 s; the commands that finished are listed in the report. {LEFT_ONE_OUT}"
        )
    );
}

// ---- commands whose file the chosen command does not run -------------------------------------------------------------------------------

#[test]
fn b844_a_left_out_line_says_that_no_command_of_the_report_runs_the_named_file() {
    assert_eq!(
        details_under("PLAN NOTE: left out `python3 -m unittest lib/test_new.py`; not reached")[1],
        format!(
            "`python3 -m unittest lib/test_new.py` of the lifecycle log was left out: it {ONE_PART}, and the report found no command for the whole suite that runs the file it names."
        )
    );
    assert_eq!(
        details_under("PLAN NOTE: left out `pytest extra/test_b.py` and 1 more; not reached")[1],
        "`pytest extra/test_b.py` and 1 more of the lifecycle log were left out: they name test files, tests or filters, and the report found no command for the whole suite that runs the files they name."
    );
    // Words that only start like the reason are not agentd's.
    assert_eq!(
        details_under("PLAN NOTE: left out `pytest extra/test_b.py`; not reached by the suite"),
        [GO_LINE]
    );
}

// ---- commands that work in more than one project folder ---------------------------------------------------------------------------------

#[test]
fn b844_a_left_out_line_says_that_the_log_runs_the_tests_in_more_than_one_project_folder() {
    assert_eq!(
        details_under(
            "PLAN NOTE: left out `cd worker && python3 -m pytest tests/test_models.py -q` and 1 more; more than one project"
        )[1],
        "`cd worker && python3 -m pytest tests/test_models.py -q` and 1 more of the lifecycle log were left out: they name test files, tests or filters, and the log runs these tests in more than one project folder, so the report added no command at the root."
    );
    assert_eq!(
        details_under(
            "PLAN NOTE: left out `pytest api/tests/test_a.py`; more than one project folder"
        ),
        [GO_LINE]
    );
}

// ---- what the log recorded for a command that was left out ------------------------------------------------------------------------------

#[test]
fn b844_a_left_out_line_says_what_the_log_recorded_for_a_run_that_did_not_pass() {
    let no_suite = format!(
        "`npx jest src/a.test.ts` of the lifecycle log was left out: it {ONE_PART}, and the report found no command for the whole suite"
    );
    assert_eq!(
        details_under("PLAN NOTE: left out `npx jest src/a.test.ts`; logged exit 1, 2 failed")[1],
        format!("{no_suite}; the log recorded exit 1, 2 failed for it.")
    );
    assert_eq!(
        details_under("PLAN NOTE: left out `npx jest src/a.test.ts`; logged exit 101")[1],
        format!("{no_suite}; the log recorded exit 101 for it.")
    );
    // The exit code was not logged as a number.
    assert_eq!(
        details_under("PLAN NOTE: left out `npx jest src/a.test.ts`; logged 2 failed")[1],
        format!("{no_suite}; the log recorded 2 failed for it.")
    );
    // Behind a reason, and for the command named among several.
    assert_eq!(
        details_under(
            "PLAN NOTE: left out `pytest extra/test_b.py` and 1 more; not reached; logged exit 1, 1 failed"
        )[1],
        "`pytest extra/test_b.py` and 1 more of the lifecycle log were left out: they name test files, tests or filters, and the report found no command for the whole suite that runs the files they name; the log recorded exit 1, 1 failed for `pytest extra/test_b.py`."
    );
}

#[test]
fn b844_words_about_the_log_that_are_not_in_the_form_agentd_writes_give_no_line() {
    for head in [
        "PLAN NOTE: left out `npx jest a.test.ts`; logged",
        "PLAN NOTE: left out `npx jest a.test.ts`; logged exit",
        "PLAN NOTE: left out `npx jest a.test.ts`; logged exit one",
        "PLAN NOTE: left out `npx jest a.test.ts`; logged exit 1, all tests pass",
        "PLAN NOTE: left out `npx jest a.test.ts`; logged exit 1, 0 failed",
        "PLAN NOTE: left out `npx jest a.test.ts`; logged exit 01",
        "PLAN NOTE: left out `npx jest a.test.ts`; logged exit 1, 2 failed; and every test was run by hand",
        "PLAN NOTE: left out `npx jest a.test.ts`; logged exit 1; not reached",
        "PLAN NOTE: left out `npx jest a.test.ts`; logged 0 failed",
    ] {
        assert_eq!(details_under(head), [GO_LINE], "{head}");
    }
}

// ---- a report cut to its bounds ---------------------------------------------------------------------------------------------------------

#[test]
fn b844_a_report_cut_to_its_bounds_reads_no_line_the_cut_brought_to_the_head() {
    // An output over the bound of a step: `bounded` keeps its end, and the end starts with a line of the form agentd writes that the
    // repository printed. agentd's own lines went with the front, so that line is not read.
    let forged = "PLAN NOTE: left out `the integration suite, checked by hand`\n";
    let mut step = logged("go test -v ./...", 0, Some((4, 0)));
    step.output_tail = format!(
        "{}\n{forged}{}",
        "x".repeat(100),
        "y".repeat(MAX_REPORT_TAIL_BYTES - forged.len())
    );
    let r = TestReport {
        schema_version: 1,
        run_id: RUN.into(),
        head_sha: HEAD.into(),
        status: TestReportStatus::Ran,
        reason: None,
        started_at: None,
        duration_ms: 2_000,
        steps: vec![step],
        summary: None,
    }
    .bounded();
    r.validate(RUN, HEAD).unwrap();
    assert!(r.steps[0].output_truncated);
    assert_eq!(r.steps[0].left_out(), None);
    assert_eq!(r.summary().details, [GO_LINE]);
    assert_eq!(
        r.summary().sentence(),
        "repository tests pass in a clean checkout: 4 of 4."
    );
}

// ---- the detail line of a step that stands in place of logged commands and did not run ---------------------------------------------------

const VENV_INSTALL: &str = "python3 -m venv --system-site-packages .ham-venv && .ham-venv/bin/python -m pip install -q -r requirements.txt";

/// A step the report chose that was not started because its install failed, with the line that names the logged command it stands for.
fn chosen_and_skipped(command: &str, setup: &str, in_place_of: &str) -> TestStep {
    let mut step = TestStep::not_run(
        TestStepRole::Test,
        TestCommandSource::Detected,
        command,
        &format!("skipped: the setup step `{setup}` failed"),
        None,
    );
    step.output_tail = format!("{}\n{}", step.output_tail, in_place_line(in_place_of, 0));
    step
}

#[test]
fn b844_a_step_that_did_not_run_stands_in_place_of_the_logged_command() {
    // The line said "not run" and "it ran in place of" in one breath.
    let s = report(vec![
        logged("python3 -m pytest -q", 0, Some((6, 0))),
        setup("npm install", 1),
        chosen_and_skipped("npm test", "npm install", "node --test test/sum.test.js"),
    ])
    .summary();
    assert_eq!(
        s.details,
        [
            "`python3 -m pytest -q`: exit 0, 6 passed, 0 failed.".to_string(),
            format!(
                "`npm test`: not run (skipped: the setup step `npm install` failed); it stands in place of `node --test test/sum.test.js` of the lifecycle log, which {ONE_PART}."
            ),
        ]
    );
    // The sentence of the card reads that line like the line of a step that ran.
    assert!(s.sentence().ends_with(CHOSE_FOR_ONE), "{}", s.sentence());
    // A step that was stopped at its limit did not run to its end either.
    let mut stopped = with_head(
        detected("python3 -m pytest -q", 0, None),
        &in_place_line("pytest tests/test_a.py", 0),
    );
    stopped.exit_code = None;
    stopped.timed_out = true;
    let mut r = report(vec![stopped]);
    r.status = TestReportStatus::TimedOut;
    assert_eq!(
        r.summary().details,
        [format!(
            "`python3 -m pytest -q`: timed out after 300 s; it stands in place of `pytest tests/test_a.py` of the lifecycle log, which {ONE_PART}."
        )]
    );
}

#[test]
fn b844_a_long_reason_gives_way_to_the_words_about_the_logged_command() {
    // The reason names the install of the report's environment, 110 characters long: the line reached its limit and ended in the middle
    // of the words about the logged command.
    let s = report(vec![
        logged("node --test", 0, Some((3, 0))),
        setup(VENV_INSTALL, 9),
        chosen_and_skipped(
            ".ham-venv/bin/python -m pytest -q",
            VENV_INSTALL,
            "python3 -m pytest tests/test_textstats.py -q",
        ),
    ])
    .summary();
    let line = &s.details[1];
    assert!(line.chars().count() <= MAX_SUMMARY_DETAIL_CHARS, "{line}");
    assert!(
        line.starts_with(
            "`.ham-venv/bin/python -m pytest -q`: not run (skipped: the setup step `python3 -m venv --system-site-packages .ham-venv && .ham-venv/bin/python -m pip"
        ),
        "{line}"
    );
    assert!(
        line.ends_with(&format!(
            "); it stands in place of `python3 -m pytest tests/test_textstats.py -q` of the lifecycle log, which {ONE_PART}."
        )),
        "{line}"
    );
    assert!(s.sentence().ends_with(CHOSE_FOR_ONE), "{}", s.sentence());
    // A command of the step itself that fills the line gives way too.
    let long = format!("python3 -m pytest {}", "tests/very_long_name ".repeat(40));
    let s = report(vec![with_head(
        detected(long.trim(), 0, Some((6, 0))),
        &in_place_line("pytest tests/test_a.py", 0),
    )])
    .summary();
    assert!(s.details[0].chars().count() <= MAX_SUMMARY_DETAIL_CHARS);
    assert!(
        s.details[0].ends_with(&format!(
            "; it ran in place of `pytest tests/test_a.py` of the lifecycle log, which {ONE_PART}."
        )),
        "{}",
        s.details[0]
    );
    assert!(s.sentence().ends_with(CHOSE_FOR_ONE), "{}", s.sentence());
}

// ---- a step the report added that ran out of time ---------------------------------------------------------------------------------------

/// A step the report chose that agentd lists as not run for `reason`, with the line that names the logged command it stands for.
fn chosen_not_run(role: TestStepRole, command: &str, reason: &str) -> TestStep {
    let mut step = TestStep::not_run(role, TestCommandSource::Detected, command, reason, None);
    if role == TestStepRole::Test {
        step.output_tail = format!(
            "{}\n{}",
            step.output_tail,
            in_place_line("python3 -m pytest tests/test_a.py -q", 0)
        );
    }
    step
}

#[test]
fn b844_the_sentence_of_a_chosen_step_that_was_stopped_starts_with_the_commands_that_ran() {
    let go = || logged("go test -v ./...", 0, Some((2, 0)));
    let ran = "The test commands that ran exited 0 in a clean checkout; the runner reported 2 passed, 0 failed.";
    let stopped = report(vec![
        go(),
        chosen_not_run(
            TestStepRole::Test,
            "python3 -m pytest -q",
            "stopped at its time limit of 2 s",
        ),
    ])
    .summary();
    assert_eq!(stopped.claim(), None);
    assert_eq!(stopped.timed_out, None);
    assert_eq!(
        stopped.sentence(),
        format!(
            "{ran} `python3 -m pytest -q` was stopped at its time limit of 2 s. {CHOSE_FOR_ONE}"
        )
    );
    // The report's time ended before the step could start.
    let never = report(vec![
        go(),
        chosen_not_run(
            TestStepRole::Test,
            "python3 -m pytest -q",
            "the report's time ended before it started",
        ),
    ])
    .summary();
    assert_eq!(
        never.sentence(),
        format!(
            "{ran} `python3 -m pytest -q` did not run: the report's time ended before it started. {CHOSE_FOR_ONE}"
        )
    );
    // The install of the step was stopped: the step itself is counted behind it.
    let install = report(vec![
        go(),
        chosen_not_run(
            TestStepRole::Setup,
            VENV_INSTALL,
            "stopped at its time limit of 300 s",
        ),
        chosen_not_run(
            TestStepRole::Test,
            ".ham-venv/bin/python -m pytest -q",
            &format!("skipped: the setup step `{VENV_INSTALL}` failed"),
        ),
    ])
    .summary();
    assert_eq!(
        install.sentence(),
        format!(
            "{ran} `{VENV_INSTALL}` was stopped at its time limit of 300 s, and 1 more command did not run. {CHOSE_FOR_ONE}"
        )
    );
}

// ---- two installs that failed -----------------------------------------------------------------------------------------------------------

#[test]
fn b844_a_second_install_that_failed_is_named_with_the_command_it_stopped() {
    // The delivered log in a tree with a requirements.txt: `npm install` fails and the report's environment cannot be made. Each install
    // stopped one command; the sentence gave both to the first.
    let venv_python = ".ham-venv/bin/python -m pytest -q";
    let s = report(vec![
        setup("npm install", 1),
        skipped("node --test", "npm install"),
        setup(VENV_INSTALL, 9),
        chosen_and_skipped(
            venv_python,
            VENV_INSTALL,
            "python3 -m pytest tests/test_textstats.py -q",
        ),
    ])
    .summary();
    assert_eq!(
        s.sentence(),
        format!(
            "Setup `npm install` exited 1 in a clean checkout, so `node --test` did not run; setup `{VENV_INSTALL}` failed too, so `{venv_python}` did not run. {CHOSE_FOR_ONE}"
        )
    );
    // Beside commands that ran, with two commands for the first install.
    let s = report(vec![
        logged("go test -v ./...", 0, Some((2, 0))),
        setup("npm ci", 1),
        skipped("npm test", "npm ci"),
        skipped("npm run test:e2e", "npm ci"),
        setup(VENV_INSTALL, 9),
        skipped(venv_python, VENV_INSTALL),
    ])
    .summary();
    assert_eq!(
        s.sentence(),
        format!(
            "Setup `npm ci` exited 1 in a clean checkout, so `npm test` and 1 more did not run; setup `{VENV_INSTALL}` failed too, so `{venv_python}` did not run; for the test commands that ran, the runner reported 2 passed, 0 failed."
        )
    );
}

#[test]
fn b844_a_command_is_counted_with_the_install_that_stopped_it() {
    // The report's environment could not be made, and the logged Python command ran as the log wrote it. `npm install` failed later and
    // stopped `npm test`: the first install did not.
    let s = report(vec![
        setup(VENV_INSTALL, 9),
        logged("python3 -m pytest -q", 0, Some((6, 0))),
        setup("npm install", 1),
        skipped("npm test", "npm install"),
    ])
    .summary();
    assert_eq!(
        s.sentence(),
        format!(
            "Setup `{VENV_INSTALL}` exited 9 in a clean checkout, so the test commands ran without it; setup `npm install` failed too, so `npm test` did not run; the runner reported 6 passed, 0 failed."
        )
    );
    // A command that was not run for another reason is no command the install stopped.
    let s = report(vec![
        setup("npm ci", 1),
        skipped("npm test", "npm ci"),
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
        s.sentence(),
        "Setup `npm ci` exited 1 in a clean checkout, so `npm test` did not run."
    );
    assert_eq!(
        s.details.len(),
        2,
        "each command keeps its line: {:?}",
        s.details
    );
}

// ---- the note of a Cargo workspace --------------------------------------------------------------------------------------------------------

#[test]
fn b844_the_note_of_a_cargo_workspace_says_why_the_report_chose_the_command() {
    let rest = "so the harness chose `cargo test` from the manifest (Cargo.toml). It runs the tests of the whole workspace, 2 members. A `.ham/test-command` file in the repository names a narrower command.";
    // The step stands in place of a logged command beside a command of the model's own: the repository's log does name test commands.
    let mut chosen = detected("cargo test", 0, Some((40, 0)));
    chosen.output_tail = format!(
        "{}\n{}\ntest result: ok. 40 passed\n",
        plan_note_line(2),
        in_place_line("cargo test --test smoke", 0)
    );
    let s = report(vec![logged("node --test", 0, Some((3, 0))), chosen]).summary();
    assert_eq!(
        s.plan_note(),
        Some(format!("The lifecycle log names only a part of the Rust tests, {rest}").as_str())
    );
    // A command the layout alone gave keeps the words of before.
    let mut alone = detected("cargo test", 0, Some((40, 0)));
    alone.output_tail = format!("{}\ntest result: ok. 40 passed\n", plan_note_line(2));
    assert_eq!(
        report(vec![alone]).summary().plan_note(),
        Some(format!("The repository names no test command, {rest}").as_str())
    );
}

// ---- lines and reasons with text that could mislead the readers -----------------------------------------------------------------------

#[test]
fn b844_a_count_no_log_could_hold_is_not_a_line_of_agentds() {
    // The number of the other commands is bounded: a line with a count beyond it is not in agentd's form, and no count overflows.
    for head in [
        "PLAN NOTE: left out `pytest tests/a.py` and 18446744073709551615 more",
        "PLAN NOTE: left out `pytest tests/a.py` and 1000001 more",
    ] {
        let s = report(vec![with_head(
            logged("go test -v ./...", 0, Some((4, 0))),
            head,
        )])
        .summary();
        assert_eq!(s.details, [GO_LINE], "{head}");
        assert_eq!(
            s.sentence(),
            "repository tests pass in a clean checkout: 4 of 4.",
            "{head}"
        );
    }
    let step = with_head(
        detected("python3 -m pytest -q", 0, Some((6, 0))),
        "PLAN NOTE: in place of `pytest tests/a.py` and 18446744073709551615 more",
    );
    assert_eq!(step.in_place_of(), None);
    // The largest count that is accepted is read and counted.
    let s = report(vec![with_head(
        logged("go test -v ./...", 0, Some((4, 0))),
        "PLAN NOTE: left out `pytest tests/a.py` and 1000000 more",
    )])
    .summary();
    assert!(
        s.sentence().ends_with(
            " The report left out 1000001 commands of the lifecycle log that name parts of a suite; details are in EVIDENCE.md."
        ),
        "{}",
        s.sentence()
    );
}

#[test]
fn b844_a_setup_command_with_the_words_of_a_count_still_names_the_command_it_stopped() {
    // The setup command holds the words that close a count in the summary's line.
    let odd = "false # ), and junk";
    let s = report(vec![setup(odd, 1), skipped("pytest", odd)]).summary();
    assert_eq!(
        s.sentence(),
        "Setup `false # ), and junk` exited 1 in a clean checkout, so `pytest` did not run."
    );
}
