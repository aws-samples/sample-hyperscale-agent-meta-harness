//! The seccomp filter and the doors around Landlock's TCP rules. Landlock closes `connect()` to a named port, and
//! that is the only thing between the engine and agentd's own port, the platform's credential-vending port and port 80. It does not see the
//! implicit connect of `sendto(MSG_FASTOPEN)` on kernels without the fix, it does not apply to multipath TCP sockets, and a send queued on
//! io_uring never reaches the filter's argument rules. So the launcher's filter answers `EPERM` to all three.
//!
//! The check is the probe's own rows (`fixtures/direct-probe/sandbox_probe.py`): run unconfined they must be able to fail, and run under the filter
//! they must pass. The kernel's Landlock is not needed for either; the filter alone is the subject.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::sandbox::ipc;
use common::{repo_root, scratch};
use serde_json::Value;
use std::net::TcpListener;
use std::process::Command;

const CHILD: &str = "HAMH_IPC_NETWORK_CHILD";
const ROWS: [&str; 3] = ["fast-open-agentd-port", "mptcp-agentd-port", "io-uring"];

fn script() -> std::path::PathBuf {
    repo_root().join("fixtures/direct-probe/sandbox_probe.py")
}

/// Runs the probe next to a listener on a loopback port of this test (agentd's port stands in for it) and returns the report.
fn run_probe(name: &str) -> Value {
    let dir = scratch(name);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let private = dir.join("private.txt");
    std::fs::write(&private, "private").unwrap();
    // The holder the probe's other rows look at: another process of this user.
    let mut holder = Command::new("sleep").arg("120").spawn().unwrap();
    let holder_pid = holder.id().to_string();
    let out = Command::new("python3")
        .arg(script())
        .args(["--mode", "landlock", "--private-file"])
        .arg(&private)
        .args(["--secret-pid", &holder_pid, "--signal-pid", &holder_pid])
        .args(["--canary-sha256", "0000"])
        .args(["--launcher", env!("CARGO_BIN_EXE_agentd")])
        .args(["--agentd-port", &port.to_string()])
        .output()
        .unwrap();
    let _ = holder.kill();
    let _ = holder.wait();
    drop(listener);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{e}: {text}\n{}", String::from_utf8_lossy(&out.stderr)))
}

fn status(report: &Value, row: &str) -> String {
    report["checks"][row]["status"]
        .as_str()
        .unwrap_or("missing")
        .to_string()
}

fn detail(report: &Value, row: &str) -> String {
    report["checks"][row]["detail"]
        .as_str()
        .unwrap_or("missing")
        .to_string()
}

#[test]
fn the_probe_has_a_row_for_each_door() {
    let report = run_probe("ipc-net-rows");
    for row in ROWS {
        assert_ne!(
            status(&report, row),
            "missing",
            "{row} is not a row of the probe"
        );
    }
}

#[test]
fn an_unconfined_process_opens_a_connection_with_fast_open() {
    // The row has to be able to fail: with no filter the data goes to the listener. A kernel that has TCP Fast Open off for clients refuses.
    let on = std::fs::read_to_string("/proc/sys/net/ipv4/tcp_fastopen")
        .ok()
        .and_then(|t| t.trim().parse::<u32>().ok())
        .is_some_and(|v| v & 1 == 1);
    if !on || !cfg!(target_os = "linux") {
        eprintln!("NOT RUN: TCP Fast Open is not enabled for clients on this machine");
        return;
    }
    let report = run_probe("ipc-net-open");
    assert_eq!(
        status(&report, "fast-open-agentd-port"),
        "fail",
        "{}",
        detail(&report, "fast-open-agentd-port")
    );
}

#[test]
fn a_process_with_the_filter_is_refused_fast_open_multipath_tcp_and_io_uring() {
    if std::env::var(CHILD).is_ok() {
        // The child: the filter first, then the probe, started from this thread so that it inherits the filter.
        ipc::install().unwrap();
        let report = run_probe("ipc-net-filtered");
        let not_refused: Vec<String> = ROWS
            .iter()
            .filter(|row| status(&report, row) != "pass")
            .map(|row| format!("{row}: {} ({})", status(&report, row), detail(&report, row)))
            .collect();
        assert!(
            not_refused.is_empty(),
            "under the filter these doors are open: {not_refused:?}"
        );
        return;
    }
    if !cfg!(target_os = "linux") {
        eprintln!("NOT RUN: seccomp is Linux only");
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let out = Command::new(exe)
        .args([
            "--exact",
            "a_process_with_the_filter_is_refused_fast_open_multipath_tcp_and_io_uring",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
