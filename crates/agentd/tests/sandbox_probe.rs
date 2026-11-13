//! The isolation probe of the engine: it must be able to fail. Run unconfined, as the user
//! that owns the secrets, it has to report every refusal it looks for as missing; the live gate runs it through the launcher on the Instances machine,
//! where each of them has to hold. The text of the script follows the rules of its directory.
#![allow(clippy::unwrap_used)]

mod common;

use common::{repo_root, scratch, unique_suffix};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::process::Command;

fn script() -> std::path::PathBuf {
    repo_root().join("fixtures/direct-probe/sandbox_probe.py")
}

/// Runs the probe unconfined next to a holder of the same user; returns the report, the raw text and the exit code.
fn run_probe(name: &str, extra: &[String]) -> (Value, String, Option<i32>) {
    let dir = scratch(name);
    let canary = format!("canary-value-{}", unique_suffix());
    let digest: String = Sha256::digest(canary.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    // The holder: another process of this user, with the canary in its environment, that outlives the probe.
    let mut holder = Command::new("sleep")
        .arg("120")
        .env("HAMH_PROBE_HOLDER_VALUE", &canary)
        .spawn()
        .unwrap();
    let private = dir.join("private.txt");
    std::fs::write(&private, "private").unwrap();
    let out = Command::new("python3")
        .arg(script())
        .args(["--mode", "landlock", "--private-file"])
        .arg(&private)
        .args(["--secret-pid", &holder.id().to_string()])
        .args(["--signal-pid", &holder.id().to_string()])
        .args(["--canary-sha256", &digest])
        .args(["--launcher", env!("CARGO_BIN_EXE_agentd")])
        .args(["--agentd-port", "9"])
        .args(extra)
        .output()
        .unwrap();
    let _ = holder.kill();
    let _ = holder.wait();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let report: Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{e}: {text}\n{}", String::from_utf8_lossy(&out.stderr)));
    (report, text, out.status.code())
}

fn failed_checks(report: &Value) -> Vec<String> {
    report["failed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn an_unconfined_process_of_the_secret_holders_user_fails_every_refusal_the_probe_looks_for() {
    let (report, text, code) = run_probe("sandbox-probe", &[]);
    assert_eq!(report["schema"], "hamh-sandbox-probe/1");
    let failed = failed_checks(&report);
    for must in [
        "private-file",
        "proc-environ",
        "proc-mem",
        "signal-to-secret-holder",
        "canary-absent",
    ] {
        assert!(
            failed.iter().any(|f| f == must),
            "{must} must fail unconfined: {text}"
        );
    }
    assert_eq!(code, Some(1), "{text}");
    let status = |name: &str| {
        report["checks"][name]["status"]
            .as_str()
            .unwrap_or("missing")
            .to_string()
    };
    for control in [
        "control-tmp-write",
        "control-tmp-literal",
        "control-var-tmp",
        "control-dev-shm-lock",
        "control-loopback",
        "control-exec",
        "no-inherited-descriptor",
        "launcher-fails-closed",
    ] {
        assert_eq!(status(control), "pass", "{control}: {text}");
    }
    // Without the ids of objects made outside, the SysV checks say they did not look.
    assert_eq!(status("sysv-shm"), "info", "{text}");
    assert_eq!(status("sysv-msg"), "info", "{text}");
}

/// Makes a SysV object with `ipcmk` (util-linux) and returns its id; `flag` is `-M` (segment) or `-Q` (queue).
fn ipcmk(flag: &str) -> Option<String> {
    let out = Command::new("ipcmk").args([flag, "4096"]).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .last()
        .filter(|w| w.parse::<u64>().is_ok())
        .map(str::to_string)
}

#[test]
fn an_unconfined_process_reaches_a_sysv_segment_and_queue_another_process_made() {
    let (Some(shm), Some(msg)) = (ipcmk("-M"), ipcmk("-Q")) else {
        eprintln!("NOT RUN: ipcmk (util-linux) is not installed");
        return;
    };
    let (report, text, _) = run_probe(
        "sandbox-probe-ipc",
        &[
            "--sysv-shm-id".to_string(),
            shm.clone(),
            "--sysv-msg-id".to_string(),
            msg.clone(),
        ],
    );
    let _ = Command::new("ipcrm").args(["-m", &shm]).output();
    let _ = Command::new("ipcrm").args(["-q", &msg]).output();
    let failed = failed_checks(&report);
    for must in ["sysv-shm", "sysv-msg"] {
        assert!(
            failed.iter().any(|f| f == must),
            "{must} must fail unconfined: {text}"
        );
    }
}

#[test]
fn the_script_follows_the_rules_of_its_directory() {
    let text = std::fs::read_to_string(script()).unwrap();
    assert!(text.is_ascii(), "ASCII only");
    assert!(!text.contains('\t'), "no tab");
    for (i, line) in text.lines().enumerate() {
        assert!(
            line.len() <= 160,
            "line {} is {} characters",
            i + 1,
            line.len()
        );
    }
    assert!(!text.ends_with("\n\n"), "no trailing blank line");
    for word in ["verified", "tested", "gated", "evaluated"] {
        assert!(!text.to_lowercase().contains(word), "the word {word}");
    }
    for shape in [
        "AKIA",
        "ASIA",
        "ghp_",
        "github_pat_",
        "BEGIN RSA PRIVATE",
        "BEGIN PRIVATE KEY",
        "BEGIN OPENSSH",
    ] {
        assert!(!text.contains(shape), "key-shaped text {shape}");
    }
}
