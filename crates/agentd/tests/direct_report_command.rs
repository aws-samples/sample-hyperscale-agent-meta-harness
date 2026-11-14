//! The clean-checkout test report reads a command before it runs it. The report takes the commands of the run's lifecycle log
//! (`RUN | <command> | exit N | pass P fail F | <summary>`), and a model writes notes into that column: `python3 -m unittest pkg.test_mod
//! (3 tests)`. Handed to `sh -c` as written, that line ends in `syntax error near unexpected token '('` and exit 2, and the card said "exited 2; the
//! runner's test counts could not be read" for a suite that passes (seen in a live run).
//!
//! These cases go through the public planner and the public step runner. The program the steps start is a stand-in called `python3` that prints the
//! arguments it was given, so the cases show what reached it. Plain commands run with a search path that holds no `sh` at all: nothing may
//! start a shell for them.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::report::plan::{Plan, PlannedStep, plan};
use agentd::direct::report::run::{RunEnv, run_step, setup_stops_the_run};
use harness_protocol::direct::{
    LoggedRun, TestCommandSource, TestCounts, TestReport, TestReportStatus, TestStep, TestStepRole,
};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Prints every argument, the value of `MODE`, the directory it runs in and a passing `unittest` summary. It uses shell builtins only.
const STAND_IN: &str = "#!/bin/sh\nfor a in \"$@\"; do echo \"ARG[$a]\"; done\necho \"MODE=${MODE-unset}\"\npwd\necho \"Ran 3 tests in 0.002s\"\necho\necho OK\n";

fn stand_in_dir(work: &Path) -> PathBuf {
    let bin = work.join("stand-in");
    std::fs::create_dir_all(&bin).unwrap();
    for (name, body) in [
        ("python3", STAND_IN),
        ("echo", "#!/bin/sh\nprintf STUB-ECHO\n"),
    ] {
        let program = bin.join(name);
        std::fs::write(&program, body).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// The stand-in first; with `system` the usual directories after it (`sh`, `tail`, `cd` lines need them), without it nothing else: no shell.
fn env_with(work: &Path, system: bool) -> RunEnv {
    let mut path = vec![stand_in_dir(work)];
    if system {
        path.extend([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    }
    RunEnv {
        sandbox: None,
        work: work.to_path_buf(),
        run_as: None,
        path,
        engine_env: BTreeMap::new(),
        engine_home: None,
        cargo_config: None,
    }
}

fn step_of(command: &str) -> PlannedStep {
    PlannedStep {
        role: TestStepRole::Test,
        source: TestCommandSource::LifecycleLog,
        command: command.to_string(),
        logged: None,
        install: None,
        fallback: None,
    }
}

async fn run_in(env: &RunEnv, cwd: &Path, step: &PlannedStep) -> TestStep {
    run_step(env, step, cwd, Duration::from_secs(30), 4096).await
}

/// Runs `command` in a fresh checkout directory with the stand-in; `system` as in [`env_with`].
async fn run(label: &str, command: &str, system: bool) -> TestStep {
    let work = common::scratch(label);
    let cwd = work.join("checkout");
    std::fs::create_dir_all(cwd.join("sub")).unwrap();
    run_in(&env_with(&work, system), &cwd, &step_of(command)).await
}

fn args_of(step: &TestStep) -> Vec<String> {
    step.output_tail
        .lines()
        .filter_map(|l| l.strip_prefix("ARG["))
        .filter_map(|l| l.strip_suffix(']'))
        .map(str::to_string)
        .collect()
}

fn log_tree(label: &str, log: &str) -> PathBuf {
    let dir = common::scratch(label);
    let file = dir.join(".ham/lifecycle/LOG.md");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, log).unwrap();
    dir
}

fn planned(dir: &Path) -> Vec<PlannedStep> {
    match plan(dir) {
        Plan::Steps(s) => s,
        Plan::NoCommand(why) => panic!("no steps: {why}"),
    }
}

fn unreadable(s: &TestStep, command: &str) {
    assert!(s.is_not_run(), "{s:?}");
    assert_eq!(
        s.exit_code, None,
        "an unreadable command has no exit code: {s:?}"
    );
    assert_eq!(s.counts, None, "{s:?}");
    assert_eq!(
        s.not_run_reason(),
        Some("could not read the command"),
        "{s:?}"
    );
    assert!(
        s.output_tail
            .starts_with("NOT RUN: could not read the command\n"),
        "{:?}",
        s.output_tail
    );
    assert_eq!(s.command, command, "the step keeps the command text");
}

// ---- the line from the judged run ------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_line_from_the_judged_run_is_planned_without_its_note_and_runs_with_the_real_counts() {
    let dir = log_tree(
        "cmd-evidence",
        "RUN | python3 -m unittest pkg.test_mod (3 tests) | exit 0 | pass 3 fail 0 | OK\n",
    );
    let steps = planned(&dir);
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert_eq!(steps[0].command, "python3 -m unittest pkg.test_mod");
    assert_eq!(
        steps[0].logged,
        Some(LoggedRun {
            exit_code: Some(0),
            passed: Some(3),
            failed: Some(0)
        })
    );
    let work = common::scratch("cmd-evidence-run");
    let s = run_in(&env_with(&work, true), &dir, &steps[0]).await;
    assert_eq!(s.exit_code, Some(0), "{s:?}");
    assert_eq!(
        args_of(&s),
        ["-m", "unittest", "pkg.test_mod"],
        "the note never reaches the program"
    );
    assert_eq!(
        s.counts,
        Some(TestCounts {
            passed: 3,
            failed: 0,
            skipped: 0
        })
    );
    assert_eq!(s.counts_from.as_deref(), Some("unittest"));
}

#[test]
fn notes_in_other_wordings_are_dropped_and_one_command_stays_one_step() {
    let dir = log_tree(
        "cmd-notes",
        "\
RUN | python3 -m unittest pkg.test_mod (3 tests) | exit 0 | pass 3 fail 0 | OK
RUN | python3 -m unittest pkg.test_mod (final run after the last edit) | exit 0 | pass 3 fail 0 | OK
RUN | npm test # all green | exit 0 | pass 3 fail 0 | ok
RUN | cargo test --workspace -> exit 0 | exit 0 | pass 9 fail 0 | ok
RUN | python3 -m pytest -q | 12 tests | exit 0 | pass 12 fail 0 | 12 passed
",
    );
    let commands: Vec<String> = planned(&dir).into_iter().map(|s| s.command).collect();
    assert_eq!(
        commands,
        [
            "python3 -m unittest pkg.test_mod",
            "npm test",
            "cargo test --workspace",
            "python3 -m pytest -q"
        ]
    );
}

#[test]
fn shell_syntax_in_the_command_column_is_kept_as_written() {
    let dir = log_tree(
        "cmd-syntax",
        "\
RUN | cd api && python3 -m pytest -q 2>&1 | tail -5 | exit 0 | pass 4 fail 0 | ok
RUN | python3 -m pytest -q | tee out.txt | exit 0 | pass 3 fail 0 | 3 passed
",
    );
    let commands: Vec<String> = planned(&dir).into_iter().map(|s| s.command).collect();
    assert_eq!(
        commands,
        [
            "cd api && python3 -m pytest -q 2>&1 | tail -5",
            "python3 -m pytest -q | tee out.txt"
        ]
    );
}

// ---- a plain command is an argument vector, not a shell line --------------------------------------------------------------------

#[tokio::test]
async fn a_plain_command_runs_without_a_shell_and_keeps_its_quoted_words() {
    // The search path holds the stand-in and nothing else: there is no `sh` to start.
    let s = run(
        "cmd-plain",
        "python3 -m unittest 'dir with space/test x' plain\\ word",
        false,
    )
    .await;
    assert_eq!(s.exit_code, Some(0), "{s:?}");
    assert_eq!(
        args_of(&s),
        ["-m", "unittest", "dir with space/test x", "plain word"]
    );
}

#[tokio::test]
async fn an_environment_prefix_reaches_the_program_without_a_shell() {
    let s = run("cmd-env", "MODE=\"fast one\" python3 -m unittest x", false).await;
    assert_eq!(s.exit_code, Some(0), "{s:?}");
    assert!(
        s.output_tail.contains("MODE=fast one\n"),
        "{:?}",
        s.output_tail
    );
    assert_eq!(args_of(&s), ["-m", "unittest", "x"]);
}

#[tokio::test]
async fn a_comment_at_the_end_is_not_an_argument() {
    let s = run(
        "cmd-comment",
        "python3 -m unittest x # the whole suite",
        false,
    )
    .await;
    assert_eq!(s.exit_code, Some(0), "{s:?}");
    assert_eq!(args_of(&s), ["-m", "unittest", "x"]);
}

#[tokio::test]
async fn a_word_with_a_hash_inside_is_one_word() {
    let s = run("cmd-hash", "python3 -m unittest suite#1", false).await;
    assert_eq!(args_of(&s), ["-m", "unittest", "suite#1"]);
}

// ---- shell syntax the line clearly intends still goes to the shell --------------------------------------------------------------

#[tokio::test]
async fn a_cd_and_an_and_still_go_through_the_shell() {
    let s = run("cmd-cd", "cd sub && python3 -m unittest x", true).await;
    assert_eq!(s.exit_code, Some(0), "{s:?}");
    assert!(
        s.output_tail.lines().any(|l| l.ends_with("/checkout/sub")),
        "{:?}",
        s.output_tail
    );
    assert_eq!(args_of(&s), ["-m", "unittest", "x"]);
}

#[tokio::test]
async fn a_pipe_to_tail_still_goes_through_the_shell() {
    let s = run("cmd-pipe", "python3 -m unittest x 2>&1 | tail -3", true).await;
    assert_eq!(s.exit_code, Some(0), "{s:?}");
    assert_eq!(
        s.counts,
        Some(TestCounts {
            passed: 3,
            failed: 0,
            skipped: 0
        })
    );
    assert!(
        args_of(&s).is_empty(),
        "tail -3 keeps the end: {:?}",
        s.output_tail
    );
}

#[tokio::test]
async fn a_quoted_operator_is_a_word_and_a_variable_in_double_quotes_goes_to_the_shell() {
    let quoted = run("cmd-quoted", "python3 -m unittest 'a && b; c | d'", false).await;
    assert_eq!(args_of(&quoted), ["-m", "unittest", "a && b; c | d"]);
    // A variable is the shell's to expand, so a word that holds one is shell syntax and the line goes to the shell.
    let var = run("cmd-var", "cd sub && python3 -m unittest \"$PWD\"", true).await;
    let got = args_of(&var);
    assert_eq!(got.len(), 3, "{got:?}");
    assert!(got[2].ends_with("/checkout/sub"), "{got:?}");
}

// ---- a command that cannot be read is not run ------------------------------------------------------------------------------------

#[tokio::test]
async fn an_empty_command_is_not_run() {
    for command in ["", "   ", "# only a note"] {
        let s = run("cmd-empty", command, true).await;
        unreadable(&s, command);
    }
}

#[tokio::test]
async fn a_command_with_an_unbalanced_quote_is_not_run() {
    for command in [
        "python3 -m unittest \"scripts.tests.test_a",
        "python3 -m unittest 'scripts.tests.test_a",
    ] {
        let s = run("cmd-quote", command, true).await;
        unreadable(&s, command);
        assert!(
            args_of(&s).is_empty(),
            "nothing was started: {:?}",
            s.output_tail
        );
    }
}

#[tokio::test]
async fn text_the_shell_cannot_read_is_not_run_and_is_no_exit_code_of_the_suite() {
    // Not from the log column, so no note is cut: the line is run as written, and the shell would say "syntax error" and exit 2.
    let command = "python3 -m unittest pkg.test_mod (3 tests)";
    let s = run("cmd-syntax-err", command, true).await;
    unreadable(&s, command);
    assert!(
        s.output_tail.lines().count() >= 2,
        "the second line says what the reader found: {:?}",
        s.output_tail
    );
}

#[tokio::test]
async fn an_unreadable_command_is_not_a_failure_in_the_summary_of_the_report() {
    let command = "python3 -m unittest pkg.test_mod (3 tests)";
    let s = run("cmd-summary", command, true).await;
    let mut report =
        TestReport::without_steps("01TESTRUN", &"0".repeat(40), TestReportStatus::Ran, None);
    report.steps.push(s);
    let summary = report.summary();
    assert_eq!(summary.claim(), None);
    assert_eq!(summary.failed_command, None);
    assert_eq!(summary.exit_code, None);
    assert!(!summary.all_exit_zero);
    let sentence = summary.sentence();
    assert!(sentence.starts_with("NOT RUN: "), "{sentence}");
    assert!(
        sentence.contains("could not read the command"),
        "{sentence}"
    );
    assert!(sentence.contains("(3 tests)"), "{sentence}");
    assert!(!sentence.contains("exited"), "{sentence}");
}

#[tokio::test]
async fn a_program_that_is_not_installed_keeps_its_own_wording() {
    let s = run(
        "cmd-missing",
        "no-such-tool-for-the-report test --workspace",
        true,
    )
    .await;
    assert!(s.is_not_run(), "{s:?}");
    assert_eq!(
        s.exit_code,
        Some(127),
        "the attempt's own code stays: {s:?}"
    );
    assert_eq!(
        s.not_run_reason(),
        Some("no-such-tool-for-the-report is not installed here")
    );
}

// ---- review of 2026-10-25: what the shell does that a direct start does not -------------------------------------------------------

#[tokio::test]
async fn a_shell_builtin_is_the_builtin_even_when_a_file_of_that_name_is_on_the_path() {
    // The stand-in directory holds an `echo` that prints STUB-ECHO; the shell's own `echo` is what the command always ran.
    let s = run("cmd-builtin", "echo hi", true).await;
    assert_eq!(s.exit_code, Some(0), "{s:?}");
    assert!(s.output_tail.contains("hi"), "{:?}", s.output_tail);
    assert!(!s.output_tail.contains("STUB-ECHO"), "{:?}", s.output_tail);
}

#[tokio::test]
async fn an_executable_script_with_no_interpreter_line_still_runs_through_the_shell() {
    let work = common::scratch("cmd-no-shebang");
    let cwd = work.join("checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    let script = cwd.join("run-tests.sh");
    std::fs::write(&script, "echo script-ran-by-the-shell\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let s = run_in(&env_with(&work, true), &cwd, &step_of("./run-tests.sh")).await;
    assert_eq!(s.exit_code, Some(0), "{s:?}");
    assert!(
        s.output_tail.contains("script-ran-by-the-shell"),
        "{:?}",
        s.output_tail
    );
}

#[tokio::test]
async fn a_script_with_an_interpreter_line_runs_directly_by_its_path_in_the_checkout() {
    let work = common::scratch("cmd-shebang");
    let cwd = work.join("checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    let script = cwd.join("run-tests.sh");
    std::fs::write(&script, "#!/bin/sh\necho script-with-shebang \"$1\"\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    // No shell on the search path: only a direct start can run it.
    let s = run_in(
        &env_with(&work, false),
        &cwd,
        &step_of("./run-tests.sh 'two words'"),
    )
    .await;
    assert_eq!(s.exit_code, Some(0), "{s:?}");
    assert!(
        s.output_tail.contains("script-with-shebang two words"),
        "{:?}",
        s.output_tail
    );
}

#[tokio::test]
async fn a_failure_of_the_privilege_change_is_not_a_syntax_error() {
    // With a user to run as, the start goes through `setpriv`, which fails (exit 1, a message on standard error) for a user who may not drop
    // groups. The syntax check holds no such change: that failure must not read as "could not read the command". As root nothing fails.
    use std::os::unix::fs::MetadataExt;
    let work = common::scratch("cmd-setpriv");
    let cwd = work.join("checkout");
    std::fs::create_dir_all(cwd.join("sub")).unwrap();
    let me = std::fs::metadata(&cwd).unwrap();
    let mut env = env_with(&work, true);
    env.run_as = Some((me.uid(), me.gid()));
    let s = run_in(&env, &cwd, &step_of("cd sub && python3 -m unittest x")).await;
    assert_ne!(
        s.not_run_reason(),
        Some("could not read the command"),
        "{s:?}"
    );
}

// ---- a setup that cannot be read ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_setup_that_cannot_be_read_ends_the_run_of_steps_like_a_failed_setup() {
    // The tests after an install that did not happen would fail for want of what the install brings: a red result the repository did not earn.
    let work = common::scratch("cmd-setup");
    let cwd = work.join("checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    let env = env_with(&work, true);
    let setup = |command: &str| PlannedStep {
        role: TestStepRole::Setup,
        ..step_of(command)
    };
    let unread = run_in(
        &env,
        &cwd,
        &setup("pip install -q -r requirements.txt (see the notes)"),
    )
    .await;
    assert!(unread.is_not_run(), "{unread:?}");
    assert!(setup_stops_the_run(&unread), "{unread:?}");
    let failed = run_in(&env, &cwd, &setup("exit 3")).await;
    assert!(setup_stops_the_run(&failed), "{failed:?}");
    let passed = run_in(&env, &cwd, &setup("python3 -m unittest x")).await;
    assert!(!setup_stops_the_run(&passed), "{passed:?}");
    // A setup whose program the session lacks was not run for want of a service: it did not fail, and the tests go on.
    let missing = run_in(&env, &cwd, &setup("no-such-tool-for-the-report fetch")).await;
    assert!(missing.is_not_run(), "{missing:?}");
    assert!(!setup_stops_the_run(&missing), "{missing:?}");
    // A test step that cannot be read is no setup.
    let test = run_in(&env, &cwd, &step_of("python3 -m unittest \"unclosed")).await;
    assert!(test.is_not_run(), "{test:?}");
    assert!(!setup_stops_the_run(&test), "{test:?}");
}
