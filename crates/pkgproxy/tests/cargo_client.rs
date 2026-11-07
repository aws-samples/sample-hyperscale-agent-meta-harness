//! A real cargo against the proxy. `cargo fetch --locked` and then `cargo build --locked --offline` run on a one-dependency
//! workspace with exactly the `config.toml` that agentd writes into the engine's `CARGO_HOME` (`Handle::cargo_config_toml`). The
//! fake CodeArtifact serves a tiny sparse registry over HTTPS: `config.json`, one index entry and one real `.crate` tarball built
//! here from a two-file crate, with its SHA-256 in the index and in the lock file. It answers 401 to every request that does not
//! carry the token as the whole `Authorization` value, so the run passes only when the proxy adds the token and cargo itself sends
//! none. Nothing leaves the loopback interface: cargo talks to the proxy, the proxy to the fake.
//!
//! The cargo that runs is the one that runs this test (`CARGO`, set by cargo and by nextest) with a scrubbed environment: no
//! `RUSTFLAGS`, no wrapper, its own `CARGO_HOME` and target directory under the temporary directory.
#![allow(clippy::unwrap_used)]

mod common;
use common::*;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;
use tokio::process::Command;

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// One ustar entry: a 512-byte header and the data padded to a block.
fn tar_entry(name: &str, data: &[u8]) -> Vec<u8> {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[100..107].copy_from_slice(b"0000644");
    h[108..115].copy_from_slice(b"0000000");
    h[116..123].copy_from_slice(b"0000000");
    h[124..135].copy_from_slice(format!("{:011o}", data.len()).as_bytes());
    h[136..147].copy_from_slice(b"00000000000");
    h[148..156].copy_from_slice(b"        ");
    h[156] = b'0';
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
    let mut out = h.to_vec();
    out.extend_from_slice(data);
    out.resize(512 + data.len().div_ceil(512) * 512, 0);
    out
}

/// A gzip stream made of stored deflate blocks (no compression).
fn gzip_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    let mut chunks = data.chunks(65535).peekable();
    if chunks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(c) = chunks.next() {
        out.push(u8::from(chunks.peek().is_none()));
        let n = c.len() as u16;
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&(!n).to_le_bytes());
        out.extend_from_slice(c);
    }
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

const FOO_TOML: &str = "[package]\nname = \"foo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
const FOO_LIB: &str = "pub fn one() -> u32 {\n    1\n}\n";

/// The `.crate` file of `foo 0.1.0`: a gzip'd tar of `foo-0.1.0/Cargo.toml` and `foo-0.1.0/src/lib.rs`.
fn foo_crate() -> Vec<u8> {
    let mut tar = tar_entry("foo-0.1.0/Cargo.toml", FOO_TOML.as_bytes());
    tar.extend(tar_entry("foo-0.1.0/src/lib.rs", FOO_LIB.as_bytes()));
    tar.extend(std::iter::repeat_n(0u8, 1024));
    gzip_stored(&tar)
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// CodeArtifact's cargo endpoint for the one crate `foo` (a three-letter name: its index file is `3/f/foo`).
fn registry(tok: String, crate_bytes: Vec<u8>) -> Handler {
    let cksum = sha256_hex(&crate_bytes);
    handler(move |s| {
        if s.header("authorization") != Some(tok.as_str()) {
            return Reply::status(401).header("Content-Type", "text/plain");
        }
        match s.target.as_str() {
            "/cargo/packages/config.json" => Reply::ok(format!(
                "{{\"dl\":\"https://{CA_HOST}/cargo/packages/crates/{{crate}}/{{version}}\",\"api\":\"https://{CA_HOST}/cargo/packages/-\",\"auth-required\":true}}"
            ))
            .header("Content-Type", "application/json"),
            "/cargo/packages/3/f/foo" => Reply::ok(format!(
                "{{\"name\":\"foo\",\"vers\":\"0.1.0\",\"deps\":[],\"cksum\":\"{cksum}\",\"features\":{{}},\"yanked\":false}}\n"
            ))
            .header("Content-Type", "application/octet-stream")
            .header("ETag", "\"foo-index-1\""),
            "/cargo/packages/crates/foo/0.1.0" => Reply::ok(crate_bytes.clone())
                .header("Content-Type", "application/gzip")
                .header("Content-Disposition", "attachment"),
            _ => Reply::status(404),
        }
    })
}

/// An application that depends on `foo`, with its lock file (`version = 3` is read by every cargo since 1.53).
fn write_workspace(dir: &Path, cksum: &str) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nfoo = \"0.1\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub fn two() -> u32 {\n    foo::one() + 1\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("Cargo.lock"),
        format!(
            "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 3\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"foo\",\n]\n\n[[package]]\nname = \"foo\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{cksum}\"\n"
        ),
    )
    .unwrap();
}

struct Scratch {
    _dir: tempfile::TempDir,
    app: PathBuf,
    home: PathBuf,
    target: PathBuf,
}

/// A workspace, an empty `CARGO_HOME` holding the config the engine gets, and a target directory.
fn scratch(config_toml: &str, cksum: &str) -> Scratch {
    let dir = tempfile::tempdir().unwrap();
    let (app, home, target) = (
        dir.path().join("app"),
        dir.path().join("cargo-home"),
        dir.path().join("target"),
    );
    write_workspace(&app, cksum);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.toml"), config_toml).unwrap();
    Scratch {
        _dir: dir,
        app,
        home,
        target,
    }
}

/// Runs cargo with an environment built from a short list, so nothing of the test run (flags, wrappers, a target directory, a
/// registry setting) changes what it does.
async fn cargo(sc: &Scratch, args: &[&str]) -> Output {
    let bin = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(bin);
    cmd.args(args).current_dir(&sc.app).env_clear();
    for key in [
        "PATH",
        "HOME",
        "USER",
        "LANG",
        "TMPDIR",
        "LD_LIBRARY_PATH",
        "RUSTUP_HOME",
        "RUSTUP_TOOLCHAIN",
    ] {
        if let Some(v) = std::env::var_os(key) {
            cmd.env(key, v);
        }
    }
    cmd.env("CARGO_HOME", &sc.home)
        .env("CARGO_TARGET_DIR", &sc.target)
        .env("CARGO_BUILD_JOBS", "2")
        .env("CARGO_INCREMENTAL", "0")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(600), cmd.output())
        .await
        .expect("cargo finished within 10 minutes")
        .expect("cargo started")
}

fn text(o: &Output) -> String {
    format!(
        "status={:?}\n--- stdout\n{}\n--- stderr\n{}",
        o.status.code(),
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cargo_fetches_and_builds_a_locked_dependency_through_the_proxy_without_a_token_of_its_own()
{
    let krate = foo_crate();
    let r = rig(registry(token(), krate.clone()), nothing(), nothing()).await;
    let sc = scratch(&r.proxy.cargo_config_toml(), &sha256_hex(&krate));

    let fetch = cargo(&sc, &["fetch", "--locked"]).await;
    assert!(fetch.status.success(), "{}", text(&fetch));
    let seen = r.ca.seen();
    let targets: Vec<&str> = seen.iter().map(|s| s.target.as_str()).collect();
    for want in [
        "/cargo/packages/config.json",
        "/cargo/packages/3/f/foo",
        "/cargo/packages/crates/foo/0.1.0",
    ] {
        assert!(targets.contains(&want), "{want} not asked for: {targets:?}");
    }
    for s in &seen {
        assert_eq!(
            s.headers_named("authorization"),
            vec![r.token.as_str()],
            "{}: the proxy's token only, as the whole value",
            s.target
        );
    }

    // The agent's usual second step: the crates are in the cache, so the build needs no registry at all.
    let before = r.ca.count();
    let build = cargo(&sc, &["build", "--locked", "--offline"]).await;
    assert!(build.status.success(), "{}", text(&build));
    assert_eq!(r.ca.count(), before, "an offline build asked for nothing");

    // A cold second run (a new CARGO_HOME) goes through the proxy again and works the same.
    let sc2 = scratch(&r.proxy.cargo_config_toml(), &sha256_hex(&krate));
    let again = cargo(&sc2, &["fetch", "--locked"]).await;
    assert!(again.status.success(), "{}", text(&again));
    assert!(r.ca.count() > before);
    let snap = r.proxy.snapshot();
    assert!(
        snap.by_prefix.get("cargo").copied().unwrap_or(0) >= 6,
        "{snap:?}"
    );
    assert!(
        !text(&fetch).contains(&r.token) && !text(&build).contains(&r.token),
        "the token appears nowhere in what cargo printed"
    );
}

/// Cargo reads a 401 for `config.json` as "this registry needs a credential" and prints advice about credential providers, which an
/// expired package token cannot be cured by. The proxy answers 403 with a plain body for that one path, and cargo prints the body.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cargo_names_an_expired_token_instead_of_asking_for_a_credential_provider() {
    let krate = foo_crate();
    // Expired from the start: the first request, for config.json, is refused locally and the fake is never asked.
    let mut o = Opts::new(registry(token(), krate.clone()), nothing(), nothing());
    o.expires_at_unix = Some(5_100);
    o.clock_now = 6_000;
    let r = rig_with(o).await;
    let sc = scratch(&r.proxy.cargo_config_toml(), &sha256_hex(&krate));
    let fetch = cargo(&sc, &["fetch", "--locked"]).await;
    assert!(!fetch.status.success(), "{}", text(&fetch));
    let shown = text(&fetch);
    assert!(shown.contains("got 403"), "{shown}");
    assert!(shown.contains("package token expired"), "{shown}");
    assert!(!shown.contains("credential-provider"), "{shown}");
    assert_eq!(r.ca.count(), 0, "an expired token is refused locally");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cargo_names_a_token_the_registry_refuses_in_the_same_way() {
    let krate = foo_crate();
    // The token is wrong: the fake answers 401 to everything, as CodeArtifact does.
    let r = rig(
        registry("not-the-token".into(), krate.clone()),
        nothing(),
        nothing(),
    )
    .await;
    let sc = scratch(&r.proxy.cargo_config_toml(), &sha256_hex(&krate));
    let fetch = cargo(&sc, &["fetch", "--locked"]).await;
    assert!(!fetch.status.success(), "{}", text(&fetch));
    let shown = text(&fetch);
    assert!(shown.contains("got 403"), "{shown}");
    assert!(
        shown.contains("package registry refused the token"),
        "{shown}"
    );
    assert!(!shown.contains("credential-provider"), "{shown}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crate_the_registry_does_not_have_is_reported_as_missing() {
    let krate = foo_crate();
    let inner = registry(token(), krate.clone());
    let down = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = down.clone();
    let r = rig(
        handler(move |s| {
            if s.target == "/cargo/packages/3/f/foo" {
                // The lock names `foo`, so cargo asks its index file; a 404 there is "no such crate".
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Reply::status(404);
            }
            inner(s)
        }),
        nothing(),
        nothing(),
    )
    .await;
    let sc = scratch(&r.proxy.cargo_config_toml(), &sha256_hex(&krate));
    let fetch = cargo(&sc, &["fetch", "--locked"]).await;
    assert!(!fetch.status.success(), "{}", text(&fetch));
    assert!(
        text(&fetch).contains("no matching package named `foo` found"),
        "{}",
        text(&fetch)
    );
    assert!(down.load(std::sync::atomic::Ordering::SeqCst) >= 1);
}

/// What a session commits must work outside it: with the source replaced, cargo still writes the public registry into
/// `Cargo.lock`, so a lock file made through the proxy names nothing of the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lock_file_made_through_the_proxy_names_the_public_registry_and_not_the_session() {
    let krate = foo_crate();
    let r = rig(registry(token(), krate.clone()), nothing(), nothing()).await;
    let sc = scratch(&r.proxy.cargo_config_toml(), &sha256_hex(&krate));
    std::fs::remove_file(sc.app.join("Cargo.lock")).unwrap();
    let made = cargo(&sc, &["generate-lockfile"]).await;
    assert!(made.status.success(), "{}", text(&made));
    let lock = std::fs::read_to_string(sc.app.join("Cargo.lock")).unwrap();
    assert!(
        lock.contains("source = \"registry+https://github.com/rust-lang/crates.io-index\""),
        "{lock}"
    );
    assert!(
        lock.contains(&format!("checksum = \"{}\"", sha256_hex(&krate))),
        "{lock}"
    );
    for session in ["127.0.0.1", "hamh-proxy", "localhost", CA_HOST] {
        assert!(
            !lock.contains(session),
            "{session} in the lock file: {lock}"
        );
    }
    // The lock file it made is the one a later locked fetch accepts.
    let fetch = cargo(&sc, &["fetch", "--locked"]).await;
    assert!(fetch.status.success(), "{}", text(&fetch));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_source_replacement_cargo_does_not_reach_the_proxy() {
    // The check that the first test can fail: the same run with a config that leaves crates.io alone never asks the fake for
    // anything. (Offline, so no byte leaves the machine whatever the config says.)
    let krate = foo_crate();
    let r = rig(registry(token(), krate.clone()), nothing(), nothing()).await;
    let sc = scratch(
        "[net]\nretry = 0\noffline = true\n[term]\ncolor = \"never\"\n",
        &sha256_hex(&krate),
    );
    let fetch = cargo(&sc, &["fetch", "--locked"]).await;
    assert!(!fetch.status.success(), "{}", text(&fetch));
    assert_eq!(r.ca.count(), 0);
}
