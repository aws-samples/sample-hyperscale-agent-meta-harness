//! The clean-checkout report finds the test command of a plain Python or Go layout with no configuration file, and when it finds none it
//! says what it looked for. A fresh-setup run in an empty repository delivered a pytest suite of two files (23 tests passed on a build host), and
//! the card said "No test command was found, so no clean-checkout test report was made": the repository had no `.ham/test-command`, no pytest
//! settings and no `requirements.txt`, and the plan knew no other Python layout.

#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::report::plan::{Plan, plan};
use harness_protocol::direct::{TestCommandSource, TestStepRole};
use std::path::Path;

fn tree(label: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let root = common::scratch(label);
    for (rel, text) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    root
}

fn commands(root: &Path) -> Vec<String> {
    match plan(root) {
        Plan::Steps(s) => {
            assert!(
                s.iter().all(|x| x.source == TestCommandSource::Detected),
                "{s:?}"
            );
            s.iter()
                .filter(|x| x.role == TestStepRole::Test)
                .map(|x| x.command.clone())
                .collect()
        }
        Plan::NoCommand(why) => panic!("no steps: {why}"),
    }
}

const PYTEST_FILE: &str = "from calc import add\n\n\ndef test_add():\n    assert add(1, 2) == 3\n";
const UNITTEST_FILE: &str = "import unittest\n\nfrom calc import add\n\n\nclass AddTest(unittest.TestCase):\n    def test_add(self):\n        self.assertEqual(add(1, 2), 3)\n";

#[test]
fn b696_a_plain_pytest_layout_in_tests_is_found() {
    let root = tree(
        "b696-py",
        &[
            ("calc.py", "def add(a, b):\n    return a + b\n"),
            ("tests/test_calc.py", PYTEST_FILE),
        ],
    );
    assert_eq!(commands(&root), ["python3 -m pytest -q"]);
}

#[test]
fn b696_a_plain_pytest_file_at_the_root_is_found() {
    let root = tree(
        "b696-root",
        &[("calc.py", "x = 1\n"), ("calc_test.py", PYTEST_FILE)],
    );
    assert_eq!(commands(&root), ["python3 -m pytest -q"]);
}

#[test]
fn b696_a_unittest_layout_runs_the_standard_library_runner() {
    let root = tree(
        "b696-ut",
        &[
            ("calc.py", "x = 1\n"),
            ("tests/test_calc.py", UNITTEST_FILE),
        ],
    );
    assert_eq!(
        commands(&root),
        ["python3 -m unittest discover -s tests -v"]
    );
    // A tests folder that is a package is discovered from the root, so its imports of the package keep working.
    let pkg = tree(
        "b696-utpkg",
        &[
            ("calc.py", "x = 1\n"),
            ("tests/__init__.py", ""),
            ("tests/test_calc.py", UNITTEST_FILE),
        ],
    );
    assert_eq!(commands(&pkg), ["python3 -m unittest discover -v"]);
}

#[test]
fn b696_a_test_file_that_imports_pytest_is_a_pytest_layout_even_beside_unittest() {
    let root = tree(
        "b696-mixed",
        &[
            ("tests/test_a.py", UNITTEST_FILE),
            (
                "tests/test_b.py",
                "import pytest\n\n\ndef test_b():\n    assert True\n",
            ),
        ],
    );
    assert_eq!(commands(&root), ["python3 -m pytest -q"]);
}

#[test]
fn b696_a_go_module_with_test_files_is_found() {
    let root = tree(
        "b696-go",
        &[
            ("go.mod", "module example.com/calc\n\ngo 1.22\n"),
            ("calc_test.go", "package calc\n"),
        ],
    );
    assert_eq!(commands(&root), ["go test -v ./..."]);
}

#[test]
fn b696_a_tree_with_no_known_layout_says_what_the_report_looked_for() {
    let root = tree(
        "b696-none",
        &[("NOTES.md", "notes\n"), ("src/main.py", "print(1)\n")],
    );
    let Plan::NoCommand(why) = plan(&root) else {
        panic!("a plan for a tree with no tests");
    };
    assert!(
        why.starts_with(harness_protocol::direct::NO_COMMAND_LOOKED_FOR),
        "{why}"
    );
    for looked_for in [
        ".ham/test-command",
        "lifecycle log",
        "goal card",
        "scripts/test-all.sh",
        "pytest settings",
        "test_*.py",
        "package.json",
        "go.mod",
        "Cargo.toml",
        "pom.xml",
        "gradlew",
    ] {
        assert!(why.contains(looked_for), "{looked_for} missing from: {why}");
    }
}

#[test]
fn b696_a_python_file_outside_a_test_name_is_not_a_test_layout() {
    let root = tree(
        "b696-notest",
        &[
            ("tests/helpers.py", "def h():\n    return 1\n"),
            ("contest.py", "x = 1\n"),
        ],
    );
    assert!(matches!(plan(&root), Plan::NoCommand(_)));
}
