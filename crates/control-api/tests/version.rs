//! The backend reports its version.
//!
//! The root `VERSION` file (`N.NN`) is embedded at build time by `build.rs` and returned in the `x-hamh-backend-version` header of every answer.
//! These tests pin the compiled constant to the file, the shape check that `build.rs` and the library share, the build script's own behaviour
//! (two output lines; a missing or malformed file stops the build) and the header on answers that come out of the real handler.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, OTHER, World, request, run_item};
use control_api::handler::{V1Request, handle_logged};
use control_api::lambda::to_response;
use control_api::version::{LEGACY_VERSION_HEADER, VERSION, VERSION_HEADER};
use control_api::version_check::check_version;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::Ordering;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The root `VERSION` file as written.
fn root_version_file() -> String {
    std::fs::read_to_string(manifest_dir().join("../../VERSION")).unwrap()
}

/// `^[0-9]+\.[0-9]{2}$`, written out here so the check does not grade itself.
fn is_n_dot_nn(s: &str) -> bool {
    let Some((major, counter)) = s.split_once('.') else {
        return false;
    };
    !major.is_empty()
        && major.chars().all(|c| c.is_ascii_digit())
        && counter.chars().count() == 2
        && counter.chars().all(|c| c.is_ascii_digit())
}

#[test]
fn the_header_name_is_the_one_the_client_reads() {
    assert_eq!(VERSION_HEADER, "x-hamh-backend-version");
}

#[test]
fn the_earlier_header_name_is_the_one_an_earlier_client_reads() {
    assert_eq!(LEGACY_VERSION_HEADER, "x-cah-backend-version"); // COMPAT-CAH
    assert_ne!(LEGACY_VERSION_HEADER, VERSION_HEADER);
}

#[test]
fn the_compiled_version_is_the_content_of_the_root_version_file() {
    let file = root_version_file();
    assert_eq!(
        VERSION,
        file.trim(),
        "the build embeds the file it was built from"
    );
    assert!(
        is_n_dot_nn(VERSION),
        "{VERSION:?} must look like N.NN (release candidate, two-digit counter)"
    );
    // The file is the version and one line break, nothing else (the shared contract: `^[0-9]+\.[0-9]{2}\n$`).
    assert_eq!(file, format!("{VERSION}\n"));
}

#[test]
fn the_shape_check_accepts_n_dot_nn_and_trims_the_file_s_line_break() {
    for good in [
        "4.01",
        "4.01\n",
        " 4.01 \r\n",
        "0.00",
        "10.99",
        "123.45\n\n",
    ] {
        let v = check_version(good).unwrap();
        assert!(is_n_dot_nn(v), "{good:?} -> {v:?}");
        assert_eq!(v, good.trim());
    }
}

#[test]
fn the_shape_check_refuses_everything_else_naming_the_value() {
    for bad in [
        "",
        "\n",
        "4",
        "4.",
        ".01",
        "4.1",
        "4.001",
        "4.01.1",
        "v4.01",
        "4.0a",
        "a.01",
        "-4.01",
        "+4.01",
        "4,01",
        "4 .01",
        "4. 01",
        "4\n.01",
        "4.01 beta",
        "4.01\n4.02",
        "\u{664}.\u{660}\u{661}",
        "4.\u{ff10}\u{ff11}",
    ] {
        let err = check_version(bad).unwrap_err();
        assert!(
            err.contains(&format!("{:?}", bad.trim())),
            "the message names the value that was refused: {err}"
        );
        assert!(
            err.contains("N.NN"),
            "the message says what is expected: {err}"
        );
    }
}

/// Compiles `build.rs` on its own into a fresh directory under the target tmp dir (no delete: every run has its own name) and returns the
/// directory (`<dir>/crates/control-api` is the manifest dir the script is run with, `<dir>/VERSION` the file it reads) and the binary.
fn compile_build_script(name: &str) -> (PathBuf, PathBuf) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "version-build-{name}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(dir.join("crates/control-api")).unwrap();
    let bin = dir.join("build_script");
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let out = Command::new(rustc)
        .args(["--edition=2024", "--crate-type=bin", "-o"])
        .arg(&bin)
        .arg(manifest_dir().join("build.rs"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "build.rs does not compile on its own: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (dir, bin)
}

/// Runs the compiled script as cargo does: the crate's manifest dir in `CARGO_MANIFEST_DIR`, the crate dir as the working directory.
fn run_build_script(dir: &Path, bin: &Path) -> Output {
    let manifest = dir.join("crates/control-api");
    Command::new(bin)
        .env_clear()
        .env("CARGO_MANIFEST_DIR", &manifest)
        .current_dir(&manifest)
        .output()
        .unwrap()
}

#[test]
fn the_build_script_prints_the_version_and_the_file_to_watch_and_nothing_else() {
    let (dir, bin) = compile_build_script("prints");
    std::fs::write(dir.join("VERSION"), "7.03\n").unwrap();
    let out = run_build_script(&dir, &bin);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let mut lines: Vec<&str> = stdout.lines().collect();
    lines.sort_unstable();
    assert_eq!(
        lines,
        [
            "cargo:rerun-if-changed=../../VERSION",
            "cargo:rustc-env=HAMH_VERSION=7.03"
        ]
    );
    // The trim: a file with more line breaks gives the same value.
    std::fs::write(dir.join("VERSION"), "7.03\n\n").unwrap();
    let again = run_build_script(&dir, &bin);
    assert!(again.status.success());
    assert!(
        String::from_utf8(again.stdout)
            .unwrap()
            .contains("cargo:rustc-env=HAMH_VERSION=7.03\n")
    );
}

#[test]
fn the_build_script_stops_the_build_when_the_file_is_missing() {
    let (dir, bin) = compile_build_script("missing");
    // No `<dir>/VERSION`: there is no fallback version.
    let out = run_build_script(&dir, &bin);
    assert!(
        !out.status.success(),
        "a build without a VERSION file must fail"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("HAMH_VERSION"),
        "no version is announced: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("VERSION"), "{stderr}");
}

#[test]
fn the_build_script_stops_the_build_when_the_file_is_not_n_dot_nn() {
    let (dir, bin) = compile_build_script("malformed");
    for bad in [
        "", "4\n", "4.1\n", "v4.01\n", "4.001\n", "4.01.2\n", "latest\n",
    ] {
        std::fs::write(dir.join("VERSION"), bad).unwrap();
        let out = run_build_script(&dir, &bin);
        assert!(!out.status.success(), "{bad:?} must fail the build");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout.contains("HAMH_VERSION"),
            "no version is announced for {bad:?}: {stdout}"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("VERSION"), "{bad:?}: {stderr}");
        assert!(stderr.contains("N.NN"), "{bad:?}: {stderr}");
    }
}

#[test]
fn the_build_script_runs_the_check_the_library_tests_cover() {
    // `build.rs` cannot be a test target, so the shape check sits in one file that the script includes and the library exports.
    let script = std::fs::read_to_string(manifest_dir().join("build.rs")).unwrap();
    assert!(
        script.contains("include!(\"src/version_check.rs\")"),
        "{script}"
    );
    assert!(script.contains("check_version("), "{script}");
}

/// A finished run of `OTHER`, which `ME` does not own.
fn world_with_a_foreign_run() -> World {
    let w = World::new();
    w.add_run(run_item(
        "01K5ZQ4T8M3N6P7R9S0V1W2X3Y",
        OTHER,
        "complete",
        "2026-10-15T13:00:00Z",
        serde_json::json!({"mode": "s3-only"}),
    ));
    w
}

#[tokio::test]
async fn answers_of_the_real_handler_carry_the_version_whatever_their_status() {
    let w = world_with_a_foreign_run();
    let mut no_caller = request(ME, "GET", "/v1/whoami", &[], "");
    no_caller.caller_arn = None;
    let cases: Vec<(&str, V1Request)> = vec![
        ("whoami", request(ME, "GET", "/v1/whoami", &[], "")),
        ("no caller", no_caller),
        (
            "a run of another caller",
            request(ME, "GET", "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y", &[], ""),
        ),
        (
            "a bad body",
            request(ME, "POST", "/v1/direct/runs", &[], "{not json"),
        ),
        (
            "an unknown route",
            request(ME, "GET", "/v1/nowhere", &[], ""),
        ),
    ];
    let mut statuses = Vec::new();
    for (what, req) in cases {
        let (out, _logs) = handle_logged(req, &chat_support::deps(&w)).await;
        let resp = to_response(out.clone()).unwrap();
        assert_eq!(resp.status(), out.status, "{what}");
        assert_eq!(
            resp.headers()[VERSION_HEADER],
            VERSION,
            "{what} ({}) carries the version",
            out.status
        );
        assert_eq!(
            resp.headers()[LEGACY_VERSION_HEADER],
            VERSION,
            "{what} ({}) carries the version under the earlier name too",
            out.status
        );
        statuses.push(out.status);
    }
    // The cases span a success, a refusal of the caller, a missing run and a client error.
    assert!(statuses.contains(&200), "{statuses:?}");
    assert!(statuses.contains(&403), "{statuses:?}");
    assert!(statuses.contains(&404), "{statuses:?}");
    assert!(
        statuses.iter().any(|s| (400..500).contains(s)),
        "{statuses:?}"
    );
}

#[tokio::test]
async fn a_failing_store_still_answers_with_the_version() {
    // A 5xx comes from a dependency that is down; the answer still names the backend that gave it.
    let w = World::new();
    w.add_run(run_item(
        "01K5ZQ4T8M3N6P7R9S0V1W2X3Y",
        ME,
        "complete",
        "2026-10-15T13:00:00Z",
        serde_json::json!({"mode": "s3-only"}),
    ));
    w.objects_down.store(true, Ordering::SeqCst);
    let (out, _logs) = handle_logged(
        request(
            ME,
            "GET",
            "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y/head",
            &[],
            "",
        ),
        &chat_support::deps(&w),
    )
    .await;
    assert!(out.status >= 500, "a store outage is a 5xx: {}", out.status);
    let resp = to_response(out).unwrap();
    assert_eq!(resp.headers()[VERSION_HEADER], VERSION);
    assert_eq!(resp.headers()[LEGACY_VERSION_HEADER], VERSION);
}
