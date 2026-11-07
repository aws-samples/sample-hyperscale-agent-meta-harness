// COMPAT-CAH-DATA: this file names the earlier records folder and the earlier variable spellings, which are read until VERSION 6.00.
//! Module `report`: which commands the clean-checkout report runs, and where each comes from. Order of sources: the project's own
//! `.ham/test-command`, the lifecycle log, the goal card, then the manifests of the tree. Nothing here starts a process. The log and the card sit
//! in `.ham/lifecycle` unless `.ham/records` names another folder.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::report::plan::{
    MAX_TEST_STEPS, Plan, PlannedStep, is_focused, is_test_command, parse_log_line, plan,
};
use harness_protocol::direct::{LoggedRun, TestCommandSource, TestStepRole};
use std::path::{Path, PathBuf};

fn tree(label: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = common::scratch(label);
    for (name, body) in files {
        let p = dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    dir
}

fn steps(dir: &Path) -> Vec<PlannedStep> {
    match plan(dir) {
        Plan::Steps(s) => s,
        Plan::NoCommand(why) => panic!("no steps: {why}"),
    }
}

fn commands(dir: &Path) -> Vec<(TestStepRole, TestCommandSource, String)> {
    steps(dir)
        .into_iter()
        .map(|s| (s.role, s.source, s.command))
        .collect()
}

// ---- what counts as a test command -----------------------------------------------------------------------------------------

#[test]
fn test_runner_commands_are_recognised_and_other_commands_are_not() {
    for yes in [
        "python3 -m pytest -q",
        "pytest",
        "cd api && python -m pytest",
        "FOO=1 pytest tests",
        "python3 -m unittest discover",
        "npm test",
        "npm run test",
        "pnpm test",
        "yarn test",
        "npx jest",
        "npx vitest run",
        "node --test",
        "go test ./...",
        "go test -v ./...",
        "cargo test --workspace",
        "cargo nextest run",
        "mvn -B test",
        "./mvnw verify",
        "./gradlew test",
        "make test",
        "sh scripts/test-all.sh",
        "scripts/test-unit.sh",
        "dotnet test",
    ] {
        assert!(is_test_command(yes), "{yes}");
    }
    for no in [
        "ruff check .",
        "npm install",
        "npm ci",
        "git status",
        "curl -s http://localhost:8080/health",
        "echo test",
        "python3 scripts/check.py",
        "cat tests.txt",
        "mvn -B package -DskipTests",
    ] {
        assert!(!is_test_command(no), "{no}");
    }
}

#[test]
fn a_command_that_names_one_test_or_a_filter_is_focused() {
    for yes in [
        "python3 -m pytest tests/test_a.py::test_x",
        "pytest -k parse",
        "go test -run TestA ./...",
        "npx jest --testNamePattern sums",
        "mvn -B test -Dtest=FooTest",
        "python3 -m pytest tests/test_a.py",
        "npx vitest run src/a.test.ts",
        "cargo test --test api",
    ] {
        assert!(is_focused(yes), "{yes}");
    }
    for no in [
        "python3 -m pytest -q",
        "python3 -m pytest tests",
        "npm test",
        "go test ./...",
        "cargo test --workspace",
        "mvn -B test",
    ] {
        assert!(!is_focused(no), "{no}");
    }
}

// ---- the log line ----------------------------------------------------------------------------------------------------------

#[test]
fn a_run_line_of_the_lifecycle_log_gives_the_command_and_what_the_log_said() {
    let (cmd, logged) =
        parse_log_line("RUN | python3 -m pytest -q | exit 0 | pass 12 fail 0 | 12 passed in 0.3s")
            .unwrap();
    assert_eq!(cmd, "python3 -m pytest -q");
    assert_eq!(
        logged,
        LoggedRun {
            exit_code: Some(0),
            passed: Some(12),
            failed: Some(0)
        }
    );
    let (_, unknown) =
        parse_log_line("RUN | npm test | exit 1 | pass unknown fail unknown | tests failed")
            .unwrap();
    assert_eq!(
        unknown,
        LoggedRun {
            exit_code: Some(1),
            passed: None,
            failed: None
        }
    );
}

#[test]
fn a_command_with_a_pipe_in_it_is_read_up_to_the_exit_field() {
    let (cmd, logged) = parse_log_line(
        "RUN | python3 -m pytest -q | tee out.txt | exit 0 | pass 3 fail 0 | 3 passed",
    )
    .unwrap();
    assert_eq!(cmd, "python3 -m pytest -q | tee out.txt");
    assert_eq!(logged.passed, Some(3));
}

#[test]
fn lines_that_are_not_run_lines_give_nothing() {
    for line in [
        "NOT RUN | integration | no docker",
        "RUN | pytest | exit zero | pass 1 fail 0 | ok",
        "RUN pytest exit 0",
        "",
        "# a heading",
        "RUN |  | exit 0 | pass 1 fail 0 | ok",
    ] {
        assert!(parse_log_line(line).is_none(), "{line:?}");
    }
}

// ---- the sources, in order ---------------------------------------------------------------------------------------------------

#[test]
fn the_project_test_command_file_comes_first_with_setup_lines_marked() {
    let dir = tree(
        "plan-file",
        &[
            (
                ".ham/test-command",
                "# how to test this project\n\nsetup: pip install -q -r requirements.txt\npython3 -m pytest -q\n  sh scripts/check.sh  \n",
            ),
            (
                ".ham/lifecycle/LOG.md",
                "RUN | npm test | exit 0 | pass 1 fail 0 | ok\n",
            ),
        ],
    );
    assert_eq!(
        commands(&dir),
        vec![
            (
                TestStepRole::Setup,
                TestCommandSource::TestCommandFile,
                "pip install -q -r requirements.txt".to_string()
            ),
            (
                TestStepRole::Test,
                TestCommandSource::TestCommandFile,
                "python3 -m pytest -q".to_string()
            ),
            (
                TestStepRole::Test,
                TestCommandSource::TestCommandFile,
                "sh scripts/check.sh".to_string()
            ),
        ]
    );
}

#[test]
fn the_test_command_file_is_bounded_in_lines_and_length() {
    let mut body = String::new();
    for i in 0..20 {
        body.push_str(&format!("python3 -m pytest tests/part{i}\n"));
    }
    body.push_str(&format!("python3 -m pytest {}\n", "a".repeat(2_100)));
    let dir = tree("plan-file-big", &[(".ham/test-command", &body)]);
    let s = steps(&dir);
    assert_eq!(
        s.iter().filter(|x| x.role == TestStepRole::Test).count(),
        MAX_TEST_STEPS
    );
    assert!(s.iter().all(|x| x.command.chars().count() <= 2_000));
}

#[test]
fn the_lifecycle_log_gives_the_last_whole_suite_run_and_what_the_log_said_about_it() {
    let log = "\
RUN | python3 -m pytest -q | exit 1 | pass 10 fail 2 | 2 failed, 10 passed in 0.3s
RUN | ruff check . | exit 0 | pass unknown fail unknown | All checks passed
RUN | python3 -m pytest tests/test_a.py::test_x | exit 0 | pass 1 fail 0 | 1 passed
RUN | python3 -m pytest -q | exit 0 | pass 12 fail 0 | 12 passed in 0.3s
NOT RUN | integration | no docker
";
    let dir = tree("plan-log", &[(".ham/lifecycle/LOG.md", log)]);
    let s = steps(&dir);
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!(s[0].role, TestStepRole::Test);
    assert_eq!(s[0].source, TestCommandSource::LifecycleLog);
    assert_eq!(s[0].command, "python3 -m pytest -q");
    assert_eq!(
        s[0].logged,
        Some(LoggedRun {
            exit_code: Some(0),
            passed: Some(12),
            failed: Some(0)
        }),
        "the last run of the command is what the log said"
    );
}

#[test]
fn separate_suites_in_the_log_are_all_kept_in_the_order_of_their_last_run() {
    let log = "\
RUN | npm test | exit 0 | pass 5 fail 0 | ok
RUN | python3 -m pytest -q | exit 0 | pass 7 fail 0 | ok
RUN | npm test | exit 0 | pass 6 fail 0 | ok
";
    let dir = tree("plan-log-two", &[(".ham/lifecycle/LOG.md", log)]);
    let c = commands(&dir);
    assert_eq!(
        c.iter().map(|x| x.2.as_str()).collect::<Vec<_>>(),
        ["python3 -m pytest -q", "npm test"]
    );
}

// ---- where the records are: `.ham/records` moves the log and the goal card, or turns them off ----------------------------

const PYTEST_LOG: &str = "RUN | python3 -m pytest -q | exit 0 | pass 3 fail 0 | 3 passed\n";
const NPM_LOG: &str = "RUN | npm test | exit 0 | pass 1 fail 0 | 1 passed\n";
const GOAL_CARD: &str = "\
SPECIFIC: add a function.
MEASURABLE: the goal is met when every check passes.
1. `go test ./...` -> exit 0
ACHIEVABLE: nothing else changes.
";

#[test]
fn a_repository_without_a_locator_reads_the_hidden_default_and_not_docs() {
    // The records sit in .ham/lifecycle unless the repository says otherwise; a log left in docs/lifecycle by an older run is not read.
    let dir = tree(
        "plan-default-hidden",
        &[
            (".ham/lifecycle/LOG.md", PYTEST_LOG),
            ("docs/lifecycle/LOG.md", NPM_LOG),
        ],
    );
    assert_eq!(
        commands(&dir),
        vec![(
            TestStepRole::Test,
            TestCommandSource::LifecycleLog,
            "python3 -m pytest -q".to_string()
        )],
        "the hidden default is the log; a log in docs/lifecycle is not read without a locator"
    );
    let card = tree(
        "plan-default-hidden-card",
        &[
            (".ham/lifecycle/GOAL.md", GOAL_CARD),
            ("docs/lifecycle/GOAL.md", "1. `npm test` -> exit 0\n"),
        ],
    );
    assert_eq!(
        commands(&card),
        vec![(
            TestStepRole::Test,
            TestCommandSource::GoalCard,
            "go test ./...".to_string()
        )]
    );
}

#[test]
fn the_explicit_docs_layout_gives_the_log_of_docs_and_not_the_hidden_one() {
    let dir = tree(
        "plan-moved-log",
        &[
            (".ham/records", "docs\n"),
            ("docs/lifecycle/LOG.md", PYTEST_LOG),
            (".ham/lifecycle/LOG.md", NPM_LOG),
        ],
    );
    let c = commands(&dir);
    assert_eq!(
        c,
        vec![(
            TestStepRole::Test,
            TestCommandSource::LifecycleLog,
            "python3 -m pytest -q".to_string()
        )],
        "the log of docs is the log; a log left in .ham/lifecycle is stale"
    );
}

#[test]
fn the_hidden_folder_named_in_the_locator_is_the_default() {
    // `.ham` in .ham/records is what a missing file gives (the older opt-in of this repository): the same places, the same log.
    let dir = tree(
        "plan-explicit-hidden",
        &[
            (".ham/records", ".ham\n"),
            (".ham/lifecycle/LOG.md", PYTEST_LOG),
            ("docs/lifecycle/LOG.md", NPM_LOG),
        ],
    );
    assert_eq!(steps(&dir).len(), 1);
    assert_eq!(commands(&dir)[0].2, "python3 -m pytest -q");
}

#[test]
fn a_moved_goal_card_gives_its_checks_when_the_moved_log_gives_none() {
    let dir = tree(
        "plan-moved-goal",
        &[
            (".ham/records", "run-records\n"),
            ("run-records/lifecycle/GOAL.md", GOAL_CARD),
            (".ham/lifecycle/GOAL.md", "1. `npm test` -> exit 0\n"),
        ],
    );
    assert_eq!(
        commands(&dir),
        vec![(
            TestStepRole::Test,
            TestCommandSource::GoalCard,
            "go test ./...".to_string()
        )]
    );
}

#[test]
fn records_off_reads_neither_the_log_nor_the_goal_card_and_falls_through_to_the_manifests() {
    let dir = tree(
        "plan-records-off",
        &[
            (".ham/records", "off\n"),
            (".ham/lifecycle/LOG.md", NPM_LOG),
            (".ham/lifecycle/GOAL.md", GOAL_CARD),
            ("pytest.ini", "[pytest]\n"),
        ],
    );
    let c = commands(&dir);
    assert!(!c.is_empty());
    assert!(
        c.iter().all(|x| x.1 == TestCommandSource::Detected),
        "{c:?}"
    );
    assert!(c.iter().any(|x| x.2.contains("pytest")), "{c:?}");
}

#[test]
fn an_invalid_locator_reads_the_default_places() {
    let dir = tree(
        "plan-records-invalid",
        &[
            (".ham/records", "../outside\n"),
            (".ham/lifecycle/LOG.md", PYTEST_LOG),
        ],
    );
    let s = steps(&dir);
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!(s[0].source, TestCommandSource::LifecycleLog);
}

#[test]
fn the_project_test_command_file_still_comes_first_when_the_records_are_moved_or_off() {
    for records in ["docs\n", ".ham\n", "off\n"] {
        let dir = tree(
            "plan-records-first",
            &[
                (".ham/records", records),
                (".ham/test-command", "make check\n"),
                (".ham/lifecycle/LOG.md", PYTEST_LOG),
            ],
        );
        let c = commands(&dir);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].1, TestCommandSource::TestCommandFile);
    }
}

#[test]
fn a_log_with_only_focused_runs_falls_through_to_the_next_source() {
    let dir = tree(
        "plan-log-focused",
        &[
            (
                ".ham/lifecycle/LOG.md",
                "RUN | python3 -m pytest tests/test_a.py::test_x | exit 0 | pass 1 fail 0 | ok\n",
            ),
            ("go.mod", "module example.com/x\n"),
        ],
    );
    let c = commands(&dir);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].1, TestCommandSource::Detected);
}

#[test]
fn the_goal_card_gives_its_test_checks_when_the_log_gives_none() {
    let goal = "\
SPECIFIC: add a function.
MEASURABLE: the goal is met when every check passes.
1. `python3 -m pytest -q` -> exit 0, 12 passed
2. `curl -s http://localhost:8080/health` -> ok
3. `npm test` -> exit 0
4. `python3 -m pytest tests/test_a.py::test_x` -> exit 0
ACHIEVABLE: nothing else changes.
";
    let dir = tree("plan-goal", &[(".ham/lifecycle/GOAL.md", goal)]);
    assert_eq!(
        commands(&dir),
        vec![
            (
                TestStepRole::Test,
                TestCommandSource::GoalCard,
                "python3 -m pytest -q".to_string()
            ),
            (
                TestStepRole::Test,
                TestCommandSource::GoalCard,
                "npm test".to_string()
            ),
        ]
    );
}

#[test]
fn the_log_wins_over_the_goal_card() {
    let dir = tree(
        "plan-log-goal",
        &[
            (
                ".ham/lifecycle/LOG.md",
                "RUN | go test -v ./... | exit 0 | pass 4 fail 0 | ok\n",
            ),
            (".ham/lifecycle/GOAL.md", "1. `npm test` -> exit 0\n"),
        ],
    );
    let c = commands(&dir);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].1, TestCommandSource::LifecycleLog);
}

// ---- detection -------------------------------------------------------------------------------------------------------------

#[test]
fn a_python_tree_runs_pytest_and_installs_requirements_first_when_it_has_them() {
    let plain = tree(
        "plan-py",
        &[
            ("pytest.ini", "[pytest]\n"),
            ("tests/test_a.py", "def test_a():\n    pass\n"),
        ],
    );
    assert_eq!(
        commands(&plain),
        vec![(
            TestStepRole::Test,
            TestCommandSource::Detected,
            "python3 -m pytest -q".to_string()
        )]
    );
    let with = tree(
        "plan-py-req",
        &[
            ("pyproject.toml", "[tool.pytest.ini_options]\n"),
            ("requirements.txt", "requests==2.32.0\n"),
        ],
    );
    let c = commands(&with);
    assert_eq!(c.len(), 2, "{c:?}");
    assert_eq!(c[0].0, TestStepRole::Setup);
    assert!(
        c[0].2.contains("pip install -q -r requirements.txt"),
        "{}",
        c[0].2
    );
    assert_eq!(
        c[1],
        (
            TestStepRole::Test,
            TestCommandSource::Detected,
            ".ham-venv/bin/python -m pytest -q".to_string()
        )
    );
}

#[test]
fn a_python_tree_with_only_test_files_and_no_runner_setting_names_no_command() {
    let dir = tree(
        "plan-py-bare",
        &[
            ("src/calc.py", "def add(a, b):\n    return a + b\n"),
            ("tests/test_calc.py", "def test_add():\n    pass\n"),
        ],
    );
    assert!(
        matches!(plan(&dir), Plan::NoCommand(_)),
        "a guess from file names is not made"
    );
    let conftest = tree(
        "plan-py-conftest",
        &[("conftest.py", ""), ("tests/test_calc.py", "")],
    );
    assert_eq!(commands(&conftest)[0].2, "python3 -m pytest -q");
}

#[test]
fn a_node_tree_installs_from_the_lockfile_when_there_is_one_and_runs_npm_test() {
    let locked = tree(
        "plan-node",
        &[
            ("package.json", "{\"scripts\": {\"test\": \"jest\"}}"),
            ("package-lock.json", "{}"),
        ],
    );
    assert_eq!(
        commands(&locked),
        vec![
            (
                TestStepRole::Setup,
                TestCommandSource::Detected,
                "npm ci".to_string()
            ),
            (
                TestStepRole::Test,
                TestCommandSource::Detected,
                "npm test".to_string()
            ),
        ]
    );
    let unlocked = tree(
        "plan-node2",
        &[("package.json", "{\"scripts\": {\"test\": \"vitest run\"}}")],
    );
    assert_eq!(commands(&unlocked)[0].2, "npm install");
}

#[test]
fn the_placeholder_test_script_of_a_new_node_package_is_not_a_test_command() {
    let dir = tree(
        "plan-node-none",
        &[(
            "package.json",
            "{\"scripts\": {\"test\": \"echo \\\"Error: no test specified\\\" && exit 1\"}}",
        )],
    );
    assert!(matches!(plan(&dir), Plan::NoCommand(_)));
    let none = tree("plan-node-none2", &[("package.json", "{\"name\": \"x\"}")]);
    assert!(matches!(plan(&none), Plan::NoCommand(_)));
}

#[test]
fn go_rust_maven_and_gradle_trees_get_their_own_command() {
    assert_eq!(
        commands(&tree("plan-go", &[("go.mod", "module x\n")]))[0].2,
        "go test -v ./..."
    );
    assert_eq!(
        commands(&tree("plan-rs", &[("Cargo.toml", "[package]\n")]))[0].2,
        "cargo test"
    );
    assert_eq!(
        commands(&tree("plan-mvn", &[("pom.xml", "<project/>")]))[0].2,
        "mvn -B test"
    );
    assert_eq!(
        commands(&tree(
            "plan-mvnw",
            &[("pom.xml", "<project/>"), ("mvnw", "#!/bin/sh\n")]
        ))[0]
            .2,
        "./mvnw -B test"
    );
    assert_eq!(
        commands(&tree(
            "plan-gradle",
            &[("build.gradle", "plugins {}"), ("gradlew", "#!/bin/sh\n")]
        ))[0]
            .2,
        "./gradlew test"
    );
}

#[test]
fn a_project_entry_script_is_used_instead_of_guessing_from_manifests() {
    let dir = tree(
        "plan-entry",
        &[
            ("scripts/test-all.sh", "#!/bin/sh\n"),
            ("go.mod", "module x\n"),
            ("pytest.ini", "[pytest]\n"),
        ],
    );
    assert_eq!(
        commands(&dir),
        vec![(
            TestStepRole::Test,
            TestCommandSource::Detected,
            "sh scripts/test-all.sh".to_string()
        )]
    );
}

#[test]
fn several_ecosystems_at_the_root_each_get_their_steps() {
    let dir = tree(
        "plan-multi",
        &[
            ("package.json", "{\"scripts\": {\"test\": \"jest\"}}"),
            ("package-lock.json", "{}"),
            ("go.mod", "module x\n"),
        ],
    );
    let c: Vec<String> = commands(&dir).into_iter().map(|x| x.2).collect();
    assert_eq!(c, ["npm ci", "npm test", "go test -v ./..."]);
}

#[test]
fn a_tree_that_names_no_test_command_says_so() {
    let dir = tree("plan-empty", &[("README.md", "hello\n")]);
    match plan(&dir) {
        Plan::NoCommand(why) => assert!(why.contains("no test command"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        plan(&common::scratch("plan-empty2")),
        Plan::NoCommand(_)
    ));
}

#[test]
fn a_missing_or_unreadable_source_is_skipped_and_never_an_error() {
    let dir = tree(
        "plan-odd",
        &[
            (
                ".ham/lifecycle/LOG.md",
                "\u{0}\u{1}garbage without a run line\n",
            ),
            (".ham/test-command", "\n\n# only comments\n"),
        ],
    );
    assert!(matches!(plan(&dir), Plan::NoCommand(_)));
}

// ---- how many members `cargo test` runs at the root -----------------------------------------------------------------------------
//
// For a Cargo workspace the detected command is `cargo test`. The report says it was chosen from the manifest and how many members it runs, so
// the number has to be what cargo selects: all members of a virtual manifest, its `default-members` when it names some, and only the root package
// when the manifest has one. Anything the reading is not sure of gives no number.

use agentd::direct::report::plan::cargo_workspace_members;

const PKG: &str = "[package]\nname = \"m\"\nversion = \"0.1.0\"\n";

fn members_of(label: &str, files: &[(&str, &str)]) -> Option<u32> {
    cargo_workspace_members(&tree(label, files).canonicalize().unwrap())
}

#[test]
fn a_virtual_workspace_runs_every_member_it_lists() {
    let n = members_of(
        "ws-explicit",
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"a\", \"b\", \"crates/c\"]\n",
            ),
            ("a/Cargo.toml", PKG),
            ("b/Cargo.toml", PKG),
            ("crates/c/Cargo.toml", PKG),
        ],
    );
    assert_eq!(n, Some(3));
}

#[test]
fn a_star_at_the_end_of_a_member_path_lists_the_folders_that_hold_a_manifest() {
    let n = members_of(
        "ws-glob",
        &[
            (
                "Cargo.toml",
                "[workspace]\nresolver = \"3\"\nmembers = [\"crates/*\"]\n",
            ),
            ("crates/a/Cargo.toml", PKG),
            ("crates/b/Cargo.toml", PKG),
            ("crates/c/Cargo.toml", PKG),
            ("crates/notes/README.md", "no manifest here\n"),
            ("crates/loose.txt", "a file, no folder\n"),
        ],
    );
    assert_eq!(n, Some(3));
}

#[test]
fn excluded_members_are_not_counted_whether_they_were_listed_or_matched() {
    let listed = members_of(
        "ws-exclude-listed",
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nexclude = [\"b\"]\n",
            ),
            ("a/Cargo.toml", PKG),
            ("b/Cargo.toml", PKG),
            ("c/Cargo.toml", PKG),
        ],
    );
    assert_eq!(listed, Some(2));
    let matched = members_of(
        "ws-exclude-matched",
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/old/\"]\n",
            ),
            ("crates/a/Cargo.toml", PKG),
            ("crates/b/Cargo.toml", PKG),
            ("crates/old/Cargo.toml", PKG),
        ],
    );
    assert_eq!(matched, Some(2));
}

#[test]
fn default_members_decide_what_cargo_test_runs() {
    let files = [
        ("a/Cargo.toml", PKG),
        ("b/Cargo.toml", PKG),
        ("c/Cargo.toml", PKG),
    ];
    let mut two = vec![(
        "Cargo.toml",
        "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\ndefault-members = [\"a\", \"c\"]\n",
    )];
    two.extend(files);
    assert_eq!(members_of("ws-default-two", &two), Some(2));
    let mut one = vec![(
        "Cargo.toml",
        "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\ndefault-members = [\"b\"]\n",
    )];
    one.extend(files);
    assert_eq!(
        members_of("ws-default-one", &one),
        None,
        "one member is no workspace run"
    );
}

#[test]
fn a_root_package_with_a_workspace_runs_only_itself_unless_default_members_say_more() {
    let manifest = format!("{PKG}\n[workspace]\nmembers = [\"a\", \"b\"]\n");
    let n = members_of(
        "ws-root-package",
        &[
            ("Cargo.toml", &manifest),
            ("a/Cargo.toml", PKG),
            ("b/Cargo.toml", PKG),
        ],
    );
    assert_eq!(n, None);
    let with_default =
        format!("{PKG}\n[workspace]\nmembers = [\"a\", \"b\"]\ndefault-members = [\".\", \"a\"]\n");
    let m = members_of(
        "ws-root-package-default",
        &[
            ("Cargo.toml", &with_default),
            ("a/Cargo.toml", PKG),
            ("b/Cargo.toml", PKG),
        ],
    );
    assert_eq!(m, Some(2));
    // The package table may come after the workspace table.
    let after = format!("[workspace]\nmembers = [\"a\", \"b\"]\n\n{PKG}");
    let k = members_of(
        "ws-package-after",
        &[
            ("Cargo.toml", &after),
            ("a/Cargo.toml", PKG),
            ("b/Cargo.toml", PKG),
        ],
    );
    assert_eq!(k, None);
}

#[test]
fn a_tree_without_a_workspace_or_with_one_member_gives_no_number() {
    assert_eq!(members_of("ws-none", &[("Cargo.toml", PKG)]), None);
    assert_eq!(members_of("ws-no-manifest", &[("README.md", "x\n")]), None);
    assert_eq!(
        members_of(
            "ws-one",
            &[
                ("Cargo.toml", "[workspace]\nmembers = [\"a\"]\n"),
                ("a/Cargo.toml", PKG),
            ],
        ),
        None
    );
    // A member that is listed but has no manifest is not counted: cargo would refuse the workspace.
    assert_eq!(
        members_of(
            "ws-missing-member",
            &[
                ("Cargo.toml", "[workspace]\nmembers = [\"a\", \"gone\"]\n"),
                ("a/Cargo.toml", PKG),
            ],
        ),
        None
    );
}

#[test]
fn comments_single_quotes_trailing_commas_and_long_arrays_are_read() {
    let manifest = "# the workspace\n[workspace] # members follow\nresolver = \"3\"\nmembers = [ # all of them\n  \"a\", # first\n  'b',\n  \"c\", # the # in this comment is not a member \"d\"\n]\n";
    let n = members_of(
        "ws-comments",
        &[
            ("Cargo.toml", manifest),
            ("a/Cargo.toml", PKG),
            ("b/Cargo.toml", PKG),
            ("c/Cargo.toml", PKG),
            ("d/Cargo.toml", PKG),
        ],
    );
    assert_eq!(n, Some(3));
    // Keys of other tables with the same names are not the workspace's.
    let other = "[workspace]\nmembers = [\"a\", \"b\"]\n\n[workspace.metadata.docs]\nmembers = [\"x\", \"y\", \"z\"]\n\n[profile.dev]\nexclude = [\"a\"]\n";
    let m = members_of(
        "ws-other-tables",
        &[
            ("Cargo.toml", other),
            ("a/Cargo.toml", PKG),
            ("b/Cargo.toml", PKG),
            ("x/Cargo.toml", PKG),
            ("y/Cargo.toml", PKG),
            ("z/Cargo.toml", PKG),
        ],
    );
    assert_eq!(m, Some(2));
}

#[test]
fn dotted_keys_at_the_top_are_read_and_a_dotted_package_key_means_a_package() {
    let dotted = "workspace.members = [\"a\", \"b\"]\n";
    let n = members_of(
        "ws-dotted",
        &[
            ("Cargo.toml", dotted),
            ("a/Cargo.toml", PKG),
            ("b/Cargo.toml", PKG),
        ],
    );
    assert_eq!(n, Some(2));
    let with_package = "package.name = \"root\"\nworkspace.members = [\"a\", \"b\"]\n";
    let m = members_of(
        "ws-dotted-package",
        &[
            ("Cargo.toml", with_package),
            ("a/Cargo.toml", PKG),
            ("b/Cargo.toml", PKG),
        ],
    );
    assert_eq!(m, None);
}

#[test]
fn a_member_pattern_the_reading_does_not_know_gives_no_number() {
    for pattern in [
        "crates/**",
        "crates/*-api",
        "cr*/a",
        "crates/[ab]",
        "crates/?",
    ] {
        let manifest = format!("[workspace]\nmembers = [\"{pattern}\", \"z\"]\n");
        let n = members_of(
            &format!("ws-pattern-{}", pattern.len()),
            &[
                ("Cargo.toml", &manifest),
                ("crates/a/Cargo.toml", PKG),
                ("crates/b/Cargo.toml", PKG),
                ("z/Cargo.toml", PKG),
            ],
        );
        assert_eq!(n, None, "{pattern}");
    }
}

#[test]
fn a_member_path_that_leaves_the_tree_is_not_counted() {
    // A folder next to the tree holds a manifest; listed by a relative path with `..` and by its absolute path, it is still not a member.
    let base = tree(
        "ws-escape",
        &[
            ("Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\n"),
            ("a/Cargo.toml", PKG),
            ("b/Cargo.toml", PKG),
        ],
    );
    let outside = base.parent().unwrap().join("ws-escape-outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("Cargo.toml"), PKG).unwrap();
    let manifest = format!(
        "[workspace]\nmembers = [\"a\", \"b\", \"../ws-escape-outside\", \"{}\"]\n",
        outside.display()
    );
    std::fs::write(base.join("Cargo.toml"), manifest).unwrap();
    assert_eq!(
        cargo_workspace_members(&base.canonicalize().unwrap()),
        Some(2)
    );
}

#[test]
fn a_manifest_that_is_not_readable_gives_no_number() {
    assert_eq!(
        members_of(
            "ws-broken",
            &[("Cargo.toml", "[workspace\nmembers = [\"a\", \"b\"\n")]
        ),
        None
    );
    assert_eq!(members_of("ws-empty", &[("Cargo.toml", "")]), None);
}

#[test]
fn arrays_over_several_lines_in_tables_the_reading_does_not_use_are_skipped() {
    // `keywords` and `categories` often run over several lines; their lines have no `=` of their own.
    let manifest = "[package]\nname = \"root\"\nkeywords = [\n  \"a\",\n  \"b\",\n]\n\n[workspace]\nmembers = [\"a\", \"b\"]\ndefault-members = [\"a\", \"b\"]\n\n[workspace.package]\ncategories = [\n  \"x\", # one\n]\n\n[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\nplugins = [\n  \"p\",\n  \"q\",\n]\n";
    let n = members_of(
        "ws-multiline-other",
        &[
            ("Cargo.toml", manifest),
            ("a/Cargo.toml", PKG),
            ("b/Cargo.toml", PKG),
        ],
    );
    assert_eq!(n, Some(2));
    // A string that holds a bracket does not open an array.
    let text = "[workspace]\nmembers = [\"a\", \"b\"]\n\n[workspace.package]\ndescription = \"see [the docs\"\n";
    let m = members_of(
        "ws-bracket-in-string",
        &[
            ("Cargo.toml", text),
            ("a/Cargo.toml", PKG),
            ("b/Cargo.toml", PKG),
        ],
    );
    assert_eq!(m, Some(2));
}

// ---- the earlier folder (COMPAT-CAH-DATA) ---------------------------------------------------------------------------------------

#[test]
fn the_earlier_test_command_file_is_read_when_the_new_one_is_absent() {
    let dir = tree(
        "plan-earlier-file",
        &[(".cah/test-command", "setup: make deps\nmake check\n")],
    );
    assert_eq!(
        commands(&dir),
        vec![
            (
                TestStepRole::Setup,
                TestCommandSource::TestCommandFile,
                "make deps".to_string()
            ),
            (
                TestStepRole::Test,
                TestCommandSource::TestCommandFile,
                "make check".to_string()
            ),
        ]
    );
    // The new file wins when both are there.
    let dir = tree(
        "plan-both-files",
        &[
            (".ham/test-command", "pytest -q\n"),
            (".cah/test-command", "make check\n"),
        ],
    );
    let got = commands(&dir);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].2, "pytest -q");
}

#[test]
fn the_log_and_the_goal_card_of_the_earlier_folder_give_their_commands() {
    let dir = tree(
        "plan-earlier-log",
        &[(
            ".cah/lifecycle/LOG.md",
            "RUN | npm test | exit 0 | pass 1 fail 0 | ok\n",
        )],
    );
    let got = commands(&dir);
    assert!(got.iter().any(|c| c.2 == "npm test"), "{got:?}");
}
