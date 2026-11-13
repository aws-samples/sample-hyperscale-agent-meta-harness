//! Shared test helpers: scratch dirs under TMPDIR (the workspace tmp), canary credentials, git.
#![allow(dead_code, clippy::unwrap_used)]

use harness_protocol::api::internal::ProcessCredentials;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

pub const CANARY_SECRET: &str = "canary-secret-agentd-must-never-print";
pub const CANARY_TOKEN: &str = "canary-token-agentd-must-never-print";
pub const CANARY_NONCE: &str = "canary-nonce-agentd-must-never-print";

/// The last 16 characters of the canary session key ID; [`canary_key_id`] joins them to `ASIA` at run
/// time, so no AWS key-ID-shaped literal is committed.
const CANARY_ID_TAIL: &str = "TESTTESTTESTTEST";

/// The canary session access key ID: `ASIA` and 16 base-32 characters, the shape STS issues.
pub fn canary_key_id() -> String {
    format!("ASIA{CANARY_ID_TAIL}")
}

static SEQ: AtomicU32 = AtomicU32::new(0);

/// A short suffix that differs between runs of the same process id: the clock's microseconds modulo 2^20, at most 5 hex digits
/// (the credential socket's path has to stay under 107 bytes).
pub fn unique_suffix() -> String {
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros());
    format!("{:x}", micros % 0x10_0000)
}

/// A fresh directory under TMPDIR (env.sh points it at the workspace tmp).
pub fn scratch(label: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    // The process id alone is not unique over a host's days: nextest starts one process per test, ids are reused, and a run
    // leaves read-only directories (the rules and locked settings) that `remove_dir_all` cannot clear, so a reused id found the
    // earlier run's repository and rules and failed tests that assume a fresh tree. The time makes the name new. The prefix is
    // short because the credential socket tests put a unix socket in the directory (107 bytes for the whole path).
    let d = std::env::temp_dir().join(format!(
        "at-{}-{}-{n}-{label}",
        std::process::id(),
        unique_suffix()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn creds(expiration: &str) -> ProcessCredentials {
    ProcessCredentials {
        version: 1,
        access_key_id: canary_key_id(),
        secret_access_key: CANARY_SECRET.into(),
        session_token: CANARY_TOKEN.into(),
        expiration: expiration.into(),
    }
}

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// The files of the stand-in goal plugin folder of [`plugin_folder`]: the five names the worker image's lock lists for `goal/`.
pub const GOAL_STAND_IN: [(&str, &str); 5] = [
    (
        "index.js",
        "// stand-in for the goal plugin bundle that the image build fetches\nexport default { name: 'goal', hooks: {} };\n",
    ),
    ("package.json", "{\"name\":\"goal\",\"type\":\"module\"}\n"),
    ("LICENSE", "stand-in licence text\n"),
    ("NOTICE", "stand-in notice\n"),
    ("PROVENANCE", "stand-in provenance\n"),
];

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A plugin folder like the one the worker image carries at `/opt/harness/engine/plugins`, made new for a test.
///
/// The goal plugin is not in the repository: `scripts/engine-fetch build goal-plugin` fetches and bundles it when the image is built.
/// These tests place files, check them against a lock, and supervise a loop. They never run the plugin's JavaScript, which the image
/// build checks with the 14 checks of `engine/plugins/build/plugin-unit.mjs`. So `goal/` holds the five stand-in files of [`GOAL_STAND_IN`],
/// `harness-rounds/` is the repository's own observer plugin, and `lock.json` lists the SHA-256 of all seven files. The observer's hashes are read
/// back against the tracked `engine/plugins/lock.json`, so the real lock stays under test.
pub fn plugin_folder() -> PathBuf {
    let dir = scratch("plugins").join("plugins");
    let goal = dir.join("goal");
    let rounds = dir.join("harness-rounds");
    std::fs::create_dir_all(&goal).unwrap();
    std::fs::create_dir_all(&rounds).unwrap();
    let mut files = serde_json::Map::new();
    for (name, body) in GOAL_STAND_IN {
        std::fs::write(goal.join(name), body).unwrap();
        files.insert(format!("goal/{name}"), sha256_hex(body.as_bytes()).into());
    }
    let tracked: serde_json::Value = serde_json::from_slice(
        &std::fs::read(repo_root().join("engine/plugins/lock.json")).unwrap(),
    )
    .unwrap();
    for name in ["index.js", "package.json"] {
        let bytes =
            std::fs::read(repo_root().join("engine/plugins/harness-rounds").join(name)).unwrap();
        let key = format!("harness-rounds/{name}");
        assert_eq!(
            tracked["files"][&key].as_str(),
            Some(sha256_hex(&bytes).as_str()),
            "the tracked engine/plugins/lock.json holds {key}"
        );
        std::fs::write(rounds.join(name), &bytes).unwrap();
        files.insert(key, sha256_hex(&bytes).into());
    }
    let lock = serde_json::json!({"version": 1, "files": files});
    std::fs::write(
        dir.join("lock.json"),
        serde_json::to_vec_pretty(&lock).unwrap(),
    )
    .unwrap();
    dir
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub const BASE_CALC: &str =
    "\"\"\"Small calculator used by the E2E fixture.\"\"\"\n\n\ndef add(a, b):\n    return a + b\n";
pub const BASE_TEST: &str =
    "from src.calc import add\n\n\ndef test_add():\n    assert add(2, 3) == 5\n";

/// A python-calc repository like the E2E fixture, committed on main.
pub fn python_calc(dir: &Path) -> PathBuf {
    let repo = dir.join("python-calc");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(repo.join("tests")).unwrap();
    std::fs::write(repo.join("src/calc.py"), BASE_CALC).unwrap();
    std::fs::write(repo.join("tests/test_calc.py"), BASE_TEST).unwrap();
    std::fs::create_dir_all(repo.join(".cline/hooks")).unwrap();
    std::fs::write(
        repo.join(".cline/hooks/pre.sh"),
        "echo repo-supplied hook\n",
    )
    .unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    repo
}

/// The names among `names` (an environment) that a helper's `cargo build` must not inherit from the
/// test run, so that build behaves like the plain `cargo` of the CI's cargo step:
///
/// - the variables `cargo test` and `cargo nextest` set for the test's own package
///   (`CARGO_MANIFEST_DIR`, `CARGO_PKG_*`, `CARGO_BIN_EXE_*`, `OUT_DIR` and the like, and `NEXTEST*`).
///   The build script of `ring` tracks `CARGO_MANIFEST_DIR` and `OUT_DIR`, so a build that inherits
///   them recompiles `ring` and everything that depends on it, the fake engine included, and flips
///   that fingerprint back for every plain run after;
/// - `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, `LLVM_PROFILE_FILE` and `__LLVM_PROFILE_RT_INIT_ONCE`;
/// - everything `cargo llvm-cov` 0.9.1 adds: it instruments through `RUSTC_WRAPPER` (and
///   `RUSTC_WORKSPACE_WRAPPER`) with `__CARGO_LLVM_COV_*`, and `CARGO_LLVM_COV*` describe the run. A
///   rebuild that keeps them yields an instrumented binary that writes `default_*.profraw` into the
///   directory it runs in. A `RUSTC_WRAPPER` of the user's own (sccache) stays unless llvm-cov is running;
///   then [`plain_build_env_sets`] puts back the one llvm-cov displaced.
///
/// `CARGO_HOME`, `RUSTUP_HOME`, `CARGO_TARGET_DIR`, `PATH` and the rest stay.
pub fn plain_build_env_removals(names: &[String]) -> Vec<String> {
    let llvm_cov = names.iter().any(|n| n == "__CARGO_LLVM_COV_RUSTC_WRAPPER");
    names
        .iter()
        .filter(|n| {
            let n = n.as_str();
            matches!(
                n,
                "RUSTFLAGS"
                    | "CARGO_ENCODED_RUSTFLAGS"
                    | "LLVM_PROFILE_FILE"
                    | "CARGO_MANIFEST_DIR"
                    | "CARGO_MANIFEST_PATH"
                    | "CARGO_CRATE_NAME"
                    | "CARGO_BIN_NAME"
                    | "CARGO_PRIMARY_PACKAGE"
                    | "CARGO_TARGET_TMPDIR"
                    | "OUT_DIR"
                    | "__LLVM_PROFILE_RT_INIT_ONCE"
            ) || n.starts_with("NEXTEST")
                || n.starts_with("CARGO_PKG_")
                || n.starts_with("CARGO_BIN_EXE_")
                || n.starts_with("CARGO_LLVM_COV")
                || n.starts_with("__CARGO_LLVM_COV")
                || (llvm_cov && matches!(n, "RUSTC_WRAPPER" | "RUSTC_WORKSPACE_WRAPPER"))
        })
        .cloned()
        .collect()
}

/// Variables a helper's `cargo build` gets set, from `env` (name, value pairs of this process): while
/// `cargo llvm-cov` 0.9.1 runs, the `RUSTC_WRAPPER` it displaced (`__CARGO_LLVM_COV_RUSTC_WRAPPER_PRE_EXISTING`,
/// sccache in the CI), so the helper build has the cargo step's environment and its compiler cache.
pub fn plain_build_env_sets(env: &[(String, String)]) -> Vec<(String, String)> {
    let value = |name: &str| env.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    match (
        value("__CARGO_LLVM_COV_RUSTC_WRAPPER"),
        value("__CARGO_LLVM_COV_RUSTC_WRAPPER_PRE_EXISTING"),
    ) {
        (Some(_), Some(wrapper)) if !wrapper.is_empty() => {
            vec![("RUSTC_WRAPPER".to_string(), wrapper.to_string())]
        }
        _ => Vec::new(),
    }
}

/// Removes from `cmd` what [`plain_build_env_removals`] names in this process's environment and sets what
/// [`plain_build_env_sets`] returns.
pub fn scrub_build_env(cmd: &mut Command) -> &mut Command {
    let env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    let names: Vec<String> = env.iter().map(|(k, _)| k.clone()).collect();
    for name in plain_build_env_removals(&names) {
        cmd.env_remove(name);
    }
    for (name, value) in plain_build_env_sets(&env) {
        cmd.env(name, value);
    }
    cmd
}

/// Builds the fake engine once and returns the directory holding `fake-acp-engine`. `HAMH_FAKE_ENGINE_DIR` names a directory that
/// already holds it and skips the build: the root tests run in a container that has no toolchain (`direct_uid_split`).
pub fn fake_engine_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("HAMH_FAKE_ENGINE_DIR").map(PathBuf::from) {
        assert!(
            dir.join("fake-acp-engine").is_file(),
            "{} lacks fake-acp-engine",
            dir.display()
        );
        return dir;
    }
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join("target"));
    let status = scrub_build_env(
        Command::new(env!("CARGO"))
            .args(["build", "-q", "-p", "ham-e2e", "--bin", "fake-acp-engine"])
            .current_dir(repo_root())
            .env("CARGO_TARGET_DIR", &target),
    )
    .status()
    .unwrap();
    assert!(status.success(), "building fake-acp-engine");
    let dir = target.join("debug");
    assert!(
        dir.join("fake-acp-engine").is_file(),
        "{} lacks fake-acp-engine",
        dir.display()
    );
    dir
}
