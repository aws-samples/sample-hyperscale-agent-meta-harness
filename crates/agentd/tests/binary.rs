//! The agentd binary: `agentd serve` answers /ping on the configured address; usage errors exit 2.
#![allow(clippy::unwrap_used)]

mod common;

use common::scratch;
use std::process::Command;
use std::time::{Duration, Instant};

const EXE: &str = env!("CARGO_BIN_EXE_agentd");

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn serve_answers_ping_until_killed() {
    let d = scratch("bin-serve");
    let port = free_port();
    let mut child = Command::new(EXE)
        .arg("serve")
        .env("AGENTD_LISTEN", format!("127.0.0.1:{port}"))
        .env("AGENTD_WORKSPACE_ROOT", d.join("ws"))
        .env("AGENTD_DATA_ROOT", d.join("data"))
        .env("AGENTD_RUNTIME_DIR", d.join("run"))
        .env_remove("AGENTD_EXE")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let body = loop {
        let out = Command::new("curl")
            .args(["-s", &format!("http://127.0.0.1:{port}/ping")])
            .output()
            .unwrap();
        if out.status.success() && !out.stdout.is_empty() {
            break String::from_utf8(out.stdout).unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "agentd serve never answered /ping"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    child.kill().unwrap();
    let _ = child.wait();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["status"], "Healthy");
    assert!(v["time_of_last_update"].as_u64().unwrap() > 0);
}

#[test]
fn a_taken_port_exits_1_and_usage_errors_exit_2() {
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = busy.local_addr().unwrap().to_string();
    let out = Command::new(EXE)
        .args(["serve", "--listen", &addr])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for args in [
        vec!["serve", "--listen", "not-an-address"],
        vec!["frobnicate"],
        vec![],
    ] {
        assert_eq!(
            Command::new(EXE)
                .args(&args)
                .output()
                .unwrap()
                .status
                .code(),
            Some(2),
            "{args:?}"
        );
    }
    let v = Command::new(EXE).arg("--version").output().unwrap();
    assert!(v.status.success());
    assert!(String::from_utf8_lossy(&v.stdout).starts_with("agentd "));
}
