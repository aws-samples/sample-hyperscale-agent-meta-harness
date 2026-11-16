//! A whole-suite command that the lifecycle log recorded as failed is not run again when a later row ran the same suite and
//! passed.
//!
//! One was a row it wrote to record a form of the command that does not work in that repository, with its own exit code in the row: `pytest
//! -q (plain, without python -m) | exit 2`. A later row ran the suite as `python3 -m pytest -q -p no:cacheprovider` and passed 7 tests. The
//! report ran all three again, the form that does not work failed as the log said it would, and the Tests line of a correct delivery was
//! red. Now:
//!
//! - a logged whole-suite command whose own row says its last run failed is left out of the plan when a later row of the log started the
//!   same runner with the same words, passed, and counted at least as many tests as the failed run counted. An option or a `NAME=value`
//!   in front is one of those words unless it is known to select no test, and a failed run that counted no test is stood for only by a
//!   later command with no other option or prefix;
//! - the first step of the report names the command it did not run again, what the log recorded for it and the later row that passed;
//! - the Tests line of such a run is about the commands that ran, so a suite that passes gives the claim with the passing count;
//! - every other failed row is run again as before: the last word of the log on a suite, a row of another suite, a row whose later run
//!   counted fewer tests, a command the shell has to read, and a runner whose suite the report cannot tell from its program. A suite that
//!   fails in the clean checkout still reads as failed.
//!
//! The plan tests start no process. The step tests run stand-in programs made here (`python3`, `pytest`) through the module's own
//! `run_planned`.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::report::plan::{
    LeftOut, NotAgain, Omitted, Plan, PlanNotes, PlannedStep, plan_with_notes,
};
use agentd::direct::report::progress::Progress;
use agentd::direct::report::run::{MAX_NOT_AGAIN_LINES, NOT_AGAIN_PREFIX, RunEnv, not_again_line};
use agentd::direct::report::{Settings, note_left_out, run_planned};
use agentd::events::EventRecorder;
use harness_protocol::direct::{
    LeftOutWhy, LoggedRun, TestCommandSource, TestReport, TestStep, TestStepRole,
};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The lifecycle log of the run, as it was delivered (`.ham/lifecycle/LOG.md` of the zip).
const LIVE_LOG: &str = "RUN | python3 -m pytest tests/test_wordcount.py -q (before wordcount.py existed) | exit 2 | pass 0 fail unknown | 1 error in 0.09s (ModuleNotFoundError: No module named 'wordcount')
RUN | python3 -m pytest tests/test_wordcount.py -q (stub returning {}) | exit 1 | pass 1 fail 3 | 3 failed, 1 passed in 0.02s
RUN | python3 -m pytest -q | exit 0 | pass 4 fail 0 | 4 passed in 0.01s
RUN | PYTHONPATH=/tmp/pyyaml python3 -c \"import yaml; ... assert push and pull_request\" | exit 0 | pass 1 fail 0 | ok
RUN | grep -n '3.12' .github/workflows/ci.yml | exit 0 | pass 1 fail 0 | 17:          python-version: \"3.12\"
NOT RUN | GitHub Actions execution of ci.yml | needs GitHub; not available in this session
RUN | python3 -m pytest -q -p no:cacheprovider (after adding 3 tests) | exit 0 | pass 7 fail 0 | 7 passed in 0.01s
RUN | 12 hand-made mutants of wordcount.py, (each run with python3 -m pytest -q in /tmp/mut/w, exit shown is the last mutant) | exit 1 | pass unknown fail unknown | every mutant failed at least 1 test (before the 3 new tests, 3 of 11 mutants passed: rstrip-only, ';' dropped, split(\" \"))
RUN | pytest -q (plain, without python -m) | exit 2 | pass 0 fail unknown | ModuleNotFoundError: No module named 'wordcount' (documented: use python -m pytest)
RUN | python3 -m pytest -q -p no:cacheprovider (final, after last code commit) | exit 0 | pass 7 fail 0 | 7 passed in 0.01s
";
const PY_TEST: &str = "def test_a():\n    pass\n";
/// What plain `pytest -q` printed in the clean checkout of the run: the test module cannot import the module beside it.
const COLLECTION_ERROR: &str = "==================================== ERRORS ====================================\n___________________ ERROR collecting tests/test_wordcount.py ___________________\nE   ModuleNotFoundError: No module named 'wordcount'\n=========================== short test summary info ============================\nERROR tests/test_wordcount.py\n!!!!!!!!!!!!!!!!!!!! Interrupted: 1 error during collection !!!!!!!!!!!!!!!!!!!!\n1 error in 0.09s";
const SEVEN_PASSED: &str = ".......                                                                  [100%]\n7 passed in 0.01s";
/// The line the first step of the run's report carries.
const LIVE_NOTE: &str = "PLAN NOTE: not run again `pytest -q`; logged exit 2; passed later as `python3 -m pytest -q -p no:cacheprovider`";

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
            ("wordcount.py", "def count_words(text):\n    return {}\n"),
            (
                "tests/test_wordcount.py",
                "from wordcount import count_words\n\n\ndef test_empty():\n    assert count_words('') == {}\n",
            ),
            (".github/workflows/ci.yml", "name: CI\n"),
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

/// A row of the log: the command column, the exit code and the counts as the log writes them (`7`, `unknown`).
fn row(command: &str, exit: i32, pass: &str, fail: &str) -> String {
    format!("RUN | {command} | exit {exit} | pass {pass} fail {fail} | summary\n")
}

/// A tree with pytest files in two folders and a Go module, and `rows` as its lifecycle log.
fn tree_with_log(label: &str, rows: &[String]) -> PathBuf {
    tree(
        label,
        &[
            (".ham/lifecycle/LOG.md", &rows.concat()),
            ("tests/unit/test_a.py", PY_TEST),
            ("tests/integration/test_b.py", PY_TEST),
            ("go.mod", "module example.com/x\n"),
        ],
    )
}

// ---- the plan -----------------------------------------------------------------------------------------------------------------------

#[test]
fn b851_the_log_of_the_run_leaves_out_the_command_it_recorded_as_failed() {
    let root = live_tree("b851-live-plan");
    // The two rows that name the test file are parts of the suite and stay out, as before. Of the three whole-suite commands the plain
    // `pytest -q` is the one whose own row says exit 2, and the row after it ran the suite as `python3 -m pytest` and passed.
    assert_eq!(
        lines(&root),
        [
            "test log python3 -m pytest -q",
            "test log python3 -m pytest -q -p no:cacheprovider",
        ]
    );
}

#[test]
fn b851_a_failed_row_that_is_the_last_word_of_the_log_on_its_suite_still_runs() {
    // The passing run came first: the log's last word on the suite is the failure, so the report runs the command again.
    let root = tree_with_log(
        "b851-last-word",
        &[
            row("python3 -m pytest -q", 0, "7", "0"),
            row("pytest -q (plain, without python -m)", 2, "0", "unknown"),
        ],
    );
    assert_eq!(
        lines(&root),
        ["test log python3 -m pytest -q", "test log pytest -q"]
    );
    // A failed row alone runs again too.
    let alone = tree_with_log("b851-alone", &[row("pytest -q", 1, "5", "2")]);
    assert_eq!(lines(&alone), ["test log pytest -q"]);
}

#[test]
fn b851_a_later_row_of_another_suite_does_not_stand_for_the_failed_one() {
    // The same runner with other words besides options runs other tests: a folder, a package pattern, a script of the manifest.
    let folders = tree_with_log(
        "b851-folders",
        &[
            row("pytest tests/unit -q", 1, "40", "2"),
            row("pytest tests/integration -q", 0, "100", "0"),
        ],
    );
    assert_eq!(
        lines(&folders),
        [
            "test log pytest tests/unit -q",
            "test log pytest tests/integration -q",
        ]
    );
    let packages = tree_with_log(
        "b851-packages",
        &[
            row("go test ./...", 1, "20", "1"),
            row("go test ./internal/...", 0, "30", "0"),
        ],
    );
    assert_eq!(
        lines(&packages),
        ["test log go test ./...", "test log go test ./internal/..."]
    );
    // Another runner of the same language is another command: the report cannot tell that both collect the same tests.
    let runners = tree_with_log(
        "b851-runners",
        &[
            row("python3 -m unittest discover -v", 1, "5", "1"),
            row("python3 -m pytest -q", 0, "9", "0"),
        ],
    );
    assert_eq!(
        lines(&runners),
        [
            "test log python3 -m unittest discover -v",
            "test log python3 -m pytest -q",
        ]
    );
    // An option of pytest that names a test selection keeps its value among the words: `-m slow` is not the whole suite.
    let marker = tree_with_log(
        "b851-marker",
        &[
            row("pytest -q -m slow", 1, "2", "1"),
            row("pytest -q", 0, "40", "0"),
        ],
    );
    assert_eq!(
        lines(&marker),
        ["test log pytest -q -m slow", "test log pytest -q"]
    );
}

#[test]
fn b851_a_later_row_that_counted_fewer_tests_does_not_stand_for_the_failed_one() {
    // The later run passed with fewer tests than the failed run counted (an option left tests out): the failed command runs again.
    let fewer = tree_with_log(
        "b851-fewer",
        &[
            row("pytest -q", 1, "40", "2"),
            row("pytest -q --ignore=tests/integration", 0, "30", "0"),
        ],
    );
    assert_eq!(
        lines(&fewer),
        [
            "test log pytest -q",
            "test log pytest -q --ignore=tests/integration",
        ]
    );
    // A later row with no count says nothing about how much it ran.
    let unknown = tree_with_log(
        "b851-unknown",
        &[
            row("go test ./...", 1, "unknown", "unknown"),
            row("go test -count=1 ./...", 0, "unknown", "unknown"),
        ],
    );
    assert_eq!(
        lines(&unknown),
        ["test log go test ./...", "test log go test -count=1 ./..."]
    );
    // A later row that exits 0 and counts a failed test did not pass.
    let odd = tree_with_log(
        "b851-odd",
        &[
            row("pytest -q", 1, "5", "2"),
            row("python3 -m pytest -q", 0, "6", "1"),
        ],
    );
    assert_eq!(
        lines(&odd),
        ["test log pytest -q", "test log python3 -m pytest -q"]
    );
}

#[test]
fn b851_a_later_row_with_an_option_that_may_leave_tests_out_does_not_stand_for_the_failed_one() {
    // The failed run counted no test (the row of the run reads `exit 2 | pass 0 fail unknown`), so no count can show that the later
    // run was a narrower one: its words have to. An option that is not known to select nothing makes another suite, written with a
    // space or with `=`, and so does a `NAME=value` in front that is not known to select nothing.
    for (label, failed, exit, later, passed) in [
        (
            "ignore",
            "pytest -q",
            2,
            "pytest -q --ignore=tests/integration",
            "30",
        ),
        ("last-failed", "pytest -q", 2, "pytest -q --lf", "1"),
        ("marker", "pytest -q", 2, "pytest -q -m=fast", "12"),
        (
            "go-run",
            "go test ./...",
            1,
            "go test ./... -run=TestParse",
            "3",
        ),
        ("go-short", "go test ./...", 1, "go test -short ./...", "20"),
        ("cargo-lib", "cargo test", 101, "cargo test --lib", "12"),
        (
            "npm-pattern",
            "npm test",
            1,
            "npm test -- --testPathIgnorePatterns=e2e",
            "25",
        ),
        (
            "prefix",
            "pytest -q",
            2,
            "SKIP_INTEGRATION=1 pytest -q",
            "30",
        ),
    ] {
        let root = tree_with_log(
            &format!("b851-narrow-{label}"),
            &[
                row(failed, exit, "0", "unknown"),
                row(later, 0, passed, "0"),
            ],
        );
        assert_eq!(
            lines(&root),
            [format!("test log {failed}"), format!("test log {later}")],
            "{label}"
        );
        assert!(planned(&root).1.not_again.is_empty(), "{label}");
    }
    // Tests were added between the two runs: the later run counted more than the failed one and still left a folder out.
    let grown = tree_with_log(
        "b851-narrow-grown",
        &[
            row("pytest -q", 1, "40", "2"),
            row("pytest -q --ignore=tests/integration", 0, "45", "0"),
        ],
    );
    assert_eq!(
        lines(&grown),
        [
            "test log pytest -q",
            "test log pytest -q --ignore=tests/integration",
        ]
    );
}

#[test]
fn b851_a_switch_that_can_take_tests_away_is_a_word_of_the_suite() {
    // A plugin switch of pytest can take a collector away (`-p no:doctest`), the race detector of Go sets a build tag that a test
    // file can exclude, and behind `--` pytest reads every word as a path. None of these stands for the command without it.
    for (label, failed, exit, later, passed) in [
        ("plugin", "pytest -q", 2, "pytest -q -p no:doctest", "30"),
        (
            "plugin-attached",
            "pytest -q",
            2,
            "pytest -q -pno:unittest",
            "30",
        ),
        ("race", "go test ./...", 1, "go test -race ./...", "20"),
        ("path", "pytest -q", 2, "pytest -- -q", "30"),
    ] {
        let root = tree_with_log(
            &format!("b851-switch-{label}"),
            &[
                row(failed, exit, "0", "unknown"),
                row(later, 0, passed, "0"),
            ],
        );
        assert_eq!(
            lines(&root),
            [format!("test log {failed}"), format!("test log {later}")],
            "{label}"
        );
    }
    // `npm test --grep=smoke` gives the option to npm, and `npm test -- --grep=smoke` hands it to the test script, which then runs
    // a part. The counts do not tell the two apart when tests were added in between.
    let forwarded = tree_with_log(
        "b851-switch-forwarded",
        &[
            row("npm test --grep=smoke", 1, "40", "2"),
            row("npm test -- --grep=smoke", 0, "45", "0"),
        ],
    );
    assert_eq!(
        lines(&forwarded),
        [
            "test log npm test --grep=smoke",
            "test log npm test -- --grep=smoke",
        ]
    );
    // The switch that turns the cache of pytest off collects nothing, as the next word or right behind the letter, and the first
    // `--` of cargo only stands in front of the options of the test program.
    for (label, failed, exit, later) in [
        (
            "cache",
            "pytest -q",
            2,
            "python3 -m pytest -q -p no:cacheprovider",
        ),
        (
            "cache-attached",
            "pytest -q",
            2,
            "pytest -q -pno:cacheprovider",
        ),
        (
            "cargo-program",
            "cargo test",
            101,
            "cargo test -- --nocapture --test-threads=1",
        ),
    ] {
        let root = tree_with_log(
            &format!("b851-switch-kept-{label}"),
            &[row(failed, exit, "0", "unknown"), row(later, 0, "7", "0")],
        );
        assert_eq!(lines(&root), [format!("test log {later}")], "{label}");
    }
}

#[test]
fn b851_an_option_that_only_sets_how_the_run_goes_keeps_the_suite() {
    // The options of pytest, `go test` and cargo that select no test, with their value as the next word or behind `=`, and a
    // `NAME=value` in front that only says where the code is. The failed run counted nothing and the later one passed 7 tests.
    for (label, failed, exit, later) in [
        (
            "pytest-spaced",
            "pytest",
            2,
            "python3 -m pytest -q -v -x -s -p no:cacheprovider -r A --tb short --color yes --durations 5 --maxfail 1",
        ),
        (
            "pytest-equals",
            "pytest -q",
            2,
            "pytest -qx --tb=short --color=yes --durations=5 --maxfail=1 -rA -pno:cacheprovider",
        ),
        (
            "go-equals",
            "go test ./...",
            1,
            "go test -count=1 -v -cover -timeout=60s ./...",
        ),
        (
            "go-spaced",
            "go test ./...",
            1,
            "go test -count 1 -timeout 60s ./...",
        ),
        (
            "cargo",
            "cargo test",
            101,
            "cargo test -q -j 4 --color always",
        ),
        ("prefix", "pytest -q", 2, "PYTHONPATH=src pytest -q"),
    ] {
        let root = tree_with_log(
            &format!("b851-setting-{label}"),
            &[row(failed, exit, "0", "unknown"), row(later, 0, "7", "0")],
        );
        assert_eq!(lines(&root), [format!("test log {later}")], "{label}");
    }
}

#[test]
fn b851_an_option_that_can_fail_a_run_is_a_word_of_the_suite() {
    // An option that selects no test can still decide whether the run passes: `cargo test --locked` fails when `Cargo.lock` does
    // not match `Cargo.toml`, `--offline` and `--frozen` when a package is not at hand, `pytest -W error` on a warning, and `-n`
    // runs the tests side by side. The same tests pass without it, so the later command without the option does not stand for the
    // strict one: the clean checkout runs both, and the report shows what the strict command does there.
    for (label, failed, exit, pass, fail, later, later_pass) in [
        (
            "cargo-locked",
            "cargo test --locked",
            101,
            "0",
            "unknown",
            "cargo test",
            "40",
        ),
        (
            "cargo-offline",
            "cargo test --offline",
            101,
            "0",
            "unknown",
            "cargo test",
            "40",
        ),
        (
            "cargo-frozen",
            "cargo test --frozen",
            101,
            "0",
            "unknown",
            "cargo test -q",
            "40",
        ),
        (
            "pytest-warnings",
            "pytest -q -W error",
            1,
            "40",
            "2",
            "pytest -q",
            "42",
        ),
        (
            "pytest-warnings-joined",
            "pytest -q -Werror",
            1,
            "40",
            "2",
            "pytest -q",
            "42",
        ),
        (
            "pytest-workers",
            "pytest -q -n 4",
            1,
            "40",
            "2",
            "pytest -q",
            "42",
        ),
        (
            "pytest-workers-joined",
            "pytest -q -n4",
            1,
            "40",
            "2",
            "pytest -q",
            "42",
        ),
    ] {
        let root = tree_with_log(
            &format!("b851-strict-{label}"),
            &[
                row(failed, exit, pass, fail),
                row(later, 0, later_pass, "0"),
            ],
        );
        assert_eq!(
            lines(&root),
            [format!("test log {failed}"), format!("test log {later}")],
            "{label}"
        );
        assert!(planned(&root).1.not_again.is_empty(), "{label}");
    }
}

#[test]
fn b851_an_option_the_report_does_not_know_stands_only_beside_a_count() {
    // Both rows carry the same option, one the report does not know. With a count from the failed run the later row stands for it:
    // the same words, and at least as many tests.
    let counted = tree_with_log(
        "b851-unknown-counted",
        &[
            row("pytest -q --cov=src", 1, "28", "2"),
            row("python3 -m pytest -q --cov=src", 0, "30", "0"),
        ],
    );
    assert_eq!(lines(&counted), ["test log python3 -m pytest -q --cov=src"]);
    // Without a count nothing but the words says that the two runs covered the same tests, and the report does not know what the
    // option does: the failed command runs again.
    let uncounted = tree_with_log(
        "b851-unknown-uncounted",
        &[
            row("pytest -q --cov=src", 2, "0", "unknown"),
            row("python3 -m pytest -q --cov=src", 0, "30", "0"),
        ],
    );
    assert_eq!(
        lines(&uncounted),
        [
            "test log pytest -q --cov=src",
            "test log python3 -m pytest -q --cov=src",
        ]
    );
}

#[test]
fn b851_a_later_spelling_of_the_same_suite_that_passed_stands_for_the_failed_one() {
    // The program in front of the runner, an option that selects no test (with its value) and a `NAME=value` in front that only says
    // where the code is do not change which tests a command runs.
    for (label, failed, passed) in [
        ("module", "pytest -q", "python -m pytest -q"),
        (
            "plugin",
            "python3 -m pytest -q",
            "pytest -q -p no:cacheprovider",
        ),
        ("options", "pytest", "pytest -x -q --tb short --color=no"),
        ("prefix", "pytest -q", "PYTHONPATH=. pytest -q"),
        ("py-test", "py.test -q", "python3.12 -m pytest -q"),
        ("go", "go test ./...", "go test -count=1 ./..."),
        (
            "folder",
            "pytest tests/unit -q",
            "python3 -m pytest -q tests/unit",
        ),
    ] {
        let root = tree_with_log(
            &format!("b851-same-{label}"),
            &[row(failed, 1, "5", "2"), row(passed, 0, "7", "0")],
        );
        assert_eq!(lines(&root), [format!("test log {passed}")], "{label}");
    }
    // The later run counted exactly the tests the failed run counted.
    let equal = tree_with_log(
        "b851-same-count",
        &[
            row("pytest -q", 1, "5", "2"),
            row("python3 -m pytest -q", 0, "7", "0"),
        ],
    );
    assert_eq!(lines(&equal), ["test log python3 -m pytest -q"]);
}

#[test]
fn b851_a_command_the_shell_reads_or_whose_suite_cannot_be_told_is_run_again() {
    // A command that changes the folder or pipes its output runs as written, as before: its words are not read.
    let shell = tree_with_log(
        "b851-shell",
        &[
            row("cd api && pytest -q", 1, "5", "2"),
            row("cd api && python3 -m pytest -q", 0, "7", "0"),
        ],
    );
    assert_eq!(
        lines(&shell),
        [
            "test log cd api && pytest -q",
            "test log cd api && python3 -m pytest -q",
        ]
    );
    // `make` may run the suite of any language, and two targets are two commands.
    let make = tree_with_log(
        "b851-make",
        &[
            row("make test", 2, "0", "unknown"),
            row("make check", 0, "7", "0"),
        ],
    );
    assert_eq!(lines(&make), ["test log make test", "test log make check"]);
}

#[test]
fn b851_the_last_run_of_one_command_is_still_the_row_of_its_step() {
    // The rule of before: a command the log ran red and then green is one step with the row of its last run.
    let root = tree_with_log(
        "b851-red-green",
        &[row("pytest -q", 1, "5", "2"), row("pytest -q", 0, "7", "0")],
    );
    let (steps, _) = planned(&root);
    assert_eq!(
        steps.iter().map(line).collect::<Vec<_>>(),
        ["test log pytest -q"]
    );
    assert_eq!(steps[0].logged.and_then(|l| l.exit_code), Some(0));
    // And green and then red is one step with the red row: the report runs it again.
    let red = tree_with_log(
        "b851-green-red",
        &[row("pytest -q", 0, "7", "0"), row("pytest -q", 1, "5", "2")],
    );
    let (steps, _) = planned(&red);
    assert_eq!(
        steps.iter().map(line).collect::<Vec<_>>(),
        ["test log pytest -q"]
    );
    assert_eq!(steps[0].logged.and_then(|l| l.exit_code), Some(1));
}

// ---- the steps, with stand-in programs -------------------------------------------------------------------------------------------------

fn executable(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Stand-ins: `python3 -m pytest` prints `python_output` and exits `python_exit`; plain `pytest` fails as it did in the clean checkout of
/// the run, with a collection error and exit 2.
fn fake_bin(python_output: &str, python_exit: i32) -> PathBuf {
    let bin = common::scratch("b851-bin");
    executable(
        &bin.join("python3"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" = -m ] && [ \"$2\" = pytest ]; then\ncat <<'STAND_IN'\n{python_output}\nSTAND_IN\nexit {python_exit}\nfi\nexit 9\n"
        ),
    );
    executable(
        &bin.join("pytest"),
        &format!("#!/bin/sh\ncat <<'STAND_IN'\n{COLLECTION_ERROR}\nSTAND_IN\nexit 2\n"),
    );
    bin
}

fn env_for(bin: &Path) -> RunEnv {
    RunEnv {
        sandbox: None,
        work: common::scratch("b851-work"),
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
    let (steps, notes) = planned(root);
    let settings = Settings {
        enabled: true,
        budget: Duration::from_secs(60),
        step: Duration::from_secs(30),
        tail: 4096,
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
async fn b851_the_run_reads_as_passing_and_its_report_names_the_command_it_did_not_run_again() {
    let r = report_with(&live_tree("b851-live-run"), &fake_bin(SEVEN_PASSED, 0)).await;
    assert_eq!(
        brief(&r.steps),
        [
            "python3 -m pytest -q exit=Some(0) not_run=false",
            "python3 -m pytest -q -p no:cacheprovider exit=Some(0) not_run=false",
        ],
        "{r:?}"
    );
    // The first step says at the head of its output which logged command the report did not run again, what the log recorded for it and
    // the later row that passed; what pytest printed follows.
    assert_eq!(
        r.steps[0].output_tail,
        format!("{LIVE_NOTE}\n{SEVEN_PASSED}")
    );
    // A step with no line of agentd's is left as it is, byte for byte.
    assert_eq!(r.steps[1].output_tail, format!("{SEVEN_PASSED}\n"));
    let s = r.summary();
    // The Tests line of the card gives the passing count.
    assert_eq!(
        s.sentence(),
        "repository tests pass in a clean checkout: 7 of 7."
    );
    assert_eq!(
        s.details,
        [
            "`python3 -m pytest -q`: exit 0, 7 passed, 0 failed.",
            "`python3 -m pytest -q -p no:cacheprovider`: exit 0, 7 passed, 0 failed.",
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b851_a_suite_that_fails_in_the_clean_checkout_still_reads_as_failed() {
    // The same log, and the suite fails in the clean checkout: the commands that ran say so, and the note stays on the first step.
    let failed = "..F....\n1 failed, 6 passed in 0.02s";
    let r = report_with(&live_tree("b851-live-red"), &fake_bin(failed, 1)).await;
    assert_eq!(
        brief(&r.steps),
        [
            "python3 -m pytest -q exit=Some(1) not_run=false",
            "python3 -m pytest -q -p no:cacheprovider exit=Some(1) not_run=false",
        ],
        "{r:?}"
    );
    assert_eq!(r.steps[0].output_tail, format!("{LIVE_NOTE}\n{failed}"));
    let s = r.summary();
    assert_eq!(s.claim(), None);
    assert_eq!(
        s.sentence(),
        "`python3 -m pytest -q` exited 1 in a clean checkout; the runner reported 6 passed, 1 failed."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b851_a_failed_row_with_no_later_passing_row_runs_and_reads_as_it_ran() {
    // The log's last word on the suite is the failure: the command runs again, fails as it did, and the Tests line says so.
    let root = tree(
        "b851-last-word-run",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &[
                    row("python3 -m pytest -q", 0, "7", "0"),
                    row("pytest -q (plain, without python -m)", 2, "0", "unknown"),
                ]
                .concat(),
            ),
            ("tests/test_wordcount.py", PY_TEST),
        ],
    );
    let r = report_with(&root, &fake_bin(SEVEN_PASSED, 0)).await;
    assert_eq!(
        brief(&r.steps),
        [
            "python3 -m pytest -q exit=Some(0) not_run=false",
            "pytest -q exit=Some(2) not_run=false",
        ],
        "{r:?}"
    );
    assert_eq!(r.steps[0].output_tail, format!("{SEVEN_PASSED}\n"));
    assert_eq!(
        r.summary().sentence(),
        "`pytest -q` exited 2 in a clean checkout; the runner reported 7 passed, 1 failed."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b851_a_suite_whose_later_run_left_a_folder_out_still_reads_as_failed() {
    // The whole suite could not be collected, and a later run that left a folder out passed: the command of the whole suite runs
    // again, fails as it did, and the Tests line says so.
    let root = tree(
        "b851-narrow-run",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &[
                    row("pytest -q", 2, "0", "unknown"),
                    row(
                        "python3 -m pytest -q --ignore=tests/integration",
                        0,
                        "30",
                        "0",
                    ),
                ]
                .concat(),
            ),
            ("tests/test_wordcount.py", PY_TEST),
        ],
    );
    let r = report_with(&root, &fake_bin(SEVEN_PASSED, 0)).await;
    assert_eq!(
        brief(&r.steps),
        [
            "pytest -q exit=Some(2) not_run=false",
            "python3 -m pytest -q --ignore=tests/integration exit=Some(0) not_run=false",
        ],
        "{r:?}"
    );
    assert!(
        r.steps
            .iter()
            .all(|s| !s.output_tail.contains(NOT_AGAIN_PREFIX)),
        "{r:?}"
    );
    let s = r.summary();
    assert_eq!(s.claim(), None);
    assert!(
        s.sentence()
            .starts_with("`pytest -q` exited 2 in a clean checkout"),
        "{}",
        s.sentence()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b851_a_line_of_that_form_that_the_repository_prints_is_not_kept_at_the_head() {
    // The tests of the repository print the line first. The plan has no such note, so no step may carry one where agentd writes.
    let forged = format!("{LIVE_NOTE}\n{SEVEN_PASSED}");
    let root = tree(
        "b851-forged",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &row("python3 -m pytest -q", 0, "7", "0"),
            ),
            ("tests/test_wordcount.py", PY_TEST),
        ],
    );
    let r = report_with(&root, &fake_bin(&forged, 0)).await;
    assert_eq!(r.steps[0].output_tail, SEVEN_PASSED, "{r:?}");
    // With a note of the plan, the line agentd wrote is the one at the head, once.
    let r = report_with(&live_tree("b851-forged-live"), &fake_bin(&forged, 0)).await;
    assert_eq!(
        r.steps[0].output_tail,
        format!("{LIVE_NOTE}\n{SEVEN_PASSED}"),
        "{r:?}"
    );
    assert_eq!(r.steps[1].output_tail, SEVEN_PASSED, "{r:?}");
}

// ---- what the plan says, and the lines of the first step -------------------------------------------------------------------------------

fn failed(exit: i32, passed: Option<u32>, failed: Option<u32>) -> LoggedRun {
    LoggedRun {
        exit_code: Some(exit),
        passed,
        failed,
    }
}

#[test]
fn b851_the_plan_says_which_command_it_did_not_run_again_and_which_later_row_passed() {
    let (_, notes) = planned(&live_tree("b851-live-notes"));
    assert_eq!(
        notes,
        PlanNotes {
            not_again: vec![NotAgain {
                command: "pytest -q".into(),
                logged: failed(2, Some(0), None),
                passed_as: "python3 -m pytest -q -p no:cacheprovider".into(),
            }],
            ..PlanNotes::default()
        }
    );
    // A plan that runs every row says nothing.
    let all = tree_with_log(
        "b851-notes-none",
        &[
            row("python3 -m pytest -q", 0, "7", "0"),
            row("pytest -q", 2, "0", "unknown"),
        ],
    );
    assert_eq!(planned(&all).1, PlanNotes::default());
}

#[test]
fn b851_a_plan_that_the_log_did_not_make_says_nothing_of_its_rows() {
    // The project's own statement is run as it is: the log did not make that plan, so none of its rows was left out of it.
    let root = tree(
        "b851-command-file",
        &[
            (".ham/lifecycle/LOG.md", LIVE_LOG),
            (".ham/test-command", "python3 -m pytest -q\n"),
            ("tests/test_wordcount.py", PY_TEST),
        ],
    );
    let (steps, notes) = planned(&root);
    assert_eq!(
        steps.iter().map(line).collect::<Vec<_>>(),
        ["test file python3 -m pytest -q"]
    );
    assert_eq!(notes, PlanNotes::default());
}

#[test]
fn b851_the_line_gives_the_command_what_the_log_recorded_and_the_later_command() {
    assert_eq!(
        not_again_line(
            "pytest -q",
            &failed(2, Some(0), None),
            "python3 -m pytest -q -p no:cacheprovider"
        ),
        LIVE_NOTE
    );
    // Failed tests the log counted are part of what it recorded.
    assert_eq!(
        not_again_line(
            "go test ./...",
            &failed(1, Some(18), Some(2)),
            "go test -count=1 ./..."
        ),
        "PLAN NOTE: not run again `go test ./...`; logged exit 1, 2 failed; passed later as `go test -count=1 ./...`"
    );
    // A command is cut to 80 characters, and a backtick of its own cannot end the quoted command.
    let long = format!("pytest -q {}", "a".repeat(200));
    let line = not_again_line(&long, &failed(1, None, None), "pytest `x` -q");
    assert_eq!(
        line,
        format!(
            "PLAN NOTE: not run again `pytest -q {}`; logged exit 1; passed later as `pytest 'x' -q`",
            "a".repeat(70)
        )
    );
    assert!(line.starts_with(NOT_AGAIN_PREFIX));
}

#[test]
fn b851_the_lines_stand_behind_the_lines_the_summary_reads_and_in_front_of_the_other_head_lines() {
    // A step that carries every kind of line about the plan and a `NEEDS:` line: the summary reads its own lines as before.
    let mut steps = vec![TestStep {
        role: TestStepRole::Test,
        source: TestCommandSource::Detected,
        command: "python3 -m pytest -q".into(),
        exit_code: Some(1),
        timed_out: false,
        duration_ms: 10,
        limit_s: Some(30),
        counts: None,
        counts_from: None,
        output_tail: "NEEDS: postgres: connection refused\n1 failed in 0.01s".into(),
        output_truncated: false,
        logged: None,
    }];
    let notes = PlanNotes {
        stand_ins: vec![(
            0,
            LeftOut {
                command: "python3 -m pytest tests/test_a.py -q".into(),
                more: 0,
            },
        )],
        left_out: vec![Omitted {
            rows: LeftOut {
                command: "npx jest src/a.test.ts".into(),
                more: 0,
            },
            why: LeftOutWhy::NoSuiteCommand,
            logged: None,
        }],
        chosen_from: None,
        not_again: vec![NotAgain {
            command: "pytest -q".into(),
            logged: failed(2, Some(0), None),
            passed_as: "python3 -m pytest -q -p no:cacheprovider".into(),
        }],
    };
    note_left_out(&mut steps, &notes, 4096);
    assert_eq!(
        steps[0].output_tail,
        format!(
            "PLAN NOTE: in place of `python3 -m pytest tests/test_a.py -q`\nPLAN NOTE: left out `npx jest src/a.test.ts`\n{LIVE_NOTE}\nNEEDS: postgres: connection refused\n1 failed in 0.01s"
        )
    );
    assert_eq!(
        steps[0].in_place_of(),
        Some(("python3 -m pytest tests/test_a.py -q", 0))
    );
    assert_eq!(steps[0].left_out(), Some(("npx jest src/a.test.ts", 0)));
    assert_eq!(steps[0].left_out_notes().len(), 1);
    assert_eq!(steps[0].needs(), [("postgres", "connection refused")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn b851_a_report_names_eight_such_commands_and_counts_the_others() {
    // Ten spellings of the suite that the log recorded as failed, and one later row that passed.
    let spellings = [
        "pytest",
        "pytest -q",
        "pytest -x",
        "pytest -v",
        "pytest -s",
        "pytest -ra",
        "pytest -vv",
        "pytest -q -x",
        "pytest -q -s",
        "pytest --tb=short",
    ];
    let mut rows: Vec<String> = spellings
        .iter()
        .map(|c| row(c, 2, "0", "unknown"))
        .collect();
    rows.push(row("python3 -m pytest -q", 0, "7", "0"));
    let root = tree(
        "b851-many",
        &[
            (".ham/lifecycle/LOG.md", &rows.concat()),
            ("tests/test_wordcount.py", PY_TEST),
        ],
    );
    let (steps, notes) = planned(&root);
    assert_eq!(
        steps.iter().map(line).collect::<Vec<_>>(),
        ["test log python3 -m pytest -q"]
    );
    assert_eq!(
        notes
            .not_again
            .iter()
            .map(|n| n.command.as_str())
            .collect::<Vec<_>>(),
        spellings
    );
    let r = report_with(&root, &fake_bin(SEVEN_PASSED, 0)).await;
    let head: Vec<&str> = r.steps[0]
        .output_tail
        .lines()
        .take_while(|l| l.starts_with(NOT_AGAIN_PREFIX))
        .collect();
    assert_eq!(head.len(), MAX_NOT_AGAIN_LINES + 1, "{r:?}");
    assert_eq!(
        head[0],
        "PLAN NOTE: not run again `pytest`; logged exit 2; passed later as `python3 -m pytest -q`"
    );
    assert_eq!(
        head[7],
        "PLAN NOTE: not run again `pytest -q -x`; logged exit 2; passed later as `python3 -m pytest -q`"
    );
    assert_eq!(head[8], "PLAN NOTE: not run again 2 more");
    assert_eq!(
        r.summary().sentence(),
        "repository tests pass in a clean checkout: 7 of 7."
    );
}
