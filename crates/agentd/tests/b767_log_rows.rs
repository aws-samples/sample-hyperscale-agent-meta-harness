//! The `logged` row of a report step is the last run of the command in the model's lifecycle log, in whatever spelling the model typed it.
//!
//! Two delivered runs of a release gate showed `exit 127` in the `logged` member of test steps that exited 0 in the clean checkout. The rows are real:
//! the model ran `bash scripts/test-unit.sh` before it had made the script (`bash: scripts/test-unit.sh: No such file or directory`, exit 127), and
//! `NODE_ENV=production NPM_CONFIG_OMIT=dev bash scripts/test-ui-unit.sh` while the script still lacked `--include=dev` (`vitest: command not found`).
//! The report picked those rows because (1) a later green run typed as `timeout 240 bash scripts/test-unit.sh` was not read as a test command, and
//! (2) a script the model ran in two spellings took the spelling whose last run came first. The rows below are the relevant lines of those logs.
//! Nothing here starts a process except the one contract test at the end, which runs two stand-in scripts.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::report::command::strip_timeout;
use agentd::direct::report::plan::{Plan, PlannedStep, is_focused, is_test_command, plan};
use agentd::direct::report::run::{RunEnv, run_step_scanned};
use harness_protocol::direct::{
    LoggedRun, TestCommandSource, TestCounts, TestReport, TestReportStatus, TestStepRole,
};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn tree(label: &str, files: &[&str], log: &str, goal: Option<&str>) -> PathBuf {
    let root = common::scratch(label).join("tree");
    for f in files {
        write(&root.join(f), "#!/bin/sh\nexit 0\n");
    }
    write(
        &root.join(".ham/lifecycle/LOG.md"),
        &format!("# Observed runs\n{log}\n"),
    );
    if let Some(goal) = goal {
        write(
            &root.join(".ham/lifecycle/GOAL.md"),
            &format!("# Goal\n{goal}\n"),
        );
    }
    root
}

fn tests_of(dir: &Path) -> Vec<PlannedStep> {
    match plan(dir) {
        Plan::Steps(s) => s
            .into_iter()
            .filter(|s| s.role == TestStepRole::Test)
            .collect(),
        Plan::NoCommand(why) => panic!("no steps: {why}"),
    }
}

fn logged(exit: i32, passed: Option<u32>, failed: Option<u32>) -> Option<LoggedRun> {
    Some(LoggedRun {
        exit_code: Some(exit),
        passed,
        failed,
    })
}

/// The lines of the log of events run A (Go, gotestsum) that matter here, in file order.
const EVENTS_LOG: &str = "RUN | go version | exit 0 | pass unknown fail unknown | go version go1.27.1 linux/arm64
RUN | go test ./tests/contracts | exit 1 | pass unknown fail unknown | \tgo mod init
RUN | bash scripts/test-unit.sh | exit 127 | pass unknown fail unknown | bash: scripts/test-unit.sh: No such file or directory
RUN | bash scripts/test-integration.sh | exit 127 | pass unknown fail unknown | bash: scripts/test-integration.sh: No such file or directory
RUN | bash scripts/test-e2e.sh | exit 127 | pass unknown fail unknown | bash: scripts/test-e2e.sh: No such file or directory
RUN | bash scripts/check.sh | exit 127 | pass unknown fail unknown | bash: scripts/check.sh: No such file or directory
RUN | timeout 120 go test -json ./internal/api -count=1 | exit 1 | pass unknown fail unknown | FAIL\texample.com/order-events/internal/api [build failed]
RUN | timeout 120 go test -json ./internal/api -count=1 | exit 0 | pass 19 fail 0 | ok  \texample.com/order-events/internal/api\t0.007s
RUN | timeout 240 bash scripts/test-unit.sh | exit 0 | pass unknown fail unknown | DONE 63 tests in 0.390s
RUN | timeout 180 bash scripts/test-integration.sh | exit 0 | pass unknown fail unknown | DONE 4 tests in 0.053s
RUN | timeout 180 bash scripts/test-e2e.sh | exit 0 | pass unknown fail unknown | DONE 1 tests in 0.053s
RUN | timeout 600 bash scripts/check.sh | exit 0 | pass unknown fail unknown | CHECK PASS
RUN | timeout 240 bash scripts/test-unit.sh | exit 0 | pass unknown fail unknown | DONE 76 tests in 0.772s
RUN | timeout 180 bash scripts/test-integration.sh | exit 0 | pass unknown fail unknown | DONE 7 tests in 0.085s
RUN | timeout 180 bash scripts/test-e2e.sh | exit 0 | pass unknown fail unknown | DONE 1 tests in 0.053s
RUN | timeout 600 bash scripts/check.sh | exit 0 | pass unknown fail unknown | CHECK PASS";

/// The lines of the log of web run B that matter here, in file order (the row numbers of the delivered log are in the comments).
const WEB_LOG: &str = "RUN | bash scripts/test-ui-unit.sh | exit 127 | pass unknown fail unknown | bash: scripts/test-ui-unit.sh: No such file or directory
RUN | bash scripts/test-ui-unit.sh | exit 0 | pass 17 fail 0 | Tests  17 passed (17)
RUN | NODE_ENV=production NPM_CONFIG_OMIT=dev bash scripts/test-ui-unit.sh | exit 127 | pass unknown fail unknown | sh: line 1: vitest: command not found
RUN | bash scripts/test-ui-unit.sh (NODE_ENV=production NPM_CONFIG_OMIT=dev) | exit 1 | pass 2 fail 15 | Tests  15 failed / 2 passed (17)
RUN | bash scripts/test-ui-unit.sh (NODE_ENV=production NPM_CONFIG_OMIT=dev) | exit 0 | pass 17 fail 0 | Tests  17 passed (17)
RUN | bash scripts/test-ui-unit.sh | exit 0 | pass 17 fail 0 | Tests  17 passed (17)
RUN | bash scripts/test-ui-unit.sh (follow-up VERIFY) | exit 0 | pass 17 fail 0 | Tests  17 passed (17)";

const SCRIPTS: [&str; 3] = [
    "scripts/test-unit.sh",
    "scripts/test-integration.sh",
    "scripts/test-e2e.sh",
];

// ---- the plan: which row a step carries ---------------------------------------------------------------------------------------

#[test]
fn b767_a_script_run_again_under_a_timeout_wrapper_carries_that_last_run_and_not_the_first_attempt()
{
    let root = tree("b767-events", &SCRIPTS, EVENTS_LOG, None);
    let got = tests_of(&root);
    assert_eq!(
        got.iter().map(|s| s.command.as_str()).collect::<Vec<_>>(),
        [
            "bash scripts/test-unit.sh",
            "bash scripts/test-integration.sh",
            "bash scripts/test-e2e.sh"
        ]
    );
    for s in &got {
        assert_eq!(s.source, TestCommandSource::LifecycleLog, "{}", s.command);
        assert_eq!(
            s.logged,
            logged(0, None, None),
            "{}: the last run was green, the first attempt (exit 127) was made before the script existed",
            s.command
        );
    }
}

#[test]
fn b767_a_script_the_model_ran_with_and_without_a_prefix_follows_its_last_run() {
    let root = tree("b767-web", &["scripts/test-ui-unit.sh"], WEB_LOG, None);
    let got = tests_of(&root);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].command, "bash scripts/test-ui-unit.sh");
    assert_eq!(got[0].logged, logged(0, Some(17), Some(0)));
    assert_eq!(got[0].source, TestCommandSource::LifecycleLog);
}

#[test]
fn b767_the_last_run_of_a_script_is_carried_also_when_it_failed() {
    let log = "RUN | bash scripts/test-unit.sh | exit 0 | pass 5 fail 0 | ok
RUN | timeout 60 bash scripts/test-unit.sh | exit 1 | pass 4 fail 1 | one failed";
    let root = tree("b767-red", &["scripts/test-unit.sh"], log, None);
    let got = tests_of(&root);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].logged, logged(1, Some(4), Some(1)));
}

#[test]
fn b767_a_prefix_the_model_used_in_its_last_run_stays_and_an_earlier_prefix_goes() {
    let log = "RUN | CI=1 bash scripts/test-unit.sh | exit 0 | pass 5 fail 0 | ok
RUN | bash scripts/test-integration.sh | exit 0 | pass 2 fail 0 | ok
RUN | bash scripts/test-unit.sh | exit 0 | pass 5 fail 0 | ok
RUN | CI=1 bash scripts/test-integration.sh | exit 0 | pass 2 fail 0 | ok";
    let root = tree(
        "b767-prefix",
        &["scripts/test-unit.sh", "scripts/test-integration.sh"],
        log,
        None,
    );
    let got: Vec<String> = tests_of(&root).into_iter().map(|s| s.command).collect();
    assert_eq!(
        got,
        [
            "bash scripts/test-unit.sh",
            "CI=1 bash scripts/test-integration.sh"
        ]
    );
}

#[test]
fn b767_a_script_the_log_did_not_run_comes_from_the_goal_card_after_the_log() {
    let log = "RUN | timeout 240 bash scripts/test-unit.sh | exit 0 | pass 9 fail 0 | ok";
    let goal = "1. `bash scripts/test-unit.sh` -> exit 0; unit tests\n2. `bash scripts/test-integration.sh` -> exit 0; integration tests";
    let root = tree(
        "b767-goal",
        &["scripts/test-unit.sh", "scripts/test-integration.sh"],
        log,
        Some(goal),
    );
    let got: Vec<(String, TestCommandSource, Option<LoggedRun>)> = tests_of(&root)
        .into_iter()
        .map(|s| (s.command, s.source, s.logged))
        .collect();
    assert_eq!(
        got,
        [
            (
                "bash scripts/test-unit.sh".to_string(),
                TestCommandSource::LifecycleLog,
                logged(0, Some(9), Some(0))
            ),
            (
                "bash scripts/test-integration.sh".to_string(),
                TestCommandSource::GoalCard,
                None
            )
        ]
    );
}

#[test]
fn b767_a_free_form_command_typed_with_a_timeout_wrapper_is_planned_without_it() {
    let log = "RUN | timeout 120 python3 -m pytest -q | exit 0 | pass 3 fail 0 | x
RUN | timeout -k 5 --signal=INT 60 npm test | exit 0 | pass 4 fail 0 | x
RUN | CI=1 timeout 30 go test ./... | exit 0 | pass 5 fail 0 | x
RUN | timeout 120 go test -run TestX ./... | exit 0 | pass 1 fail 0 | x
RUN | timeout --bogus 5 cargo test | exit 0 | pass 6 fail 0 | x
RUN | timeout 600 bash -c 'eval \"$(ham-services env)\"; bash scripts/check.sh' | exit 0 | pass unknown fail unknown | x";
    let root = tree("b767-free", &[], log, None);
    let got: Vec<(String, Option<LoggedRun>)> = tests_of(&root)
        .into_iter()
        .map(|s| (s.command, s.logged))
        .collect();
    assert_eq!(
        got,
        [
            (
                "python3 -m pytest -q".to_string(),
                logged(0, Some(3), Some(0))
            ),
            ("npm test".to_string(), logged(0, Some(4), Some(0))),
            (
                "CI=1 go test ./...".to_string(),
                logged(0, Some(5), Some(0))
            )
        ],
        "a focused run and a form the reader does not know stay out"
    );
}

// ---- the wrapper ----------------------------------------------------------------------------------------------------------------

#[test]
fn b767_a_leading_timeout_with_its_options_and_duration_is_cut_and_the_rest_is_kept_as_typed() {
    for (before, after) in [
        ("timeout 120 pytest -q", "pytest -q"),
        ("timeout 2m pytest", "pytest"),
        ("timeout 1.5h pytest", "pytest"),
        ("timeout 90s pytest", "pytest"),
        ("timeout 1d make test", "make test"),
        ("timeout -k 5 60 pytest", "pytest"),
        ("timeout -k5 60 pytest", "pytest"),
        ("timeout --kill-after=5 60 pytest", "pytest"),
        ("timeout -s INT 60 pytest", "pytest"),
        ("timeout -s SIGTERM 60 pytest", "pytest"),
        ("timeout -s 9 60 pytest", "pytest"),
        ("timeout --signal=KILL 60 pytest", "pytest"),
        ("timeout -k 2.5s 60 pytest", "pytest"),
        ("/bin/timeout 60 pytest", "pytest"),
        ("timeout -sINT 60 pytest", "pytest"),
        ("timeout --signal=INT 60 pytest", "pytest"),
        (
            "timeout --foreground --preserve-status -v 60 pytest",
            "pytest",
        ),
        ("timeout --verbose 60 pytest", "pytest"),
        ("timeout -- 60 pytest", "pytest"),
        ("  timeout   60   pytest -q  ", "pytest -q"),
        ("/usr/bin/timeout 60 pytest", "pytest"),
        ("CI=1 timeout 60 pytest", "CI=1 pytest"),
        ("A=1 B=2 timeout 60 pytest -q", "A=1 B=2 pytest -q"),
        ("timeout 60 timeout 30 pytest", "pytest"),
        ("timeout 60 pytest | tail -5", "pytest | tail -5"),
        (
            "timeout 600 bash -c 'eval \"$(ham-services env)\"; bash scripts/check.sh'",
            "bash -c 'eval \"$(ham-services env)\"; bash scripts/check.sh'",
        ),
    ] {
        assert_eq!(strip_timeout(before), after, "{before:?}");
    }
}

#[test]
fn b767_a_command_that_is_not_a_plain_timeout_wrapper_is_left_as_it_is() {
    for same in [
        "pytest -q",
        "go test ./...",
        "timeouts 5 pytest",
        "timeout",
        "timeout 60",
        "timeout 60 ",
        "timeout --bogus 5 pytest",
        "timeout abc pytest",
        "timeout 5x pytest",
        "timeout -1 pytest",
        "timeout -k garbage 10 go test ./...",
        "timeout --kill-after=soon 10 go test ./...",
        "timeout --signal=NOT_A_SIGNAL 10 go test ./...",
        "timeout -s NOPE 10 go test ./...",
        "timeout -s 999 10 go test ./...",
        "timeout 10 CI=1 go test ./...",
        "timeout 5 FOO=1 pytest",
        "./timeout 5 bash scripts/test-unit.sh",
        "scripts/timeout 5 pytest",
        "timeout -k 5",
        "timeout -k",
        "FOO='a b' timeout 5 pytest",
        "FOO=\"a b\" timeout 5 pytest",
        "time timeout 60 pytest",
        "sudo timeout 60 pytest",
        "echo timeout 60 pytest",
        "",
    ] {
        assert_eq!(strip_timeout(same), same.trim(), "{same:?}");
    }
}

#[test]
fn b767_gotestsum_commands_are_test_commands_and_other_go_run_commands_are_not() {
    for yes in [
        "gotestsum --format testname ./...",
        "gotestsum -- -count=1 ./...",
        "go run gotest.tools/gotestsum@v1.13.0 --junitfile r.xml -- ./...",
        "go run gotest.tools/gotestsum -- ./...",
        "go run -mod=mod gotest.tools/gotestsum@latest -- ./...",
        "go tool gotestsum -- ./...",
        "CI=1 gotestsum -- ./...",
        "gotestsum --format pkgname -- -run TestA ./...",
        "go tool gotestsum --format testname -- ./...",
        "/home/agent/go/bin/gotestsum -- ./...",
    ] {
        assert!(is_test_command(yes), "{yes}");
    }
    for no in [
        "go run ./cmd/api",
        "go run main.go",
        "go build ./...",
        "go vet ./...",
        "go tool cover -html=c.out",
        "go run gotest.tools/gotestsum-extra@v1 ./...",
        "echo gotestsum",
        "cat gotestsum.log",
        "gotestsum --version",
        "gotestsum --help",
        "gotestsum help",
        "gotestsum tool slowest --jsonfile r.json",
        "go run gotest.tools/gotestsum@latest --version",
        "go run gotest.tools/gotestsum@latest tool ci-matrix",
        "go tool -n gotestsum",
        "go tool gotestsum --version",
    ] {
        assert!(!is_test_command(no), "{no}");
    }
    assert!(is_focused("gotestsum -- -run TestA ./..."));
    assert!(!is_focused("gotestsum -- -count=1 ./..."));
}

// ---- the contract: what a delivered events report states ---------------------------------------------------------------------

fn executable(path: &Path, text: &str) {
    write(path, text);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn env_for(work: &Path) -> RunEnv {
    RunEnv {
        sandbox: None,
        work: work.to_path_buf(),
        run_as: None,
        path: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        engine_env: BTreeMap::new(),
        engine_home: None,
        cargo_config: None,
    }
}

/// The three steps of an events report, as the report makes them: the planner reads the log of the delivered tree, the runner runs the scripts
/// (here stand-ins that print the end of the real output of the gate's scripts) and the protocol makes the summary a reader sees.
#[tokio::test]
async fn b767_an_events_report_states_go_counts_like_the_web_and_java_reports_do() {
    let outputs = [
        (
            "scripts/test-unit.sh",
            include_str!("b767_fixtures/events_a_unit.txt"),
        ),
        (
            "scripts/test-integration.sh",
            include_str!("b767_fixtures/events_a_integration.txt"),
        ),
        (
            "scripts/test-e2e.sh",
            include_str!("b767_fixtures/events_a_e2e.txt"),
        ),
    ];
    let root = tree("b767-contract", &[], EVENTS_LOG, None);
    for (script, out) in outputs {
        let data = format!("{script}.out");
        write(&root.join(&data), out);
        executable(
            &root.join(script),
            &format!(
                "#!/bin/sh\ncat \"$(dirname \"$0\")/{}\"\n",
                data.trim_start_matches("scripts/")
            ),
        );
    }
    let steps = tests_of(&root);
    assert_eq!(steps.len(), 3);
    let env = env_for(&root);
    let mut ran = Vec::new();
    for p in &steps {
        let (s, _) = run_step_scanned(&env, p, &root, Duration::from_secs(30), 4096).await;
        ran.push(s);
    }
    for s in &ran {
        assert_eq!(s.exit_code, Some(0), "{}", s.command);
        assert_eq!(s.counts_from.as_deref(), Some("gotestsum"), "{}", s.command);
        assert_eq!(
            s.logged,
            logged(0, None, None),
            "{}: the row beside the step is the last green run, not exit 127",
            s.command
        );
    }
    assert_eq!(
        ran[0].counts,
        Some(TestCounts {
            passed: 80,
            failed: 0,
            skipped: 0
        })
    );
    assert_eq!(
        ran[1].counts,
        Some(TestCounts {
            passed: 8,
            failed: 0,
            skipped: 0
        })
    );
    assert_eq!(
        ran[2].counts,
        Some(TestCounts {
            passed: 1,
            failed: 0,
            skipped: 0
        })
    );
    let report = TestReport {
        schema_version: 1,
        run_id: "R1".into(),
        head_sha: "a".repeat(40),
        status: TestReportStatus::Ran,
        reason: None,
        started_at: None,
        duration_ms: 1,
        steps: ran,
        summary: None,
    };
    let summary = report.summary();
    assert!(summary.all_exit_zero);
    assert!(
        summary.counts.is_some(),
        "counts are read for every test step"
    );
    assert!(
        summary
            .details
            .iter()
            .all(|d| !d.contains("counts not read")),
        "{:?}",
        summary.details
    );
    assert!(
        summary
            .details
            .contains(&"`bash scripts/test-unit.sh`: exit 0, 80 passed, 0 failed.".to_string()),
        "{:?}",
        summary.details
    );
    // The same rule as for the other runners: the largest step of one runner stands for its group.
    assert_eq!(
        summary.claim().as_deref(),
        Some("repository tests pass in a clean checkout: 80 of 80")
    );
}
