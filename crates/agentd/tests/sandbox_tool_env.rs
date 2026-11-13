//! What a Landlock run's tools can reach and how they find it.
//! The shipped rules and the lifecycle pack tell the engine to keep logs in `/tmp`, Python's `multiprocessing` makes its locks under `/dev/shm`, and a
//! Java program takes `user.home` from the password database (`/root` for the user of the Instances container) and `java.io.tmpdir` from `/tmp`, not
//! from `HOME` and `TMPDIR`. A policy that grants none of the folders and an environment that names neither makes `mvn test` and `> /tmp/log` fail in the
//! domain where they work in the microVM.
//!
//! The kernel's Landlock is not needed: the launcher is replaced by a script that keeps the policy it was given and runs the program unconfined, so the
//! policy and the environment the driver composed can be read back on any machine. The script is `agentd sandbox-exec --policy-json <json> -- <program>`'s
//! stand-in: it writes `$HOME/policy.json` and executes what follows the `--`.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::config::Config;
use agentd::direct::Deps;
use agentd::direct::report::run::{RunEnv, run_step};
use agentd::sandbox::{
    Identity, IdentitySetting, Mode, Policy, Preflight, Sandbox, SandboxError, Setting,
};
use common::scratch;
use direct_support::{Direct, Opts};
use harness_protocol::direct::{TestCommandSource, TestStepRole};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

struct Allow;

impl Preflight for Allow {
    fn check(&self) -> Result<(), SandboxError> {
        Ok(())
    }
}

/// The stand-in launcher: `$1` is `sandbox-exec`, `$2` is `--policy-json`, `$3` the policy, `$4` is `--`, and the program follows.
fn stand_in_launcher(dir: &Path) -> PathBuf {
    let p = dir.join("launcher.sh");
    std::fs::write(
        &p,
        "#!/bin/sh\nprintf '%s\\n' \"$3\" > \"$HOME/policy.json\"\nshift 4\nexec \"$@\"\n",
    )
    .unwrap();
    let mut perm = std::fs::metadata(&p).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(&p, perm).unwrap();
    p
}

/// A whole run in the `landlock` mode whose launcher is the stand-in. Returns the run, the policy the driver gave the launcher and the engine's
/// environment as the engine printed it.
async fn landlock_run() -> (Direct, Value, String) {
    let launcher = stand_in_launcher(&scratch("tool-env-launcher"));
    let d = Direct::start(Opts {
        edit_cfg: Some(Box::new(move |cfg: &mut Config| {
            cfg.engine_sandbox = Setting::Mode(Mode::Landlock);
            // These tests read the policy of the engine that runs as agentd's own user; the nobody route has its own (sandbox_nobody_driver.rs).
            cfg.engine_identity = IdentitySetting::Identity(Identity::Agentd);
            cfg.agentd_exe = launcher;
        })),
        ..Opts::default()
    })
    .await;
    let deps = Deps {
        preflight: Arc::new(Allow),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    d.wait_healthy().await;
    let home = d.agent_data().join("home");
    let policy: Value = serde_json::from_str(
        &std::fs::read_to_string(home.join("policy.json"))
            .unwrap_or_else(|e| panic!("the launcher was not started: {e}")),
    )
    .unwrap();
    let probe = std::fs::read_to_string(home.join("engine-probe.txt")).unwrap_or_default();
    (d, policy, probe)
}

fn paths(policy: &Value, member: &str) -> Vec<String> {
    policy[member]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .map(|g| {
            g["path"]
                .as_str()
                .or_else(|| g.as_str())
                .unwrap_or("")
                .to_string()
        })
        .collect()
}

fn env_line(probe: &str, name: &str) -> Option<String> {
    probe
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name}=")))
        .map(str::to_string)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_engine_may_write_the_shared_temp_directories() {
    let (_d, policy, _) = landlock_run().await;
    let rw = paths(&policy, "readWrite");
    for dir in ["/tmp", "/var/tmp", "/dev/shm"] {
        assert!(
            rw.iter().any(|p| p == dir),
            "{dir} is a folder the engine writes: {rw:?}"
        );
    }
    // A machine without one of them still launches.
    for g in policy["readWrite"].as_array().unwrap() {
        let path = g["path"].as_str().unwrap();
        if ["/tmp", "/var/tmp", "/dev/shm"].contains(&path) {
            assert_eq!(g["optional"], true, "{path}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn agentds_own_state_is_declared_private_so_no_grant_can_cover_it() {
    let (d, policy, _) = landlock_run().await;
    let private = paths(&policy, "private");
    let agent_data = d.agent_data();
    let want = [
        d.app.cfg.runtime_dir.clone(),
        agent_data.join("input"),
        agent_data.join("input.bundle"),
        agent_data.join("turns"),
        agent_data.join("snapshot.git"),
        agent_data.join("facts.git"),
        agent_data.join("probe.git"),
    ];
    for p in want {
        assert!(
            private.iter().any(|x| Path::new(x) == p),
            "{} is agentd's own and private: {private:?}",
            p.display()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_java_tool_finds_its_home_and_its_temp_directory_where_the_engine_may_write() {
    let (d, _, probe) = landlock_run().await;
    let agent_data = d.agent_data();
    let home = agent_data.join("home");
    let tmp = agent_data.join("tmp");
    assert!(
        !probe.is_empty(),
        "the engine ran and printed its environment"
    );
    assert_eq!(
        env_line(&probe, "JAVA_TOOL_OPTIONS"),
        Some(format!(
            "-Duser.home={} -Djava.io.tmpdir={}",
            home.display(),
            tmp.display()
        )),
        "{probe}"
    );
    assert_eq!(
        env_line(&probe, "GRADLE_USER_HOME"),
        Some(home.join(".gradle").display().to_string())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_microvm_mode_leaves_the_java_variables_alone() {
    let d = Direct::start(Opts::default()).await;
    d.invoke().await;
    d.wait_healthy().await;
    let probe =
        std::fs::read_to_string(d.agent_data().join("home/engine-probe.txt")).unwrap_or_default();
    assert!(!probe.is_empty());
    assert_eq!(env_line(&probe, "JAVA_TOOL_OPTIONS"), None, "{probe}");
    assert_eq!(env_line(&probe, "GRADLE_USER_HOME"), None);
}

fn step_env(work: &Path, launcher: &Path) -> RunEnv {
    let cfg = Config::from_vars(|_| None);
    RunEnv {
        sandbox: Some(Sandbox {
            launcher: launcher.to_path_buf(),
            policy: Arc::new(Policy::for_config(&cfg, None)),
            identity_helper: None,
        }),
        work: work.to_path_buf(),
        run_as: None,
        path: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        engine_env: BTreeMap::new(),
        engine_home: None,
        cargo_config: None,
    }
}

#[tokio::test]
async fn a_report_step_in_the_sandbox_gets_the_same_java_variables_for_its_own_folders() {
    let work = scratch("tool-env-step");
    let launcher = stand_in_launcher(&scratch("tool-env-step-launcher"));
    let cwd = work.join("checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    let env = step_env(&work, &launcher);
    let step = agentd::direct::report::plan::PlannedStep {
        role: TestStepRole::Test,
        source: TestCommandSource::TestCommandFile,
        command: "echo \"opts=$JAVA_TOOL_OPTIONS gradle=$GRADLE_USER_HOME\"".to_string(),
        logged: None,
    };
    let done = run_step(&env, &step, &cwd, Duration::from_secs(30), 4096).await;
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    let want = format!(
        "opts=-Duser.home={} -Djava.io.tmpdir={} gradle={}",
        work.join("home").display(),
        work.join("tmp").display(),
        work.join("home/.gradle").display()
    );
    assert!(done.output_tail.contains(&want), "{}", done.output_tail);
}
