//!
//! * `pip` and `uv` run on the host when they exist.
//! * npm and Go run in containers (`--network host`) and are ignored by default: `cargo nextest run -p pkgproxy --test clients
//!   --run-ignored ignored-only` on a host with Docker.
#![allow(clippy::unwrap_used)]

mod common;
use bytes::Bytes;
use common::*;
use sha2::{Digest, Sha256, Sha512};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

// ---------------------------------------------------------------------------------------------------------------------
// Fixtures: a stored zip (wheel, Go module) written by hand

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
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

fn stored_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in files {
        let offset = out.len() as u32;
        let crc = crc32(data);
        let n = name.len() as u16;
        let sz = data.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0x21, 0]); // flags, method (stored), time, date 1980-01-01
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&sz.to_le_bytes());
        out.extend_from_slice(&sz.to_le_bytes());
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0x21, 0]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&sz.to_le_bytes());
        central.extend_from_slice(&sz.to_le_bytes());
        central.extend_from_slice(&n.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra length
        central.extend_from_slice(&0u16.to_le_bytes()); // comment length
        central.extend_from_slice(&0u16.to_le_bytes()); // disk number
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
        central.extend_from_slice(&0u32.to_le_bytes()); // external attributes
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn wheel() -> Vec<u8> {
    let info = "pkgproxy_demo-1.0.dist-info";
    let metadata = "Metadata-Version: 2.1\nName: pkgproxy-demo\nVersion: 1.0\n";
    let wheel =
        "Wheel-Version: 1.0\nGenerator: pkgproxy-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
    let init = "VALUE = 42\n";
    let record = format!(
        "pkgproxy_demo/__init__.py,,\n{info}/METADATA,,\n{info}/WHEEL,,\n{info}/RECORD,,\n"
    );
    stored_zip(&[
        ("pkgproxy_demo/__init__.py", init.as_bytes()),
        (&format!("{info}/METADATA"), metadata.as_bytes()),
        (&format!("{info}/WHEEL"), wheel.as_bytes()),
        (&format!("{info}/RECORD"), record.as_bytes()),
    ])
}

const WHEEL_NAME: &str = "pkgproxy_demo-1.0-py3-none-any.whl";

/// A CodeArtifact fake serving the demo wheel: an index page with an absolute link (rewritten by the proxy), the wheel behind a 302 to S3.
fn pypi_fakes(wheel_bytes: Vec<u8>) -> (Handler, Handler) {
    let sha = hex(&Sha256::digest(&wheel_bytes));
    let ca = handler(move |s| {
        let path = s.target.split('?').next().unwrap();
        if path == "/pypi/packages/simple/pkgproxy-demo/" {
            Reply::ok(format!(
                "<!DOCTYPE html><html><body><a href=\"https://{CA_HOST}/pypi/packages/files/{WHEEL_NAME}#sha256={sha}\">{WHEEL_NAME}</a></body></html>"
            ))
            .header("Content-Type", "text/html; charset=utf-8")
        } else if path == format!("/pypi/packages/files/{WHEEL_NAME}") {
            Reply::redirect(
                302,
                &format!("https://{S3_HOST}/blobs/{WHEEL_NAME}?X-Amz-Signature=abc"),
            )
        } else {
            Reply::status(404)
        }
    });
    let s3 = handler(move |s| {
        if s.target.starts_with(&format!("/blobs/{WHEEL_NAME}")) {
            Reply::ok(Bytes::from(wheel_bytes.clone()))
                .header("Content-Type", "application/octet-stream")
        } else {
            Reply::status(404)
        }
    });
    (ca, s3)
}

// ---------------------------------------------------------------------------------------------------------------------
// Tool lookup

fn require_clients() -> bool {
    std::env::var_os("PKGPROXY_REQUIRE_CLIENTS").is_some()
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn tool(name: &str) -> Option<PathBuf> {
    let found = which(name);
    if found.is_none() {
        assert!(
            !require_clients(),
            "PKGPROXY_REQUIRE_CLIENTS is set and `{name}` is not on PATH"
        );
        eprintln!("SKIPPED: `{name}` is not on PATH");
    }
    found
}

/// `python -m pip --version` succeeds: the interpreter has the pip module. A host python without it
/// cannot run the pip row, so the row is skipped like a missing tool, not failed.
fn has_pip(python: &Path) -> bool {
    std::process::Command::new(python)
        .args(["-m", "pip", "--version"])
        .stdin(Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `python3` with pip, or `None` after a SKIPPED line (a failure when `PKGPROXY_REQUIRE_CLIENTS` is set).
fn python_with_pip() -> Option<PathBuf> {
    let python = tool("python3")?;
    if has_pip(&python) {
        return Some(python);
    }
    assert!(
        !require_clients(),
        "PKGPROXY_REQUIRE_CLIENTS is set and `{}` has no pip module",
        python.display()
    );
    eprintln!("SKIPPED: `{}` has no pip module", python.display());
    None
}

async fn run(cmd: &mut Command, what: &str) -> (bool, String) {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(180),
        cmd.stdin(Stdio::null()).output(),
    )
    .await
    .unwrap_or_else(|_| panic!("{what} timed out"))
    .unwrap();
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn clean_env(cmd: &mut Command, home: &Path, env: &BTreeMap<String, String>) {
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("TMPDIR", home)
        .env("PIP_CONFIG_FILE", "/dev/null")
        .env("PIP_CACHE_DIR", home.join("pip-cache"))
        .env("UV_CACHE_DIR", home.join("uv-cache"))
        .envs(env);
}

#[test]
fn a_python_without_the_pip_module_is_not_a_pip_client() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let fake = |name: &str, body: &str| {
        let p = dir.path().join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    };
    let without = fake(
        "python-no-pip",
        "echo '/usr/bin/python3: No module named pip' >&2; exit 1",
    );
    let with = fake("python-pip", "echo 'pip 24.0 from /x (python 3.12)'");
    assert!(!has_pip(&without), "exit 1 means no pip module");
    assert!(has_pip(&with));
    assert!(
        !has_pip(&dir.path().join("missing")),
        "a path that is not a program has no pip"
    );
}

// ---------------------------------------------------------------------------------------------------------------------
// pip and uv, on the host

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pip_downloads_a_wheel_through_the_proxy_with_the_documented_environment() {
    let Some(python) = python_with_pip() else {
        return;
    };
    let w = wheel();
    let (ca, s3) = pypi_fakes(w.clone());
    let r = rig(ca, s3, nothing()).await;
    let home = tempfile::tempdir().unwrap();
    let dest = home.path().join("dl");
    std::fs::create_dir_all(&dest).unwrap();
    let mut cmd = Command::new(python);
    cmd.args([
        "-m",
        "pip",
        "download",
        "--no-deps",
        "--disable-pip-version-check",
        "--no-cache-dir",
        "-d",
    ])
    .arg(&dest)
    .arg("pkgproxy-demo==1.0");
    clean_env(&mut cmd, home.path(), &r.proxy.env());
    let (ok, text) = run(&mut cmd, "pip download").await;
    assert!(ok, "pip failed:\n{text}");
    let got = std::fs::read(dest.join(WHEEL_NAME)).expect("the wheel was downloaded");
    assert_eq!(
        got, w,
        "the wheel arrived byte for byte (pip also checked the #sha256 of the rewritten link)"
    );

    let ca = r.ca.seen();
    assert!(
        ca.iter()
            .all(|s| s.header("authorization") == Some(basic_for(&r.token).as_str()))
    );
    assert_eq!(
        r.s3.seen()[0].header("authorization"),
        None,
        "the signed S3 URL got no credential"
    );
    assert!(!text.contains(&r.token));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uv_installs_a_wheel_through_the_proxy_with_the_documented_environment() {
    let Some(uv) = tool("uv") else { return };
    let Some(python) = std::env::var_os("HAMH_PYTHON")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| which("python3.12"))
    else {
        assert!(
            !require_clients(),
            "PKGPROXY_REQUIRE_CLIENTS is set and no python >= 3.8 for uv (HAMH_PYTHON)"
        );
        eprintln!("SKIPPED: no python >= 3.8 for uv (set HAMH_PYTHON)");
        return;
    };
    let w = wheel();
    let (ca, s3) = pypi_fakes(w);
    let r = rig(ca, s3, nothing()).await;
    let home = tempfile::tempdir().unwrap();
    let target = home.path().join("site");
    let mut cmd = Command::new(uv);
    cmd.args(["pip", "install", "--no-deps", "--no-config", "--python"])
        .arg(&python)
        .arg("--target")
        .arg(&target)
        .arg("pkgproxy-demo==1.0");
    clean_env(&mut cmd, home.path(), &r.proxy.env());
    let (ok, text) = run(&mut cmd, "uv pip install").await;
    assert!(ok, "uv failed:\n{text}");
    assert!(target.join("pkgproxy_demo/__init__.py").is_file(), "{text}");
    assert!(
        r.ca.seen()
            .iter()
            .all(|s| s.header("authorization") == Some(basic_for(&r.token).as_str()))
    );
}

// ---------------------------------------------------------------------------------------------------------------------
// Maven: the wire sequence of the resolver, replayed

#[tokio::test]
async fn maven_resolver_requests_work_through_the_proxy() {
    let pom = "<project><modelVersion>4.0.0</modelVersion><groupId>com.example</groupId><artifactId>demo</artifactId><version>1.0</version></project>";
    let jar = Bytes::from_static(b"PK\x03\x04-not-really-a-jar");
    let sha1 = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
    let jar2 = jar.clone();
    let ca = handler(move |s| {
        let path = s.target.split('?').next().unwrap();
        match path {
            "/maven/packages/com/example/demo/1.0/demo-1.0.pom" => {
                Reply::ok(pom).header("Content-Type", "application/xml")
            }
            "/maven/packages/com/example/demo/1.0/demo-1.0.pom.sha1" => {
                Reply::ok(sha1).header("Content-Type", "text/plain")
            }
            "/maven/packages/com/example/demo/1.0/demo-1.0.jar" => Reply::ok(jar2.clone())
                .header("Content-Type", "application/java-archive")
                .header("Accept-Ranges", "bytes"),
            _ => Reply::status(404).header("Content-Type", "text/plain"),
        }
    });
    let r = rig(ca, nothing(), nothing()).await;
    // Maven's resolver sends these on every request: gzip, a Maven user agent, cache-busting headers, a connection header.
    let maven = [
        ("User-Agent", "Apache-Maven/3.9.9 (Java 21; Linux 6.1)"),
        ("Accept-Encoding", "gzip"),
        ("Cache-control", "no-cache, no-store"),
        ("Pragma", "no-cache"),
    ];
    let p = |rest: &str| format!("/maven/com/example/demo/1.0/{rest}");
    let pom_resp = get_with(r.addr(), &p("demo-1.0.pom"), &maven).await;
    assert_eq!(pom_resp.status, 200);
    assert!(pom_resp.text().starts_with("<project>"));
    assert_eq!(
        get_with(r.addr(), &p("demo-1.0.pom.sha1"), &maven)
            .await
            .text(),
        sha1
    );
    let missing = get_with(
        r.addr(),
        "/maven/com/example/demo/maven-metadata.xml",
        &maven,
    )
    .await;
    assert_eq!(
        missing.status, 404,
        "a missing metadata file is a plain 404, as Maven expects"
    );
    let h = send_raw(
        r.addr(),
        &request("HEAD", &p("demo-1.0.jar"), r.addr(), &maven),
        true,
    )
    .await;
    assert_eq!(h.status, 200);
    let jar_resp = get_with(r.addr(), &p("demo-1.0.jar"), &maven).await;
    assert_eq!(jar_resp.body, jar.to_vec());
    for s in r.ca.seen() {
        assert_eq!(
            s.header("authorization"),
            Some(basic_for(&r.token).as_str())
        );
        assert_eq!(s.header("accept-encoding"), Some("identity"));
        assert_eq!(s.header("pragma"), None);
        assert_eq!(s.header("cache-control"), None);
        assert_eq!(
            s.header("user-agent"),
            Some("Apache-Maven/3.9.9 (Java 21; Linux 6.1)")
        );
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// npm and Go, in containers

fn docker_run(
    image: &str,
    scratch: &Path,
    workdir: &str,
    env: &BTreeMap<String, String>,
    script: &str,
) -> Command {
    let mut cmd = Command::new("docker");
    cmd.args(["run", "--rm", "--network", "host", "--user"])
        .arg(format!("{}:{}", uid(), gid()))
        .arg("-v")
        .arg(format!("{}:/work", scratch.display()))
        .args([
            "-w",
            workdir,
            "-e",
            "HOME=/work/home",
            "-e",
            "npm_config_cache=/work/npm-cache",
            "-e",
            "GOPATH=/work/gopath",
            "-e",
            "GOCACHE=/work/gocache",
        ]);
    for (k, v) in env {
        cmd.arg("-e").arg(format!("{k}={v}"));
    }
    cmd.arg(image).args(["sh", "-ec", script]);
    cmd
}

fn id_of(flag: &str) -> String {
    let out = std::process::Command::new("id").arg(flag).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
fn uid() -> String {
    id_of("-u")
}
fn gid() -> String {
    id_of("-g")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Docker with the node image; run with --run-ignored ignored-only"]
async fn npm_installs_a_package_through_the_proxy_in_a_container() {
    const NODE: &str = "node:24-bookworm";
    let scratch = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(scratch.path().join("home")).unwrap();
    std::fs::create_dir_all(scratch.path().join("pkg")).unwrap();
    // Pack a one-file package with npm itself (no network needed).
    let pack = r#"cd /work/pkg && printf '{"name":"pkgproxy-demo","version":"1.0.0","main":"index.js"}' > package.json && echo 'module.exports=42' > index.js && npm pack --silent"#;
    let (ok, text) = run(
        &mut docker_run(NODE, scratch.path(), "/work", &BTreeMap::new(), pack),
        "npm pack",
    )
    .await;
    assert!(ok, "npm pack failed:\n{text}");
    let tgz = std::fs::read(scratch.path().join("pkg/pkgproxy-demo-1.0.0.tgz")).unwrap();
    let integrity = format!("sha512-{}", b64(&Sha512::digest(&tgz)));
    let doc = format!(
        r#"{{"name":"pkgproxy-demo","dist-tags":{{"latest":"1.0.0"}},"versions":{{"1.0.0":{{"name":"pkgproxy-demo","version":"1.0.0","main":"index.js","dist":{{"tarball":"https://{CA_HOST}/npm/packages/pkgproxy-demo/-/pkgproxy-demo-1.0.0.tgz","integrity":"{integrity}"}}}}}}}}"#
    );
    let ca = handler(move |s| match s.target.as_str() {
        "/npm/packages/pkgproxy-demo" => {
            Reply::ok(doc.clone()).header("Content-Type", "application/json")
        }
        "/npm/packages/pkgproxy-demo/-/pkgproxy-demo-1.0.0.tgz" => {
            Reply::ok(Bytes::from(tgz.clone())).header("Content-Type", "application/octet-stream")
        }
        _ => Reply::status(404),
    });
    let r = rig(ca, nothing(), nothing()).await;
    std::fs::create_dir_all(scratch.path().join("proj")).unwrap();
    let install = "cd /work/proj && npm init -y >/dev/null && npm install pkgproxy-demo@1.0.0 --no-audit --no-fund && node -e 'if (require(\"pkgproxy-demo\") !== 42) process.exit(3)'";
    // A cache of its own: `npm pack` filled the first one with the tarball, and an install would never ask the proxy for it.
    let mut env = r.proxy.env();
    env.insert("npm_config_cache".into(), "/work/npm-cache-install".into());
    let (ok, text) = run(
        &mut docker_run(NODE, scratch.path(), "/work", &env, install),
        "npm install",
    )
    .await;
    assert!(ok, "npm install failed:\n{text}");
    for s in r.ca.seen() {
        assert_eq!(
            s.header("authorization"),
            Some(format!("Bearer {}", r.token).as_str()),
            "{}",
            s.target
        );
    }
    assert!(
        r.ca.count() >= 2,
        "packument and tarball: {:?}",
        r.ca.seen()
            .iter()
            .map(|s| s.target.clone())
            .collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Docker and the golang image (pulled on first use); run with --run-ignored ignored-only"]
async fn go_gets_a_module_through_the_proxy_in_a_container() {
    const GO: &str = "golang:1.23-bookworm";
    let module = "example.com/pkgproxydemo";
    let version = "v1.0.0";
    let gomod = format!("module {module}\n\ngo 1.21\n");
    let src = "package pkgproxydemo\n\nconst Value = 42\n";
    let zip = stored_zip(&[
        (&format!("{module}@{version}/go.mod"), gomod.as_bytes()),
        (&format!("{module}@{version}/demo.go"), src.as_bytes()),
    ]);
    let (gm, zp) = (gomod.clone(), zip.clone());
    let go = handler(move |s| {
        let base = format!("/{module}/@v/");
        let t = s.target.as_str();
        if t == format!("{base}list") {
            Reply::ok(format!("{version}\n")).header("Content-Type", "text/plain")
        } else if t == format!("{base}{version}.info") {
            Reply::ok(format!(
                "{{\"Version\":\"{version}\",\"Time\":\"2026-01-01T00:00:00Z\"}}"
            ))
            .header("Content-Type", "application/json")
        } else if t == format!("{base}{version}.mod") {
            Reply::ok(gm.clone()).header("Content-Type", "text/plain")
        } else if t == format!("{base}{version}.zip") {
            Reply::ok(Bytes::from(zp.clone())).header("Content-Type", "application/zip")
        } else {
            Reply::status(404)
        }
    });
    let r = rig(nothing(), nothing(), go).await;
    let scratch = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(scratch.path().join("home")).unwrap();
    std::fs::create_dir_all(scratch.path().join("proj")).unwrap();
    let script = format!(
        "cd /work/proj && printf 'module t\\n\\ngo 1.21\\n' > go.mod && go get {module}@{version} && test -f /work/gopath/pkg/mod/{module}@{version}/demo.go"
    );
    let (ok, text) = run(
        &mut docker_run(GO, scratch.path(), "/work", &r.proxy.env(), &script),
        "go get",
    )
    .await;
    assert!(ok, "go get failed:\n{text}");
    for s in r.go.seen() {
        assert_eq!(s.header("authorization"), None, "{}", s.target);
    }
    assert!(r.go.count() >= 3);
    // audit: the files a Go project commits hold no module proxy URL.
    for f in ["go.mod", "go.sum"] {
        let committed = std::fs::read_to_string(scratch.path().join("proj").join(f)).unwrap();
        assert!(
            !committed.contains("127.0.0.1"),
            "{f} names the session: {committed}"
        );
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Lockfiles: what a file that is committed may name

/// A package of the lockfile tests: its name, the file name in its tarball URL, its tarball and its dependencies as JSON.
struct LkPkg {
    name: &'static str,
    url_file: &'static str,
    tgz: Vec<u8>,
    deps: &'static str,
}

/// Three packages made by `npm pack` (no network): `lk-a` depends on `lk-b` and the scoped `@lk/c`.
async fn pack_lk_packages(scratch: &Path) -> Vec<LkPkg> {
    std::fs::create_dir_all(scratch.join("home")).unwrap();
    let pack = r#"set -e
mkdir -p /work/pack/a /work/pack/b /work/pack/c /work/tgz
cd /work/pack/b && printf '{"name":"lk-b","version":"1.0.0","main":"index.js"}' > package.json && echo 'module.exports=2' > index.js && npm pack --silent --pack-destination /work/tgz
cd /work/pack/c && printf '{"name":"@lk/c","version":"1.0.0","main":"index.js"}' > package.json && echo 'module.exports=3' > index.js && npm pack --silent --pack-destination /work/tgz
cd /work/pack/a && printf '{"name":"lk-a","version":"1.0.0","main":"index.js","dependencies":{"lk-b":"1.0.0","@lk/c":"1.0.0"}}' > package.json && echo 'module.exports=1' > index.js && npm pack --silent --pack-destination /work/tgz"#;
    let (ok, text) = run(
        &mut docker_run("node:24-bookworm", scratch, "/work", &BTreeMap::new(), pack),
        "npm pack",
    )
    .await;
    assert!(ok, "npm pack failed:\n{text}");
    let read = |f: &str| std::fs::read(scratch.join("tgz").join(f)).unwrap();
    vec![
        LkPkg {
            name: "lk-a",
            url_file: "lk-a-1.0.0.tgz",
            tgz: read("lk-a-1.0.0.tgz"),
            deps: r#"{"lk-b":"1.0.0","@lk/c":"1.0.0"}"#,
        },
        LkPkg {
            name: "lk-b",
            url_file: "lk-b-1.0.0.tgz",
            tgz: read("lk-b-1.0.0.tgz"),
            deps: "{}",
        },
        LkPkg {
            // npm names the scoped tarball in the registry URL `c-1.0.0.tgz`, and `npm pack` `lk-c-1.0.0.tgz`.
            name: "@lk/c",
            url_file: "c-1.0.0.tgz",
            tgz: read("lk-c-1.0.0.tgz"),
            deps: "{}",
        },
    ]
}

/// CodeArtifact's npm repository for those packages: packuments whose tarball links name CodeArtifact's own host.
fn lk_registry(pkgs: &[LkPkg]) -> Handler {
    let table: Vec<(String, String, Bytes, String)> = pkgs
        .iter()
        .map(|p| {
            let integrity = format!("sha512-{}", b64(&Sha512::digest(&p.tgz)));
            let doc = format!(
                r#"{{"name":"{n}","dist-tags":{{"latest":"1.0.0"}},"versions":{{"1.0.0":{{"name":"{n}","version":"1.0.0","main":"index.js","dependencies":{d},"dist":{{"tarball":"https://{CA_HOST}/npm/packages/{n}/-/{f}","integrity":"{integrity}"}}}}}}}}"#,
                n = p.name,
                d = p.deps,
                f = p.url_file
            );
            (
                p.name.to_string(),
                p.url_file.to_string(),
                Bytes::from(p.tgz.clone()),
                doc,
            )
        })
        .collect();
    handler(move |s| {
        let path = s
            .target
            .split('?')
            .next()
            .unwrap()
            .replace("%2f", "/")
            .replace("%2F", "/");
        let Some(rest) = path.strip_prefix("/npm/packages/") else {
            return Reply::status(404);
        };
        for (name, file, tgz, doc) in &table {
            if rest == name {
                return Reply::ok(doc.clone()).header("Content-Type", "application/json");
            }
            if rest == format!("{name}/-/{file}") {
                return Reply::ok(tgz.clone()).header("Content-Type", "application/octet-stream");
            }
        }
        Reply::status(404)
    })
}

/// How many tarball downloads (`.../-/<file>.tgz`) CodeArtifact has seen so far.
fn tarball_requests(r: &Rig) -> usize {
    r.ca.seen()
        .iter()
        .filter(|s| s.target.contains("/-/"))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Docker with the node images; run with --run-ignored ignored-only"]
async fn npm_writes_a_lockfile_that_names_the_public_registry_and_npm_ci_reads_it_back() {
    let scratch = tempfile::tempdir().unwrap();
    let pkgs = pack_lk_packages(scratch.path()).await;
    let r = rig(lk_registry(&pkgs), nothing(), nothing()).await;
    // npm 11.19.0 (Node 24, the session's) and npm 10.9.x (Node 22, the clean room's).
    for (i, (image, major)) in [("node:24-bookworm", "11."), ("node:22-bookworm", "10.")]
        .into_iter()
        .enumerate()
    {
        for d in [format!("proj{i}"), format!("ci{i}")] {
            std::fs::create_dir_all(scratch.path().join(d)).unwrap();
        }
        let mut env = r.proxy.env();
        env.insert("npm_config_cache".into(), format!("/work/cache-{i}"));
        let before = tarball_requests(&r);
        let install = format!(
            "npm --version > /work/npm-version-{i} && cd /work/proj{i} && printf '{{\"name\":\"proj\",\"version\":\"1.0.0\"}}' > package.json && npm install lk-a --no-audit --no-fund && node -e 'if (require(\"lk-a\") !== 1) process.exit(3)'"
        );
        let (ok, text) = run(
            &mut docker_run(image, scratch.path(), "/work", &env, &install),
            "npm install",
        )
        .await;
        assert!(ok, "npm install failed on {image}:\n{text}");
        let version =
            std::fs::read_to_string(scratch.path().join(format!("npm-version-{i}"))).unwrap();
        assert!(version.starts_with(major), "{image}: npm {version}");

        // The lockfile names the public registry and no session.
        let lock =
            std::fs::read_to_string(scratch.path().join(format!("proj{i}/package-lock.json")))
                .unwrap();
        assert!(!lock.contains("127.0.0.1"), "npm {version}: {lock}");
        let resolved: Vec<&str> = lock
            .lines()
            .filter(|l| l.contains("\"resolved\""))
            .collect();
        assert_eq!(resolved.len(), 3, "npm {version}: {lock}");
        for l in &resolved {
            assert!(
                l.contains("\"https://registry.npmjs.org/"),
                "npm {version}: {l}"
            );
        }
        // ... and the tarballs still came through the proxy (replace-registry-host), with its credential.
        assert_eq!(tarball_requests(&r) - before, 3, "npm {version}");
        for s in r.ca.seen() {
            assert_eq!(
                s.header("authorization"),
                Some(format!("Bearer {}", r.token).as_str()),
                "{}",
                s.target
            );
        }

        // A clean install from the lockfile alone, with an empty cache, fetches the same three through the proxy.
        env.insert("npm_config_cache".into(), format!("/work/cache-ci-{i}"));
        let before = tarball_requests(&r);
        let ci = format!(
            "cd /work/ci{i} && cp /work/proj{i}/package.json /work/proj{i}/package-lock.json . && npm ci --no-audit --no-fund && node -e 'if (require(\"lk-a\") !== 1) process.exit(3)'"
        );
        let (ok, text) = run(
            &mut docker_run(image, scratch.path(), "/work", &env, &ci),
            "npm ci",
        )
        .await;
        assert!(ok, "npm ci failed on {image}:\n{text}");
        assert_eq!(tarball_requests(&r) - before, 3, "npm ci, npm {version}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Docker, the node image and the public npm registry (pnpm is installed from it); run with --run-ignored ignored-only"]
async fn pnpm_installs_through_the_proxy_and_its_lockfile_holds_no_url() {
    const NODE: &str = "node:24-bookworm";
    let scratch = tempfile::tempdir().unwrap();
    let pkgs = pack_lk_packages(scratch.path()).await;
    let r = rig(lk_registry(&pkgs), nothing(), nothing()).await;
    // pnpm comes from the public registry, with no proxy in the way.
    let tool = "mkdir -p /work/tool && cd /work/tool && npm init -y >/dev/null && npm install pnpm@12.6.0 --no-audit --no-fund >/dev/null 2>&1 && node node_modules/pnpm/bin/pnpm.mjs --version";
    let (ok, text) = run(
        &mut docker_run(NODE, scratch.path(), "/work", &BTreeMap::new(), tool),
        "install pnpm",
    )
    .await;
    assert!(ok && text.contains("12.6.0"), "installing pnpm:\n{text}");
    std::fs::create_dir_all(scratch.path().join("proj")).unwrap();
    let before = tarball_requests(&r);
    let add = "cd /work/proj && printf '{\"name\":\"proj\",\"version\":\"1.0.0\"}' > package.json && node /work/tool/node_modules/pnpm/bin/pnpm.mjs add lk-a && node -e 'if (require(\"lk-a\") !== 1) process.exit(3)'";
    let (ok, text) = run(
        &mut docker_run(NODE, scratch.path(), "/work", &r.proxy.env(), add),
        "pnpm add",
    )
    .await;
    assert!(ok, "pnpm add failed:\n{text}");
    let lock = std::fs::read_to_string(scratch.path().join("proj/pnpm-lock.yaml")).unwrap();
    assert!(lock.contains("lk-a@1.0.0"), "{lock}");
    assert!(!lock.contains("127.0.0.1"), "{lock}");
    assert!(!lock.contains("tarball"), "{lock}");
    assert_eq!(
        tarball_requests(&r) - before,
        3,
        "pnpm followed the loopback links it was given"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uv_lock_records_the_session_index_and_uv_pip_compile_does_not() {
    // Known and accepted, 2026-10-15 (uv 0.12.18): `uv lock` writes the index it fetched from, and the file URLs of that index, into
    // uv.lock, and no uv setting hides them, so a uv.lock made in a session is not portable. The rules forbid committing it
    // (crates/ham-core/assets/default-rules.md); a harness-side check of committed files is backlog. If this test starts to
    // fail because uv.lock no longer names the index, the rule and this note can go.
    let Some(uv) = tool("uv") else { return };
    let Some(python) = std::env::var_os("HAMH_PYTHON")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| which("python3.12"))
    else {
        assert!(
            !require_clients(),
            "PKGPROXY_REQUIRE_CLIENTS is set and no python >= 3.8 for uv (HAMH_PYTHON)"
        );
        eprintln!("SKIPPED: no python >= 3.8 for uv (set HAMH_PYTHON)");
        return;
    };
    let (ca, s3) = pypi_fakes(wheel());
    let r = rig(ca, s3, nothing()).await;
    let home = tempfile::tempdir().unwrap();
    let proj = home.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join("pyproject.toml"),
        "[project]\nname = \"demo\"\nversion = \"0.1.0\"\nrequires-python = \">=3.8\"\ndependencies = [\"pkgproxy-demo==1.0\"]\n",
    )
    .unwrap();
    std::fs::write(proj.join("req.in"), "pkgproxy-demo==1.0\n").unwrap();
    let mut cmd = Command::new(&uv);
    cmd.args(["lock", "--no-config", "--python"])
        .arg(&python)
        .current_dir(&proj);
    clean_env(&mut cmd, home.path(), &r.proxy.env());
    let (ok, text) = run(&mut cmd, "uv lock").await;
    assert!(ok, "uv lock failed:\n{text}");
    let lock = std::fs::read_to_string(proj.join("uv.lock")).unwrap();
    assert!(lock.contains("127.0.0.1"), "{lock}");

    let mut cmd = Command::new(&uv);
    cmd.args(["pip", "compile", "req.in", "--no-config", "--python"])
        .arg(&python)
        .current_dir(&proj);
    clean_env(&mut cmd, home.path(), &r.proxy.env());
    let (ok, text) = run(&mut cmd, "uv pip compile").await;
    assert!(ok, "uv pip compile failed:\n{text}");
    assert!(
        text.contains("pkgproxy-demo==1.0") && !text.contains("127.0.0.1"),
        "{text}"
    );
}
