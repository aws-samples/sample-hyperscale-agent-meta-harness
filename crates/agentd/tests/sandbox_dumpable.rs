//! In the `landlock` mode `agentd serve` makes itself non-dumpable at once, so a process of the same user cannot read its environment or memory.
#![allow(clippy::unwrap_used)]

mod common;

use std::io::ErrorKind;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn serve(mode: &str, log: &std::path::Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["serve", "--listen", "127.0.0.1:0"])
        .env_clear()
        .env("AGENTD_ENGINE_SANDBOX", mode)
        .env("AGENTD_ENGINE_USER", "")
        .stderr(std::fs::File::create(log).unwrap())
        .stdout(Stdio::null())
        .spawn()
        .unwrap()
}

/// Whether the effective capability set of this process is not empty.
fn has_capabilities() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .is_some_and(|v| !v.trim().trim_start_matches('0').is_empty())
}

fn wait_listening(log: &std::path::Path) {
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end {
        if std::fs::read_to_string(log)
            .unwrap_or_default()
            .contains("listening on")
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "agentd did not report listening: {}",
        std::fs::read_to_string(log).unwrap_or_default()
    );
}

#[test]
fn landlock_mode_denies_a_same_user_reader_the_environment_and_uid_mode_does_not() {
    if has_capabilities() {
        eprintln!(
            "NOT RUN: this process holds capabilities, which read any process (a root with none, as in the Instances container, is fine)"
        );
        return;
    }
    let dir = common::scratch("sandbox-dumpable");
    for (mode, expect_denied) in [("uid", false), ("landlock", true)] {
        let log = dir.join(format!("{mode}.log"));
        let mut child = serve(mode, &log);
        wait_listening(&log);
        let read = std::fs::read(format!("/proc/{}/environ", child.id()));
        let _ = child.kill();
        let _ = child.wait();
        match (read, expect_denied) {
            (Err(e), true) => assert_eq!(e.kind(), ErrorKind::PermissionDenied, "{mode}: {e}"),
            (Ok(_), false) => {}
            (other, _) => panic!("{mode}: expected denied={expect_denied}, got {other:?}"),
        }
    }
}
