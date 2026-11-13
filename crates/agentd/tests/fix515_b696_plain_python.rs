//! A plain Python layout comes with the install step its tree needs, or its tests are not run.
//! The report finds the test files of a tree with no pytest settings since 5.15. A tree whose `requirements.txt` does not name pytest then got
//! a test step with no install step, so the clean checkout ran the tests without the project's packages, where 5.14 made no report for the same
//! tree. And unittest files at the root and in `tests/` were discovered in the folder only. Now:
//!
//! - a `requirements.txt` is installed into the report's own virtual environment, and the tests run in it;
//! - a tree whose packages are named by a file the report has no install step for is not run, and the reason says so in the card's one sentence;
//! - the test command is one that finds every test file the plan found.
//!
//! Nothing here starts a process; `b825_install_step.rs` runs the steps of such a tree with a stand-in `python3`.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::report::plan::{Plan, PlannedStep, plan};
use harness_protocol::direct::{
    TestCommandSource, TestReport, TestReportStatus, TestStepRole, banned_claim_word,
};
use std::path::{Path, PathBuf};

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

const APP: &str = "from flask import Flask\n\napp = Flask(__name__)\n";
const PYTEST_FILE: &str = "from app import app\n\n\ndef test_index():\n    assert app.test_client().get('/').status_code == 404\n";
const UNITTEST_FILE: &str = "import unittest\n\nfrom app import app\n\n\nclass IndexTest(unittest.TestCase):\n    def test_index(self):\n        self.assertEqual(app.test_client().get('/').status_code, 404)\n";
const PLAIN_UNITTEST: &str = "import unittest\n\n\nclass T(unittest.TestCase):\n    def test_x(self):\n        self.assertTrue(True)\n";

#[test]
fn fix515_b696_a_plain_python_tree_with_requirements_installs_them_before_its_tests() {
    let pytest = tree(
        "fix515-req-pytest",
        &[
            ("requirements.txt", "flask==3.0.3\n"),
            ("app.py", APP),
            ("tests/test_app.py", PYTEST_FILE),
        ],
    );
    assert_eq!(
        lines(&pytest),
        [
            format!("setup detected {VENV_INSTALL}"),
            "test detected .ham-venv/bin/python -m pytest -q".to_string()
        ]
    );
    let unittest = tree(
        "fix515-req-unittest",
        &[
            ("requirements.txt", "flask==3.0.3\n"),
            ("app.py", APP),
            ("tests/test_app.py", UNITTEST_FILE),
        ],
    );
    assert_eq!(
        lines(&unittest),
        [
            format!("setup detected {VENV_INSTALL}"),
            "test detected .ham-venv/bin/python -m unittest discover -s tests -v".to_string()
        ]
    );
}

fn no_command(root: &Path) -> String {
    match plan(root) {
        Plan::NoCommand(why) => why,
        Plan::Steps(s) => panic!("a plan: {s:?}"),
    }
}

#[test]
fn fix515_b696_a_plain_python_tree_whose_packages_the_report_does_not_install_is_not_run_and_says_why()
 {
    for (manifest, text) in [
        (
            "pyproject.toml",
            "[project]\nname = \"app\"\ndependencies = [\n    \"flask>=3\", # the web framework\n]\n",
        ),
        ("pyproject.toml", "project.dependencies = [\"flask\"]\n"),
        (
            "pyproject.toml",
            "[tool.poetry.dependencies]\npython = \"^3.12\"\nflask = \"^3.0\"\n",
        ),
        ("Pipfile", "[packages]\nflask = \"*\"\n"),
        (
            "setup.py",
            "from setuptools import setup\n\nsetup(name=\"app\", install_requires=[\"flask\"])\n",
        ),
        ("setup.cfg", "[options]\ninstall_requires =\n    flask\n"),
    ] {
        let root = tree(
            "fix515-uninstalled",
            &[
                (manifest, text),
                ("app.py", APP),
                ("tests/test_app.py", PYTEST_FILE),
            ],
        );
        let why = no_command(&root);
        assert_eq!(
            why,
            format!(
                "it found Python test files (tests/test_app.py), and {manifest} names packages that the report does not install (it installs only the packages of requirements.txt); a .ham/test-command file in the repository names the install and test commands"
            ),
            "{manifest}: {text}"
        );
        // The card's sentence says it in one sentence.
        let report = TestReport::without_steps(
            "R1",
            &"a".repeat(40),
            TestReportStatus::NoCommand,
            Some(why.clone()),
        );
        report.validate("R1", &"a".repeat(40)).unwrap();
        let sentence = report.summary().sentence();
        assert_eq!(
            sentence,
            format!("The clean-checkout test report did not run the tests: {why}.")
        );
        assert_eq!(banned_claim_word(&sentence), None);
    }
}

#[test]
fn fix515_b696_a_settings_file_that_names_no_package_leaves_the_plain_layout_as_it_was() {
    for (manifest, text) in [
        ("pyproject.toml", "[tool.ruff]\nline-length = 100\n"),
        (
            "pyproject.toml",
            "[project]\nname = \"app\"\ndependencies = []\n",
        ),
        (
            "pyproject.toml",
            "[tool.poetry.dependencies]\npython = \"^3.12\"\n",
        ),
        ("Pipfile", "[dev-packages]\n\n[packages]\n"),
        ("setup.cfg", "[flake8]\nmax-line-length = 100\n"),
    ] {
        let root = tree(
            "fix515-settings-only",
            &[
                (manifest, text),
                ("tests/test_calc.py", "def test_a():\n    pass\n"),
            ],
        );
        assert_eq!(
            lines(&root),
            ["test detected python3 -m pytest -q"],
            "{manifest}: {text}"
        );
    }
    // A setup file that names no package: an empty list, the word in a comment, an empty value.
    for (manifest, text) in [
        (
            "setup.py",
            "from setuptools import setup\n\nsetup(name=\"app\", install_requires=[])\n",
        ),
        (
            "setup.py",
            "from setuptools import setup\n\n# no install_requires: the standard library is enough\nsetup(name=\"app\")\n",
        ),
        (
            "setup.py",
            "\"\"\"The packages would go in install_requires. There are none.\"\"\"\nfrom setuptools import setup\n\nsetup(name=\"app\")\n",
        ),
        (
            "setup.cfg",
            "[options]\ninstall_requires =\n\n[flake8]\nmax-line-length = 100\n",
        ),
        (
            "setup.cfg",
            "[options]\ninstall_requires =\n    # none yet\npackages = find:\n",
        ),
    ] {
        let root = tree(
            "fix515-setup-names-none",
            &[
                (manifest, text),
                ("tests/test_calc.py", "def test_a():\n    pass\n"),
            ],
        );
        assert_eq!(
            lines(&root),
            ["test detected python3 -m pytest -q"],
            "{manifest}: {text}"
        );
    }
    // A setup file whose packages are named where the reading does not follow (a variable), or on the line of the key: not run.
    for (manifest, text) in [
        (
            "setup.py",
            "from setuptools import setup\n\nREQUIRED = open(\"deps.txt\").read().split()\nsetup(name=\"app\", install_requires=REQUIRED)\n",
        ),
        ("setup.cfg", "[options]\ninstall_requires = flask\n"),
    ] {
        let root = tree(
            "fix515-setup-names-some",
            &[
                (manifest, text),
                ("tests/test_calc.py", "def test_a():\n    pass\n"),
            ],
        );
        assert!(
            no_command(&root).contains(&format!("{manifest} names packages")),
            "{manifest}: {text}"
        );
    }
    // An array of arrays before the packages (a line that starts with a bracket is no table header while an array is open).
    let nested = tree(
        "fix515-nested-array",
        &[
            (
                "pyproject.toml",
                "[project]\nmatrix = [\n    [\"a\", \"b\"],\n]\ndependencies = [\"flask\"]\n",
            ),
            ("tests/test_calc.py", "def test_a():\n    pass\n"),
        ],
    );
    assert!(no_command(&nested).contains("pyproject.toml names packages"));
    // An array of another key that never closes swallows the rest of the file: nothing after it is read, and the tests run.
    let open_other = tree(
        "fix515-open-other-array",
        &[
            ("pyproject.toml", "[project]\nkeywords = [\n    \"calc\n"),
            ("tests/test_calc.py", "def test_a():\n    pass\n"),
        ],
    );
    assert_eq!(lines(&open_other), ["test detected python3 -m pytest -q"]);
    // A pyproject.toml whose array never closes is not one the report is sure of: the tests are not run.
    let open = tree(
        "fix515-open-array",
        &[
            (
                "pyproject.toml",
                "[project]\ndependencies = [\n    \"flask\n",
            ),
            ("tests/test_calc.py", "def test_a():\n    pass\n"),
        ],
    );
    assert!(
        no_command(&open)
            .contains("pyproject.toml names packages that the report does not install")
    );
    // Several test files: the reason names the first and counts the others.
    let several = tree(
        "fix515-several",
        &[
            ("Pipfile", "[packages]\nflask = \"*\"\n"),
            ("test_a.py", "def test_a():\n    pass\n"),
            ("tests/test_b.py", "def test_b():\n    pass\n"),
            ("tests/test_c.py", "def test_c():\n    pass\n"),
        ],
    );
    assert!(
        no_command(&several).starts_with(
            "it found Python test files (test_a.py and 2 more), and Pipfile names packages"
        ),
        "{}",
        no_command(&several)
    );
    // No test file: nothing was found, so the reason is the list of what the report looked for.
    let no_tests = tree(
        "fix515-no-tests",
        &[
            ("pyproject.toml", "[project]\ndependencies = [\"flask\"]\n"),
            ("app.py", APP),
        ],
    );
    assert!(
        no_command(&no_tests).starts_with(harness_protocol::direct::NO_COMMAND_LOOKED_FOR),
        "{}",
        no_command(&no_tests)
    );
    // With a requirements.txt beside it the report has its install step, and the tests run.
    let both = tree(
        "fix515-both",
        &[
            ("pyproject.toml", "[project]\ndependencies = [\"flask\"]\n"),
            ("requirements.txt", "flask==3.0.3\n"),
            ("tests/test_calc.py", "def test_a():\n    pass\n"),
        ],
    );
    assert_eq!(lines(&both).len(), 2);
    // Another ecosystem of the tree still gets its steps, and the Python files stay out of the plan.
    let mixed = tree(
        "fix515-mixed",
        &[
            ("pyproject.toml", "[project]\ndependencies = [\"flask\"]\n"),
            ("tests/test_calc.py", "def test_a():\n    pass\n"),
            ("go.mod", "module x\n"),
        ],
    );
    assert_eq!(lines(&mixed), ["test detected go test -v ./..."]);
}

#[test]
fn fix515_b696_unittest_files_in_more_than_one_place_are_all_run() {
    // The root and a folder that is no package: one unittest discovery starts in one of them, so each gets its own, as a step of its own.
    // The files stay with the runner they were written for: pytest would import two files of one name as one module and stop.
    let split = tree(
        "fix515-split",
        &[
            ("calc.py", "def add(a, b):\n    return a + b\n"),
            ("test_common.py", PLAIN_UNITTEST),
            ("tests/test_common.py", PLAIN_UNITTEST),
        ],
    );
    assert_eq!(
        lines(&split),
        [
            "test detected python3 -m unittest discover -v",
            "test detected python3 -m unittest discover -s tests -v"
        ]
    );
    // Two folders that are no packages.
    let two = tree(
        "fix515-two-folders",
        &[
            ("tests/test_a.py", PLAIN_UNITTEST),
            ("test/test_b.py", PLAIN_UNITTEST),
        ],
    );
    assert_eq!(
        lines(&two),
        [
            "test detected python3 -m unittest discover -s tests -v",
            "test detected python3 -m unittest discover -s test -v"
        ]
    );
    // With a requirements.txt every discovery runs in the environment that holds its packages.
    let split_req = tree(
        "fix515-split-req",
        &[
            ("requirements.txt", "flask==3.0.3\n"),
            ("test_root.py", PLAIN_UNITTEST),
            ("tests/test_more.py", PLAIN_UNITTEST),
        ],
    );
    assert_eq!(
        lines(&split_req),
        [
            format!("setup detected {VENV_INSTALL}"),
            "test detected .ham-venv/bin/python -m unittest discover -v".to_string(),
            "test detected .ham-venv/bin/python -m unittest discover -s tests -v".to_string(),
        ]
    );
    // A file the default pattern of unittest discovery (`test*.py`) does not find.
    let named = tree(
        "fix515-suffix-name",
        &[("tests/calc_test.py", PLAIN_UNITTEST)],
    );
    assert_eq!(lines(&named), ["test detected python3 -m pytest -q"]);
    // The root and a folder that is a package: one discovery from the root finds both.
    let package = tree(
        "fix515-root-and-package",
        &[
            ("test_root.py", PLAIN_UNITTEST),
            ("tests/__init__.py", ""),
            ("tests/test_more.py", PLAIN_UNITTEST),
        ],
    );
    assert_eq!(
        lines(&package),
        ["test detected python3 -m unittest discover -v"]
    );
    // Files at the root only.
    let root_only = tree("fix515-root-only", &[("test_root.py", PLAIN_UNITTEST)]);
    assert_eq!(
        lines(&root_only),
        ["test detected python3 -m unittest discover -v"]
    );
}

// ---- other files that name the packages of a plain Python tree (re-review of the 5.15 integration head, finding N1) --------------------------

/// A plain tree (`app.py` imports flask, `tests/test_app.py`, no pytest settings and no `requirements.txt`) whose packages `file` names: the
/// tests are found and not run, and the reason and the card's one sentence name the file.
fn fix515b_assert_not_run(label: &str, file: &str, text: &str) {
    let root = tree(
        label,
        &[
            (file, text),
            ("app.py", APP),
            ("tests/test_app.py", PYTEST_FILE),
        ],
    );
    let why = no_command(&root);
    assert_eq!(
        why,
        format!(
            "it found Python test files (tests/test_app.py), and {file} names packages that the report does not install (it installs only the packages of requirements.txt); a .ham/test-command file in the repository names the install and test commands"
        ),
        "{file}: {text}"
    );
    let report = TestReport::without_steps(
        "R1",
        &"a".repeat(40),
        TestReportStatus::NoCommand,
        Some(why.clone()),
    );
    report.validate("R1", &"a".repeat(40)).unwrap();
    let sentence = report.summary().sentence();
    assert_eq!(
        sentence,
        format!("The clean-checkout test report did not run the tests: {why}.")
    );
    assert_eq!(banned_claim_word(&sentence), None);
}

#[test]
fn fix515b_requirements_dev_txt_names_packages_the_report_does_not_install() {
    fix515b_assert_not_run("fix515b-req-dev", "requirements-dev.txt", "flask==3.0.3\n");
}

#[test]
fn fix515b_dev_requirements_txt_names_packages_the_report_does_not_install() {
    fix515b_assert_not_run(
        "fix515b-dev-req",
        "dev-requirements.txt",
        "pytest\nflask==3.0.3\n",
    );
}

#[test]
fn fix515b_test_requirements_txt_names_packages_the_report_does_not_install() {
    fix515b_assert_not_run(
        "fix515b-test-req",
        "test-requirements.txt",
        "pytest\nflask==3.0.3\n",
    );
}

#[test]
fn fix515b_requirements_test_txt_names_packages_the_report_does_not_install() {
    fix515b_assert_not_run(
        "fix515b-req-test",
        "requirements-test.txt",
        "# what the tests need\npytest\nflask==3.0.3\n",
    );
}

#[test]
fn fix515b_a_file_in_the_requirements_folder_names_packages_the_report_does_not_install() {
    fix515b_assert_not_run(
        "fix515b-folder-dev",
        "requirements/dev.txt",
        "pytest\nflask==3.0.3\n",
    );
    fix515b_assert_not_run(
        "fix515b-folder-test",
        "requirements/test.txt",
        "pytest\nflask==3.0.3\n",
    );
    // Several files in the folder: the reason names the first by name that names a package. A line that points at another file counts.
    let several = tree(
        "fix515b-folder-several",
        &[
            ("requirements/base.txt", "# the packages of the app\n"),
            ("requirements/dev.txt", "-r base.txt\n"),
            ("requirements/prod.txt", "flask==3.0.3\n"),
            ("app.py", APP),
            ("tests/test_app.py", PYTEST_FILE),
        ],
    );
    assert!(
        no_command(&several)
            .contains(", and requirements/dev.txt names packages that the report does not install"),
        "{}",
        no_command(&several)
    );
}

#[test]
fn fix515b_pipfile_dev_packages_name_packages_the_report_does_not_install() {
    fix515b_assert_not_run(
        "fix515b-pipfile-dev",
        "Pipfile",
        "[packages]\n\n[dev-packages]\npytest = \"*\"\nflask = \"*\"\n",
    );
}

// The groups of a `pyproject.toml`. None of these trees names a package under `[project]` `dependencies` or `[tool.poetry.dependencies]`.

#[test]
fn fix515b_pyproject_optional_dependencies_name_packages_the_report_does_not_install() {
    for text in [
        "[project]\nname = \"app\"\n\n[project.optional-dependencies]\ndev = [\n    \"flask>=3\", # the web framework\n]\n",
        "[project]\nname = \"app\"\noptional-dependencies = { dev = [\"flask\"] }\n",
    ] {
        fix515b_assert_not_run("fix515b-pyproject-extras", "pyproject.toml", text);
    }
}

#[test]
fn fix515b_pyproject_dependency_groups_name_packages_the_report_does_not_install() {
    fix515b_assert_not_run(
        "fix515b-pyproject-groups",
        "pyproject.toml",
        "[project]\nname = \"app\"\ndependencies = []\n\n[dependency-groups]\ntest = [\"flask\"]\n",
    );
}

#[test]
fn fix515b_pyproject_poetry_groups_name_packages_the_report_does_not_install() {
    fix515b_assert_not_run(
        "fix515b-pyproject-poetry",
        "pyproject.toml",
        "[tool.poetry.dependencies]\npython = \"^3.12\"\n\n[tool.poetry.group.dev.dependencies]\nflask = \"^3.0\"\n",
    );
}

/// With a `requirements.txt` beside any of these files the report has its install step, and the tests run in its environment.
#[test]
fn fix515b_a_requirements_txt_beside_these_files_is_installed_and_the_tests_run() {
    for (file, text) in [
        ("requirements-dev.txt", "httpx\n"),
        ("dev-requirements.txt", "httpx\n"),
        ("test-requirements.txt", "httpx\n"),
        ("requirements-test.txt", "httpx\n"),
        ("requirements/dev.txt", "httpx\n"),
        ("Pipfile", "[dev-packages]\nhttpx = \"*\"\n"),
        ("pyproject.toml", "[dependency-groups]\ndev = [\"httpx\"]\n"),
    ] {
        let root = tree(
            "fix515b-beside",
            &[
                ("requirements.txt", "flask==3.0.3\n"),
                (file, text),
                ("app.py", APP),
                ("tests/test_app.py", PYTEST_FILE),
            ],
        );
        assert_eq!(
            lines(&root),
            [
                format!("setup detected {VENV_INSTALL}"),
                "test detected .ham-venv/bin/python -m pytest -q".to_string()
            ],
            "{file}: {text}"
        );
    }
}

/// A file of these names that names no package, and a file that is none of them, leave the plain layout as it was.
#[test]
fn fix515b_a_file_that_names_no_package_leaves_the_plain_layout_as_it_was() {
    for (file, text) in [
        ("requirements-dev.txt", "# none yet\n\n"),
        ("requirements/dev.txt", ""),
        (
            "Pipfile",
            "[dev-packages]\n\n[packages]\n\n[requires]\npython_version = \"3.12\"\n",
        ),
        (
            "pyproject.toml",
            "[project]\nname = \"app\"\n\n[project.optional-dependencies]\ndev = []\n",
        ),
        ("pyproject.toml", "[dependency-groups]\ndev = []\n"),
        (
            "pyproject.toml",
            "[tool.poetry.group.dev]\noptional = true\n\n[tool.poetry.group.dev.dependencies]\n",
        ),
        // A table deeper than the dependencies of a Poetry group is none of Poetry's.
        (
            "pyproject.toml",
            "[tool.poetry.group.dev.extra.dependencies]\nflask = \"^3.0\"\n",
        ),
        // A requirements file in another folder, and a file that is no requirements file.
        ("notes/requirements-dev.txt", "flask\n"),
        ("requirements-dev.md", "flask\n"),
        ("requirements/README.md", "flask\n"),
    ] {
        let root = tree(
            "fix515b-names-none",
            &[
                (file, text),
                ("tests/test_calc.py", "def test_a():\n    pass\n"),
            ],
        );
        assert_eq!(
            lines(&root),
            ["test detected python3 -m pytest -q"],
            "{file}: {text}"
        );
    }
}
