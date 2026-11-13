//! The clean-checkout test report installs what a tree declares before it runs test commands that came from the lifecycle log or the goal
//! card.
//!
//! A run whose test commands came from the lifecycle log ended with "`npm test` exited 1" and 90 passed, with two test files not loaded
//! ("Cannot find module 'jsdom'"), for code that passes 123 of 123 once the packages of `package-lock.json` are present. Only the plan that the
//! report detects from the manifests carried an install step; a plan made from the log or the goal card had none. Now the install step the tree
//! needs stands in front of those commands, for the ecosystems the detection knows: the lockfile or `npm install` for a Node command, and the
//! report's own virtual environment with the packages of `requirements.txt` for a Python command. An install that fails is named, and the tests
//! behind it are listed as not run: none is counted as failed.
//!
//! The plan tests start no process. The step tests run stand-in programs made here (`npm`, `python3`) from a folder of the test, through the
//! module's own `run_steps`.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::report::plan::{Plan, PlannedStep, plan};
use agentd::direct::report::progress::Progress;
use agentd::direct::report::run::RunEnv;
use agentd::direct::report::{Settings, run_steps};
use agentd::events::EventRecorder;
use harness_protocol::direct::{
    LoggedRun, TestCommandSource, TestReport, TestReportStatus, TestStep, TestStepRole,
};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const VENV_INSTALL: &str = "python3 -m venv --system-site-packages .ham-venv && .ham-venv/bin/python -m pip install -q -r requirements.txt";

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

/// `role source command` of every step, in order.
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

const LOCKED_PACKAGE: &str = "{\"scripts\": {\"test\": \"node --test tests/*.test.js\"}, \"devDependencies\": {\"jsdom\": \"31.0.0\"}}";
const APP: &str = "from flask import Flask\n\napp = Flask(__name__)\n";
const PYTEST_FILE: &str = "from app import app\n\n\ndef test_index():\n    assert app.test_client().get('/').status_code == 404\n";

// ----: the plan of commands that come from the lifecycle log or the goal card --------------------------------------------------

#[test]
fn b825_logged_commands_in_a_tree_with_a_lockfile_get_the_install_step_first() {
    // The owner's run of 2026-11-11: two logged commands, a package-lock.json, no requirements.txt.
    let root = tree(
        "b825-lock",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["npm test", "python3 -m pytest -q tests"]),
            ),
            ("package.json", LOCKED_PACKAGE),
            ("package-lock.json", "{}"),
        ],
    );
    assert_eq!(
        lines(&root),
        [
            "setup detected npm ci",
            "test log npm test",
            "test log python3 -m pytest -q tests"
        ]
    );
    // The row of the log stays with its command.
    let s = steps(&root);
    assert_eq!(s[0].logged, None);
    assert_eq!(
        s[1].logged,
        Some(LoggedRun {
            exit_code: Some(0),
            passed: Some(3),
            failed: Some(0)
        })
    );
}

#[test]
fn b825_the_install_follows_the_lockfile_or_the_packages_the_manifest_names() {
    let pnpm = tree(
        "b825-pnpm",
        &[
            (".ham/lifecycle/LOG.md", &log(&["pnpm test"])),
            ("package.json", LOCKED_PACKAGE),
            ("pnpm-lock.yaml", "lockfileVersion: '9.0'\n"),
        ],
    );
    assert_eq!(
        lines(&pnpm),
        [
            "setup detected pnpm install --frozen-lockfile",
            "test log pnpm test"
        ]
    );
    // No lockfile: the manifest names packages, so they are installed from it.
    let unlocked = tree(
        "b825-unlocked",
        &[
            (".ham/lifecycle/LOG.md", &log(&["npx vitest run"])),
            ("package.json", LOCKED_PACKAGE),
        ],
    );
    assert_eq!(
        lines(&unlocked),
        ["setup detected npm install", "test log npx vitest run"]
    );
    // No lockfile and no package named: nothing to install.
    let bare = tree(
        "b825-bare",
        &[
            (".ham/lifecycle/LOG.md", &log(&["node --test"])),
            (
                "package.json",
                "{\"scripts\": {\"test\": \"node --test\"}, \"dependencies\": {}}",
            ),
        ],
    );
    assert_eq!(lines(&bare), ["test log node --test"]);
    // A manifest that cannot be read names nothing.
    let broken = tree(
        "b825-broken",
        &[
            (".ham/lifecycle/LOG.md", &log(&["npm test"])),
            ("package.json", "{not json"),
        ],
    );
    assert_eq!(lines(&broken), ["test log npm test"]);
}

#[test]
fn b825_the_commands_of_the_goal_card_get_the_install_step_too() {
    let root = tree(
        "b825-card",
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
        lines(&root),
        [
            "setup detected npm ci".to_string(),
            format!("setup detected {VENV_INSTALL}"),
            "test card npm test".to_string(),
            "test card .ham-venv/bin/python -m pytest -q".to_string(),
        ]
    );
}

#[test]
fn b825_logged_python_commands_run_in_an_environment_that_holds_the_requirements() {
    let root = tree(
        "b825-py",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&[
                    "python3 -m pytest -q",
                    "CI=1 python -m unittest discover -v",
                    "pytest tests",
                ]),
            ),
            ("requirements.txt", "flask==3.0.3\n"),
        ],
    );
    assert_eq!(
        lines(&root),
        [
            format!("setup detected {VENV_INSTALL}"),
            "test log .ham-venv/bin/python -m pytest -q".to_string(),
            "test log CI=1 .ham-venv/bin/python -m unittest discover -v".to_string(),
            "test log .ham-venv/bin/python -m pytest tests".to_string(),
        ]
    );
    let s = steps(&root);
    assert!(s[1].logged.is_some(), "{s:?}");
    // Without a requirements.txt the commands are the log's own, as before.
    let bare = tree(
        "b825-py-bare",
        &[(".ham/lifecycle/LOG.md", &log(&["python3 -m pytest -q"]))],
    );
    assert_eq!(lines(&bare), ["test log python3 -m pytest -q"]);
}

#[test]
fn b825_a_command_the_install_would_not_serve_gets_none() {
    // Another runner: the lockfile is not its concern.
    let go = tree(
        "b825-go",
        &[
            (".ham/lifecycle/LOG.md", &log(&["go test ./..."])),
            ("package.json", LOCKED_PACKAGE),
            ("package-lock.json", "{}"),
            ("requirements.txt", "flask==3.0.3\n"),
        ],
    );
    assert_eq!(lines(&go), ["test log go test ./..."]);
    // A command that works in another folder: the manifests of the root are not its manifests.
    let sub = tree(
        "b825-sub",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["cd web && npm test", "cd api && python3 -m pytest -q"]),
            ),
            ("package.json", LOCKED_PACKAGE),
            ("package-lock.json", "{}"),
            ("requirements.txt", "flask==3.0.3\n"),
        ],
    );
    assert_eq!(
        lines(&sub),
        [
            "test log cd web && npm test",
            "test log cd api && python3 -m pytest -q"
        ]
    );
    // A tool that is told to work in another folder, and a subshell that goes there.
    for command in [
        "npm test --prefix web",
        "npm test --prefix=web",
        "pnpm test -C web",
        "pnpm test --dir=web",
        "yarn test --cwd web",
        "( cd web && npm test )",
    ] {
        let other = tree(
            "b825-other-folder",
            &[
                (".ham/lifecycle/LOG.md", &log(&[command])),
                ("package.json", LOCKED_PACKAGE),
                ("package-lock.json", "{}"),
            ],
        );
        assert_eq!(lines(&other), [format!("test log {command}")]);
    }
    // A lockfile of a tool the report has no install step for (yarn, bun): the tree may carry its packages itself, and `npm install` is
    // not its install.
    for lockfile in ["yarn.lock", "bun.lock", "bun.lockb"] {
        let yarn = tree(
            "b825-yarn",
            &[
                (".ham/lifecycle/LOG.md", &log(&["yarn test"])),
                ("package.json", LOCKED_PACKAGE),
                (lockfile, ""),
            ],
        );
        assert_eq!(lines(&yarn), ["test log yarn test"], "{lockfile}");
    }
    // A Python command the shell has to read, and tox, which makes its own environments, stay as they are and bring no install.
    let shell = tree(
        "b825-shell",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["python3 -m pytest -q | tee out.txt", "tox"]),
            ),
            ("requirements.txt", "flask==3.0.3\n"),
        ],
    );
    assert_eq!(
        lines(&shell),
        [
            "test log python3 -m pytest -q | tee out.txt",
            "test log tox"
        ]
    );
}

#[test]
fn b825_the_projects_own_command_file_and_its_test_scripts_are_run_as_they_are() {
    let file = tree(
        "b825-file",
        &[
            (".ham/test-command", "npm test\n"),
            ("package.json", LOCKED_PACKAGE),
            ("package-lock.json", "{}"),
        ],
    );
    assert_eq!(lines(&file), ["test file npm test"]);
    let script = tree(
        "b825-script",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["bash scripts/test-unit.sh"]),
            ),
            ("scripts/test-unit.sh", "#!/bin/sh\nnpm ci && npm test\n"),
            ("package.json", LOCKED_PACKAGE),
            ("package-lock.json", "{}"),
        ],
    );
    assert_eq!(lines(&script), ["test log bash scripts/test-unit.sh"]);
}

// ---- the steps, with stand-in programs -------------------------------------------------------------------------------------------------

fn executable(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A stand-in `npm`: `ci` makes `node_modules/jsdom` (or ends with `ci_code` when that is not 0), and `test` passes three tests when the folder is
/// there and fails to load a test file when it is not.
fn fake_npm(ci_code: i32) -> PathBuf {
    let bin = common::scratch("fix515-npm-bin");
    executable(
        &bin.join("npm"),
        &format!(
            "#!/bin/sh\ncase \"$1\" in\n  ci) if [ {ci_code} -ne 0 ]; then echo 'npm error code EUSAGE' >&2; echo 'npm error The package-lock.json file is out of date' >&2; exit {ci_code}; fi; mkdir -p node_modules/jsdom; echo 'added 1 package in 1s' ;;\n  test) if [ -d node_modules/jsdom ]; then echo '# tests 3'; echo '# pass 3'; echo '# fail 0'; exit 0; fi; echo \"Error: Cannot find module 'jsdom'\"; echo '# tests 1'; echo '# pass 0'; echo '# fail 1'; exit 1 ;;\n  *) exit 9 ;;\nesac\n"
        ),
    );
    bin
}

/// A stand-in `python3`: `-m venv <dir>` makes `<dir>/bin/python`, whose `-m pip install` leaves a mark and whose `-m pytest` passes five tests when
/// the mark is there. Its own `-m pytest`, outside that environment, fails to import a package.
fn fake_python() -> PathBuf {
    let bin = common::scratch("fix515-py-bin");
    let venv_python = "#!/bin/sh\nhere=$(dirname \"$0\")\nif [ \"$1\" = -m ] && [ \"$2\" = pip ]; then touch \"$here/../installed\"; exit 0; fi\nif [ \"$1\" = -m ] && [ \"$2\" = pytest ] && [ -f \"$here/../installed\" ]; then echo '5 passed in 0.10s'; exit 0; fi\necho \"ModuleNotFoundError: No module named 'flask'\"; echo '1 error in 0.05s'; exit 2\n";
    executable(
        &bin.join("python3"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" = -m ] && [ \"$2\" = venv ]; then\n  for dir; do :; done\n  mkdir -p \"$dir/bin\"\n  cat > \"$dir/bin/python\" <<'STAND_IN'\n{venv_python}STAND_IN\n  chmod 755 \"$dir/bin/python\"\n  exit 0\nfi\necho \"ModuleNotFoundError: No module named 'flask'\"; echo '1 error in 0.05s'; exit 2\n"
        ),
    );
    bin
}

fn env_for(bin: &Path) -> RunEnv {
    RunEnv {
        sandbox: None,
        work: common::scratch("fix515-work"),
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

/// The report of the plan of `root`, run as the module runs it.
async fn report_of(root: &Path, bin: &Path) -> TestReport {
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

#[tokio::test(flavor = "multi_thread")]
async fn b825_the_packages_of_the_lockfile_are_installed_before_the_logged_test_command_runs() {
    let root = tree(
        "b825-run",
        &[
            (".ham/lifecycle/LOG.md", &log(&["npm test"])),
            ("package.json", LOCKED_PACKAGE),
            ("package-lock.json", "{}"),
        ],
    );
    let r = report_of(&root, &fake_npm(0)).await;
    assert_eq!(
        brief(&r.steps),
        [
            "npm ci exit=Some(0) not_run=false",
            "npm test exit=Some(0) not_run=false"
        ],
        "{r:?}"
    );
    assert_eq!(r.status, TestReportStatus::Ran);
    let s = r.summary();
    assert_eq!(
        s.claim().as_deref(),
        Some("repository tests pass in a clean checkout: 3 of 3"),
        "{s:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b825_an_install_that_fails_is_named_and_the_tests_behind_it_are_not_run_and_not_counted() {
    let root = tree(
        "b825-run-fail",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["npm test", "npm run test:e2e"]),
            ),
            ("package.json", LOCKED_PACKAGE),
            ("package-lock.json", "{}"),
        ],
    );
    let r = report_of(&root, &fake_npm(1)).await;
    assert_eq!(
        brief(&r.steps),
        [
            "npm ci exit=Some(1) not_run=false",
            "npm test exit=None not_run=true",
            "npm run test:e2e exit=None not_run=true"
        ],
        "{r:?}"
    );
    assert_eq!(
        r.steps[1].output_tail,
        "NOT RUN: skipped: the setup step `npm ci` failed"
    );
    // What the log said about the command stays beside it.
    assert!(r.steps[1].logged.is_some());
    assert_eq!(r.status, TestReportStatus::Ran);
    let s = r.summary();
    assert!(s.setup_failed, "{s:?}");
    assert_eq!(s.failed_command.as_deref(), Some("npm ci"));
    assert_eq!(s.exit_code, Some(1));
    // No test is counted: not as passed and not as failed.
    assert_eq!(s.counts, None, "{s:?}");
    assert_eq!(s.claim(), None);
    assert_eq!(
        s.sentence(),
        "Setup `npm ci` exited 1 in a clean checkout, so the test command did not run."
    );
    assert!(
        s.details.contains(
            &"`npm test`: not run (skipped: the setup step `npm ci` failed).".to_string()
        ),
        "{:?}",
        s.details
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b825_the_requirements_are_installed_before_a_logged_python_command_runs() {
    let root = tree(
        "b825-run-py",
        &[
            (".ham/lifecycle/LOG.md", &log(&["python3 -m pytest -q"])),
            ("requirements.txt", "flask==3.0.3\n"),
        ],
    );
    let r = report_of(&root, &fake_python()).await;
    assert_eq!(
        brief(&r.steps),
        [
            format!("{VENV_INSTALL} exit=Some(0) not_run=false"),
            ".ham-venv/bin/python -m pytest -q exit=Some(0) not_run=false".to_string()
        ],
        "{r:?}"
    );
    assert_eq!(
        r.summary().claim().as_deref(),
        Some("repository tests pass in a clean checkout: 5 of 5")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fix515_b696_the_requirements_are_installed_before_the_tests_of_a_plain_python_tree_run() {
    let root = tree(
        "fix515-run-plain",
        &[
            ("requirements.txt", "flask==3.0.3\n"),
            ("app.py", APP),
            ("tests/test_app.py", PYTEST_FILE),
        ],
    );
    let r = report_of(&root, &fake_python()).await;
    assert_eq!(
        brief(&r.steps),
        [
            format!("{VENV_INSTALL} exit=Some(0) not_run=false"),
            ".ham-venv/bin/python -m pytest -q exit=Some(0) not_run=false".to_string()
        ],
        "{r:?}"
    );
    assert_eq!(
        r.summary().claim().as_deref(),
        Some("repository tests pass in a clean checkout: 5 of 5")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fix515_a_setup_step_that_cannot_be_read_names_itself_on_the_tests_it_stops() {
    let root = tree(
        "fix515-unreadable-setup",
        &[(
            ".ham/test-command",
            "setup: pip install -r 'requirements.txt\npython3 -m pytest -q\n",
        )],
    );
    let r = report_of(&root, &fake_python()).await;
    assert_eq!(r.steps.len(), 2, "{r:?}");
    assert!(r.steps[0].is_not_run(), "{r:?}");
    assert_eq!(
        r.steps[1].output_tail,
        "NOT RUN: skipped: the setup step `pip install -r 'requirements.txt` could not be read"
    );
}
