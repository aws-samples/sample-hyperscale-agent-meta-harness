//! The `cargo build` a test helper runs for a binary (`fake-acp-engine`) must behave
//! like the plain `cargo` of the CI's cargo step, in every run that reaches it:
//!
//! - `cargo nextest` hands each test `CARGO_MANIFEST_DIR` and the other `CARGO_PKG_*` variables, and the
//!   build script of `ring` tracks `CARGO_MANIFEST_DIR` (`rerun-if-env-changed`). A helper build that
//!   inherits the test's value sees a changed environment, reruns that script and recompiles `ring` and
//!   everything that depends on it, the engine included. Measured 2026-10-02 with
//!   `CARGO_LOG=cargo::core::compiler::fingerprint=trace`: `dirty: EnvVarChanged { name:
//!   "CARGO_MANIFEST_DIR", old_value: None, new_value: Some(".../crates/agentd") }` for
//!   `ring v0.17.14/RunCustomBuild`.
//! - The same script tracks `OUT_DIR`, which cargo sets for the tests of a package with a build script
//!   (agentd had one when this was measured): with `CARGO_MANIFEST_DIR` scrubbed, the next trace still said `dirty:
//!   EnvVarChanged { name: "OUT_DIR", old_value: None, new_value: Some(".../agentd-<hash>/out") }`.
//! - `cargo llvm-cov` 0.9.1 instruments through `RUSTC_WRAPPER` (with `__CARGO_LLVM_COV_*`), so removing
//!   `RUSTFLAGS` does not keep a rebuild plain.
#![allow(clippy::unwrap_used)]

mod common;

use common::{plain_build_env_removals, plain_build_env_sets, scrub_build_env};
use std::process::Command;

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn the_variables_a_test_run_adds_for_its_own_package_are_removed() {
    let removed = plain_build_env_removals(&names(&[
        "PATH",
        "HOME",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "CARGO_TARGET_DIR",
        "CARGO_MANIFEST_DIR",
        "CARGO_MANIFEST_PATH",
        "CARGO_PKG_NAME",
        "CARGO_PKG_VERSION",
        "CARGO_CRATE_NAME",
        "CARGO_BIN_NAME",
        "CARGO_PRIMARY_PACKAGE",
        "CARGO_TARGET_TMPDIR",
        "CARGO_BIN_EXE_fake-acp-engine",
        "OUT_DIR",
        "NEXTEST",
        "NEXTEST_RUN_ID",
        "NEXTEST_BIN_EXE_agentd",
    ]));
    for gone in [
        "CARGO_MANIFEST_DIR",
        "CARGO_MANIFEST_PATH",
        "CARGO_PKG_NAME",
        "CARGO_PKG_VERSION",
        "CARGO_CRATE_NAME",
        "CARGO_BIN_NAME",
        "CARGO_PRIMARY_PACKAGE",
        "CARGO_TARGET_TMPDIR",
        "CARGO_BIN_EXE_fake-acp-engine",
        "OUT_DIR",
        "NEXTEST",
        "NEXTEST_RUN_ID",
        "NEXTEST_BIN_EXE_agentd",
    ] {
        assert!(
            removed.iter().any(|r| r == gone),
            "{gone} stays: {removed:?}"
        );
    }
    for kept in [
        "PATH",
        "HOME",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "CARGO_TARGET_DIR",
    ] {
        assert!(!removed.iter().any(|r| r == kept), "{kept} is removed");
    }
}

#[test]
fn llvm_cov_instrumentation_is_removed_in_every_form_it_takes() {
    let removed = plain_build_env_removals(&names(&[
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "LLVM_PROFILE_FILE",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_LLVM_COV",
        "CARGO_LLVM_COV_SHOW_ENV",
        "CARGO_LLVM_COV_TARGET_DIR",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER_RUSTFLAGS",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER_CRATE_NAMES",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER_PRE_EXISTING",
        "__LLVM_PROFILE_RT_INIT_ONCE",
    ]));
    for gone in [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "LLVM_PROFILE_FILE",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_LLVM_COV",
        "CARGO_LLVM_COV_SHOW_ENV",
        "CARGO_LLVM_COV_TARGET_DIR",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER_RUSTFLAGS",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER_CRATE_NAMES",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER_PRE_EXISTING",
        "__LLVM_PROFILE_RT_INIT_ONCE",
    ] {
        assert!(
            removed.iter().any(|r| r == gone),
            "{gone} stays: {removed:?}"
        );
    }
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn the_wrapper_llvm_cov_displaced_comes_back_for_the_helper_build() {
    // llvm-cov keeps the wrapper it replaced (sccache in the CI) in __..._PRE_EXISTING. Restored, the
    // helper build matches the cargo step's and still uses the compiler cache.
    let running = pairs(&[
        ("RUSTC_WRAPPER", "/tools/cargo-llvm-cov"),
        ("__CARGO_LLVM_COV_RUSTC_WRAPPER", "1"),
        (
            "__CARGO_LLVM_COV_RUSTC_WRAPPER_PRE_EXISTING",
            "/tools/bin/sccache",
        ),
    ]);
    assert_eq!(
        plain_build_env_sets(&running),
        pairs(&[("RUSTC_WRAPPER", "/tools/bin/sccache")])
    );
    // llvm-cov displaced no wrapper: nothing to restore (the removal alone leaves none).
    let bare = pairs(&[
        ("RUSTC_WRAPPER", "/tools/cargo-llvm-cov"),
        ("__CARGO_LLVM_COV_RUSTC_WRAPPER", "1"),
    ]);
    assert_eq!(plain_build_env_sets(&bare), pairs(&[]));
    // Not under llvm-cov: the user's own wrapper is left exactly as it is.
    let plain = pairs(&[("RUSTC_WRAPPER", "/tools/bin/sccache")]);
    assert_eq!(plain_build_env_sets(&plain), pairs(&[]));
}

#[test]
fn a_rustc_wrapper_of_the_users_own_stays_when_llvm_cov_is_not_running() {
    // The CI's cargo step runs under sccache; only llvm-cov's own wrapper goes.
    let removed = plain_build_env_removals(&names(&["RUSTC_WRAPPER", "PATH"]));
    assert!(!removed.iter().any(|r| r == "RUSTC_WRAPPER"), "{removed:?}");
}

#[test]
fn the_scrubbed_command_runs_without_the_test_runs_package_variables() {
    // `cargo test` and `cargo nextest` both set CARGO_MANIFEST_DIR for the test process.
    assert!(
        std::env::var_os("CARGO_MANIFEST_DIR").is_some(),
        "the test runner did not set CARGO_MANIFEST_DIR"
    );
    let mut cmd = Command::new("env");
    scrub_build_env(&mut cmd);
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let seen: Vec<&str> = text
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, _)| k))
        .collect();
    assert!(seen.contains(&"PATH"), "PATH was removed: {seen:?}");
    for k in &seen {
        assert!(
            !k.starts_with("CARGO_PKG_")
                && !k.starts_with("CARGO_BIN_EXE_")
                && !k.starts_with("CARGO_LLVM_COV")
                && !k.starts_with("__CARGO_LLVM_COV")
                && !k.starts_with("NEXTEST")
                && !matches!(
                    *k,
                    "CARGO_MANIFEST_DIR" | "CARGO_CRATE_NAME" | "LLVM_PROFILE_FILE" | "OUT_DIR"
                ),
            "{k} reached the helper's cargo"
        );
    }
}
