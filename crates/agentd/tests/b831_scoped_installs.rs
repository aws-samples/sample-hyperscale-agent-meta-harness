//! An install step that fails stops only the test commands that need it.
//!
//! Since 5.15 the report put every install step in front of the test commands of the lifecycle log and stopped at the first that failed. The
//! re-review found two runs that 5.14 had reported and 5.15 did not: a log with `go test ./...` and `npm test` whose `npm ci` failed left the Go
//! suite not run (5.14: exit 0), and a pytest log whose `requirements.txt` pip could not install ran no test (5.14: 5 of 5 passed). Now each
//! install stands right before the first command it serves, a failed Node install stops only the Node commands, and a logged Python command
//! whose virtual environment could not be made runs as the log wrote it. The report names the install that failed on each command it stopped.
//!
//! The plan tests start no process. The step tests run stand-in programs made here (`npm`, `go`, `python3`) through the module's own `run_steps`.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::report::plan::{Plan, PlannedStep, plan};
use agentd::direct::report::progress::Progress;
use agentd::direct::report::run::RunEnv;
use agentd::direct::report::{Settings, run_steps};
use agentd::events::EventRecorder;
use harness_protocol::direct::{TestCommandSource, TestReport, TestStep, TestStepRole};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const VENV_INSTALL: &str = "python3 -m venv --system-site-packages .ham-venv && .ham-venv/bin/python -m pip install -q -r requirements.txt";
const LOCKED_PACKAGE: &str = "{\"scripts\": {\"test\": \"node --test tests/*.test.js\"}, \"devDependencies\": {\"jsdom\": \"31.0.0\"}}";

fn tree(label: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = common::scratch(label).join("tree");
    for (rel, text) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    root
}

fn steps(root: &Path) -> Vec<PlannedStep> {
    match plan(root) {
        Plan::Steps(s) => s,
        Plan::NoCommand(why) => panic!("no steps: {why}"),
    }
}

fn lines(root: &Path) -> Vec<String> {
    steps(root)
        .iter()
        .map(|s| {
            let role = match s.role {
                TestStepRole::Setup => "setup",
                TestStepRole::Test => "test",
            };
            let source = match s.source {
                TestCommandSource::TestCommandFile => "file",
                TestCommandSource::LifecycleLog => "log",
                TestCommandSource::GoalCard => "card",
                TestCommandSource::Detected => "detected",
            };
            format!("{role} {source} {}", s.command)
        })
        .collect()
}

fn log(commands: &[&str]) -> String {
    commands
        .iter()
        .map(|c| format!("RUN | {c} | exit 0 | pass 3 fail 0 | ok\n"))
        .collect()
}

fn two_languages(label: &str) -> PathBuf {
    tree(
        label,
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["go test -v ./...", "npm test"]),
            ),
            ("go.mod", "module example.com/two\n\ngo 1.22\n"),
            ("package.json", LOCKED_PACKAGE),
            ("package-lock.json", "{}"),
        ],
    )
}

// ---- the plan -----------------------------------------------------------------------------------------------------------------------

#[test]
fn b831_each_install_stands_right_before_the_first_command_it_serves() {
    assert_eq!(
        lines(&two_languages("b831-plan")),
        [
            "test log go test -v ./...",
            "setup detected npm ci",
            "test log npm test",
        ]
    );
    let card = tree(
        "b831-card",
        &[
            (
                ".ham/lifecycle/GOAL.md",
                "MEASURABLE: the goal is met when every check passes.\n1. `npm test` -> exit 0\n2. `python3 -m pytest -q` -> exit 0\n",
            ),
            ("package.json", LOCKED_PACKAGE),
            ("package-lock.json", "{}"),
            ("requirements.txt", "flask==3.0.3\n"),
        ],
    );
    assert_eq!(
        lines(&card),
        [
            "setup detected npm ci".to_string(),
            "test card npm test".to_string(),
            format!("setup detected {VENV_INSTALL}"),
            "test card .ham-venv/bin/python -m pytest -q".to_string(),
        ]
    );
}

#[test]
fn b831_the_node_install_stands_before_a_command_that_may_use_node_modules() {
    let package = [
        ("package.json", LOCKED_PACKAGE),
        ("package-lock.json", "{}"),
    ];
    let plan_of = |label: &str, commands: &[&str]| {
        let log = log(commands);
        let mut files: Vec<(&str, &str)> = vec![(".ham/lifecycle/LOG.md", log.as_str())];
        files.extend(package);
        lines(&tree(label, &files))
    };
    // `make test` may call npm or jest itself: it runs after the install, as it did before the installs were scoped.
    assert_eq!(
        plan_of("b831-make", &["make test", "npm test"]),
        [
            "setup detected npm ci",
            "test log make test",
            "test log npm test"
        ]
    );
    // A runner of another language stands ahead of the install, whatever stands between.
    assert_eq!(
        plan_of(
            "b831-go-make",
            &["go test -v ./...", "make test", "npm test"]
        ),
        [
            "test log go test -v ./...",
            "setup detected npm ci",
            "test log make test",
            "test log npm test"
        ]
    );
    assert_eq!(
        plan_of("b831-cd-go", &["cd api && go test ./...", "npm test"]),
        [
            "test log cd api && go test ./...",
            "setup detected npm ci",
            "test log npm test"
        ]
    );
    // Nothing here uses Node, so no install is added, as before.
    assert_eq!(
        plan_of("b831-make-alone", &["make test"]),
        ["test log make test"]
    );
}

// ---- the steps, with stand-in programs -------------------------------------------------------------------------------------------------

fn executable(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Stand-ins: `npm ci` fails as a lockfile out of date does, `npm test` passes only with `node_modules`, `go test -v` passes two tests, the
/// image's `python3 -m pytest` passes five tests that need no package, and the environment's pip cannot install `requirements.txt`.
fn fake_bin() -> PathBuf {
    let bin = common::scratch("b831-bin");
    executable(
        &bin.join("npm"),
        "#!/bin/sh\ncase \"$1\" in\n  ci) echo 'npm error code EUSAGE' >&2; echo 'npm error The package-lock.json file is out of date' >&2; exit 1 ;;\n  test) if [ -d node_modules ]; then echo '# pass 3'; exit 0; fi; echo \"Error: Cannot find module 'jsdom'\"; exit 1 ;;\n  *) exit 9 ;;\nesac\n",
    );
    executable(
        &bin.join("go"),
        "#!/bin/sh\necho '=== RUN   TestA'\necho '--- PASS: TestA (0.00s)'\necho '=== RUN   TestB'\necho '--- PASS: TestB (0.00s)'\necho 'PASS'\necho 'ok  \texample.com/two\t0.01s'\nexit 0\n",
    );
    let venv_python = "#!/bin/sh\nif [ \"$1\" = -m ] && [ \"$2\" = pip ]; then echo 'ERROR: Could not find a version that satisfies the requirement flask==99.0'; exit 1; fi\nif [ \"$1\" = -m ] && [ \"$2\" = pytest ]; then echo 'the environment was used'; exit 3; fi\nexit 9\n";
    executable(
        &bin.join("python3"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" = -m ] && [ \"$2\" = venv ]; then\n  for dir; do :; done\n  mkdir -p \"$dir/bin\"\n  cat > \"$dir/bin/python\" <<'STAND_IN'\n{venv_python}STAND_IN\n  chmod 755 \"$dir/bin/python\"\n  exit 0\nfi\nif [ \"$1\" = -m ] && [ \"$2\" = pytest ]; then echo '.....'; echo '5 passed in 0.10s'; exit 0; fi\nexit 9\n"
        ),
    );
    bin
}

fn env_for(bin: &Path) -> RunEnv {
    RunEnv {
        sandbox: None,
        work: common::scratch("b831-work"),
        run_as: None,
        path: vec![
            bin.to_path_buf(),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
        ],
        engine_env: BTreeMap::new(),
        engine_home: None,
        cargo_config: None,
    }
}

async fn report_with(root: &Path, bin: &Path) -> TestReport {
    let planned = steps(root);
    let settings = Settings {
        enabled: true,
        budget: Duration::from_secs(60),
        step: Duration::from_secs(30),
        tail: 4096,
    };
    let progress = Progress::new(EventRecorder::new("R1", "direct"));
    let (done, status, reason) = run_steps(
        &env_for(bin),
        &planned,
        root,
        &settings,
        Instant::now(),
        &progress,
        &[],
        &[],
    )
    .await;
    let report = TestReport {
        schema_version: 1,
        run_id: "R1".into(),
        head_sha: "a".repeat(40),
        status,
        reason,
        started_at: None,
        duration_ms: 1,
        steps: done,
        summary: None,
    };
    report.validate("R1", &"a".repeat(40)).unwrap();
    report
}

async fn report_of(root: &Path) -> TestReport {
    report_with(root, &fake_bin()).await
}

fn brief(steps: &[TestStep]) -> Vec<String> {
    steps
        .iter()
        .map(|s| {
            format!(
                "{} exit={:?} not_run={}",
                s.command,
                s.exit_code,
                s.is_not_run()
            )
        })
        .collect()
}

/// Stand-ins for a tree whose `make test` and `npm test` both need `node_modules`: `npm ci` makes the folder when `ci_ok` and fails as a lockfile out of
/// date does when not.
fn node_bin(ci_ok: bool) -> PathBuf {
    let bin = common::scratch("b831-node-bin");
    let ci = if ci_ok {
        "mkdir -p node_modules; echo 'added 1 package'; exit 0"
    } else {
        "echo 'npm error code EUSAGE' >&2; exit 1"
    };
    executable(
        &bin.join("npm"),
        &format!(
            "#!/bin/sh\ncase \"$1\" in\n  ci) {ci} ;;\n  test) if [ -d node_modules ]; then echo '# pass 3'; exit 0; fi; echo \"Error: Cannot find module 'jsdom'\"; exit 1 ;;\n  *) exit 9 ;;\nesac\n"
        ),
    );
    executable(
        &bin.join("make"),
        "#!/bin/sh\nif [ \"$1\" = test ] && [ -d node_modules ]; then echo '3 passing'; exit 0; fi\necho \"Error: Cannot find module 'jsdom'\"\nexit 1\n",
    );
    bin
}

fn make_then_npm(label: &str) -> PathBuf {
    tree(
        label,
        &[
            (".ham/lifecycle/LOG.md", &log(&["make test", "npm test"])),
            ("package.json", LOCKED_PACKAGE),
            ("package-lock.json", "{}"),
        ],
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn b831_a_make_target_that_uses_node_modules_passes_after_the_install() {
    let r = report_with(&make_then_npm("b831-make-run"), &node_bin(true)).await;
    assert_eq!(
        brief(&r.steps),
        [
            "npm ci exit=Some(0) not_run=false",
            "make test exit=Some(0) not_run=false",
            "npm test exit=Some(0) not_run=false",
        ],
        "{r:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b831_a_failed_install_stops_only_the_commands_that_name_a_node_tool() {
    // `make test` is not known to need the install, so it is not skipped for it: it runs and says what it says.
    let r = report_with(&make_then_npm("b831-make-fail"), &node_bin(false)).await;
    assert_eq!(
        brief(&r.steps),
        [
            "npm ci exit=Some(1) not_run=false",
            "make test exit=Some(1) not_run=false",
            "npm test exit=None not_run=true",
        ],
        "{r:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b831_a_failed_npm_install_leaves_the_go_suite_running() {
    let r = report_of(&two_languages("b831-run")).await;
    assert_eq!(
        brief(&r.steps),
        [
            "go test -v ./... exit=Some(0) not_run=false",
            "npm ci exit=Some(1) not_run=false",
            "npm test exit=None not_run=true",
        ],
        "{r:?}"
    );
    assert_eq!(
        r.steps[2].output_tail,
        "NOT RUN: skipped: the setup step `npm ci` failed"
    );
    let s = r.summary();
    assert_eq!(s.claim(), None);
    assert_eq!(
        s.sentence(),
        "Setup `npm ci` exited 1 in a clean checkout, so `npm test` did not run; for the test commands that ran, the runner reported 2 passed, 0 failed."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b831_a_logged_python_command_runs_as_written_when_its_environment_cannot_be_made() {
    let root = tree(
        "b831-py",
        &[
            (".ham/lifecycle/LOG.md", &log(&["python3 -m pytest -q"])),
            ("requirements.txt", "flask==99.0\n"),
        ],
    );
    let r = report_of(&root).await;
    assert_eq!(
        brief(&r.steps),
        [
            format!("{VENV_INSTALL} exit=Some(1) not_run=false"),
            "python3 -m pytest -q exit=Some(0) not_run=false".to_string(),
        ],
        "{r:?}"
    );
    // The row of the log stays with the command.
    assert!(r.steps[1].logged.is_some(), "{r:?}");
    let s = r.summary();
    assert_eq!(s.claim(), None);
    assert_eq!(
        s.sentence(),
        format!(
            "Setup `{VENV_INSTALL}` exited 1 in a clean checkout, so the test commands ran without it; the runner reported 5 passed, 0 failed."
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b831_a_detected_plan_stops_only_the_commands_of_the_failed_install() {
    // No log: the plan comes from the manifests. The environment cannot be made, so its tests do not run, and the Go suite still does.
    let root = tree(
        "b831-detected",
        &[
            ("requirements.txt", "flask==99.0\n"),
            ("pytest.ini", "[pytest]\n"),
            ("tests/test_a.py", "def test_a():\n    pass\n"),
            ("go.mod", "module example.com/two\n\ngo 1.22\n"),
        ],
    );
    let r = report_of(&root).await;
    assert_eq!(
        brief(&r.steps),
        [
            format!("{VENV_INSTALL} exit=Some(1) not_run=false"),
            ".ham-venv/bin/python -m pytest -q exit=None not_run=true".to_string(),
            "go test -v ./... exit=Some(0) not_run=false".to_string(),
        ],
        "{r:?}"
    );
    assert_eq!(
        r.steps[1].not_run_reason(),
        Some(format!("skipped: the setup step `{VENV_INSTALL}` failed").as_str())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b831_a_setup_line_of_the_projects_own_file_still_stops_every_command() {
    // `.ham/test-command` says what to install; the report cannot know which command needs which line, so a failed line stops them all.
    let root = tree(
        "b831-file",
        &[(
            ".ham/test-command",
            "setup: npm ci\ngo test -v ./...\nnpm test\n",
        )],
    );
    let r = report_of(&root).await;
    assert_eq!(
        brief(&r.steps),
        [
            "npm ci exit=Some(1) not_run=false",
            "go test -v ./... exit=None not_run=true",
            "npm test exit=None not_run=true",
        ],
        "{r:?}"
    );
}
