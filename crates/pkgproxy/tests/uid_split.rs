//! That uid can use the proxy (it needs the packages) and cannot reach the token: the proxy process's `environ` and `mem` belong to
//! root with no group or other permission bit (a metadata check, nothing is read as the engine), and the token is in no file under
//! the data root or the run directory. The uid drop needs root, so the test is ignored in plain CI (as `agentd`'s
//! `package_token_uid.rs` is); run it in a root container:
//!
//! ```text
//! docker run --rm --user 0 -v <test binary>:/t:ro --tmpfs /tmp node:24-bookworm /t --ignored --nocapture
//! ```
#![allow(clippy::unwrap_used)]

mod common;
use common::*;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

const ENGINE_UID: u32 = 65534; // nobody

fn euid() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .and_then(|l| l.split_whitespace().nth(2))
        .and_then(|v| v.parse().ok())
        .unwrap()
}

/// `argv` as the engine uid: (exit code, stdout, stderr).
async fn as_engine(argv: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(argv[0])
        .args(&argv[1..])
        .uid(ENGINE_UID)
        .gid(ENGINE_UID)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn files_holding(root: &Path, needle: &str, hits: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(md) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if md.is_dir() {
            files_holding(&p, needle, hits);
        } else if md.is_file()
            && std::fs::read(&p)
                .map(|b| find(&b, needle.as_bytes()).is_some())
                .unwrap_or(false)
        {
            hits.push(p.display().to_string());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs root for a real uid split; run as root in a container (module comment)"]
async fn the_engine_uid_can_use_the_proxy_and_cannot_read_the_token() {
    assert_eq!(
        euid(),
        0,
        "run this test as root: it drops to the engine uid"
    );

    // agentd's layout: a data root and a run directory on disk, the proxy in the root process.
    let data_root = tempfile::tempdir().unwrap();
    let run_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        data_root.path().join("note.txt"),
        "agentd state, no token\n",
    )
    .unwrap();
    let r = rig(
        always(Reply::ok("served").header("Content-Type", "application/octet-stream")),
        nothing(),
        nothing(),
    )
    .await;
    let port = r.addr().port();
    let pid = std::process::id();

    // 1. The engine can use the proxy (it installs packages through it).
    let url = format!("http://127.0.0.1:{port}/pypi/simple/demo/");
    let (code, out, err) = as_engine(&["curl", "-s", "-S", "--max-time", "10", &url]).await;
    assert_eq!(
        (code, out.as_str()),
        (Some(0), "served"),
        "the engine could not use the proxy: {err}"
    );
    assert_eq!(
        r.ca.seen()[0].header("authorization"),
        Some(basic_for(&r.token).as_str()),
        "the proxy added the credential for the engine's request"
    );

    // 2. The kernel keeps the proxy process's environment and memory from other uids: both belong to root and carry no group or
    // other permission bit. Checked by metadata in this process; nothing is read as the engine uid.
    for f in ["environ", "mem"] {
        let path = format!("/proc/{pid}/{f}");
        let md = std::fs::metadata(&path).unwrap();
        assert_eq!(md.uid(), 0, "{path} belongs to root");
        assert_eq!(md.mode() & 0o077, 0, "{path} is open to a group or other");
    }

    // 3. The token is in no file: not under the data root, the run directory or the process's temp directory.
    let needle: String = r.token.chars().take(12).collect();
    let b64_needle: String = b64(format!("aws:{}", r.token).as_bytes())
        .chars()
        .take(12)
        .collect();
    let mut hits = Vec::new();
    let tmp = std::env::temp_dir();
    for root in [data_root.path(), run_dir.path(), tmp.as_path()] {
        files_holding(root, &needle, &mut hits);
        files_holding(root, &b64_needle, &mut hits);
    }
    assert!(
        hits.is_empty(),
        "the token (or its Basic form) was written to disk: {hits:?}"
    );

    // 4. The command line of the proxy's process does not carry it either (the cmdline is world-readable).
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
    assert!(find(&cmdline, needle.as_bytes()).is_none());

    // 5. What the engine sees of the proxy (env, settings.xml) holds no token.
    let env = format!("{:?}{}", r.proxy.env(), r.proxy.maven_settings_xml());
    assert!(!env.contains(&needle));
}
