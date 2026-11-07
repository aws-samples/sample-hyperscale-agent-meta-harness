//! The compiled `ham` in a clean environment against the local API: fresh XDG directories, one
//! `config.toml` naming the fake and a `credential_process` profile (as `tests/cli.rs`), for the
//! command tests that run the binary.
#![allow(dead_code, clippy::unwrap_used)]

use super::support::{self, AKID, Fake, SECRET, TOKEN};
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Output, Stdio};
use tokio::io::AsyncWriteExt;

pub struct Home {
    pub root: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
    pub downloads: PathBuf,
}

/// Fresh XDG directories, a config naming the fake's endpoint and `profile`.
pub fn home_with(endpoint: &str, label: &str, profile: &str) -> Home {
    let root = support::scratch(label);
    let h = Home {
        config: root.join("config"),
        state: root.join("state"),
        cache: root.join("cache"),
        downloads: root.join("downloads"),
        root,
    };
    for d in [&h.config, &h.state, &h.cache, &h.downloads] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::create_dir_all(h.config.join("ham")).unwrap();
    std::fs::write(
        h.config.join("ham/config.toml"),
        format!(
            "apiEndpoint = \"{endpoint}\"\nregion = \"{}\"\nawsProfile = \"{profile}\"\n",
            super::support::REGION
        ),
    )
    .unwrap();
    let script = h.root.join("creds.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho '{{\"Version\": 1, \"AccessKeyId\": \"{AKID}\", \"SecretAccessKey\": \"{SECRET}\", \"SessionToken\": \"{TOKEN}\", \"Expiration\": \"2099-01-01T00:00:00Z\"}}'\n"
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        h.root.join("aws-config"),
        format!(
            "[profile ham-test]\ncredential_process = {}\nregion = {}\n",
            script.display(),
            super::support::REGION
        ),
    )
    .unwrap();
    std::fs::write(h.root.join("aws-credentials"), "").unwrap();
    h
}

pub fn home(fake: &Fake, label: &str) -> Home {
    home_with(&fake.endpoint, label, "ham-test")
}

/// The `ham` command in a clean environment (coverage variables kept).
pub fn command(h: &Home, args: &[&str], extra: &[(&str, &str)]) -> tokio::process::Command {
    command_of(env!("CARGO_BIN_EXE_ham"), h, args, extra)
}

/// [`command`] for the program at `program`: the binary itself, or a link to it that has another name.
pub fn command_of(
    program: impl AsRef<std::ffi::OsStr>,
    h: &Home,
    args: &[&str],
    extra: &[(&str, &str)],
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.env_clear();
    for (k, v) in std::env::vars() {
        if k.starts_with("LLVM_") || k.starts_with("CARGO_LLVM_COV") {
            cmd.env(k, v);
        }
    }
    cmd.args(args)
        .envs(extra.iter().copied())
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TMPDIR", std::env::temp_dir())
        .env("HOME", &h.root)
        .env("XDG_CONFIG_HOME", &h.config)
        .env("XDG_STATE_HOME", &h.state)
        .env("XDG_CACHE_HOME", &h.cache)
        .env("HAMH_DOWNLOAD_DIR", &h.downloads)
        .env("AWS_CONFIG_FILE", h.root.join("aws-config"))
        .env(
            "AWS_SHARED_CREDENTIALS_FILE",
            h.root.join("aws-credentials"),
        )
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .kill_on_drop(true);
    cmd
}

pub async fn ham(h: &Home, args: &[&str]) -> Output {
    ham_env(h, args, &[]).await
}

pub async fn ham_env(h: &Home, args: &[&str], extra: &[(&str, &str)]) -> Output {
    command(h, args, extra)
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap()
}

/// `ham` with `input` on stdin (closed after it).
pub async fn ham_stdin(h: &Home, args: &[&str], extra: &[(&str, &str)], input: &[u8]) -> Output {
    let mut child = command(h, args, extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input).await.unwrap();
    drop(stdin);
    child.wait_with_output().await.unwrap()
}

pub fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Every stdout line that is JSON.
pub fn json_lines(o: &Output) -> Vec<Value> {
    text(&o.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Nothing secret in either stream: no signing secret, session token or SigV4 header.
pub fn assert_no_secrets(o: &Output) {
    for stream in [text(&o.stdout), text(&o.stderr)] {
        for secret in [
            SECRET,
            TOKEN,
            "AWS4-HMAC-SHA256",
            ", Signature=",
            "authorization",
        ] {
            assert!(!stream.contains(secret), "{secret:?} leaked:\n{stream}");
        }
    }
}

/// A run of the caller, created through the API as the `direct` stage, with the harness quiet (no autopilot).
pub async fn new_run(fake: &Fake, prompt: &str) -> String {
    let api = ham::api::Api::new(std::sync::Arc::new(
        ham::https::HttpsPlane::new(
            &fake.endpoint,
            ham::sigv4::Signer::new(support::REGION, support::creds()),
        )
        .unwrap(),
    ));
    api.start_direct_run(
        &serde_json::from_value(serde_json::json!({
            "prompt": prompt, "target": {"mode": "s3-only"}, "model": "global.anthropic.claude-sonnet-5-5"
        }))
        .unwrap(),
        &format!("k-{prompt}"),
    )
    .await
    .unwrap()
    .run_id
}
