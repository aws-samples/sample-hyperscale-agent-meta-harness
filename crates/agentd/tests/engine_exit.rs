//! The engine process's exit and its stderr capture: how an exit status or a signal reads, where the
//! previous log goes, and that agentd reads the tail of the file it created, never the path (the engine's uid owns the
//! directory and can swap the path for a link while it runs).
#![allow(clippy::unwrap_used)]

mod common;

use agentd::engine::{
    Exit, LaunchSpec, describe_exit, exit_of, prev_stderr_path, spawn, spawn_captured,
};
use common::scratch;
use harness_core::engine_diagnostic::STDERR_WINDOW_BYTES;
use std::collections::BTreeMap;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

fn spec(dir: &Path, script: &str) -> LaunchSpec {
    LaunchSpec {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        env: BTreeMap::new(),
        cwd: dir.to_path_buf(),
        path: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        stderr_log: dir.join("engine-stderr.log"),
        run_as: None,
        sandbox: None,
    }
}

#[test]
fn an_exit_code_reads_as_exit_status() {
    let e = exit_of(ExitStatus::from_raw(3 << 8));
    assert_eq!(e, Exit::Code(3));
    assert_eq!(describe_exit(&e), "exit status 3");
    assert_eq!(
        describe_exit(&exit_of(ExitStatus::from_raw(0))),
        "exit status 0"
    );
}

#[test]
fn a_signal_reads_with_its_name_when_it_has_a_common_one() {
    for (raw, want) in [
        (1, "killed by signal 1 (SIGHUP)"),
        (2, "killed by signal 2 (SIGINT)"),
        (4, "killed by signal 4 (SIGILL)"),
        (6, "killed by signal 6 (SIGABRT)"),
        (7, "killed by signal 7 (SIGBUS)"),
        (9, "killed by signal 9 (SIGKILL)"),
        (11, "killed by signal 11 (SIGSEGV)"),
        (13, "killed by signal 13 (SIGPIPE)"),
        (15, "killed by signal 15 (SIGTERM)"),
        (40, "killed by signal 40"),
    ] {
        let e = exit_of(ExitStatus::from_raw(raw));
        assert_eq!(e, Exit::Signal(raw), "{raw}");
        assert_eq!(describe_exit(&e), want);
    }
}

#[test]
fn an_engine_agentd_had_to_stop_and_an_unreadable_status_say_so() {
    assert_eq!(
        describe_exit(&Exit::StoppedByAgentd { after_s: 5 }),
        "still running 5 s after its output closed; agentd stopped it"
    );
    assert_eq!(
        describe_exit(&Exit::Unknown("No child processes (os error 10)".into())),
        "exit status unknown (No child processes (os error 10))"
    );
}

#[test]
fn the_previous_log_has_a_sibling_name_that_keeps_the_extension() {
    assert_eq!(
        prev_stderr_path(Path::new("/d/engine-stderr.log")),
        PathBuf::from("/d/engine-stderr.prev.log")
    );
    assert_eq!(
        prev_stderr_path(Path::new("/d/x.stderr")),
        PathBuf::from("/d/x.prev.stderr")
    );
    assert_eq!(
        prev_stderr_path(Path::new("/d/noext")),
        PathBuf::from("/d/noext.prev")
    );
}

#[tokio::test]
async fn a_captured_spawn_keeps_the_previous_log_and_starts_a_fresh_one() {
    let d = scratch("exit-rotate");
    let (log, prev) = (
        d.join("engine-stderr.log"),
        d.join("engine-stderr.prev.log"),
    );
    let (mut c1, _cap1) = spawn_captured(&spec(&d, "echo one >&2")).unwrap();
    assert!(c1.wait().await.unwrap().success());
    assert!(!prev.exists());
    let (mut c2, cap2) = spawn_captured(&spec(&d, "echo two >&2")).unwrap();
    assert!(c2.wait().await.unwrap().success());
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "two\n");
    assert_eq!(std::fs::read_to_string(&prev).unwrap(), "one\n");
    let tail = cap2.tail().unwrap();
    assert_eq!(tail.bytes, b"two\n");
    assert!(!tail.cut);
}

#[tokio::test]
async fn a_plain_spawn_still_truncates_and_keeps_no_previous_log() {
    let d = scratch("exit-plain");
    let mut c1 = spawn(&spec(&d, "echo one >&2")).unwrap();
    assert!(c1.wait().await.unwrap().success());
    let mut c2 = spawn(&spec(&d, "echo two >&2")).unwrap();
    assert!(c2.wait().await.unwrap().success());
    assert_eq!(
        std::fs::read_to_string(d.join("engine-stderr.log")).unwrap(),
        "two\n"
    );
    assert!(!d.join("engine-stderr.prev.log").exists());
}

#[tokio::test]
async fn the_tail_is_the_last_window_and_says_when_it_cut() {
    let d = scratch("exit-window");
    let script = "yes 'row xxxxxxxxxxxxxxxxxxxx' | head -n 2000 >&2; echo END-MARK >&2";
    let (mut c, cap) = spawn_captured(&spec(&d, script)).unwrap();
    assert!(c.wait().await.unwrap().success());
    let tail = cap.tail().unwrap();
    assert!(tail.cut);
    assert_eq!(tail.bytes.len(), STDERR_WINDOW_BYTES);
    assert!(tail.bytes.ends_with(b"END-MARK\n"));
}

#[tokio::test]
async fn the_tail_is_read_from_the_file_agentd_made_not_from_the_path() {
    let d = scratch("exit-swap");
    let secret = d.join("secret.txt");
    std::fs::write(&secret, "TOPSECRET-CONTENT-123456").unwrap();
    let script = format!(
        "echo own-words >&2; ln -sf '{}' engine-stderr.log",
        secret.display()
    );
    let mut s = spec(&d, &script);
    s.cwd = d.clone();
    let (mut c, cap) = spawn_captured(&s).unwrap();
    assert!(c.wait().await.unwrap().success());
    let tail = cap.tail().unwrap();
    assert_eq!(
        tail.bytes, b"own-words\n",
        "the capture follows its own inode"
    );
}

#[test]
fn a_status_that_is_neither_an_exit_nor_a_signal_is_unknown() {
    // A stopped child (SIGSTOP): `code()` and `signal()` are both None.
    let e = exit_of(ExitStatus::from_raw(0x137f));
    assert_eq!(e, Exit::Unknown("neither an exit code nor a signal".into()));
    assert_eq!(
        describe_exit(&e),
        "exit status unknown (neither an exit code nor a signal)"
    );
}

#[tokio::test]
async fn a_name_still_taken_after_the_rotation_is_removed_once_and_the_start_goes_on() {
    // The previous-log name is a non-empty directory, so the rename fails and the old log stays in the way.
    let d = scratch("exit-in-the-way");
    let prev = d.join("engine-stderr.prev.log");
    std::fs::create_dir_all(&prev).unwrap();
    std::fs::write(prev.join("keep"), "x").unwrap();
    std::fs::write(d.join("engine-stderr.log"), "old words\n").unwrap();
    let (mut c, cap) = spawn_captured(&spec(&d, "echo new >&2")).unwrap();
    assert!(c.wait().await.unwrap().success());
    assert_eq!(
        std::fs::read_to_string(d.join("engine-stderr.log")).unwrap(),
        "new\n"
    );
    assert_eq!(cap.tail().unwrap().bytes, b"new\n");
    assert!(prev.join("keep").exists(), "the directory is left alone");
}

#[tokio::test]
async fn a_directory_at_the_log_path_is_moved_aside_like_any_other_entry() {
    let d = scratch("exit-dir");
    std::fs::create_dir_all(d.join("engine-stderr.log").join("sub")).unwrap();
    let (mut c, cap) = spawn_captured(&spec(&d, "echo x >&2")).unwrap();
    assert!(c.wait().await.unwrap().success());
    assert_eq!(cap.tail().unwrap().bytes, b"x\n");
    assert!(d.join("engine-stderr.prev.log").join("sub").is_dir());
}

#[tokio::test]
async fn a_directory_in_the_way_of_both_names_fails_the_start_and_never_blocks() {
    let d = scratch("exit-dirs");
    std::fs::create_dir_all(d.join("engine-stderr.log").join("sub")).unwrap();
    std::fs::create_dir_all(d.join("engine-stderr.prev.log").join("sub")).unwrap();
    let r = spawn_captured(&spec(&d, "echo x >&2"));
    assert!(r.is_err(), "a directory cannot be the log");
}

#[tokio::test]
async fn a_fifo_at_the_log_path_is_moved_aside_not_opened() {
    let d = scratch("exit-fifo");
    let log = d.join("engine-stderr.log");
    let made = std::process::Command::new("mkfifo")
        .arg(&log)
        .status()
        .unwrap();
    assert!(made.success());
    let (mut c, cap) = spawn_captured(&spec(&d, "echo through >&2")).unwrap();
    assert!(c.wait().await.unwrap().success());
    assert_eq!(cap.tail().unwrap().bytes, b"through\n");
    assert!(
        std::fs::metadata(d.join("engine-stderr.prev.log"))
            .unwrap()
            .file_type()
            .is_fifo(),
        "the planted entry went to the previous-log name untouched"
    );
}

// ---- Review of e44c5417, M2 (b) and the pre-run failures --------------------------------------------------------------------

#[tokio::test]
async fn a_link_at_the_log_path_is_never_written_through_even_when_the_rotation_fails() {
    // The engine left a link at the log path and a non-empty directory at the previous-log name, so the rename that would move
    // the link aside fails. `create_new` must still refuse to follow the link: a create-and-truncate would write through it, as
    // root, into the file it points at.
    let d = scratch("exit-link-norot");
    let victim = d.join("victim.txt");
    std::fs::write(&victim, "KEEP-THIS-CONTENT").unwrap();
    std::os::unix::fs::symlink(&victim, d.join("engine-stderr.log")).unwrap();
    let prev = d.join("engine-stderr.prev.log");
    std::fs::create_dir_all(prev.join("sub")).unwrap();
    let (mut c, cap) = spawn_captured(&spec(&d, "echo hi >&2")).unwrap();
    assert!(c.wait().await.unwrap().success());
    assert_eq!(
        std::fs::read_to_string(&victim).unwrap(),
        "KEEP-THIS-CONTENT",
        "a start wrote through the link at the log path"
    );
    assert_eq!(cap.tail().unwrap().bytes, b"hi\n");
    let log = std::fs::symlink_metadata(d.join("engine-stderr.log")).unwrap();
    assert!(
        log.file_type().is_file(),
        "the link was replaced by a regular file"
    );
    assert!(prev.join("sub").is_dir(), "the directory is left alone");
}

#[tokio::test]
async fn a_start_that_fails_before_the_engine_runs_leaves_the_last_real_log_where_it_was() {
    // Starts that fail on their own (a working directory that is gone, an exec that cannot happen) ran the rotation, so after
    // two of them the evidence of the last real run was an empty previous log.
    let d = scratch("exit-spawnfail");
    let (log, prev) = (
        d.join("engine-stderr.log"),
        d.join("engine-stderr.prev.log"),
    );
    let (mut c, _cap) = spawn_captured(&spec(&d, "echo REAL-EVIDENCE >&2; exit 1")).unwrap();
    c.wait().await.unwrap();
    for i in 1..=3 {
        let mut bad = spec(&d, "echo never >&2");
        bad.cwd = d.join("no-such-dir");
        assert!(spawn_captured(&bad).is_err(), "failed start {i}");
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "REAL-EVIDENCE\n",
            "after failed start {i} the last real log is still the log"
        );
    }
    assert!(!prev.exists() || std::fs::read_to_string(&prev).unwrap() != "never\n");
}

#[tokio::test]
async fn the_engine_writes_land_at_the_end_of_the_log_whatever_happens_to_the_file_meanwhile() {
    // The log is opened for appending: when the engine's uid truncates the file by its path, the next write of the child
    // still lands at the new end instead of leaving a hole of zero bytes where the old text was.
    let d = scratch("exit-append");
    let script = "echo before-truncation >&2; : > engine-stderr.log; echo after-truncation >&2";
    let (mut c, cap) = spawn_captured(&spec(&d, script)).unwrap();
    assert!(c.wait().await.unwrap().success());
    assert_eq!(
        std::fs::read(d.join("engine-stderr.log")).unwrap(),
        b"after-truncation\n"
    );
    assert_eq!(cap.tail().unwrap().bytes, b"after-truncation\n");
}

#[tokio::test]
async fn a_failed_start_that_moved_no_log_restores_nothing() {
    // No current log, only an older previous one: the rotation moves nothing, so a failed start has nothing to put back, and
    // the older log must not be dragged over the fresh empty one.
    let d = scratch("exit-nothing-moved");
    let (log, prev) = (
        d.join("engine-stderr.log"),
        d.join("engine-stderr.prev.log"),
    );
    std::fs::write(&prev, "older evidence\n").unwrap();
    let mut bad = spec(&d, "echo never >&2");
    bad.cwd = d.join("no-such-dir");
    assert!(spawn_captured(&bad).is_err());
    assert_eq!(
        std::fs::read_to_string(&prev).unwrap(),
        "older evidence\n",
        "the older log stays where it is"
    );
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "",
        "the fresh empty log stays: nothing was moved, so nothing is put back"
    );
}
