//! The report runs the commands the lifecycle log names, not the words a model wrote after them.
//!
//! The report ran `pytest -q at BASE worktree` in the clean checkout, pytest answered "ERROR: file or directory not found: at" with exit 4, and
//! that step led the card of a correct delivery. Now:
//!
//! - words after the command that start with a word of plain English (`at`, `after`, `in`, `with` ...) and name nothing in the tree are a note,
//!   cut as a note in parentheses is;
//! - a row whose note says the run was made at the base commit (`at BASE`, `(base worktree)`) is a run outside the checkout and is left out;
//! - a pytest step that still exits 4 because a word of its command names no file or directory of the clean checkout is marked not run, with the
//!   reason `could not run as written`, and is no failure of the repository's tests.
//!
//! The plan tests start no process. The step tests run a stand-in `pytest` made here, through the module's own `run_steps`.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::report::plan::{Plan, PlannedStep, plan};
use agentd::direct::report::progress::Progress;
use agentd::direct::report::run::RunEnv;
use agentd::direct::report::{Settings, run_steps};
use agentd::events::EventRecorder;
use harness_protocol::direct::{
    LoggedRun, TestCommandSource, TestReport, TestReportStatus, TestStepRole,
};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The lifecycle log of the owner's run, as it was delivered.
const OWNER_LOG: &str = "RUN | python3 -m pytest tests/test_wordcount.py -q (stub, red) | exit 1 | pass 1 fail 2 | 2 failed, 1 passed in 0.02s
RUN | python3 -m pytest -q | exit 0 | pass 3 fail 0 | 3 passed in 0.01s
RUN | python3 -c \"from wordcount import count_words; print(count_words('The the, THE! cat?'))\" | exit 0 | pass unknown fail unknown | {'the': 3, 'cat': 1}
RUN | grep -nE 'push|pull_request|3.12|pytest' .github/workflows/ci.yml | exit 0 | pass unknown fail unknown | push, pull_request, python-version \"3.12\", python -m pytest found
NOT RUN | yaml parse of ci.yml | PyYAML not installed; checked by grep instead
NOT RUN | lint/type-check | no ruff/mypy configured or installed
NOT RUN | workflow on GitHub | needs the remote repository
RUN | python3 -m pytest -q | exit 0 | pass 3 fail 0 | 3 passed
RUN | python3 -m pytest tests/test_wordcount.py -q (goal re-check 1) | exit 0 | pass 3 fail 0 | 3 passed in 0.01s
RUN | python3 -c \"from wordcount import count_words; print(count_words('The the, THE! cat?'))\" (goal re-check 2) | exit 0 | pass unknown fail unknown | {'the': 3, 'cat': 1}
RUN | grep -nE 'push|pull_request|3.12|pytest' .github/workflows/ci.yml (goal re-check 3, text form) | exit 0 | pass unknown fail unknown | push, pull_request, \"3.12\", python -m pytest found
RUN | python3 -m pytest -q tests (review, after adding 3 tests) | exit 0 | pass 6 fail 0 | 6 passed in 0.01s
RUN | mutants rstrip / split(' ') / replace against new tests | exit 1 | pass 5 fail 1 | 1 failed, 5 passed (each mutant caught)
RUN | pytest -q (plain, with pytest.ini) | exit 0 | pass 6 fail 0 | 6 passed in 0.01s
RUN | pytest -q at BASE worktree | exit 5 | pass 0 fail 0 | no tests ran in 0.00s
RUN | pytest -q (final, after last edit) | exit 0 | pass 6 fail 0 | 6 passed
";

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

fn row(command: &str, exit: i32, passed: u32) -> String {
    format!("RUN | {command} | exit {exit} | pass {passed} fail 0 | {passed} passed\n")
}

fn owner_tree(label: &str) -> PathBuf {
    tree(
        label,
        &[
            (".ham/lifecycle/LOG.md", OWNER_LOG),
            (
                "pytest.ini",
                "[pytest]\npythonpath = .\ntestpaths = tests\n",
            ),
            ("wordcount.py", "def count_words(text):\n    return {}\n"),
            ("tests/test_wordcount.py", "def test_empty():\n    pass\n"),
        ],
    )
}

// ---- the plan -----------------------------------------------------------------------------------------------------------------------

#[test]
fn b834_the_owners_log_plans_the_three_commands_it_ran_in_the_checkout() {
    let root = owner_tree("b834-owner");
    assert_eq!(
        lines(&root),
        [
            "test log python3 -m pytest -q",
            "test log python3 -m pytest -q tests",
            "test log pytest -q",
        ]
    );
    // The row beside `pytest -q` is its last run in the checkout, not the run at the base commit.
    let s = steps(&root);
    assert_eq!(
        s[2].logged,
        Some(LoggedRun {
            exit_code: Some(0),
            passed: Some(6),
            failed: Some(0)
        })
    );
}

#[test]
fn b834_words_of_plain_english_after_a_command_are_a_note() {
    let root = tree(
        "b834-prose",
        &[(
            ".ham/lifecycle/LOG.md",
            &[
                row("pytest -q after the last edit", 0, 6),
                row("go test ./... with the race detector", 0, 4),
                row("CI=1 npm test in the web folder", 0, 2),
                row("cargo test again", 0, 9),
            ]
            .concat(),
        )],
    );
    assert_eq!(
        lines(&root),
        [
            "test log pytest -q",
            "test log go test ./...",
            "test log CI=1 npm test",
            "test log cargo test",
        ]
    );
}

#[test]
fn b834_a_word_that_is_an_argument_stays() {
    let root = tree(
        "b834-arguments",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &[
                    // A quoted value is one argument, whatever its words.
                    row("pytest -q -m 'not slow and not e2e'", 0, 6),
                    // One word right after an option may be its value.
                    row("pytest -q -p at", 0, 6),
                    // A word that names a folder of the tree is a path.
                    row("pytest in tests", 0, 6),
                    // An option after the words: the words may be values.
                    row("pytest -q for --maxfail 1", 0, 6),
                ]
                .concat(),
            ),
            ("in/test_x.py", "def test_x():\n    pass\n"),
        ],
    );
    assert_eq!(
        lines(&root),
        [
            "test log pytest -q -m 'not slow and not e2e'",
            "test log pytest -q -p at",
            "test log pytest in tests",
            "test log pytest -q for --maxfail 1",
        ]
    );
}

#[test]
fn b834_a_run_the_note_places_at_the_base_commit_is_left_out() {
    let root = tree(
        "b834-base",
        &[(
            ".ham/lifecycle/LOG.md",
            &[
                row("pytest -q", 0, 6),
                row("pytest -q (at BASE)", 5, 0),
                row("go test ./... (in the base worktree)", 1, 0),
                row("npm test (base-commit checkout)", 1, 0),
                // Words that only look like it: kept.
                row("cargo test (base case)", 0, 3),
                row("make test (database tests)", 0, 3),
            ]
            .concat(),
        )],
    );
    assert_eq!(
        lines(&root),
        [
            "test log pytest -q",
            "test log cargo test",
            "test log make test"
        ]
    );
    assert_eq!(
        steps(&root)[0].logged,
        Some(LoggedRun {
            exit_code: Some(0),
            passed: Some(6),
            failed: Some(0)
        })
    );
}

#[test]
fn b834_a_note_that_only_mentions_the_base_commit_keeps_its_row() {
    // Each of these ran at the head. A note that speaks of the base commit is no marker that the run was made there: the contract asks for the
    // note `(at BASE)` and nothing wider, and dropping a head row can leave a whole suite out of the report.
    let root = tree(
        "b834-base-mention",
        &[(
            ".ham/lifecycle/LOG.md",
            &[
                row("pytest -q (fixed; red at base)", 0, 6),
                row("cargo test (ran on base and head)", 0, 9),
                row("make test (BASE_URL unset)", 0, 3),
                row("npm test (compare vs base: 3 failed)", 0, 2),
                row("go test ./... (at base: 2 failed, head: 0 failed)", 0, 4),
                row("tox on the base branch", 0, 4),
            ]
            .concat(),
        )],
    );
    assert_eq!(
        lines(&root),
        [
            "test log pytest -q",
            "test log cargo test",
            "test log make test",
            "test log npm test",
            "test log go test ./...",
            "test log tox",
        ]
    );
}

#[test]
fn b834_the_head_row_beside_a_base_mention_is_the_logged_run() {
    // The only row of the Go suite says it was green and had been red at base: it is the run of the head, and no earlier row stands in its place.
    let root = tree(
        "b834-head-row",
        &[(
            ".ham/lifecycle/LOG.md",
            &[
                row("go test ./...", 1, 0),
                row("go test ./... (green; was red at base)", 0, 4),
            ]
            .concat(),
        )],
    );
    assert_eq!(lines(&root), ["test log go test ./..."]);
    assert_eq!(
        steps(&root)[0].logged,
        Some(LoggedRun {
            exit_code: Some(0),
            passed: Some(4),
            failed: Some(0)
        })
    );
    let alone = tree(
        "b834-head-row-alone",
        &[(
            ".ham/lifecycle/LOG.md",
            &row("go test ./... (green; was red at base)", 0, 4),
        )],
    );
    assert_eq!(lines(&alone), ["test log go test ./..."]);
}

#[test]
fn b834_the_notes_that_place_a_run_at_the_base_commit_leave_the_row_out() {
    // The forms a model writes for a run made in a copy of the base commit: the note starts with where the run was made.
    let root = tree(
        "b834-base-forms",
        &[(
            ".ham/lifecycle/LOG.md",
            &[
                row("pytest -q", 0, 6),
                row("go test ./... (at BASE; 3 failed)", 1, 0),
                row("cargo test (red, at BASE)", 1, 0),
                row("npm test (at base abc1234)", 1, 0),
                row("make test (base commit abc1234)", 1, 0),
                row("tox (on the clean base checkout)", 1, 0),
                row("mvn test (BASE)", 1, 0),
            ]
            .concat(),
        )],
    );
    assert_eq!(lines(&root), ["test log pytest -q"]);
}

// ---- the steps, with a stand-in pytest ----------------------------------------------------------------------------------------------

fn executable(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A stand-in `pytest` that acts as pytest does on its positional words: one that names nothing is a usage error (exit 4), else six tests pass.
/// With `LIE` as its first word it says a word that exists is missing, as a repository's own code could print. A stand-in `python3` runs it for
/// `python3 -m pytest`.
fn fake_pytest() -> PathBuf {
    let bin = common::scratch("b834-pytest-bin");
    let body = "if [ \"$1\" = LIE ]; then echo 'ERROR: file or directory not found: tests'; echo; echo 'no tests ran in 0.00s'; exit 4; fi\nfor a; do case \"$a\" in -*) ;; *) if [ ! -e \"$a\" ]; then echo \"ERROR: file or directory not found: $a\"; echo; echo 'no tests ran in 0.00s'; exit 4; fi ;; esac; done\necho '......'; echo '6 passed in 0.01s'; exit 0\n";
    executable(&bin.join("pytest"), &format!("#!/bin/sh\n{body}"));
    executable(
        &bin.join("python3"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" = -m ] && [ \"$2\" = pytest ]; then shift 2; else exit 9; fi\n{body}"
        ),
    );
    bin
}

fn env_for(bin: &Path) -> RunEnv {
    RunEnv {
        sandbox: None,
        work: common::scratch("b834-work"),
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

#[tokio::test(flavor = "multi_thread")]
async fn b834_the_owners_run_reads_six_of_six() {
    let root = owner_tree("b834-owner-run");
    let r = report_of(&root, &fake_pytest()).await;
    let ran: Vec<(&str, Option<i32>)> = r
        .steps
        .iter()
        .map(|s| (s.command.as_str(), s.exit_code))
        .collect();
    assert_eq!(
        ran,
        [
            ("python3 -m pytest -q", Some(0)),
            ("python3 -m pytest -q tests", Some(0)),
            ("pytest -q", Some(0)),
        ],
        "{r:?}"
    );
    assert_eq!(
        r.summary().sentence(),
        "repository tests pass in a clean checkout: 6 of 6."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b834_a_path_the_clean_checkout_lacks_is_said_in_plain_words_and_is_no_failure() {
    let root = tree(
        "b834-missing-path",
        &[
            (
                ".ham/lifecycle/LOG.md",
                &[
                    row("pytest -q tests/integration", 0, 10),
                    row("pytest -q", 0, 6),
                ]
                .concat(),
            ),
            ("tests/test_a.py", "def test_a():\n    pass\n"),
        ],
    );
    let r = report_of(&root, &fake_pytest()).await;
    assert_eq!(r.status, TestReportStatus::Ran);
    assert_eq!(r.steps.len(), 2, "{r:?}");
    let missing = &r.steps[0];
    assert!(missing.is_not_run(), "{missing:?}");
    assert_eq!(
        missing.not_run_reason(),
        Some(
            "could not run as written: the clean checkout has no file or directory `tests/integration`"
        )
    );
    // What pytest printed and its exit code stay with the step.
    assert!(
        missing
            .output_tail
            .contains("ERROR: file or directory not found: tests/integration"),
        "{missing:?}"
    );
    assert_eq!(missing.exit_code, Some(4));
    let s = r.summary();
    assert_eq!(s.failed_command, None, "{s:?}");
    assert_eq!(s.claim(), None);
    assert_eq!(
        s.sentence(),
        "The test commands that ran exited 0 in a clean checkout; the runner reported 6 passed, 0 failed. `pytest -q tests/integration` could not run as written: the clean checkout has no file or directory `tests/integration`."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b834_the_words_of_the_output_alone_do_not_make_a_step_not_run() {
    // The output names `tests`, which the checkout has: the harness's own check fails, so the step stays a failure with its exit code.
    let root = tree(
        "b834-lie",
        &[
            (".ham/lifecycle/LOG.md", &row("pytest LIE tests", 0, 6)),
            ("tests/test_a.py", "def test_a():\n    pass\n"),
            ("LIE", ""),
        ],
    );
    let r = report_of(&root, &fake_pytest()).await;
    assert_eq!(r.steps.len(), 1, "{r:?}");
    assert!(!r.steps[0].is_not_run(), "{r:?}");
    assert_eq!(r.steps[0].exit_code, Some(4));
    assert_eq!(
        r.summary().failed_command.as_deref(),
        Some("pytest LIE tests")
    );
}

// ---- the gates of `not as written here` -----------------------------------------------------------------------------------------------

/// Stand-in `pytest`, `py.test` and `python3` that act as pytest does on its positional words, with the line `message` and the exit code `code` for a word that
/// names nothing; six tests pass otherwise.
fn pytest_standins(message: &str, code: i32) -> PathBuf {
    let bin = common::scratch("b834-pytest-gates-bin");
    let body = format!(
        "for a; do case \"$a\" in -*) ;; *) if [ ! -e \"$a\" ]; then echo \"{message}$a\"; echo; echo 'no tests ran in 0.00s'; exit {code}; fi ;; esac; done\necho '......'; echo '6 passed in 0.01s'; exit 0\n"
    );
    executable(&bin.join("pytest"), &format!("#!/bin/sh\n{body}"));
    executable(&bin.join("py.test"), &format!("#!/bin/sh\n{body}"));
    executable(
        &bin.join("python3"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" = -m ] && [ \"$2\" = pytest ]; then shift 2; else exit 9; fi\n{body}"
        ),
    );
    bin
}

fn gone_tree(label: &str, command: &str) -> PathBuf {
    tree(
        label,
        &[
            (".ham/lifecycle/LOG.md", &row(command, 0, 6)),
            ("tests/test_a.py", "def test_a():\n    pass\n"),
        ],
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn b834_a_word_the_command_does_not_hold_does_not_make_a_step_not_run() {
    // A repository script that is called `pytest` prints the line for a word the command never held, and the checkout lacks that word too. Only the
    // check that the word is one of the command's own words stops the step from being marked: it stays a failure with its exit code.
    let bin = common::scratch("b834-ghost-bin");
    executable(
        &bin.join("pytest"),
        "#!/bin/sh\necho 'ERROR: file or directory not found: ghost'\necho\necho 'no tests ran in 0.00s'\nexit 4\n",
    );
    let root = gone_tree("b834-ghost", "pytest -q tests");
    let r = report_of(&root, &bin).await;
    assert_eq!(r.steps.len(), 1, "{r:?}");
    assert!(!r.steps[0].is_not_run(), "{r:?}");
    assert_eq!(r.steps[0].exit_code, Some(4));
    assert_eq!(
        r.summary().failed_command.as_deref(),
        Some("pytest -q tests")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b834_the_line_with_another_exit_code_than_4_stays_a_failure() {
    // The word is the command's own and the checkout lacks it, but the runner exited 1: that is a result of the run, not pytest's usage error.
    let root = gone_tree("b834-exit1", "pytest -q tests/gone");
    let r = report_of(
        &root,
        &pytest_standins("ERROR: file or directory not found: ", 1),
    )
    .await;
    assert_eq!(r.steps.len(), 1, "{r:?}");
    assert!(!r.steps[0].is_not_run(), "{r:?}");
    assert_eq!(r.steps[0].exit_code, Some(1));
    assert_eq!(
        r.summary().failed_command.as_deref(),
        Some("pytest -q tests/gone")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b834_the_python_form_the_py_test_form_and_the_older_message_are_read_too() {
    for (command, message) in [
        (
            "python3 -m pytest -q tests/gone",
            "ERROR: file or directory not found: ",
        ),
        (
            "py.test -q tests/gone",
            "ERROR: file or directory not found: ",
        ),
        ("pytest -q tests/gone", "ERROR: file not found: "),
    ] {
        let root = gone_tree("b834-forms", command);
        let r = report_of(&root, &pytest_standins(message, 4)).await;
        assert_eq!(r.steps.len(), 1, "{command}: {r:?}");
        assert!(r.steps[0].is_not_run(), "{command}: {r:?}");
        assert_eq!(
            r.steps[0].not_run_reason(),
            Some(
                "could not run as written: the clean checkout has no file or directory `tests/gone`"
            ),
            "{command}"
        );
        assert_eq!(r.steps[0].exit_code, Some(4), "{command}");
    }
}
