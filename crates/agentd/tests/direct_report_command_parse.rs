//! How the clean-checkout test report reads a command (`report/command.rs`). A command is parsed before it runs: words, quotes,
//! `NAME=value` prefixes and a trailing comment make an argument vector; unquoted shell syntax (`&&`, `;`, `|`, redirects, `$`, globs, a
//! subshell) makes a shell line; text that cannot be read at all (empty, an unclosed quote, a trailing backslash) is `Unreadable`.
//! `strip_notes` cuts the notes a model writes after the command in the lifecycle log's command column; it is used for that column only.
//! Nothing here starts a process.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::report::command::{
    Parsed, classify, program_found, program_found_as, runs_directly, strip_notes,
};
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

fn plain(command: &str) -> (Vec<(String, String)>, Vec<String>) {
    match classify(command) {
        Ok(Parsed::Plain { env, argv }) => (env, argv),
        other => panic!("{command:?} should be plain, got {other:?}"),
    }
}

fn argv(command: &str) -> Vec<String> {
    plain(command).1
}

fn is_shell(command: &str) -> bool {
    matches!(classify(command), Ok(Parsed::Shell))
}

fn is_unreadable(command: &str) -> bool {
    classify(command).is_err()
}

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

// ---- classify -----------------------------------------------------------------------------------------------------------------

#[test]
fn words_are_an_argument_vector() {
    assert_eq!(
        argv("python3 -m unittest pkg.test_mod"),
        words(&["python3", "-m", "unittest", "pkg.test_mod"])
    );
    assert_eq!(argv("  npm   test  "), words(&["npm", "test"]));
    assert_eq!(argv("./gradlew test"), words(&["./gradlew", "test"]));
    assert_eq!(
        argv("cargo test --workspace --no-fail-fast"),
        words(&["cargo", "test", "--workspace", "--no-fail-fast"])
    );
}

#[test]
fn quotes_group_words_and_keep_what_they_hold() {
    assert_eq!(
        argv("pytest -k 'a and b' \"c d\" e\\ f"),
        words(&["pytest", "-k", "a and b", "c d", "e f"])
    );
    assert_eq!(
        argv("mytool 'a && b; c | d > e'"),
        words(&["mytool", "a && b; c | d > e"])
    );
    assert_eq!(
        argv("mytool \"say \\\"hi\\\"\""),
        words(&["mytool", "say \"hi\""])
    );
    assert_eq!(argv("mytool ''"), words(&["mytool", ""]));
    assert_eq!(argv("mytool a'b'\"c\""), words(&["mytool", "abc"]));
}

#[test]
fn name_value_prefixes_are_the_programs_environment() {
    let (env, argv) = plain("CI=1 MODE=\"fast one\" pytest -q");
    assert_eq!(
        env,
        vec![
            ("CI".to_string(), "1".to_string()),
            ("MODE".to_string(), "fast one".to_string())
        ]
    );
    assert_eq!(argv, words(&["pytest", "-q"]));
    // An equals sign after the program is an argument.
    let (env, argv) = plain("pytest --maxfail=2 A=b");
    assert!(env.is_empty());
    assert_eq!(argv, words(&["pytest", "--maxfail=2", "A=b"]));
    // A quoted name is no assignment: the shell would run a program called `A=b`.
    let (env, argv) = plain("\"A\"=b pytest");
    assert!(env.is_empty());
    assert_eq!(argv, words(&["A=b", "pytest"]));
}

#[test]
fn an_assignment_with_no_program_or_to_path_is_for_the_shell() {
    assert!(is_shell("CI=1"));
    assert!(is_shell("PATH=/opt/bin pytest"));
    assert!(is_shell("CI=$HOME pytest"));
}

#[test]
fn a_comment_at_a_word_start_ends_the_command() {
    assert_eq!(
        argv("pytest -q # the whole suite"),
        words(&["pytest", "-q"])
    );
    assert_eq!(argv("pytest -q#1 x#y"), words(&["pytest", "-q#1", "x#y"]));
    assert_eq!(
        argv("pytest '#not a comment' \\#also"),
        words(&["pytest", "#not a comment", "#also"])
    );
}

#[test]
fn unquoted_shell_syntax_makes_a_shell_line() {
    for command in [
        "cd api && pytest",
        "make test || true",
        "pytest; echo done",
        "pytest | tail -5",
        "pytest > out.txt",
        "pytest >> out.txt",
        "pytest < in.txt",
        "pytest 2>&1",
        "pytest &",
        "(cd api && pytest)",
        "pytest $HOME/tests",
        "pytest \"$HOME\"/tests",
        "pytest `pwd`",
        "pytest tests/test_*.py",
        "pytest tests/test_?.py",
        "pytest tests/test_[ab].py",
        "pytest ~/tests",
        "pytest {a,b}",
        "pytest a\nb",
    ] {
        assert!(
            is_shell(command),
            "{command:?} is shell syntax: {:?}",
            classify(command)
        );
    }
}

#[test]
fn quoted_or_escaped_metacharacters_are_plain_words() {
    for command in [
        "pytest -k 'a|b'",
        "pytest -k \"a;b\"",
        "pytest a\\&b",
        "pytest 'tests/test_*.py'",
        "pytest '$HOME' '~' '`x`'",
        "pytest a\\*b",
    ] {
        assert!(
            matches!(classify(command), Ok(Parsed::Plain { .. })),
            "{command:?}: {:?}",
            classify(command)
        );
    }
}

#[test]
fn text_that_cannot_be_read_is_unreadable() {
    for command in [
        "",
        "   ",
        "\t",
        "# only a note",
        "pytest \"unclosed",
        "pytest 'unclosed",
        "pytest \"a\\\"",
        "pytest a\u{0}b",
        "pytest a\rb",
        "cd x && make\r",
        "echo 'a\u{1}b'",
        "echo \"a\u{0}b\"",
    ] {
        assert!(
            is_unreadable(command),
            "{command:?}: {:?}",
            classify(command)
        );
    }
}

#[test]
fn a_backslash_at_the_end_is_a_character_of_the_word_as_it_is_in_the_shell() {
    assert_eq!(argv("pytest a\\"), words(&["pytest", "a\\"]));
    assert_eq!(argv("pytest \\"), words(&["pytest", "\\"]));
}

#[test]
fn shell_builtins_and_keywords_are_for_the_shell_whatever_files_exist() {
    for command in [
        "echo hi",
        "printf x",
        "test -d sub",
        "true",
        "false",
        "pwd",
        "exit 3",
        "cd sub",
        "kill 1",
        "time pytest",
        "command -v pytest",
        "exec pytest",
        "type pytest",
        ". ./env.sh",
        "if",
        "! pytest",
        "CI=1 echo hi",
        "'echo' hi",
    ] {
        assert!(is_shell(command), "{command:?}: {:?}", classify(command));
    }
    assert_eq!(
        argv("python3 -m unittest echo true"),
        words(&["python3", "-m", "unittest", "echo", "true"])
    );
}

#[test]
fn a_tilde_after_an_equals_sign_or_a_colon_is_for_the_shell() {
    for command in [
        "HOME=~/x pytest",
        "PYTHONPATH=src:~/lib pytest",
        "pytest --root=~/x",
        "pytest a:~b",
    ] {
        assert!(is_shell(command), "{command:?}: {:?}", classify(command));
    }
    assert_eq!(argv("pytest a~b"), words(&["pytest", "a~b"]));
    assert_eq!(argv("pytest 'a=~'"), words(&["pytest", "a=~"]));
}

// ---- strip_notes ----------------------------------------------------------------------------------------------------------------

#[test]
fn a_trailing_parenthetical_is_a_note() {
    for (given, want) in [
        (
            "python3 -m unittest pkg.test_mod (3 tests)",
            "python3 -m unittest pkg.test_mod",
        ),
        (
            "python3 -m unittest pkg.test_mod (final run after the last edit)",
            "python3 -m unittest pkg.test_mod",
        ),
        ("pytest -q   (all green)  ", "pytest -q"),
        ("pytest -q (12 tests (2 skipped))", "pytest -q"),
        ("npm test (exit 0)", "npm test"),
    ] {
        assert_eq!(strip_notes(given), want, "{given:?}");
    }
}

#[test]
fn a_comment_at_the_end_is_a_note() {
    assert_eq!(strip_notes("npm test # all green"), "npm test");
    assert_eq!(strip_notes("pytest -q (12 tests) # ok"), "pytest -q");
    assert_eq!(strip_notes("pytest -q # (12 tests)"), "pytest -q");
}

#[test]
fn an_exit_style_suffix_is_a_note() {
    for given in [
        "cargo test --workspace -> exit 0",
        "cargo test --workspace => exit 0",
        "cargo test --workspace, exit 0",
        "cargo test --workspace - exit 0",
        "cargo test --workspace: exit 0",
        "cargo test --workspace exit 0",
        "cargo test --workspace exit code 1",
        "cargo test --workspace exit=2",
        "cargo test --workspace (exit 0)",
    ] {
        assert_eq!(strip_notes(given), "cargo test --workspace", "{given:?}");
    }
}

#[test]
fn a_pipe_into_a_number_is_a_note() {
    assert_eq!(
        strip_notes("python3 -m pytest -q | 12 tests"),
        "python3 -m pytest -q"
    );
    assert_eq!(strip_notes("pytest -q | 3"), "pytest -q");
}

#[test]
fn notes_in_a_row_are_all_cut() {
    assert_eq!(
        strip_notes("pytest -q (12 tests) -> exit 0 # fine"),
        "pytest -q"
    );
}

#[test]
fn an_apostrophe_in_a_note_does_not_stop_the_cut() {
    for (given, want) in [
        ("pytest (didn't run the slow tests)", "pytest"),
        ("pytest # it's green", "pytest"),
        ("pytest -q (12 tests) # that's all", "pytest -q"),
        (
            "python3 -m unittest x (can't tell)",
            "python3 -m unittest x",
        ),
    ] {
        assert_eq!(strip_notes(given), want, "{given:?}");
    }
}

#[test]
fn a_bracketed_note_and_exit_code_with_a_colon_are_notes() {
    assert_eq!(strip_notes("pytest -q [3 tests]"), "pytest -q");
    assert_eq!(strip_notes("pytest -q [all green]"), "pytest -q");
    assert_eq!(strip_notes("pytest exit code: 0"), "pytest");
    assert_eq!(strip_notes("pytest exit status: 1"), "pytest");
    assert_eq!(strip_notes("make test exit 0"), "make test");
    // One word in brackets is a glob, and a test command in brackets starts the line: both stay.
    assert_eq!(
        strip_notes("pytest tests/test_[ab].py"),
        "pytest tests/test_[ab].py"
    );
    assert_eq!(strip_notes("pytest [ab]"), "pytest [ab]");
    assert_eq!(strip_notes("[ -f x ]"), "[ -f x ]");
}

#[test]
fn a_group_or_a_number_that_belongs_to_the_command_is_not_cut() {
    for command in [
        "! (pytest)",
        "f() (pytest)",
        "pytest -k exit 0",
        "pytest -x exit 0",
        "if (pytest)",
        "pytest # a comment\ncargo test",
    ] {
        assert_eq!(strip_notes(command), command, "{command:?}");
    }
}

#[test]
fn a_column_of_thousands_of_characters_is_left_as_it_is() {
    let long = format!("pytest {}", "(a) ".repeat(2000));
    assert_eq!(strip_notes(&long), long.trim());
}

#[test]
fn real_shell_syntax_is_not_a_note() {
    for command in [
        "cd api && python -m pytest",
        "(cd api && python -m pytest)",
        "make test && (cd x && ls)",
        "make test || (cd x && ls)",
        "python3 -m pytest -q | tee out.txt",
        "python3 -m pytest -q 2>&1 | tail -5",
        "pytest -k \"(a or b)\"",
        "pytest -k '(a)'",
        "pytest -q; exit 3",
        "pytest -q && exit 1",
        "pytest -q || exit 1",
        "exit 3",
        "echo \"# not a comment\"",
        "pytest tests/test_#1.py",
        "pytest -q | 12x",
        "pytest x(3 tests)",
        "pytest (unclosed",
    ] {
        assert_eq!(strip_notes(command), command, "{command:?}");
    }
}

#[test]
fn nothing_left_after_the_notes_is_an_empty_command() {
    assert_eq!(strip_notes("(3 tests)"), "(3 tests)");
    assert_eq!(strip_notes("# only a note"), "");
    assert_eq!(strip_notes("   "), "");
}

// ---- program_found ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_program_is_found_on_the_search_path_or_by_its_path_and_must_be_executable() {
    let dir = common::scratch("cmd-found");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::create_dir_all(dir.join("cwd/scripts")).unwrap();
    let exe = dir.join("bin/tool");
    std::fs::write(&exe, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let flat = dir.join("bin/not-executable");
    std::fs::write(&flat, "x").unwrap();
    std::fs::set_permissions(&flat, std::fs::Permissions::from_mode(0o644)).unwrap();
    let script = dir.join("cwd/scripts/run.sh");
    std::fs::write(&script, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = vec![dir.join("bin")];
    let cwd = dir.join("cwd");
    assert!(program_found("tool", &path, &cwd));
    assert!(!program_found("not-executable", &path, &cwd));
    assert!(!program_found("no-such-tool", &path, &cwd));
    assert!(program_found("./scripts/run.sh", &path, &cwd));
    assert!(program_found("scripts/run.sh", &path, &cwd));
    assert!(!program_found("./scripts/none.sh", &path, &cwd));
    assert!(program_found(exe.to_str().unwrap(), &path, &cwd));
    assert!(!program_found("", &path, &cwd));
    let no_path: Vec<PathBuf> = Vec::new();
    assert!(!program_found("tool", &no_path, &cwd));
}

#[test]
fn the_test_user_must_be_able_to_run_the_file_not_only_the_harness() {
    let dir = common::scratch("cmd-found-as");
    let file = dir.join("tool");
    std::fs::write(&file, "#!/bin/sh\n").unwrap();
    let me = std::fs::metadata(&file).unwrap();
    let (uid, gid) = (me.uid(), me.gid());
    let path = vec![dir.clone()];
    let set =
        |mode: u32| std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
    let other = Some((uid.wrapping_add(7), gid.wrapping_add(7)));
    let same_group = Some((uid.wrapping_add(7), gid));
    let owner = Some((uid, gid.wrapping_add(7)));
    set(0o700);
    assert!(program_found_as("tool", &path, &dir, None));
    assert!(program_found_as("tool", &path, &dir, owner));
    assert!(!program_found_as("tool", &path, &dir, other));
    assert!(!program_found_as("tool", &path, &dir, same_group));
    set(0o750);
    assert!(program_found_as("tool", &path, &dir, same_group));
    assert!(!program_found_as("tool", &path, &dir, other));
    set(0o755);
    assert!(program_found_as("tool", &path, &dir, other));
    set(0o644);
    assert!(!program_found_as("tool", &path, &dir, owner));
    assert!(!program_found_as("tool", &path, &dir, other));
    assert!(!program_found_as("tool", &path, &dir, None));
}

#[test]
fn only_a_script_with_an_interpreter_line_or_a_binary_runs_without_the_shell() {
    let dir = common::scratch("cmd-runs-directly");
    let write = |name: &str, bytes: &[u8]| {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    };
    assert!(runs_directly(&write(
        "with-shebang",
        b"#!/bin/sh\necho hi\n"
    )));
    assert!(runs_directly(&write("elf", b"\x7fELF\x02\x01\x01\x00")));
    assert!(!runs_directly(&write("no-shebang", b"echo hi\n")));
    assert!(!runs_directly(&write("empty", b"")));
    assert!(!runs_directly(&write("one-byte", b"#")));
    assert!(!runs_directly(&dir.join("missing")));
    assert!(!runs_directly(&dir));
}
