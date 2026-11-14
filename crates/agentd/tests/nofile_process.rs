//! The open-files cap on the real kernel limit of this test process: after `apply`, a process that agentd's engine launcher starts and a
//! command of the report step both see the capped soft limit and the unchanged hard limit. The test lowers the limit of its own process, so it
//! is the only test of this file (the other tests that need a high limit are in `nofile_serve.rs`, which never lowers its own).
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::report::plan::PlannedStep;
use agentd::direct::report::run::{RunEnv, run_step};
use agentd::engine::{LaunchSpec, spawn};
use agentd::logging::MemoryLog;
use agentd::nofile::{KernelLimits, Limits, Outcome, Setting, apply};
use harness_protocol::direct::{TestCommandSource, TestStepRole};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// The start value of the test: a high soft limit, as a container with a huge default has, but one this host's hard limit allows.
fn raise_soft_limit() -> (u64, Option<u64>) {
    let now = getrlimit(Resource::Nofile);
    let start = now.maximum.map_or(1_048_576, |h| h.min(1_048_576));
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(start),
            maximum: now.maximum,
        },
    )
    .unwrap();
    (start, now.maximum)
}

fn shown(limit: Option<u64>) -> String {
    limit.map_or_else(|| "unlimited".to_string(), |n| n.to_string())
}

#[tokio::test]
async fn children_of_a_capped_process_see_the_cap_through_the_engine_launcher_and_the_report_step()
{
    let (start, hard) = raise_soft_limit();
    assert!(
        start >= 1_024,
        "the test needs a hard limit of at least 1024; this host allows {start}"
    );
    // The trigger: before the cap the children see the high limit. A cap below it is what this test is about.
    let cap = 512;
    assert!(start > cap, "the trigger is a soft limit above the cap");

    let log = MemoryLog::new();
    let out = apply(&Setting::Cap(cap), &KernelLimits, log.as_ref());
    assert_eq!(
        out,
        Outcome::Lowered {
            from: start,
            to: cap,
            hard: hard.unwrap_or(u64::MAX)
        },
        "{:?}",
        log.lines()
    );
    assert_eq!(KernelLimits.soft_hard().0, cap);
    let after = getrlimit(Resource::Nofile);
    assert_eq!(after.current, Some(cap));
    assert_eq!(after.maximum, hard, "the hard limit is not touched");

    // 1. A process the engine launcher starts: the stand-in prints the two limits it sees.
    let d = common::scratch("nofile-launch");
    let spec = LaunchSpec {
        argv: [
            "sh",
            "-c",
            "ulimit -S -n > seen.txt; ulimit -H -n >> seen.txt",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        env: BTreeMap::new(),
        cwd: d.clone(),
        path: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        stderr_log: d.join("stderr.log"),
        run_as: None,
        sandbox: None,
    };
    let mut child = spawn(&spec).unwrap();
    assert!(child.wait().await.unwrap().success());
    let seen = std::fs::read_to_string(d.join("seen.txt")).unwrap();
    assert_eq!(
        seen,
        format!("{cap}\n{}\n", shown(hard)),
        "the engine's process sees the cap and the hard limit as they are"
    );

    // 2. A command of the report step.
    let work = common::scratch("nofile-report");
    let cwd = work.join("checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    let env = RunEnv {
        sandbox: None,
        work: work.clone(),
        run_as: None,
        path: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        engine_env: BTreeMap::new(),
        engine_home: None,
        cargo_config: None,
    };
    let step = PlannedStep {
        role: TestStepRole::Test,
        source: TestCommandSource::TestCommandFile,
        command: "ulimit -S -n; ulimit -H -n".to_string(),
        logged: None,
        install: None,
        fallback: None,
    };
    let s = run_step(&env, &step, &cwd, Duration::from_secs(30), 4096).await;
    assert_eq!(s.exit_code, Some(0), "{s:?}");
    assert_eq!(
        s.output_tail,
        format!("{cap}\n{}\n", shown(hard)),
        "the report step's command sees the cap and the hard limit as they are"
    );
}
