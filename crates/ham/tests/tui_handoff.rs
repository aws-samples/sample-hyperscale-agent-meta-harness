//! The hand-over of the terminal to a command: `handoff::run_child_with` runs a child process with the terminal given to
//! it by a closure (the TUI passes `reader::suspend_for`), sends its standard output and error to a private file, and returns them with the
//! exit code, so the result panel can show what the terminal no longer shows. A script stands in for the child `ham`.
#![allow(clippy::unwrap_used)]

use ham::tui::handoff::run_child_with;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn scratch(label: &str) -> PathBuf {
    let base = std::env::var_os("TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let dir = base.join(format!("ham-handoff-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(ToString::to_string).collect()
}

fn text_of(out: &ham::output::CmdOutput) -> String {
    out.lines().join("\n")
}

#[test]
fn the_output_and_the_exit_code_of_the_child_come_back() {
    let dir = scratch("basic");
    let child = script(
        &dir,
        "child.sh",
        "echo pushed one\necho a warning >&2\necho pushed two\nexit 0",
    );
    let out = run_child_with(&child, &[], &dir, |c| c.status());
    assert_eq!(out.code, 0);
    let text = text_of(&out);
    assert!(
        text.contains("pushed one") && text.contains("a warning") && text.contains("pushed two"),
        "{text}"
    );
    // The two streams keep the order they were written in.
    let (a, b, c) = (
        text.find("pushed one").unwrap(),
        text.find("a warning").unwrap(),
        text.find("pushed two").unwrap(),
    );
    assert!(a < b && b < c, "{text}");
}

#[test]
fn a_failing_child_reports_its_exit_code() {
    let dir = scratch("fail");
    let child = script(&dir, "child.sh", "echo it did not work >&2\nexit 7");
    let out = run_child_with(&child, &[], &dir, |c| c.status());
    assert_eq!(out.code, 7);
    assert!(text_of(&out).contains("it did not work"));
}

#[test]
fn the_arguments_reach_the_child_as_written() {
    let dir = scratch("args");
    let child = script(&dir, "child.sh", "printf '[%s]\\n' \"$@\"");
    let words = args(&["push", "a b", "$HOME", "x&y;z"]);
    let out = run_child_with(&child, &words, &dir, |c| c.status());
    let text = text_of(&out);
    for w in ["[push]", "[a b]", "[$HOME]", "[x&y;z]"] {
        assert!(text.contains(w), "{w} is missing:\n{text}");
    }
}

#[test]
fn a_child_that_cannot_start_says_so_and_fails() {
    let dir = scratch("nostart");
    let missing = dir.join("no-such-program");
    let out = run_child_with(&missing, &args(&["push"]), &dir, |c| c.status());
    assert_ne!(out.code, 0);
    assert!(
        text_of(&out).contains("could not start"),
        "{:?}",
        out.lines()
    );
}

#[test]
fn a_child_killed_by_a_signal_fails_with_the_error_code() {
    let dir = scratch("signal");
    let child = script(&dir, "child.sh", "kill -9 $$");
    let out = run_child_with(&child, &[], &dir, |c| c.status());
    assert_eq!(out.code, ham_core::exit::ERROR);
}

#[test]
fn a_long_output_keeps_its_end_and_says_that_the_start_was_cut() {
    let dir = scratch("long");
    let child = script(
        &dir,
        "child.sh",
        "i=0\nwhile [ $i -lt 40000 ]; do echo \"line $i of the push output\"; i=$((i+1)); done",
    );
    let out = run_child_with(&child, &[], &dir, |c| c.status());
    let text = text_of(&out);
    assert!(
        text.contains("line 39999 of the push output"),
        "the end is kept"
    );
    assert!(
        !text.contains("line 0 of the push output"),
        "the start is cut"
    );
    assert!(
        text.contains("earlier output was cut"),
        "{}",
        &text[..200.min(text.len())]
    );
    assert!(text.len() < 600 * 1024, "{} bytes", text.len());
}

#[test]
fn bytes_that_are_not_text_do_not_break_the_output() {
    let dir = scratch("bytes");
    let child = script(&dir, "child.sh", "printf 'ok \\377\\376 done\\n'");
    let out = run_child_with(&child, &[], &dir, |c| c.status());
    let text = text_of(&out);
    assert!(text.contains("ok") && text.contains("done"), "{text:?}");
}

#[test]
fn the_private_directory_of_the_output_is_gone_afterwards() {
    let dir = scratch("clean");
    let child = script(&dir, "child.sh", "echo hi");
    let before: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let _ = run_child_with(&child, &[], &dir, |c| c.status());
    let after: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(before, after, "the call left files in {}", dir.display());
}

#[test]
fn the_terminal_closure_runs_the_command_with_the_output_going_to_a_file() {
    // What the closure is given is the command with its output already redirected: it only has to run it.
    let dir = scratch("closure");
    let child = script(&dir, "child.sh", "echo seen");
    let mut ran = false;
    let out = run_child_with(&child, &[], &dir, |c| {
        ran = true;
        c.status()
    });
    assert!(ran);
    assert!(text_of(&out).contains("seen"));
}

// COMPAT-CAH: the notices about the earlier names are made once, when the program starts (`HAMH_NO_DEPRECATION_NOTICES`); a child `ham` handed the
// terminal makes none of its own, so its output in the result panel holds only what the command printed.
#[test]
fn the_child_is_told_not_to_repeat_the_notices_of_the_earlier_names() {
    let dir = scratch("notices");
    let child = script(
        &dir,
        "child.sh",
        "echo \"switch=$HAMH_NO_DEPRECATION_NOTICES\"",
    );
    let out = run_child_with(&child, &[], &dir, |c| c.status());
    assert_eq!(out.code, 0);
    assert_eq!(text_of(&out).trim(), "switch=1");
}
