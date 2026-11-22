//! What `agentd serve` does with a stop signal as the first process of its machine, and as the worker behind it
//! (`server::end_on_signals`), driven in the test's own process.
//!
//! A real `agentd serve` in either place runs on after the signal until its machine ends. It never ends by itself, so it writes no
//! coverage profile; `tests/b883_first_process.rs` and `tests/b823_stop_copy.rs` hold what it does from outside. Here the same
//! function runs inside the test process, which ends by itself. For the first process the test starts itself once more, by its name,
//! as process 1 of a process-id namespace; on a machine that cannot make one it prints `SKIPPED:` and holds nothing, and with
//! `HAMH_TEST_REQUIRE_PROCESS_1=1` it fails there.
//!
//! The first test marks its process as the worker behind process 1, and the mark stays. No other test of this file reads it: the
//! copy that runs as process 1 is a process of its own.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::config::Config;
use agentd::logging::MemoryLog;
use agentd::server::{App, StopSignals, end_on_signals};
use direct_support::{as_first_process, first_process_possible};
use std::time::{Duration, Instant};

fn send(signal: &str, pid: u32) {
    let status = std::process::Command::new("kill")
        .args(["-s", signal, &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill -s {signal} {pid}: {status}");
}

/// Starts `end_on_signals` in this process and sends the process SIGTERM, then SIGINT. Each must be answered with the line that
/// holds `says`, and the task must run on after each.
async fn runs_on_after_each_stop_signal(says: &str) {
    let log = MemoryLog::new();
    let app = App::new(Config::from_env(), log.clone());
    let stops = StopSignals::watch(&*log);
    assert!(
        stops.is_some(),
        "the stop signals can be watched: {:?}",
        log.lines()
    );
    let task = tokio::spawn(end_on_signals(app, stops));
    for (answered, signal) in ["TERM", "INT"].into_iter().enumerate() {
        send(signal, std::process::id());
        let began = Instant::now();
        while log.lines().iter().filter(|l| l.contains(says)).count() <= answered {
            assert!(
                began.elapsed() < Duration::from_secs(20),
                "SIG{signal} is answered with {says:?}; log: {:?}",
                log.lines()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !task.is_finished(),
            "agentd runs on after SIG{signal}; log: {:?}",
            log.lines()
        );
    }
    let lines = log.lines();
    assert!(
        lines
            .iter()
            .any(|l| l
                == "termination signal SIGTERM: no turn is working, so there is no work to copy"),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("agentd ends")),
        "{lines:?}"
    );
    task.abort();
}

/// The worker behind process 1 answers a stop signal and runs on, as process 1 did when it served itself.
#[tokio::test]
async fn b883_the_worker_behind_process_1_runs_on_after_a_stop_signal() {
    agentd::first::note_behind_first();
    assert!(
        agentd::first::behind_first() && std::process::id() != 1,
        "TRIGGER PRESENT: this process counts as the worker behind process 1 and is not process 1"
    );
    runs_on_after_each_stop_signal(
        "agentd works behind the first process of its machine and runs on until the machine ends",
    )
    .await;
}

/// Without the two signals to watch there is nothing to answer, and the task ends at once.
#[tokio::test]
async fn b883_the_task_of_the_stop_signals_ends_at_once_when_they_cannot_be_watched() {
    let app = App::new(Config::from_env(), MemoryLog::new());
    tokio::time::timeout(Duration::from_secs(5), end_on_signals(app, None))
        .await
        .expect("the task returns when there is no signal to watch");
}

const AS_PROCESS_1: &str = "b883_process_1_runs_on_after_a_stop_signal";

/// Process 1 answers a stop signal and runs on. The test starts itself once more as process 1 of a process-id namespace, and that
/// copy drives the function in its own process and ends by itself, so in a coverage run it writes its profile.
#[tokio::test]
async fn b883_process_1_runs_on_after_a_stop_signal() {
    if std::process::id() == 1 {
        eprintln!("TRIGGER PRESENT: this copy of the test is process 1 of its machine");
        runs_on_after_each_stop_signal(
            "agentd is the first process of its machine and runs on until the machine ends",
        )
        .await;
        return;
    }
    if !first_process_possible() {
        eprintln!(
            "SKIPPED: this machine cannot start a process as the first one of a process-id namespace (unshare --user --pid --fork)"
        );
        return;
    }
    let output = as_first_process(&std::env::current_exe().unwrap())
        .args(["--exact", AS_PROCESS_1, "--nocapture"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("TRIGGER PRESENT: this copy of the test is process 1 of its machine"),
        "{text}"
    );
    assert!(
        output.status.success() && text.contains("test result: ok. 1 passed"),
        "the copy of this test that ran as process 1: {:?}\n{text}",
        output.status
    );
    eprintln!("TRIGGER PRESENT: a copy of this test ran as process 1 of a namespace and passed");
}
