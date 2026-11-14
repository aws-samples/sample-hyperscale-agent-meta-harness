//! When a step of the report's own choosing may stand in place of logged test commands that name a part of a suite, and what the report
//! says of the commands it left out.
//!
//! `b844_focused_rows.rs` replays the delivered run. This file holds the rules around it:
//!
//! - no Python step is added to a tree whose packages are named by a file the report does not install, with or without pytest settings;
//! - test files in folders below `tests/` or beside the code still give the Python suite a step;
//! - a step stands in place of a logged command only when it runs the file that command names;
//! - a logged command that chains two suites covers both languages, and a runner of one language the report has no layout for
//!   (`dotnet test`) leaves the other languages their step;
//! - a step the report added is never the reason a report reads as timed out;
//! - a row the log recorded as failed says so where it is listed as left out;
//! - rows that work in more than one project folder get no step at the root;
//! - the output of a command cannot put a line about the plan where the summary reads one.
//!
//! The plan tests start no process. The step tests run stand-in programs made here through the module's own `run_planned`.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::report::plan::{Plan, PlannedStep, plan_with_notes};
use agentd::direct::report::progress::Progress;
use agentd::direct::report::run::{RunEnv, set_plan_lines};
use agentd::direct::report::{Settings, run_planned};
use agentd::events::EventRecorder;
use harness_protocol::direct::{
    LeftOutWhy, TestCommandSource, TestReport, TestReportStatus, TestStep, TestStepRole,
    in_place_line,
};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const PY_TEST: &str = "def test_a():\n    pass\n";
const VENV_INSTALL: &str = "python3 -m venv --system-site-packages .ham-venv && .ham-venv/bin/python -m pip install -q -r requirements.txt";
/// What the detail line of a command says of the commands it stands in place of or that were left out.
const ONE_PART: &str = "names a test file, a test or a filter";

fn tree(label: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = common::scratch(label).join("tree");
    for (rel, text) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    root
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

/// The plan as lines, or the reason of a tree with no command after `none: `.
fn lines(root: &Path) -> Vec<String> {
    match plan_with_notes(root).0 {
        Plan::Steps(s) => s.iter().map(line).collect(),
        Plan::NoCommand(why) => vec![format!("none: {why}")],
    }
}

/// A lifecycle log whose rows all exited 0 with 3 passed.
fn log(rows: &[&str]) -> String {
    rows.iter()
        .map(|c| format!("RUN | {c} | exit 0 | pass 3 fail 0 | ok\n"))
        .collect()
}

fn executable(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

const NODE_THREE: &str = "#!/bin/sh\necho '# tests 3'\necho '# pass 3'\necho '# fail 0'\nexit 0\n";
const PYTHON_SIX: &str = "#!/bin/sh\necho '......'\necho '6 passed in 0.01s'\nexit 0\n";
const GO_TWO: &str = "#!/bin/sh\necho '=== RUN   TestA'\necho '--- PASS: TestA (0.00s)'\necho '=== RUN   TestB'\necho '--- PASS: TestB (0.00s)'\necho 'PASS'\necho 'ok  \texample.com/x\t0.01s'\nexit 0\n";

/// A folder of stand-in programs: each name with the text of its script.
fn bin(programs: &[(&str, &str)]) -> PathBuf {
    let dir = common::scratch("b844r-bin");
    for (name, text) in programs {
        executable(&dir.join(name), text);
    }
    dir
}

fn env_for(bin: &Path) -> RunEnv {
    RunEnv {
        sandbox: None,
        work: common::scratch("b844r-work"),
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

/// The report of the tree at `root` with the programs of `bin`, each command limited to `step_s` seconds and `tail` bytes of output.
async fn report_of(root: &Path, bin: &Path, step_s: u64, tail: usize) -> TestReport {
    let (steps, notes) = match plan_with_notes(root) {
        (Plan::Steps(s), notes) => (s, notes),
        (Plan::NoCommand(why), _) => panic!("no steps: {why}"),
    };
    let settings = Settings {
        enabled: true,
        budget: Duration::from_secs(60),
        step: Duration::from_secs(step_s),
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

async fn report_with(root: &Path, bin: &Path) -> TestReport {
    report_of(root, bin, 30, 4096).await
}

// ---- a tree whose packages the report does not install gets no Python step ------------------------------------------------------------

/// A project whose Python packages are named by `manifest` (a file the report has no install step for) and that has pytest settings: the
/// log ran the Node suite whole and one Python test file.
fn uninstalled(label: &str, manifest: (&str, &str)) -> PathBuf {
    tree(
        label,
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["node --test", "python3 -m pytest tests/test_api.py -q"]),
            ),
            ("package.json", "{\"scripts\": {\"test\": \"node --test\"}}"),
            manifest,
            (
                "tests/test_api.py",
                "import fastapi\n\n\ndef test_a():\n    pass\n",
            ),
        ],
    )
}

/// The stand-in programs of these tests: `node --test` passes three tests, and a Python that fails as pytest does when a package is missing.
fn node_and_a_python_without_packages() -> PathBuf {
    bin(&[
        ("node", NODE_THREE),
        (
            "python3",
            "#!/bin/sh\necho \"E   ModuleNotFoundError: No module named 'fastapi'\"\necho '1 error in 0.05s'\nexit 2\n",
        ),
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn b844_pytest_settings_do_not_bring_a_python_step_to_a_tree_whose_packages_the_report_does_not_install()
 {
    // pyproject.toml names the packages and holds the pytest settings: the step would run without fastapi and fail for the report's own reason.
    let settings = "[project]\nname = \"x\"\ndependencies = [\"fastapi>=0.110\"]\n\n[tool.pytest.ini_options]\naddopts = \"-q\"\n";
    // The same file without the pytest table, and a requirements file the report does not install that names pytest.
    let plain = "[project]\nname = \"x\"\ndependencies = [\"fastapi>=0.110\"]\n";
    for (label, manifest, named) in [
        (
            "b844r-f1-settings",
            ("pyproject.toml", settings),
            "pyproject.toml",
        ),
        (
            "b844r-f1-plain",
            ("pyproject.toml", plain),
            "pyproject.toml",
        ),
        (
            "b844r-f1-requirements",
            ("requirements-dev.txt", "pytest\nfastapi>=0.110\n"),
            "requirements-dev.txt",
        ),
    ] {
        let root = uninstalled(label, manifest);
        assert_eq!(lines(&root), ["test log node --test"], "{label}");
        let r = report_with(&root, &node_and_a_python_without_packages()).await;
        assert_eq!(r.steps.len(), 1, "{label}: {r:?}");
        // The first step carries the line with the reason; the output of the command follows.
        assert!(
            r.steps[0].output_tail.starts_with(&format!(
                "PLAN NOTE: left out `python3 -m pytest tests/test_api.py -q`; packages in {named}\n# tests 3\n"
            )),
            "{label}: {r:?}"
        );
        let s = r.summary();
        assert_eq!(
            s.claim().as_deref(),
            Some("repository tests pass in a clean checkout: 3 of 3"),
            "{label}"
        );
        assert_eq!(
            s.details,
            [
                "`node --test`: exit 0, 3 passed, 0 failed.".to_string(),
                format!(
                    "`python3 -m pytest tests/test_api.py -q` of the lifecycle log was left out: it {ONE_PART}, and {named} names packages that the report does not install."
                ),
            ],
            "{label}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b844_a_file_whose_name_is_not_plain_is_not_named_in_the_line() {
    // The name of a requirements file is the repository's: one with a space is not written into the line agentd puts at the head of the output.
    let root = uninstalled(
        "b844r-f1-name",
        ("dev requirements.txt", "pytest\nfastapi>=0.110\n"),
    );
    assert_eq!(lines(&root), ["test log node --test"]);
    let r = report_with(&root, &node_and_a_python_without_packages()).await;
    assert!(
        r.steps[0].output_tail.starts_with(
            "PLAN NOTE: left out `python3 -m pytest tests/test_api.py -q`; packages\n"
        ),
        "{r:?}"
    );
    assert_eq!(
        r.summary().details[1],
        format!(
            "`python3 -m pytest tests/test_api.py -q` of the lifecycle log was left out: it {ONE_PART}, and a file of the repository names packages that the report does not install."
        )
    );
}

#[test]
fn b844_a_tree_whose_packages_the_report_installs_still_gets_the_python_step() {
    // requirements.txt is the file the report installs: the step runs in the report's own environment, as before.
    let root = tree(
        "b844r-f1-installed",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["node --test", "python3 -m pytest tests/test_api.py -q"]),
            ),
            ("package.json", "{\"scripts\": {\"test\": \"node --test\"}}"),
            ("requirements.txt", "pytest\nfastapi>=0.110\n"),
            ("tests/test_api.py", PY_TEST),
        ],
    );
    assert_eq!(
        lines(&root),
        [
            "test log node --test".to_string(),
            format!("setup detected {VENV_INSTALL}"),
            "test detected .ham-venv/bin/python -m pytest -q".to_string(),
        ]
    );
}

// ---- test files in folders below `tests/` or beside the code ---------------------------------------------------------------------------

const TEXTSTATS_TEST: &str = "from textstats import word_count\n\n\ndef test_word_count():\n    assert word_count('a b') == 2\n";

#[tokio::test(flavor = "multi_thread")]
async fn b844_a_test_file_in_a_folder_below_tests_still_gets_the_python_suite_a_step() {
    // The project's only Python test file sits in tests/unit, where the layout of the detection does not read. pytest collects it from the root.
    let root = tree(
        "b844r-f2-nested",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&[
                    "node --test",
                    "python3 -m pytest tests/unit/test_textstats.py -q",
                ]),
            ),
            ("package.json", "{\"scripts\": {\"test\": \"node --test\"}}"),
            (
                "textstats.py",
                "def word_count(text):\n    return len(text.split())\n",
            ),
            ("tests/unit/test_textstats.py", TEXTSTATS_TEST),
        ],
    );
    assert_eq!(
        lines(&root),
        ["test log node --test", "test detected python3 -m pytest -q"]
    );
    let r = report_with(
        &root,
        &bin(&[("node", NODE_THREE), ("python3", PYTHON_SIX)]),
    )
    .await;
    let s = r.summary();
    assert_eq!(
        s.claim().as_deref(),
        Some("repository tests pass in a clean checkout: 9 of 9")
    );
    assert_eq!(
        s.details,
        [
            "`node --test`: exit 0, 3 passed, 0 failed.".to_string(),
            format!(
                "`python3 -m pytest -q`: exit 0, 6 passed, 0 failed; it ran in place of `python3 -m pytest tests/unit/test_textstats.py -q` of the lifecycle log, which {ONE_PART}."
            ),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b844_a_python_project_whose_test_file_sits_beside_the_code_gets_a_report() {
    // No whole-suite row, no goal card and no layout the detection reads: the row names a file pytest collects from the root.
    let root = tree(
        "b844r-f2-beside",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["python3 -m pytest src/textstats/test_textstats.py -q"]),
            ),
            (
                "src/textstats/__init__.py",
                "def word_count(text):\n    return len(text.split())\n",
            ),
            ("src/textstats/test_textstats.py", PY_TEST),
        ],
    );
    assert_eq!(lines(&root), ["test detected python3 -m pytest -q"]);
    let r = report_with(&root, &bin(&[("python3", PYTHON_SIX)])).await;
    let s = r.summary();
    assert_eq!(
        s.claim().as_deref(),
        Some("repository tests pass in a clean checkout: 6 of 6")
    );
    assert_eq!(
        s.details,
        [format!(
            "`python3 -m pytest -q`: exit 0, 6 passed, 0 failed; it ran in place of `python3 -m pytest src/textstats/test_textstats.py -q` of the lifecycle log, which {ONE_PART}."
        )]
    );
}

#[test]
fn b844_the_step_for_test_files_below_tests_brings_its_install_and_keeps_the_refusal() {
    let nested = |label: &str, extra: (&str, &str)| {
        tree(
            label,
            &[
                (
                    ".ham/lifecycle/LOG.md",
                    &log(&["go test ./...", "pytest -k parse"]),
                ),
                ("go.mod", "module example.com/x\n"),
                ("test/integration/api/parse_test.py", PY_TEST),
                extra,
            ],
        )
    };
    // A name filter names no file: the files below test/ are what pytest collects. requirements.txt is installed first.
    assert_eq!(
        lines(&nested(
            "b844r-f2-venv",
            ("requirements.txt", "requests==2.32.0\n")
        )),
        [
            "test log go test ./...".to_string(),
            format!("setup detected {VENV_INSTALL}"),
            "test detected .ham-venv/bin/python -m pytest -q".to_string(),
        ]
    );
    // A file the report does not install names the packages: no step, as for a layout the detection reads.
    assert_eq!(
        lines(&nested(
            "b844r-f2-refused",
            ("Pipfile", "[packages]\nrequests = \"*\"\n")
        )),
        ["test log go test ./..."]
    );
}

#[test]
fn b844_a_tree_with_no_file_pytest_collects_gets_no_python_step() {
    // The row names a file whose name pytest does not collect, and the tree has no test folder: a run of that one file would be a part of
    // a suite, so nothing stands in its place.
    let root = tree(
        "b844r-f2-uncollected",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["go test ./...", "python3 -m pytest checks/verify.py -q"]),
            ),
            ("go.mod", "module example.com/x\n"),
            ("checks/verify.py", PY_TEST),
        ],
    );
    assert_eq!(lines(&root), ["test log go test ./..."]);
    // The same row alone: the tree still has no command, and the reason names the row.
    let alone = tree(
        "b844r-f2-uncollected-alone",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["python3 -m pytest checks/verify.py -q"]),
            ),
            ("checks/verify.py", PY_TEST),
        ],
    );
    let why = lines(&alone);
    assert!(
        why[0].starts_with("none: it looked for ")
            && why[0].ends_with(
                "`python3 -m pytest checks/verify.py -q` of the lifecycle log names a test file, a test or a filter and was left out"
            ),
        "{why:?}"
    );
    // A unittest row names its files itself: pytest from the root is not its suite.
    let unittest = tree(
        "b844r-f2-unittest",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["go test ./...", "python3 -m unittest tests/unit/test_a.py"]),
            ),
            ("go.mod", "module example.com/x\n"),
            ("tests/unit/test_a.py", PY_TEST),
        ],
    );
    assert_eq!(lines(&unittest), ["test log go test ./..."]);
}

// ---- the sentence of the card ---------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn b844_the_tests_line_says_what_the_plan_did_with_the_logged_commands() {
    let programs = bin(&[("node", NODE_THREE), ("python3", PYTHON_SIX)]);
    let log_of_two = log(&[
        "node --test",
        "python3 -m pytest tests/test_textstats.py -q",
    ]);
    // The report ran the whole Python suite in place of the logged command that names one file.
    let chosen = tree(
        "b844r-f3-chosen",
        &[
            (".ham/lifecycle/LOG.md", &log_of_two),
            ("package.json", "{\"scripts\": {\"test\": \"node --test\"}}"),
            ("tests/test_textstats.py", PY_TEST),
        ],
    );
    assert_eq!(
        report_with(&chosen, &programs).await.summary().sentence(),
        "repository tests pass in a clean checkout: 9 of 9. The report chose a whole-suite command in place of 1 command of the lifecycle log that names part of a suite; details are in EVIDENCE.md."
    );
    // The report left the logged command out: a file it does not install names the packages of the tree.
    let left_out = tree(
        "b844r-f3-left-out",
        &[
            (".ham/lifecycle/LOG.md", &log_of_two),
            ("package.json", "{\"scripts\": {\"test\": \"node --test\"}}"),
            ("Pipfile", "[packages]\nrequests = \"*\"\n"),
            ("tests/test_textstats.py", PY_TEST),
        ],
    );
    let s = report_with(&left_out, &programs).await.summary();
    assert_eq!(
        s.claim().as_deref(),
        Some("repository tests pass in a clean checkout: 3 of 3")
    );
    assert_eq!(
        s.sentence(),
        "repository tests pass in a clean checkout: 3 of 3. The report left out 1 command of the lifecycle log that names part of a suite; details are in EVIDENCE.md."
    );
}

// ---- a step stands in place of a logged command only when it runs the file that command names ------------------------------------------

const UNITTEST: &str =
    "import unittest\n\n\nclass T(unittest.TestCase):\n    def test_a(self):\n        pass\n";
/// A Python that prints what unittest prints for two tests that pass.
const UNITTEST_TWO: &str =
    "#!/bin/sh\necho '..'\necho '----'\necho 'Ran 2 tests in 0.002s'\necho ''\necho 'OK'\nexit 0\n";
const NOT_REACHED: &str =
    "the report found no command for the whole suite that runs the file it names";

#[tokio::test(flavor = "multi_thread")]
async fn b844_a_discovery_that_does_not_reach_the_logged_file_is_not_added_in_its_place() {
    // The discovery the layout gives starts in tests/. The logged file is in lib/, so the step would run other tests and say it ran in
    // place of the logged command.
    let root = tree(
        "b844r-f4-unittest",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["node --test", "python3 -m unittest lib/test_new.py"]),
            ),
            ("package.json", "{\"scripts\": {\"test\": \"node --test\"}}"),
            ("tests/test_old.py", UNITTEST),
            ("lib/test_new.py", UNITTEST),
        ],
    );
    assert_eq!(lines(&root), ["test log node --test"]);
    let s = report_with(
        &root,
        &bin(&[("node", NODE_THREE), ("python3", UNITTEST_TWO)]),
    )
    .await
    .summary();
    assert_eq!(
        s.claim().as_deref(),
        Some("repository tests pass in a clean checkout: 3 of 3")
    );
    assert_eq!(
        s.details,
        [
            "`node --test`: exit 0, 3 passed, 0 failed.".to_string(),
            format!(
                "`python3 -m unittest lib/test_new.py` of the lifecycle log was left out: it {ONE_PART}, and {NOT_REACHED}."
            ),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b844_a_discovery_stands_in_place_of_the_logged_files_it_reaches_only() {
    let rows = [
        "python3 -m unittest tests/test_old.py",
        "python3 -m unittest lib/test_new.py",
    ];
    let programs = bin(&[("node", NODE_THREE), ("python3", UNITTEST_TWO)]);
    let in_place = format!(
        "`python3 -m unittest discover -s tests -v`: exit 0, 2 passed, 0 failed; it ran in place of `python3 -m unittest tests/test_old.py` of the lifecycle log, which {ONE_PART}."
    );
    let left_out = format!(
        "`python3 -m unittest lib/test_new.py` of the lifecycle log was left out: it {ONE_PART}, and {NOT_REACHED}."
    );
    // Behind a whole-suite command of another language: the step is added for the row it reaches.
    let root = tree(
        "b844r-f4-added",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["node --test", rows[0], rows[1]]),
            ),
            ("package.json", "{\"scripts\": {\"test\": \"node --test\"}}"),
            ("tests/test_old.py", UNITTEST),
            ("lib/test_new.py", UNITTEST),
        ],
    );
    assert_eq!(
        lines(&root),
        [
            "test log node --test",
            "test detected python3 -m unittest discover -s tests -v"
        ]
    );
    assert_eq!(
        report_with(&root, &programs).await.summary().details,
        [
            "`node --test`: exit 0, 3 passed, 0 failed.".to_string(),
            left_out.clone(),
            in_place.clone(),
        ]
    );
    // A plan that is the detection holds the discovery already: the same two lines.
    let alone = tree(
        "b844r-f4-detected",
        &[
            (".ham/lifecycle/LOG.md", &log(&rows)),
            ("tests/test_old.py", UNITTEST),
            ("lib/test_new.py", UNITTEST),
        ],
    );
    assert_eq!(
        lines(&alone),
        ["test detected python3 -m unittest discover -s tests -v"]
    );
    assert_eq!(
        report_with(&alone, &programs).await.summary().details,
        [in_place, left_out]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b844_pytest_stands_in_place_of_the_logged_files_it_collects_only() {
    // The settings of the tree keep pytest to tests/: a logged file in another folder is not collected, and neither is a file that is gone.
    let root = tree(
        "b844r-f4-testpaths",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&[
                    "go test -v ./...",
                    "pytest tests/test_a.py",
                    "pytest extra/test_b.py",
                    "pytest tests/test_gone.py",
                ]),
            ),
            ("go.mod", "module example.com/x\n"),
            (
                "pytest.ini",
                "[pytest]\ntestpaths =\n    tests\n    integration\n",
            ),
            ("tests/test_a.py", PY_TEST),
            ("extra/test_b.py", PY_TEST),
        ],
    );
    assert_eq!(
        lines(&root),
        [
            "test log go test -v ./...",
            "test detected python3 -m pytest -q"
        ]
    );
    assert_eq!(
        report_with(&root, &bin(&[("go", GO_TWO), ("python3", PYTHON_SIX)]))
            .await
            .summary()
            .details,
        [
            "`go test -v ./...`: exit 0, 2 passed, 0 failed.",
            "`pytest tests/test_gone.py` and 1 more of the lifecycle log were left out: they name test files, tests or filters, and the report found no command for the whole suite that runs the files they name.",
            "`python3 -m pytest -q`: exit 0, 6 passed, 0 failed; it ran in place of `pytest tests/test_a.py` of the lifecycle log, which names a test file, a test or a filter.",
        ]
    );
    // Without the setting pytest collects both files that are there, and a name filter is a part of whatever it collects.
    let open = tree(
        "b844r-f4-open",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&[
                    "go test -v ./...",
                    "pytest tests/test_a.py",
                    "pytest extra/test_b.py",
                    "pytest -k parse",
                ]),
            ),
            ("go.mod", "module example.com/x\n"),
            ("tests/test_a.py", PY_TEST),
            ("extra/test_b.py", PY_TEST),
        ],
    );
    assert_eq!(
        report_with(&open, &bin(&[("go", GO_TWO), ("python3", PYTHON_SIX)]))
            .await
            .summary()
            .details,
        [
            "`go test -v ./...`: exit 0, 2 passed, 0 failed.",
            "`python3 -m pytest -q`: exit 0, 6 passed, 0 failed; it ran in place of `pytest -k parse` and 2 more of the lifecycle log, which name test files, tests or filters.",
        ]
    );
}

// ---- which languages a logged command covers --------------------------------------------------------------------------------------------

/// The commands of the lifecycle log that the plan left out with nothing in their place: the command named, the number of the others and
/// the reason, one entry for each reason.
fn left_out(root: &Path) -> Vec<(String, usize, LeftOutWhy)> {
    plan_with_notes(root)
        .1
        .left_out
        .iter()
        .map(|o| (o.rows.command.clone(), o.rows.more, o.why.clone()))
        .collect()
}

/// The places of the steps the report chose in place of logged commands.
fn stand_ins(root: &Path) -> Vec<usize> {
    plan_with_notes(root)
        .1
        .stand_ins
        .iter()
        .map(|(at, _)| *at)
        .collect()
}

#[test]
fn b844_a_logged_command_that_chains_two_suites_covers_both_languages() {
    let package =
        "{\"scripts\": {\"test\": \"jest\"}, \"devDependencies\": {\"jest\": \"29.7.0\"}}";
    // The Node suite ran in the second part of the chained command: a step of the report's own would run it a second time.
    let python_first = tree(
        "b844r-f9-python-first",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["python3 -m pytest -q && npm test", "npx jest src/a.test.ts"]),
            ),
            ("package.json", package),
            ("tests/test_a.py", PY_TEST),
        ],
    );
    assert_eq!(
        lines(&python_first),
        [
            "setup detected npm install",
            "test log python3 -m pytest -q && npm test"
        ]
    );
    assert_eq!(stand_ins(&python_first), Vec::<usize>::new());
    assert_eq!(left_out(&python_first), []);
    // The other way round: the Python suite is the second part.
    let node_first = tree(
        "b844r-f9-node-first",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["npm test && python3 -m pytest -q", "pytest tests/test_a.py"]),
            ),
            ("package.json", package),
            ("tests/test_a.py", PY_TEST),
        ],
    );
    assert_eq!(
        lines(&node_first),
        [
            "setup detected npm install",
            "test log npm test && python3 -m pytest -q"
        ]
    );
    assert_eq!(stand_ins(&node_first), Vec::<usize>::new());
    assert_eq!(left_out(&node_first), []);
}

#[test]
fn b844_a_runner_of_one_language_with_no_layout_leaves_the_other_languages_their_step() {
    // `dotnet test`, `rspec` and `phpunit` run the suite of one language. The Python tests of the log are a part of another suite: they
    // get the step of the layout, as beside `node --test`.
    for (label, whole) in [
        ("b844r-f10-dotnet", "dotnet test"),
        ("b844r-f10-rspec", "bundle exec rspec"),
        ("b844r-f10-phpunit", "phpunit"),
    ] {
        let root = tree(
            label,
            &[
                (
                    ".ham/lifecycle/LOG.md",
                    &log(&[whole, "python3 -m pytest tests/test_textstats.py -q"]),
                ),
                ("tests/test_textstats.py", PY_TEST),
            ],
        );
        assert_eq!(
            lines(&root),
            [
                format!("test log {whole}"),
                "test detected python3 -m pytest -q".to_string()
            ],
            "{label}"
        );
        assert_eq!(stand_ins(&root), [1], "{label}");
    }
    // A language with no layout the report knows: its row is listed as left out.
    let no_layout = tree(
        "b844r-f10-left-out",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["dotnet test", "npx jest src/a.test.ts"]),
            ),
            ("package.json", "{\"name\": \"x\"}"),
        ],
    );
    assert_eq!(lines(&no_layout), ["test log dotnet test"]);
    assert_eq!(
        left_out(&no_layout),
        [(
            "npx jest src/a.test.ts".to_string(),
            0,
            LeftOutWhy::NoSuiteCommand
        )]
    );
}

#[test]
fn b844_a_command_that_may_run_any_suite_gets_nothing_beside_it() {
    // `make test`, a script and tox may run the Node suite themselves: no second run of it, and nothing is said.
    for (label, whole) in [
        ("b844r-f10-make", "make test"),
        ("b844r-f10-script", "bash test.sh"),
        ("b844r-f10-tox", "tox"),
    ] {
        let root = tree(
            label,
            &[
                (
                    ".ham/lifecycle/LOG.md",
                    &log(&[whole, "npx jest src/a.test.ts"]),
                ),
                ("package.json", "{\"scripts\": {\"test\": \"jest\"}}"),
                ("test.sh", "#!/bin/sh\nnpm test\n"),
            ],
        );
        let plan = lines(&root);
        assert_eq!(plan.len(), 1, "{label}: {plan:?}");
        assert!(plan[0].ends_with(whole), "{label}: {plan:?}");
        assert_eq!(stand_ins(&root), Vec::<usize>::new(), "{label}");
        assert_eq!(left_out(&root), [], "{label}");
    }
}

// ---- rows that work in more than one project folder -------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn b844_python_rows_in_more_than_one_project_folder_get_no_step_at_the_root() {
    // The engine ran the tests of two Python projects, each alone. One pytest run from the root would import both test_models.py files as
    // one module and stop: a failure outside the run's work.
    let files = [
        ("package.json", "{\"scripts\": {\"test\": \"node --test\"}}"),
        ("conftest.py", ""),
        ("api/tests/test_models.py", PY_TEST),
        ("worker/tests/test_models.py", PY_TEST),
    ];
    let with_log = |label: &str, rows: &[&str]| {
        let text = log(rows);
        let mut all = vec![(".ham/lifecycle/LOG.md", text.as_str())];
        all.extend(files);
        tree(label, &all)
    };
    let two = with_log(
        "b844r-f13-two",
        &[
            "node --test",
            "python3 -m pytest api/tests/test_models.py -q",
            "cd worker && python3 -m pytest tests/test_models.py -q",
        ],
    );
    assert_eq!(lines(&two), ["test log node --test"]);
    let s = report_with(&two, &bin(&[("node", NODE_THREE), ("python3", PYTHON_SIX)]))
        .await
        .summary();
    assert_eq!(
        s.details,
        [
            "`node --test`: exit 0, 3 passed, 0 failed.",
            "`cd worker && python3 -m pytest tests/test_models.py -q` and 1 more of the lifecycle log were left out: they name test files, tests or filters, and the log runs these tests in more than one project folder, so the report added no command at the root.",
        ]
    );
    // Two rows of the root that name files of two projects are the same case.
    let named = with_log(
        "b844r-f13-named",
        &[
            "node --test",
            "pytest api/tests/test_models.py",
            "pytest worker/tests/test_models.py",
        ],
    );
    assert_eq!(lines(&named), ["test log node --test"]);
    // The rows of one project keep the step.
    let one = with_log(
        "b844r-f13-one",
        &[
            "node --test",
            "pytest api/tests/test_models.py",
            "pytest api/tests/test_models.py::test_a",
        ],
    );
    assert_eq!(
        lines(&one),
        ["test log node --test", "test detected python3 -m pytest -q"]
    );
}

// ---- a row the log recorded as failed ---------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn b844_a_left_out_command_that_the_log_recorded_as_failed_says_so() {
    // The engine's last run of the Jest command exited 1 with 2 failed. The report leaves the command out, and the line says what the log
    // recorded for it.
    let root = tree(
        "b844r-f11",
        &[
            (
                ".ham/lifecycle/LOG.md",
                "RUN | go test -v ./... | exit 0 | pass 2 fail 0 | ok\nRUN | npx jest src/a.test.ts | exit 0 | pass 5 fail 0 | ok\nRUN | npx jest src/a.test.ts | exit 1 | pass 3 fail 2 | 2 failed\n",
            ),
            ("go.mod", "module example.com/x\n"),
            ("package.json", "{\"name\": \"x\"}"),
        ],
    );
    assert_eq!(lines(&root), ["test log go test -v ./..."]);
    let r = report_with(&root, &bin(&[("go", GO_TWO)])).await;
    assert!(
        r.steps[0].output_tail.starts_with(
            "PLAN NOTE: left out `npx jest src/a.test.ts`; logged exit 1, 2 failed\n=== RUN"
        ),
        "{r:?}"
    );
    assert_eq!(
        r.summary().details,
        [
            "`go test -v ./...`: exit 0, 2 passed, 0 failed.".to_string(),
            format!(
                "`npx jest src/a.test.ts` of the lifecycle log was left out: it {ONE_PART}, and the report found no command for the whole suite; the log recorded exit 1, 2 failed for it."
            ),
        ]
    );
    // A row whose last run passed says nothing more, as before.
    let passed = tree(
        "b844r-f11-passed",
        &[
            (
                ".ham/lifecycle/LOG.md",
                "RUN | go test -v ./... | exit 0 | pass 2 fail 0 | ok\nRUN | npx jest src/a.test.ts | exit 1 | pass 3 fail 2 | 2 failed\nRUN | npx jest src/a.test.ts | exit 0 | pass 5 fail 0 | ok\n",
            ),
            ("go.mod", "module example.com/x\n"),
            ("package.json", "{\"name\": \"x\"}"),
        ],
    );
    let r = report_with(&passed, &bin(&[("go", GO_TWO)])).await;
    assert!(
        r.steps[0]
            .output_tail
            .starts_with("PLAN NOTE: left out `npx jest src/a.test.ts`\n=== RUN"),
        "{r:?}"
    );
}

// ---- a cut of the output and the place where agentd writes ----------------------------------------------------------------------------

#[test]
fn b844_a_cut_of_the_output_brings_no_line_of_the_repository_to_the_place_agentd_writes_at() {
    // The repository prints a line of the form agentd writes as the second line of its output. The step keeps as much output as its bound
    // allows, so the output gives way from its front to agentd's own line: for one length of the first line the cut ends right in front
    // of the repository's line, which then stood where the summary reads.
    let own = in_place_line("python3 -m pytest tests/test_a.py -q", 0);
    let forged = "PLAN NOTE: left out `the integration suite, checked by hand`";
    for first in 40..90 {
        let output = format!(
            "{}\n{forged}\n{}\n6 passed in 0.01s",
            "a".repeat(first),
            "x".repeat(300)
        );
        let cap = output.len();
        let mut step = TestStep {
            role: TestStepRole::Test,
            source: TestCommandSource::Detected,
            command: "python3 -m pytest -q".into(),
            exit_code: Some(0),
            timed_out: false,
            duration_ms: 10,
            limit_s: Some(30),
            counts: None,
            counts_from: None,
            output_tail: output,
            output_truncated: false,
            logged: None,
        };
        set_plan_lines(&mut step, std::slice::from_ref(&own), cap);
        assert_eq!(
            step.in_place_of(),
            Some(("python3 -m pytest tests/test_a.py -q", 0)),
            "{first}"
        );
        assert_eq!(
            step.left_out(),
            None,
            "a first line of {first} characters: {:?}",
            step.output_tail
        );
        assert!(step.output_tail.len() <= cap, "{first}");
        assert!(step.output_tail.ends_with("6 passed in 0.01s"), "{first}");
    }
}

// ---- a step the report added that runs out of time --------------------------------------------------------------------------------------

/// A Python that never ends on its own: the report stops it at the limit of the command.
const PYTHON_SLEEPS: &str = "#!/bin/sh\nsleep 30\n";

#[tokio::test(flavor = "multi_thread")]
async fn b844_a_step_the_report_added_that_is_stopped_at_its_limit_leaves_the_result_of_the_models_commands()
 {
    // The Go suite is the model's own command and passes. The Python step is the report's addition; it is stopped after 2 s.
    let root = tree(
        "b844r-f5-added",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["go test -v ./...", "python3 -m pytest tests/test_a.py -q"]),
            ),
            ("go.mod", "module example.com/x\n"),
            ("tests/test_a.py", PY_TEST),
        ],
    );
    assert_eq!(
        lines(&root),
        [
            "test log go test -v ./...",
            "test detected python3 -m pytest -q"
        ]
    );
    let r = report_of(
        &root,
        &bin(&[("go", GO_TWO), ("python3", PYTHON_SLEEPS)]),
        2,
        4096,
    )
    .await;
    // The report did not time out: the commands of the model ran, and the stopped command is listed as not run.
    assert_eq!(r.status, TestReportStatus::Ran, "{r:?}");
    assert_eq!(r.reason, None);
    assert_eq!(r.steps[0].exit_code, Some(0), "{r:?}");
    assert!(r.steps[1].is_not_run() && !r.steps[1].timed_out, "{r:?}");
    assert!(
        r.steps[1].output_tail.starts_with(
            "NOT RUN: stopped at its time limit of 2 s\nPLAN NOTE: in place of `python3 -m pytest tests/test_a.py -q`"
        ),
        "{r:?}"
    );
    let s = r.summary();
    assert_eq!(s.timed_out, None);
    assert_eq!(s.claim(), None);
    assert_eq!(
        s.sentence(),
        "The test commands that ran exited 0 in a clean checkout; the runner reported 2 passed, 0 failed. `python3 -m pytest -q` was stopped at its time limit of 2 s. The report chose a whole-suite command in place of 1 command of the lifecycle log that names part of a suite; details are in EVIDENCE.md."
    );
    assert_eq!(
        s.details,
        [
            "`go test -v ./...`: exit 0, 2 passed, 0 failed.".to_string(),
            format!(
                "`python3 -m pytest -q`: not run (stopped at its time limit of 2 s); it stands in place of `python3 -m pytest tests/test_a.py -q` of the lifecycle log, which {ONE_PART}."
            ),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b844_a_report_whose_only_command_is_stopped_still_reads_as_timed_out() {
    // No command of the model's ran before it: the plan is the layout's own command, and its limit is the report's.
    let root = tree(
        "b844r-f5-alone",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["python3 -m pytest tests/test_a.py -q"]),
            ),
            ("tests/test_a.py", PY_TEST),
        ],
    );
    assert_eq!(lines(&root), ["test detected python3 -m pytest -q"]);
    let r = report_of(&root, &bin(&[("python3", PYTHON_SLEEPS)]), 2, 4096).await;
    assert_eq!(r.status, TestReportStatus::TimedOut, "{r:?}");
    assert!(r.steps[0].timed_out, "{r:?}");
    assert!(
        r.summary()
            .sentence()
            .starts_with("`python3 -m pytest -q` timed out after 2 s;"),
        "{}",
        r.summary().sentence()
    );
}

// ---- two installs that failed -----------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn b844_two_installs_that_fail_are_each_named_with_the_command_they_stopped() {
    // The Node packages cannot be installed and the report's environment cannot be made: each install stops its own command.
    let root = tree(
        "b844r-f7",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&[
                    "node --test",
                    "python3 -m pytest tests/test_textstats.py -q",
                ]),
            ),
            (
                "package.json",
                "{\"scripts\": {\"test\": \"node --test\"}, \"dependencies\": {\"a-package-in-no-registry\": \"1.0.0\"}}",
            ),
            ("requirements.txt", "requests==2.32.0\n"),
            ("tests/test_textstats.py", PY_TEST),
        ],
    );
    let programs = bin(&[
        ("npm", "#!/bin/sh\necho 'npm error code E404' >&2\nexit 1\n"),
        ("node", NODE_THREE),
        (
            "python3",
            "#!/bin/sh\necho 'Error: ensurepip is not available' >&2\nexit 9\n",
        ),
    ]);
    let s = report_with(&root, &programs).await.summary();
    assert_eq!(
        s.sentence(),
        format!(
            "Setup `npm install` exited 1 in a clean checkout, so `node --test` did not run; setup `{VENV_INSTALL}` failed too, so `.ham-venv/bin/python -m pytest -q` did not run. The report chose a whole-suite command in place of 1 command of the lifecycle log that names part of a suite; details are in EVIDENCE.md."
        )
    );
}

// ---- the note of a Cargo workspace ------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn b844_a_cargo_test_that_stands_in_place_of_a_logged_command_says_why_it_was_chosen() {
    // The log ran the Node suite whole and one test target of a workspace of two members.
    let root = tree(
        "b844r-f8",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["node --test", "cargo test --test smoke"]),
            ),
            ("package.json", "{\"scripts\": {\"test\": \"node --test\"}}"),
            ("Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\n"),
            ("a/Cargo.toml", "[package]\nname = \"a\"\n"),
            ("b/Cargo.toml", "[package]\nname = \"b\"\n"),
        ],
    );
    assert_eq!(
        lines(&root),
        ["test log node --test", "test detected cargo test"]
    );
    let cargo = "#!/bin/sh\necho 'running 3 tests'\necho 'test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s'\nexit 0\n";
    let s = report_with(&root, &bin(&[("node", NODE_THREE), ("cargo", cargo)]))
        .await
        .summary();
    assert_eq!(
        s.plan_note(),
        Some(
            "The lifecycle log names only a part of the Rust tests, so the harness chose `cargo test` from the manifest (Cargo.toml). It runs the tests of the whole workspace, 2 members. A `.ham/test-command` file in the repository names a narrower command."
        )
    );
    assert_eq!(
        s.details,
        [
            "`node --test`: exit 0, 3 passed, 0 failed.".to_string(),
            format!(
                "`cargo test`: exit 0, 3 passed, 0 failed; it ran in place of `cargo test --test smoke` of the lifecycle log, which {ONE_PART}."
            ),
        ]
    );
}

// ---- which step runs the logged file ----------------------------------------------------------------------------------------------------

#[test]
fn b844_the_step_that_runs_the_logged_file_is_the_one_that_stands_in_its_place() {
    // Two discoveries: one at the root, one in tests/, which is no package. The logged file is in tests/.
    let root = tree(
        "b844r-so-discoveries",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["python3 -m unittest tests/test_b.py"]),
            ),
            ("test_root.py", UNITTEST),
            ("tests/test_b.py", UNITTEST),
        ],
    );
    assert_eq!(
        lines(&root),
        [
            "test detected python3 -m unittest discover -v",
            "test detected python3 -m unittest discover -s tests -v",
        ]
    );
    assert_eq!(stand_ins(&root), [1]);
    assert_eq!(left_out(&root), []);
}

#[test]
fn b844_pytest_reads_one_settings_file_and_a_file_that_testpaths_names() {
    // pytest reads pytest.ini and no other settings file: the folder tox.ini names is not collected.
    let two = tree(
        "b844r-so-settings",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&[
                    "go test -v ./...",
                    "pytest other/test_x.py",
                    "pytest tests/test_a.py",
                ]),
            ),
            ("go.mod", "module example.com/x\n"),
            ("pytest.ini", "[pytest]\ntestpaths = tests\n"),
            ("tox.ini", "[pytest]\ntestpaths = other\n"),
            ("tests/test_a.py", PY_TEST),
            ("other/test_x.py", PY_TEST),
        ],
    );
    assert_eq!(stand_ins(&two), [1]);
    assert_eq!(
        left_out(&two),
        [(
            "pytest other/test_x.py".to_string(),
            0,
            LeftOutWhy::NotReached
        )]
    );
    // A file that the setting names itself is collected.
    let named = tree(
        "b844r-so-named",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &log(&["go test -v ./...", "pytest tests/test_one.py"]),
            ),
            ("go.mod", "module example.com/x\n"),
            ("pytest.ini", "[pytest]\ntestpaths = tests/test_one.py\n"),
            ("tests/test_one.py", PY_TEST),
        ],
    );
    assert_eq!(stand_ins(&named), [1]);
    assert_eq!(left_out(&named), []);
}
