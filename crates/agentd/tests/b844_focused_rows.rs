//! A language whose only rows in the lifecycle log name test files still gets a report step, and the report says what it left out.
//!
//! The engine logged its Python suite as `python3 -m pytest tests/test_textstats.py -q`, the project's only test file. A logged command that
//! names a test file is a part of a suite and stays out of the plan, so the plan held `node --test` alone, `npm install` exited 1, and the
//! card read "Setup `npm install` exited 1 in a clean checkout, so the test command did not run." No Python test ran in the clean checkout and
//! nothing said that a logged test command had been left out. Now:
//!
//! - the command that names test files still stays out, and when no command of the log or the goal card runs that language, the plan adds what
//!   the layout of the tree gives for it (`python3 -m pytest -q` here), after the model's own commands, with the note that it runs in place of
//!   the logged command;
//! - a logged command that was left out with nothing in its place is said in the details of the report;
//! - the sentence of a failed install names the command it stopped.
//!
//! The plan tests start no process. The step tests run stand-in programs made here (`npm`, `node`, `python3`, `go`) through the module's own
//! `run_planned`.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::report::plan::{
    LeftOut, NO_COMMAND_REASON, Omitted, Plan, PlanNotes, PlannedStep, plan, plan_with_notes,
};
use agentd::direct::report::progress::Progress;
use agentd::direct::report::run::RunEnv;
use agentd::direct::report::{Settings, run_planned};
use agentd::events::EventRecorder;
use harness_protocol::direct::{
    LeftOutWhy, LoggedRun, MAX_REPORT_REASON_CHARS, TestCommandSource, TestReport, TestStep,
    TestStepRole,
};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The lifecycle log of the run, as it was delivered (`.ham/lifecycle/LOG.md` of the zip).
const LIVE_LOG: &str = "RUN | python3 -m pytest tests/test_textstats.py -q | exit 0 | pass 6 fail 0 | 6 passed in 0.01s (after the last edit)
RUN | node --test | exit 0 | pass 1 fail 0 | ℹ pass 1, ℹ fail 0 (after the last edit)
RUN | python3 -m pytest tests/test_textstats.py -q | exit 0 | pass 6 fail 0 | 6 passed in 0.01s (verification, after the last edit)
RUN | node --test | exit 0 | pass 1 fail 0 | ℹ pass 1, ℹ fail 0 (verification, after the last edit)
RUN | python3 -m pytest tests/test_textstats.py -q | exit 0 | pass 6 fail 0 | 6 passed in 0.01s (review, after the last edit)
RUN | node --test | exit 0 | pass 3 fail 0 | ℹ pass 3, ℹ fail 0 (review, after the last edit)
";
/// The `package.json` of the run: its one dependency is in no registry, so `npm install` fails.
const LIVE_PACKAGE: &str = "{\n  \"name\": \"textstats-sum\",\n  \"version\": \"1.0.0\",\n  \"private\": true,\n  \"scripts\": {\n    \"test\": \"node --test\"\n  },\n  \"dependencies\": {\n    \"ham-proof-package-that-does-not-exist-5-19\": \"1.0.0\"\n  }\n}\n";
const LOGGED_PYTHON: &str = "python3 -m pytest tests/test_textstats.py -q";
const VENV_INSTALL: &str = "python3 -m venv --system-site-packages .ham-venv && .ham-venv/bin/python -m pip install -q -r requirements.txt";
const PY_TEST: &str = "def test_a():\n    pass\n";

fn tree(label: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = common::scratch(label).join("tree");
    for (rel, text) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    root
}

/// The tree of the run: the files of the zip that the plan reads.
fn live_tree(label: &str) -> PathBuf {
    tree(
        label,
        &[
            (".ham/lifecycle/LOG.md", LIVE_LOG),
            ("package.json", LIVE_PACKAGE),
            ("lib/sum.js", "module.exports = (a, b) => a + b;\n"),
            ("test/sum.test.js", "require('node:test');\n"),
            (
                "textstats.py",
                "def word_count(text):\n    return len(text.split())\n",
            ),
            (
                "tests/test_textstats.py",
                "from textstats import word_count\n\n\ndef test_word_count():\n    assert word_count('a b') == 2\n",
            ),
        ],
    )
}

fn planned(root: &Path) -> (Vec<PlannedStep>, PlanNotes) {
    match plan_with_notes(root) {
        (Plan::Steps(s), notes) => (s, notes),
        (Plan::NoCommand(why), _) => panic!("no steps: {why}"),
    }
}

fn line(s: &PlannedStep) -> String {
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
}

fn lines(root: &Path) -> Vec<String> {
    planned(root).0.iter().map(line).collect()
}

fn left_out(command: &str, more: usize) -> LeftOut {
    LeftOut {
        command: command.to_string(),
        more,
    }
}

/// Commands left out because the report found no command for the whole suite.
fn omitted(command: &str, more: usize) -> Omitted {
    Omitted {
        rows: left_out(command, more),
        why: LeftOutWhy::NoSuiteCommand,
        logged: None,
    }
}

fn log(rows: &[&str]) -> String {
    rows.iter()
        .map(|c| format!("RUN | {c} | exit 0 | pass 3 fail 0 | ok\n"))
        .collect()
}

// ---- the plan -----------------------------------------------------------------------------------------------------------------------

#[test]
fn b844_the_log_of_the_run_plans_the_python_layout_beside_the_node_command() {
    let root = live_tree("b844-live-plan");
    let (steps, notes) = planned(&root);
    assert_eq!(
        steps.iter().map(line).collect::<Vec<_>>(),
        [
            "setup detected npm install",
            "test log node --test",
            "test detected python3 -m pytest -q",
        ]
    );
    // The model's own command keeps the row of its last run; the step the report chose has no row of the log.
    assert_eq!(
        steps[1].logged,
        Some(LoggedRun {
            exit_code: Some(0),
            passed: Some(3),
            failed: Some(0)
        })
    );
    assert_eq!(steps[2].logged, None);
    assert_eq!(
        notes,
        PlanNotes {
            stand_ins: vec![(2, left_out(LOGGED_PYTHON, 0))],
            left_out: Vec::new(),
            // The Python step is the report's addition behind the two steps of the model's command.
            chosen_from: Some(2),
            not_again: Vec::new(),
        }
    );
    // `plan` gives the same steps.
    assert_eq!(plan(&root), Plan::Steps(steps));
}

#[test]
fn b844_a_command_that_names_test_files_among_whole_suite_rows_adds_nothing() {
    // The rule of before: the part is not run as the suite, and the whole-suite row of the same language is the report's command.
    let root = tree(
        "b844-covered",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&[
                    "python3 -m pytest tests/test_a.py -q",
                    "python3 -m pytest -q",
                    "python3 -m pytest tests/test_b.py::test_x",
                    "go test -run TestA ./...",
                    "go test ./...",
                ]),
            ),
            ("tests/test_a.py", PY_TEST),
            ("tests/test_b.py", PY_TEST),
            ("go.mod", "module example.com/x\n"),
        ],
    );
    assert_eq!(
        lines(&root),
        ["test log python3 -m pytest -q", "test log go test ./..."]
    );
    assert_eq!(planned(&root).1, PlanNotes::default());
}

#[test]
fn b844_prose_after_a_command_is_still_cut_and_a_row_of_the_base_commit_is_still_left_out() {
    let root = tree(
        "b844-prose-base",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&[
                    "node --test after the last edit",
                    // Made in a copy of the base commit: no run of this checkout, so nothing stands in its place and nothing is said of it.
                    "python3 -m pytest tests/test_a.py -q (at BASE)",
                    "pytest tests/test_a.py -q at BASE worktree",
                ]),
            ),
            ("tests/test_a.py", PY_TEST),
        ],
    );
    assert_eq!(lines(&root), ["test log node --test"]);
    assert_eq!(planned(&root).1, PlanNotes::default());
}

#[test]
fn b844_a_command_whose_language_the_report_cannot_tell_gets_nothing_beside_it() {
    // `make test` may run the Python suite itself: the report adds no second run of it and says nothing.
    let root = tree(
        "b844-make",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["python3 -m pytest tests/test_a.py -q", "make test"]),
            ),
            ("tests/test_a.py", PY_TEST),
        ],
    );
    assert_eq!(lines(&root), ["test log make test"]);
    assert_eq!(planned(&root).1, PlanNotes::default());
}

#[test]
fn b844_the_step_of_the_layout_brings_its_install() {
    // A tree with a requirements.txt: the Python layout runs in the report's virtual environment, made right before it.
    let root = tree(
        "b844-venv",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["go test ./...", "pytest tests/test_a.py", "pytest -k parse"]),
            ),
            ("go.mod", "module example.com/x\n"),
            ("requirements.txt", "requests==2.32.0\n"),
            ("tests/test_a.py", PY_TEST),
        ],
    );
    let (steps, notes) = planned(&root);
    assert_eq!(
        steps.iter().map(line).collect::<Vec<_>>(),
        [
            "test log go test ./...".to_string(),
            format!("setup detected {VENV_INSTALL}"),
            "test detected .ham-venv/bin/python -m pytest -q".to_string(),
        ]
    );
    // The last run among the commands left out is the one named; the other is counted.
    assert_eq!(
        notes,
        PlanNotes {
            stand_ins: vec![(2, left_out("pytest -k parse", 1))],
            left_out: Vec::new(),
            // The install and the step behind it are the report's additions.
            chosen_from: Some(1),
            not_again: Vec::new(),
        }
    );
}

#[test]
fn b844_a_command_left_out_with_nothing_in_its_place_is_in_the_notes() {
    // Node: the manifest has no test script, so the layout gives no command for it.
    let no_script = tree(
        "b844-no-layout",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["go test ./...", "npx jest src/a.test.ts"]),
            ),
            ("go.mod", "module example.com/x\n"),
            ("package.json", "{\"name\": \"x\"}"),
        ],
    );
    assert_eq!(lines(&no_script), ["test log go test ./..."]);
    assert_eq!(
        planned(&no_script).1,
        PlanNotes {
            stand_ins: Vec::new(),
            left_out: vec![omitted("npx jest src/a.test.ts", 0)],
            chosen_from: None,
            not_again: Vec::new(),
        }
    );
    // Python: a file names packages that the report has no install step for, so the layout is not run, as before, and the entry
    // names the file.
    let no_install = tree(
        "b844-no-install",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["node --test", "python3 -m pytest tests/test_a.py -q"]),
            ),
            (
                "pyproject.toml",
                "[project]\nname = \"x\"\ndependencies = [\"requests\"]\n",
            ),
            ("tests/test_a.py", PY_TEST),
        ],
    );
    assert_eq!(lines(&no_install), ["test log node --test"]);
    assert_eq!(
        planned(&no_install).1.left_out,
        [Omitted {
            rows: left_out("python3 -m pytest tests/test_a.py -q", 0),
            why: LeftOutWhy::Packages(Some("pyproject.toml".into())),
            logged: None,
        }]
    );
    // A command that works in another folder: the layout of the root is not its suite.
    let elsewhere = tree(
        "b844-elsewhere",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&[
                    "node --test",
                    "cd api && python3 -m pytest tests/test_a.py -q",
                ]),
            ),
            ("tests/test_root.py", PY_TEST),
            ("api/tests/test_a.py", PY_TEST),
        ],
    );
    assert_eq!(lines(&elsewhere), ["test log node --test"]);
    assert_eq!(
        planned(&elsewhere).1,
        PlanNotes {
            stand_ins: Vec::new(),
            left_out: vec![omitted("cd api && python3 -m pytest tests/test_a.py -q", 0)],
            chosen_from: None,
            not_again: Vec::new(),
        }
    );
}

#[test]
fn b844_a_log_with_only_such_rows_still_falls_through_to_the_layout_and_says_what_stands_in() {
    let root = tree(
        "b844-only-focused",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["python3 -m pytest tests/test_a.py::test_x"]),
            ),
            ("go.mod", "module example.com/x\n"),
            ("tests/test_a.py", PY_TEST),
        ],
    );
    let (steps, notes) = planned(&root);
    assert_eq!(
        steps.iter().map(line).collect::<Vec<_>>(),
        [
            "test detected python3 -m pytest -q",
            "test detected go test -v ./...",
        ]
    );
    assert_eq!(
        notes,
        PlanNotes {
            stand_ins: vec![(0, left_out("python3 -m pytest tests/test_a.py::test_x", 0))],
            left_out: Vec::new(),
            // A plan that is the detection: the step is no addition to commands of the model's.
            chosen_from: None,
            not_again: Vec::new(),
        }
    );
    // The goal card comes before the layout, as before; the Python layout is added behind its command.
    let card = tree(
        "b844-card",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["python3 -m pytest tests/test_a.py::test_x"]),
            ),
            (
                ".ham/lifecycle/GOAL.md",
                "MEASURABLE: the goal is met when every check passes.\n1. `go test ./...` -> exit 0\n",
            ),
            ("go.mod", "module example.com/x\n"),
            ("tests/test_a.py", PY_TEST),
        ],
    );
    assert_eq!(
        lines(&card),
        [
            "test card go test ./...",
            "test detected python3 -m pytest -q"
        ]
    );
}

#[test]
fn b844_the_projects_own_file_and_scripts_are_run_as_they_are_with_no_note() {
    let file = tree(
        "b844-file",
        &[
            (".ham/test-command", "node --test\n"),
            (
                ".ham/lifecycle/LOG.md",
                &log(&["python3 -m pytest tests/test_a.py -q"]),
            ),
            ("tests/test_a.py", PY_TEST),
        ],
    );
    assert_eq!(lines(&file), ["test file node --test"]);
    assert_eq!(planned(&file).1, PlanNotes::default());
}

#[test]
fn b844_a_tree_with_no_command_names_the_logged_command_it_left_out() {
    let root = tree(
        "b844-none",
        &[(
            ".ham/lifecycle/LOG.md",
            &log(&[
                "python3 -m pytest tests/test_a.py::test_x",
                "python3 -m pytest tests/test_a.py::test_y",
            ]),
        )],
    );
    match plan(&root) {
        Plan::NoCommand(why) => assert_eq!(
            why,
            format!(
                "{NO_COMMAND_REASON}; `python3 -m pytest tests/test_a.py::test_y` and 1 more of the lifecycle log name test files, tests or filters and were left out"
            )
        ),
        other => panic!("{other:?}"),
    }
    // A tree whose log names no such command gives the reason of before.
    let plain = tree("b844-none-plain", &[("README.md", "hello\n")]);
    assert_eq!(plan(&plain), Plan::NoCommand(NO_COMMAND_REASON.into()));
    // A long command is cut, so the reason stays inside the bound of a report's reason.
    let long = tree(
        "b844-none-long",
        &[(
            ".ham/lifecycle/LOG.md",
            &log(&[format!("pytest -k {}", "a".repeat(1_500)).as_str()]),
        )],
    );
    match plan(&long) {
        Plan::NoCommand(why) => {
            assert!(why.chars().count() <= MAX_REPORT_REASON_CHARS, "{why}");
            assert!(
                why.ends_with(
                    "a` of the lifecycle log names a test file, a test or a filter and was left out"
                ),
                "{why}"
            );
        }
        other => panic!("{other:?}"),
    }
}

// ---- the steps, with stand-in programs -------------------------------------------------------------------------------------------------

fn executable(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Stand-ins: `npm install` fails as it did in the run (the package is in no registry), `node --test` passes three tests, `python3 -m pytest`
/// prints `python_output` and exits 0, and `go test` passes two tests.
fn fake_bin(python_output: &str) -> PathBuf {
    let bin = common::scratch("b844-bin");
    executable(
        &bin.join("npm"),
        "#!/bin/sh\necho 'npm error code E404' >&2\necho \"npm error 404 Not Found - GET http://127.0.0.1:3128/npm/ham-proof-package-that-does-not-exist-5-19\" >&2\nexit 1\n",
    );
    executable(
        &bin.join("node"),
        "#!/bin/sh\necho '# pass 3'\necho '# fail 0'\nexit 0\n",
    );
    executable(
        &bin.join("python3"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" = -m ] && [ \"$2\" = pytest ]; then\ncat <<'STAND_IN'\n{python_output}\nSTAND_IN\nexit 0\nfi\nexit 9\n"
        ),
    );
    executable(
        &bin.join("go"),
        "#!/bin/sh\necho '=== RUN   TestA'\necho '--- PASS: TestA (0.00s)'\necho '=== RUN   TestB'\necho '--- PASS: TestB (0.00s)'\necho 'PASS'\necho 'ok  \texample.com/x\t0.01s'\nexit 0\n",
    );
    bin
}

const SIX_PASSED: &str = "......\n6 passed in 0.01s";

fn env_for(bin: &Path) -> RunEnv {
    RunEnv {
        sandbox: None,
        work: common::scratch("b844-work"),
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

async fn report_with(root: &Path, bin: &Path, tail: usize) -> TestReport {
    let (steps, notes) = planned(root);
    let settings = Settings {
        enabled: true,
        budget: Duration::from_secs(60),
        step: Duration::from_secs(30),
        tail,
    };
    let progress = Progress::new(EventRecorder::new("R1", "direct"));
    let (done, status, reason) = run_planned(
        &env_for(bin),
        &steps,
        &notes,
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
async fn b844_the_run_reports_the_python_tests_and_names_the_node_command_its_install_stopped() {
    let r = report_with(&live_tree("b844-live-run"), &fake_bin(SIX_PASSED), 4096).await;
    assert_eq!(
        brief(&r.steps),
        [
            "npm install exit=Some(1) not_run=false",
            "node --test exit=None not_run=true",
            "python3 -m pytest -q exit=Some(0) not_run=false",
        ],
        "{r:?}"
    );
    assert_eq!(
        r.steps[1].output_tail,
        "NOT RUN: skipped: the setup step `npm install` failed"
    );
    // The step that the report chose says at the head of its output which logged command it ran in place of; what pytest printed follows.
    assert_eq!(
        r.steps[2].output_tail,
        "PLAN NOTE: in place of `python3 -m pytest tests/test_textstats.py -q`\n......\n6 passed in 0.01s"
    );
    assert_eq!(r.steps[2].in_place_of(), Some((LOGGED_PYTHON, 0)));
    let s = r.summary();
    assert_eq!(s.claim(), None);
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

#[tokio::test(flavor = "multi_thread")]
async fn b844_an_install_that_stops_the_only_test_command_names_it() {
    // The plan of the run as release 5.19 made it: the Node command alone.
    let root = tree(
        "b844-node-only",
        &[
            (".ham/lifecycle/LOG.md", &log(&["node --test"])),
            ("package.json", LIVE_PACKAGE),
        ],
    );
    let r = report_with(&root, &fake_bin(SIX_PASSED), 4096).await;
    assert_eq!(
        brief(&r.steps),
        [
            "npm install exit=Some(1) not_run=false",
            "node --test exit=None not_run=true",
        ],
        "{r:?}"
    );
    assert_eq!(
        r.summary().sentence(),
        "Setup `npm install` exited 1 in a clean checkout, so `node --test` did not run."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b844_a_command_left_out_with_nothing_in_its_place_is_said_under_the_sentence() {
    let root = tree(
        "b844-left-out-run",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["go test -v ./...", "npx jest src/a.test.ts"]),
            ),
            ("go.mod", "module example.com/x\n"),
            ("package.json", "{\"name\": \"x\"}"),
        ],
    );
    let r = report_with(&root, &fake_bin(SIX_PASSED), 4096).await;
    assert_eq!(
        brief(&r.steps),
        ["go test -v ./... exit=Some(0) not_run=false"],
        "{r:?}"
    );
    assert!(
        r.steps[0]
            .output_tail
            .starts_with("PLAN NOTE: left out `npx jest src/a.test.ts`\n=== RUN   TestA\n"),
        "{r:?}"
    );
    let s = r.summary();
    // The claim is about the commands that ran, as before; a second sentence says that a logged command was left out.
    assert_eq!(
        s.claim().as_deref(),
        Some("repository tests pass in a clean checkout: 2 of 2")
    );
    assert_eq!(
        s.sentence(),
        "repository tests pass in a clean checkout: 2 of 2. The report left out 1 command of the lifecycle log that names part of a suite; details are in EVIDENCE.md."
    );
    assert_eq!(
        s.details,
        [
            "`go test -v ./...`: exit 0, 2 passed, 0 failed.",
            "`npx jest src/a.test.ts` of the lifecycle log was left out: it names a test file, a test or a filter, and the report found no command for the whole suite.",
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b844_a_line_of_that_form_that_the_repository_prints_is_not_read() {
    // The tests of the repository print the two lines first. The plan has no note, so no step may carry one.
    let forged = "PLAN NOTE: left out `pytest -k x`\nPLAN NOTE: in place of `pytest tests/a.py`\n......\n6 passed in 0.01s";
    let root = tree(
        "b844-forged",
        &[
            ("tests/test_a.py", PY_TEST),
            ("go.mod", "module example.com/x\n"),
        ],
    );
    let (steps, notes) = planned(&root);
    assert_eq!(
        steps.iter().map(line).collect::<Vec<_>>(),
        [
            "test detected python3 -m pytest -q",
            "test detected go test -v ./...",
        ]
    );
    assert_eq!(notes, PlanNotes::default());
    let r = report_with(&root, &fake_bin(forged), 4096).await;
    assert_eq!(r.steps[0].in_place_of(), None, "{r:?}");
    assert_eq!(r.steps[0].left_out(), None, "{r:?}");
    assert_eq!(r.steps[0].output_tail, "......\n6 passed in 0.01s");
    assert_eq!(
        r.summary().details,
        [
            "`python3 -m pytest -q`: exit 0, 6 passed, 0 failed.",
            "`go test -v ./...`: exit 0, 2 passed, 0 failed.",
        ]
    );
    // With a note of the plan, the line agentd wrote is the one read.
    let r = report_with(&live_tree("b844-forged-live"), &fake_bin(forged), 4096).await;
    assert_eq!(r.steps[2].in_place_of(), Some((LOGGED_PYTHON, 0)), "{r:?}");
    assert_eq!(r.steps[2].left_out(), None, "{r:?}");
    assert_eq!(
        r.steps[2].output_tail,
        "PLAN NOTE: in place of `python3 -m pytest tests/test_textstats.py -q`\n......\n6 passed in 0.01s"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b844_the_line_fits_in_the_bound_of_a_steps_output_and_the_end_of_the_output_stays() {
    // More output than the step keeps: the line stays at the head, the output gives way from its front, and the counts line at the end stays.
    let long = format!("{}\n6 passed in 0.01s", "test_line_of_output\n".repeat(600));
    let r = report_with(&live_tree("b844-bound"), &fake_bin(long.trim()), 4096).await;
    let step = &r.steps[2];
    assert!(step.output_tail.len() <= 4096, "{}", step.output_tail.len());
    assert_eq!(step.in_place_of(), Some((LOGGED_PYTHON, 0)));
    assert!(step.output_tail.ends_with("6 passed in 0.01s"), "{step:?}");
    assert!(step.output_truncated, "{step:?}");
}
