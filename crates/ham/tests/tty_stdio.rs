//! The editor of a command that runs as a child of the chat box: its screen goes to the controlling terminal when the
//! child's own standard output and error go to a file. A file stands in for `/dev/tty`, so the tests need no terminal.
#![allow(clippy::unwrap_used)]

use ham::tty::{can_ask_at, editor_stdio_at};
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn scratch(label: &str) -> PathBuf {
    let base = std::env::var_os("TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let dir = base.join(format!("ham-tty-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn both() -> Command {
    let mut c = Command::new("sh");
    c.args(["-c", "echo to-out; echo to-err >&2"]);
    c
}

#[test]
fn output_that_does_not_reach_a_terminal_goes_to_the_controlling_terminal() {
    let dir = scratch("both");
    let tty = dir.join("tty");
    std::fs::write(&tty, "").unwrap();
    let mut c = both();
    assert!(editor_stdio_at(&mut c, &tty, false, false));
    assert!(c.status().unwrap().success());
    let seen = std::fs::read_to_string(&tty).unwrap();
    assert!(
        seen.contains("to-out") && seen.contains("to-err"),
        "{seen:?}"
    );
}

#[test]
fn a_stream_that_is_a_terminal_already_is_left_alone() {
    let dir = scratch("one");
    let tty = dir.join("tty");
    std::fs::write(&tty, "").unwrap();
    let mut c = both();
    c.stdout(Stdio::piped());
    // Standard output is a terminal here, so the pipe stays; only standard error moves.
    assert!(editor_stdio_at(&mut c, &tty, true, false));
    let out = c.output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "to-out\n");
    assert_eq!(std::fs::read_to_string(&tty).unwrap(), "to-err\n");
}

#[test]
fn with_both_streams_on_a_terminal_nothing_changes() {
    let dir = scratch("none");
    let tty = dir.join("tty");
    std::fs::write(&tty, "").unwrap();
    let mut c = both();
    assert!(!editor_stdio_at(&mut c, &tty, true, true));
    assert_eq!(std::fs::read_to_string(&tty).unwrap(), "");
}

#[test]
fn without_a_controlling_terminal_the_streams_stay_as_they_are() {
    let dir = scratch("missing");
    let mut c = both();
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    assert!(!editor_stdio_at(
        &mut c,
        &dir.join("no-such-tty"),
        false,
        false
    ));
    let out = c.output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "to-out\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "to-err\n");
}

#[test]
fn an_editor_can_be_shown_when_input_is_a_terminal_and_output_reaches_one() {
    let dir = scratch("ask");
    let tty = dir.join("tty");
    std::fs::write(&tty, "").unwrap();
    let missing = dir.join("no-such-tty");
    // Input from a terminal and output to a terminal: yes.
    assert!(can_ask_at(true, true, &missing));
    // Output to a file, but the controlling terminal opens: yes (the chat box runs the command this way).
    assert!(can_ask_at(true, false, &tty));
    // Output to a file and no controlling terminal: no.
    assert!(!can_ask_at(true, false, &missing));
    // Input not from a terminal: never.
    assert!(!can_ask_at(false, true, &tty));
    assert!(!can_ask_at(false, false, &tty));
}
