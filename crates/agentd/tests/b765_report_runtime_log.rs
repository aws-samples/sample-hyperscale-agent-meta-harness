//! A report step that is not run for want of a container runtime keeps the evidence of why.
//!
//! The report keeps the last 4096 bytes of a step (a bound of the wire type). Testcontainers prints what it tried near the start of its
//! output, so the tail of the four gate reports had the exception stack and the Maven summary and none of the lines that name the cause.
//! Now the scan keeps the lines about the runtime from the whole output (at most 2048 bytes, 300 per line), and a step marked not run
//! for the runtime shows them after the reason, with one line for what the runtime's socket did when agentd connected to it. The whole
//! tail still fits the 4096 bytes. A step that is run and judged by its exit code (it passed, or it failed for another reason) is left alone.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::report::parse::{OutputScan, RUNTIME_LINE_BYTES, RUNTIME_LOG_BYTES};
use agentd::direct::report::plan::PlannedStep;
use agentd::direct::report::run::{
    RUNTIME_DOWN, RunEnv, add_runtime_block, run_step, runtime_socket_state, set_plan_note,
    socket_error_words,
};
use agentd::direct::report::services::put_head_line;
use harness_protocol::direct::{
    PLAN_NOTE_COMMAND, PLAN_NOTE_PREFIX, TestCommandSource, TestStep, TestStepRole,
};
use std::collections::BTreeMap;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What Testcontainers 2.0.5 printed in the gate runs, in order: its version, the warning of the strategy that reads `DOCKER_HOST`, the cause
/// under it, a stack frame, the error with the list of attempts, the one entry of the list.
const TESTCONTAINERS: &str = "\
12:55:54.423 [main] INFO org.testcontainers.DockerClientFactory -- Testcontainers version: 2.0.5
12:55:54.431 [main] WARN org.testcontainers.dockerclient.DockerClientProviderStrategy -- DOCKER_HOST unix:///var/run/docker.sock is not listening
java.net.ConnectException: Connection refused
\tat java.base/sun.nio.ch.UnixDomainSockets.connect0(Native Method)
12:55:54.794 [main] ERROR org.testcontainers.dockerclient.DockerClientProviderStrategy -- Could not find a valid Docker environment. Please check configuration. Attempted configurations were:
\tDockerDesktopClientProviderStrategy: failed with exception NullPointerException (Cannot invoke \"java.nio.file.Path.toString()\" because the return value of \"getSocketPath()\" is null)As no valid configuration was found, execution cannot continue.
";

fn noise(lines: usize) -> String {
    (0..lines)
        .map(|i| {
            format!("[INFO] Running com.example.catalog.persistence.Test{i} in the fixture module")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn scan(text: &str) -> agentd::direct::report::parse::Scanned {
    let mut s = OutputScan::new(4096);
    s.feed(text.as_bytes());
    s.finish("mvn verify")
}

#[test]
fn b765_the_scan_keeps_the_runtime_lines_of_the_whole_stream() {
    let text = format!("{TESTCONTAINERS}{}", noise(300));
    let out = scan(&text);
    assert!(
        !out.tail.contains("is not listening"),
        "TRIGGER PRESENT: the 4 KiB tail has lost the warning"
    );
    let joined = out.runtime_lines.join("\n");
    for needle in [
        "Testcontainers version: 2.0.5",
        "DOCKER_HOST unix:///var/run/docker.sock is not listening",
        "java.net.ConnectException: Connection refused",
        "Could not find a valid Docker environment",
        "DockerDesktopClientProviderStrategy: failed with exception NullPointerException",
    ] {
        assert!(joined.contains(needle), "{needle:?} missing from {joined}");
    }
    assert!(!joined.contains("at java.base"), "a stack frame was kept");
    assert!(!joined.contains("[INFO] Running"), "noise was kept");
}

#[test]
fn b765_the_runtime_lines_are_bounded_and_cut_on_a_character_boundary() {
    let long = format!("DOCKER_HOST x{}", "\u{e9}".repeat(400));
    let many: String = (0..100)
        .map(|i| format!("WARN docker.sock attempt {i} failed: {}\n", "y".repeat(200)))
        .collect();
    let out = scan(&format!("{long}\n{many}"));
    assert!(
        out.runtime_lines
            .iter()
            .all(|l| l.len() <= RUNTIME_LINE_BYTES),
        "{:?}",
        out.runtime_lines
    );
    let total: usize = out.runtime_lines.iter().map(|l| l.len() + 1).sum();
    assert!(total <= RUNTIME_LOG_BYTES, "{total}");
    assert!(
        out.runtime_lines[0].starts_with("DOCKER_HOST "),
        "the earliest line comes first"
    );
}

#[test]
fn b765_a_line_that_repeats_is_kept_once() {
    let text = "WARN DOCKER_HOST unix:///var/run/docker.sock is not listening\n".repeat(40);
    assert_eq!(scan(&text).runtime_lines.len(), 1);
}

// ---- the step ---------------------------------------------------------------------------------------------------------------------

const RUNTIME_VARS: [(&str, &str); 4] = [
    ("CONTAINER_HOST", "unix:///var/run/docker.sock"),
    ("DOCKER_HOST", "unix:///var/run/docker.sock"),
    ("TESTCONTAINERS_HOST_OVERRIDE", "127.0.0.1"),
    ("TESTCONTAINERS_RYUK_DISABLED", "true"),
];

fn env_for(work: &Path, docker_host: Option<&str>) -> RunEnv {
    let mut engine_env: BTreeMap<String, String> = BTreeMap::new();
    if let Some(host) = docker_host {
        for (k, v) in RUNTIME_VARS {
            engine_env.insert(k.to_string(), v.to_string());
        }
        engine_env.insert("DOCKER_HOST".to_string(), host.to_string());
    }
    RunEnv {
        sandbox: None,
        work: work.to_path_buf(),
        run_as: None,
        path: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        engine_env,
        engine_home: None,
        cargo_config: None,
    }
}

async fn run_in(env: &RunEnv, work: &Path, command: &str) -> TestStep {
    let cwd = work.join("checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    let step = PlannedStep {
        role: TestStepRole::Test,
        source: TestCommandSource::TestCommandFile,
        command: command.to_string(),
        logged: None,
        install: None,
        fallback: None,
    };
    run_step(env, &step, &cwd, Duration::from_secs(30), 4096).await
}

fn not_run(s: &TestStep) -> bool {
    s.counts_from.as_deref() == Some("not run")
}

/// A shell command that prints the Testcontainers lines, then `n` lines of Maven noise, then exits with `code`.
fn testcontainers_then_noise(n: usize, code: i32) -> String {
    format!(
        "printf '%s' '{}' | sed 's/^\\\\t/\\t/'; i=0; while [ $i -lt {n} ]; do echo \"[INFO] Running com.example.catalog.persistence.Test$i in the fixture module\"; i=$((i+1)); done; exit {code}",
        TESTCONTAINERS.replace('\'', "'\\''")
    )
}

#[tokio::test]
async fn b765_a_step_not_run_for_the_runtime_shows_the_lines_that_the_tail_lost() {
    let work = common::scratch("b765-step");
    let env = env_for(&work, Some("unix:///var/run/docker.sock"));
    let s = run_in(&env, &work, &testcontainers_then_noise(300, 1)).await;
    assert!(not_run(&s), "{s:?}");
    assert_eq!(s.exit_code, Some(1), "the raw exit code stays");
    assert_eq!(s.counts, None);
    assert!(
        s.output_tail
            .starts_with("NOT RUN: the session's container runtime did not answer\n"),
        "{}",
        s.output_tail
    );
    let block: Vec<&str> = s
        .output_tail
        .lines()
        .filter(|l| l.starts_with("RUNTIME: "))
        .collect();
    for needle in [
        "Testcontainers version: 2.0.5",
        "DOCKER_HOST unix:///var/run/docker.sock is not listening",
        "java.net.ConnectException: Connection refused",
        "Could not find a valid Docker environment",
        "DockerDesktopClientProviderStrategy: failed with exception NullPointerException",
    ] {
        assert!(
            block.iter().any(|l| l.contains(needle)),
            "{needle:?} not in the block {block:#?}"
        );
    }
    assert!(
        block[0].starts_with("RUNTIME: socket "),
        "the socket line comes first: {block:#?}"
    );
    // The end of the output is still there, and the whole stays inside the wire bound.
    assert!(
        s.output_tail.contains("Test299 in the fixture module"),
        "{}",
        s.output_tail
    );
    assert!(s.output_tail.len() <= 4096, "{}", s.output_tail.len());
    assert!(s.output_truncated);
}

#[tokio::test]
async fn b765_the_block_says_the_session_has_no_runtime_when_the_engine_started_none() {
    let work = common::scratch("b765-none");
    let env = env_for(&work, None);
    let s = run_in(
        &env,
        &work,
        "echo 'WARN DOCKER_HOST unix:///var/run/docker.sock is not listening'; echo 'Could not find a valid Docker environment'; exit 1",
    )
    .await;
    assert!(not_run(&s), "{s:?}");
    assert!(
        s.output_tail
            .starts_with("NOT RUN: the session has no container runtime\n"),
        "{}",
        s.output_tail
    );
    assert!(
        s.output_tail.lines().any(|l| l.starts_with("RUNTIME: ")
            && l.contains("DOCKER_HOST unix:///var/run/docker.sock is not listening")),
        "{}",
        s.output_tail
    );
    assert!(
        !s.output_tail.contains("RUNTIME: socket"),
        "no socket was named: {}",
        s.output_tail
    );
}

/// A listening socket and a name for it that fits the 107 bytes of a socket path wherever the scratch directory is.
struct Sock {
    _dir: std::fs::File,
    path: String,
}

fn sock(label: &str) -> (Sock, std::path::PathBuf) {
    let real = common::scratch(label);
    let dir = std::fs::File::open(&real).unwrap();
    let path = format!("/proc/self/fd/{}/s", dir.as_raw_fd());
    (Sock { _dir: dir, path }, real)
}

#[tokio::test]
async fn b765_the_block_says_what_the_runtime_socket_did() {
    let failing = "echo 'Could not find a valid Docker environment'; exit 1";
    // A socket with a listener behind it.
    let (s1, _) = sock("b765-sock-up");
    let listener = UnixListener::bind(&s1.path).unwrap();
    let work = common::scratch("b765-work-up");
    let up = run_in(
        &env_for(&work, Some(&format!("unix://{}", s1.path))),
        &work,
        failing,
    )
    .await;
    assert!(
        up.output_tail
            .contains(&format!("RUNTIME: socket {}: accepts connections", s1.path)),
        "{}",
        up.output_tail
    );
    drop(listener);
    // The listener is gone and the file stays: the state the sweep left (a socket file and nobody behind it).
    let work = common::scratch("b765-work-dead");
    let dead = run_in(
        &env_for(&work, Some(&format!("unix://{}", s1.path))),
        &work,
        failing,
    )
    .await;
    assert!(
        dead.output_tail
            .contains(&format!("RUNTIME: socket {}: connection refused", s1.path)),
        "{}",
        dead.output_tail
    );
    // No such file.
    let (s2, _) = sock("b765-sock-none");
    let work = common::scratch("b765-work-none");
    let none = run_in(
        &env_for(&work, Some(&format!("unix://{}", s2.path))),
        &work,
        failing,
    )
    .await;
    assert!(
        none.output_tail
            .contains(&format!("RUNTIME: socket {}: does not exist", s2.path)),
        "{}",
        none.output_tail
    );
}

#[tokio::test]
async fn b765_a_step_that_passes_or_fails_for_another_reason_gets_no_block() {
    let work = common::scratch("b765-plain");
    let env = env_for(&work, Some("unix:///var/run/docker.sock"));
    let passes = run_in(
        &env,
        &work,
        "echo 'WARN DOCKER_HOST unix:///var/run/docker.sock is not listening'; echo '2 passed in 0.01s'",
    )
    .await;
    assert!(!not_run(&passes));
    assert!(
        !passes.output_tail.contains("RUNTIME: "),
        "{}",
        passes.output_tail
    );
    let red = run_in(
        &env,
        &work,
        "echo 'DOCKER_HOST is set'; echo 'assertion failed'; exit 1",
    )
    .await;
    assert!(!not_run(&red));
    assert!(
        !red.output_tail.contains("RUNTIME: "),
        "{}",
        red.output_tail
    );
}

#[tokio::test]
async fn b765_the_block_is_redacted_like_the_tail() {
    let key = format!("AKIA{}", "IOSFODNN7EXAMPLE");
    let work = common::scratch("b765-redact");
    let env = env_for(&work, Some("unix:///var/run/docker.sock"));
    let s = run_in(
        &env,
        &work,
        &format!("echo 'Cannot connect to the Docker daemon with {key}'; exit 1"),
    )
    .await;
    assert!(not_run(&s), "{s:?}");
    assert!(!s.output_tail.contains(&key), "{}", s.output_tail);
}

#[tokio::test]
async fn b765_a_line_the_repository_prints_cannot_pass_for_the_socket_line() {
    // The socket line is agentd's: it comes right after the reason. A repository line of the same shape is part of the output, further down.
    let (sock, _) = sock("b765-forge-sock");
    drop(UnixListener::bind(&sock.path).unwrap());
    let work = common::scratch("b765-forge");
    let env = env_for(&work, Some(&format!("unix://{}", sock.path)));
    let forged = format!("RUNTIME: socket {}: accepts connections", sock.path);
    let s = run_in(
        &env,
        &work,
        &format!("echo '{forged}'; echo 'Could not find a valid Docker environment'; exit 1"),
    )
    .await;
    let lines: Vec<&str> = s.output_tail.lines().collect();
    assert!(lines[0].starts_with("NOT RUN: "), "{lines:?}");
    assert_eq!(
        lines[1],
        format!(
            "RUNTIME: socket {}: connection refused (nothing is listening on it)",
            sock.path
        )
    );
    // The repository's line is part of its output, shown where the output is, and nothing agentd reads.
    let at = lines
        .iter()
        .position(|l| *l == forged)
        .expect("the repository's own line is in the output");
    assert!(at > 1, "{lines:?}");
}

// ---- the pieces -------------------------------------------------------------------------------------------------------------------

fn env_of(docker_host: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("DOCKER_HOST".to_string(), docker_host.to_string())])
}

#[tokio::test]
async fn b765_no_unix_socket_in_the_environment_gives_no_socket_line() {
    assert_eq!(runtime_socket_state(&BTreeMap::new()).await, None);
    assert_eq!(
        runtime_socket_state(&env_of("tcp://127.0.0.1:2375")).await,
        None
    );
    assert_eq!(runtime_socket_state(&env_of("unix://")).await, None);
}

#[test]
fn b765_the_socket_words_name_each_way_a_connect_ends() {
    use std::io::{Error, ErrorKind};
    assert_eq!(
        socket_error_words(&Error::from(ErrorKind::NotFound)),
        "does not exist"
    );
    assert_eq!(
        socket_error_words(&Error::from(ErrorKind::ConnectionRefused)),
        "connection refused (nothing is listening on it)"
    );
    assert_eq!(
        socket_error_words(&Error::from(ErrorKind::PermissionDenied)),
        "permission denied"
    );
    assert_eq!(socket_error_words(&Error::other("boom")), "boom");
}

#[tokio::test]
async fn b765_a_socket_line_is_cut_on_a_character_boundary() {
    // A path longer than a socket address can hold ends in an error that is not one of the named kinds; the line is cut at 240 bytes.
    let path = format!("/x{}", "\u{e9}".repeat(200));
    let line = runtime_socket_state(&env_of(&format!("unix://{path}")))
        .await
        .unwrap();
    assert!(line.len() <= 240, "{}", line.len());
    assert!(line.starts_with("socket /x"), "{line}");
}

fn not_run_step(tail: &str) -> TestStep {
    let mut s = TestStep::not_run(
        TestStepRole::Test,
        TestCommandSource::TestCommandFile,
        "bash scripts/test-integration.sh",
        "the session's container runtime did not answer",
        None,
    );
    s.output_tail = tail.to_string();
    s
}

#[test]
fn b765_a_step_that_is_not_marked_not_run_is_left_as_it_is() {
    let mut s = not_run_step("plain output");
    s.counts_from = None;
    // Not run, but its tail does not start with the reason (the repository wrote it): nothing is added.
    add_runtime_block(
        &mut s,
        Some("socket /s: accepts connections"),
        &["a docker.sock line".to_string()],
        4096,
    );
    assert_eq!(s.output_tail, "plain output");
}

#[test]
fn b765_a_reason_with_no_output_after_it_gets_the_block_and_nothing_else() {
    let mut s = not_run_step("NOT RUN: the session's container runtime did not answer");
    add_runtime_block(
        &mut s,
        Some("socket /s: connection refused"),
        &["DOCKER_HOST is not listening".to_string()],
        4096,
    );
    assert_eq!(
        s.output_tail,
        "NOT RUN: the session's container runtime did not answer\nRUNTIME: socket /s: connection refused\nRUNTIME: DOCKER_HOST is not listening"
    );
}

#[test]
fn b765_the_block_never_takes_the_end_of_the_output_below_its_floor() {
    let reason = "NOT RUN: the session's container runtime did not answer";
    let end = "z".repeat(5000);
    let mut s = not_run_step(&format!("{reason}\n{end}"));
    let lines: Vec<String> = (0..40)
        .map(|i| format!("docker.sock line {i} {}", "w".repeat(250)))
        .collect();
    add_runtime_block(&mut s, Some("socket /s: accepts connections"), &lines, 4096);
    assert!(s.output_tail.len() <= 4096, "{}", s.output_tail.len());
    assert!(s.output_tail.starts_with(reason));
    let kept_end = s
        .output_tail
        .chars()
        .rev()
        .take_while(|c| *c == 'z')
        .count();
    assert!(kept_end >= 1000, "the end keeps its floor: {kept_end}");
    assert!(
        s.output_tail
            .contains("RUNTIME: socket /s: accepts connections")
    );
    assert!(
        !s.output_tail.contains("docker.sock line 39"),
        "the block stopped at its budget"
    );
    assert!(s.output_truncated);
}

// ---- the head lines that are written after the block was (review finding F1) -------------------------------------------------------

const SOCKET_LINE: &str =
    "socket /var/run/docker.sock: connection refused (nothing is listening on it)";

/// What `run_steps` has when it calls `set_plan_note`: a `cargo test` step the harness chose, marked not run for the runtime, whose tail is
/// the reason, the block and the end of a long output, filled to the wire bound.
fn cargo_step_with_block(end: &str, lines: &[String]) -> TestStep {
    let mut s = TestStep::not_run(
        TestStepRole::Test,
        TestCommandSource::Detected,
        PLAN_NOTE_COMMAND,
        RUNTIME_DOWN,
        None,
    );
    s.output_tail = format!("NOT RUN: {RUNTIME_DOWN}\n{end}");
    add_runtime_block(&mut s, Some(SOCKET_LINE), lines, 4096);
    s
}

fn block_of(tail: &str) -> Vec<String> {
    tail.lines()
        .filter(|l| l.starts_with("RUNTIME: "))
        .map(str::to_string)
        .collect()
}

fn trailing(tail: &str, c: char) -> usize {
    tail.chars().rev().take_while(|x| *x == c).count()
}

/// Runtime lines of `width` bytes, and the step they make: with the width chosen so that the block takes all its budget and the end of the
/// output keeps no more than its floor and a few bytes, which is the case where one more head line must make the block give way.
fn full_block_step() -> (TestStep, usize) {
    for width in 100..400 {
        let lines: Vec<String> = (0..40)
            .map(|i| format!("DOCKER_HOST line {i} {} \u{e9}", "w".repeat(width)))
            .collect();
        let s = cargo_step_with_block(&"z".repeat(6000), &lines);
        if trailing(&s.output_tail, 'z') <= 1024 + 8 {
            return (s, width);
        }
    }
    panic!("no width fills the budget");
}

#[test]
fn b765_the_plan_note_stays_the_line_after_the_reason_and_the_block_stays_whole() {
    // The case of the review: a block of a few lines, an output far over the bound.
    let lines = vec![
        "WARN DOCKER_HOST unix:///var/run/docker.sock is not listening".to_string(),
        "java.net.ConnectException: Connection refused".to_string(),
        "Could not find a valid Docker environment".to_string(),
    ];
    let mut s = cargo_step_with_block(&"z".repeat(6000), &lines);
    assert_eq!(
        s.output_tail.len(),
        4096,
        "TRIGGER PRESENT: the tail is at the wire bound when the note is written"
    );
    set_plan_note(&mut s, Some(7), 4096);
    let after: Vec<&str> = s.output_tail.lines().collect();
    assert!(after[0].starts_with("NOT RUN: "), "{after:?}");
    assert!(after[1].starts_with(PLAN_NOTE_PREFIX), "{after:?}");
    assert_eq!(
        s.plan_note_members(),
        Some(7),
        "the protocol reads the note from the line after the reason"
    );
    assert_eq!(after[2], format!("RUNTIME: {SOCKET_LINE}"), "{after:?}");
    assert_eq!(after[3], format!("RUNTIME: {}", lines[0]));
    assert_eq!(after[4], format!("RUNTIME: {}", lines[1]));
    assert_eq!(after[5], format!("RUNTIME: {}", lines[2]));
    assert!(s.output_tail.len() <= 4096, "{}", s.output_tail.len());
    assert!(
        trailing(&s.output_tail, 'z') >= 1024,
        "the end keeps its floor"
    );
    assert!(s.output_truncated);
}

#[test]
fn b765_when_the_head_grows_the_block_gives_up_whole_lines_to_keep_the_floor_of_the_end() {
    let (mut s, width) = full_block_step();
    let before = block_of(&s.output_tail);
    assert!(
        trailing(&s.output_tail, 'z') <= 1024 + 8,
        "TRIGGER PRESENT: with lines of {width} bytes the block took all it may, so the note makes the end give"
    );
    set_plan_note(&mut s, Some(7), 4096);
    let after = block_of(&s.output_tail);
    assert!(s.output_tail.len() <= 4096, "{}", s.output_tail.len());
    assert!(
        trailing(&s.output_tail, 'z') >= 1024,
        "the end keeps its floor"
    );
    assert!(
        after.len() < before.len() && !after.is_empty(),
        "whole trailing lines of the block went: {} of {}",
        after.len(),
        before.len()
    );
    assert_eq!(
        after,
        before[..after.len()],
        "no line is cut, the first ones stay"
    );
    assert_eq!(s.plan_note_members(), Some(7));
    assert!(
        s.output_tail
            .lines()
            .nth(2)
            .is_some_and(|l| l.starts_with("RUNTIME: socket ")),
        "the block follows the note: {}",
        s.output_tail
    );
}

#[test]
fn b765_a_block_that_ends_the_tail_is_not_glued_to_the_note() {
    let mut s = TestStep::not_run(
        TestStepRole::Test,
        TestCommandSource::Detected,
        PLAN_NOTE_COMMAND,
        RUNTIME_DOWN,
        None,
    );
    s.output_tail = format!(
        "NOT RUN: {RUNTIME_DOWN}\nRUNTIME: socket /s: does not exist\nRUNTIME: DOCKER_HOST x"
    );
    set_plan_note(&mut s, Some(3), 4096);
    let lines: Vec<&str> = s.output_tail.lines().collect();
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(lines[1].starts_with(PLAN_NOTE_PREFIX), "{lines:?}");
    assert_eq!(lines[2], "RUNTIME: socket /s: does not exist");
    assert_eq!(lines[3], "RUNTIME: DOCKER_HOST x");
    assert_eq!(s.plan_note_members(), Some(3));
}

#[test]
fn b765_the_plan_note_is_written_once_and_a_note_of_the_repository_is_no_note() {
    // The repository prints a note of its own where its output starts, right behind the block.
    let lines = vec!["DOCKER_HOST is not listening".to_string()];
    let mut s = cargo_step_with_block("PLAN NOTE: cargo workspace, 99 members\nthe end\n", &lines);
    set_plan_note(&mut s, Some(7), 4096);
    assert_eq!(s.plan_note_members(), Some(7), "{}", s.output_tail);
    let once = s.output_tail.clone();
    set_plan_note(&mut s, Some(7), 4096);
    assert_eq!(s.output_tail, once, "a second call changes nothing");
    assert_eq!(block_of(&s.output_tail).len(), 2, "{}", s.output_tail);
    // With no members the note goes and the block stays.
    set_plan_note(&mut s, None, 4096);
    assert_eq!(s.plan_note_members(), None);
    assert_eq!(block_of(&s.output_tail).len(), 2, "{}", s.output_tail);
    assert!(s.output_tail.ends_with("the end\n"), "{}", s.output_tail);
}

#[test]
fn b765_a_runtime_looking_line_on_a_step_not_run_for_another_reason_is_not_a_block() {
    // Only a step marked not run for the runtime has a block; a line of that shape in another step's output stays in the output.
    let mut s = TestStep::not_run(
        TestStepRole::Test,
        TestCommandSource::Detected,
        PLAN_NOTE_COMMAND,
        "cargo is not installed here",
        None,
    );
    s.output_tail = "NOT RUN: cargo is not installed here\nRUNTIME: printed by the repository\nsh: 1: cargo: not found\n".to_string();
    set_plan_note(&mut s, Some(3), 4096);
    let lines: Vec<&str> = s.output_tail.lines().collect();
    assert!(lines[1].starts_with(PLAN_NOTE_PREFIX), "{lines:?}");
    assert_eq!(lines[2], "RUNTIME: printed by the repository");
}

#[test]
fn b765_a_head_line_goes_before_the_block_and_the_block_stays_whole() {
    let (mut s, _) = full_block_step();
    let before = block_of(&s.output_tail);
    put_head_line(
        &mut s,
        "NEEDS: ",
        "NEEDS: postgres: nothing answers on 127.0.0.1:5432",
        4096,
    );
    let after = block_of(&s.output_tail);
    assert!(s.output_tail.len() <= 4096, "{}", s.output_tail.len());
    assert!(
        trailing(&s.output_tail, 'z') >= 1024,
        "the end keeps its floor"
    );
    assert!(!after.is_empty() && after.len() < before.len());
    assert_eq!(
        after,
        before[..after.len()],
        "no line is cut, the first ones stay"
    );
    let all: Vec<&str> = s.output_tail.lines().collect();
    assert!(all[0].starts_with("NOT RUN: "));
    assert_eq!(all[1], "NEEDS: postgres: nothing answers on 127.0.0.1:5432");
    assert!(all[2].starts_with("RUNTIME: socket "), "{all:?}");
}

#[test]
fn b765_the_note_and_a_head_line_keep_their_order_before_the_block() {
    let lines = vec!["DOCKER_HOST is not listening".to_string()];
    let mut s = cargo_step_with_block("tail of the output\n", &lines);
    set_plan_note(&mut s, Some(7), 4096);
    put_head_line(&mut s, "NEEDS: ", "NEEDS: postgres: refused", 4096);
    let all: Vec<&str> = s.output_tail.lines().collect();
    assert!(all[0].starts_with("NOT RUN: "), "{all:?}");
    assert!(all[1].starts_with(PLAN_NOTE_PREFIX), "{all:?}");
    assert_eq!(all[2], "NEEDS: postgres: refused");
    assert_eq!(all[3], format!("RUNTIME: {SOCKET_LINE}"));
    assert_eq!(all[4], "RUNTIME: DOCKER_HOST is not listening");
    assert_eq!(all[5], "tail of the output");
    assert_eq!(s.plan_note_members(), Some(7));
}
