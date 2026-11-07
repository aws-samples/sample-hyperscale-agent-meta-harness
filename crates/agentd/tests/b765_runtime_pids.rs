//! The runtime's processes are named once, right after `ham-containers up` returns and before the engine exists, and they reach the
//! module that stops the engine's processes. The real launcher reads the line `CONTAINERS PIDS front=<pid> supervisor=<pid>` from `up`'s
//! output; the run hands the pids to every module at startup (`StartupCtx::runtime_pids`); the messages module protects them in its reaper.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::hooks::{DirectHook, Hooks, StartupCtx};
use agentd::direct::messages::reap::TAG_ENV;
use agentd::direct::messages::{MemoryStore, MessagesHook, Settings};
use agentd::direct::{
    ContainersLauncher, ContainersRun, ContainersStart, Deps, ShellContainersLauncher,
    containers_env,
};
use agentd::logging::MemoryLog;
use common::scratch;
use direct_support::{Direct, Opts};
use harness_protocol::runtime::TurnOutcome;
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SECS: Duration = Duration::from_secs(10);

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn start_for(user: &str) -> (ContainersStart, Arc<MemoryLog>) {
    let log = MemoryLog::new();
    (
        ContainersStart {
            user: user.to_string(),
            log: log.clone(),
        },
        log,
    )
}

async fn processes_after_up(output: &str) -> Vec<u32> {
    let dir = scratch("b765-up-output");
    let bin = script(
        &dir,
        "ham-containers",
        &format!("cat <<'TXT'\n{output}\nTXT"),
    );
    let launcher = ShellContainersLauncher::new(bin, SECS, SECS);
    let (start, _) = start_for("agent");
    launcher.start(start).await.unwrap().processes()
}

#[tokio::test]
async fn b765_the_real_launcher_reads_the_pids_line_of_up() {
    let out = "CONTAINERS UP socket=/var/run/docker.sock (idle)\nCONTAINERS PIDS front=4101 supervisor=4102";
    assert_eq!(processes_after_up(out).await, vec![4101, 4102]);
}

#[tokio::test]
async fn b765_an_up_without_a_pids_line_names_no_processes() {
    assert!(
        processes_after_up("CONTAINERS UP socket=/var/run/docker.sock (idle)")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn b765_a_pids_line_is_read_for_the_numbers_that_can_be_pids() {
    // A word that is not a number, pid 0 and pid 1 (init) are never protected; the other member still is.
    assert_eq!(
        processes_after_up("CONTAINERS PIDS front=abc supervisor=4102").await,
        vec![4102]
    );
    assert!(
        processes_after_up("CONTAINERS PIDS front=1 supervisor=0")
            .await
            .is_empty()
    );
    assert_eq!(
        processes_after_up("CONTAINERS PIDS front=4101").await,
        vec![4101]
    );
}

#[tokio::test]
async fn b765_the_namespace_init_of_a_runtime_that_was_already_running_is_read_too() {
    assert_eq!(
        processes_after_up("CONTAINERS PIDS front=4101 supervisor=4102 holder=4109").await,
        vec![4101, 4102, 4109]
    );
}

#[tokio::test]
async fn b765_the_pids_line_is_read_from_the_start_of_a_line_only() {
    assert!(
        processes_after_up("log: CONTAINERS PIDS front=4101 supervisor=4102")
            .await
            .is_empty()
    );
}

// ---- the run ----------------------------------------------------------------------------------------------------------------------

struct FakeRun {
    pids: Vec<u32>,
}

impl ContainersRun for FakeRun {
    fn env(&self) -> BTreeMap<String, String> {
        containers_env()
    }
    fn processes(&self) -> Vec<u32> {
        self.pids.clone()
    }
    fn stop(self: Box<Self>) -> agentd::BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

struct FakeLauncher {
    pids: Vec<u32>,
}

impl ContainersLauncher for FakeLauncher {
    fn start(
        &self,
        _s: ContainersStart,
    ) -> agentd::BoxFuture<'_, Result<Box<dyn ContainersRun>, String>> {
        let pids = self.pids.clone();
        Box::pin(async move { Ok(Box::new(FakeRun { pids }) as Box<dyn ContainersRun>) })
    }
}

fn with_engine_user() -> Box<dyn FnOnce(&mut agentd::config::Config)> {
    Box::new(|c| c.engine_user = Some("agent".to_string()))
}

/// A module that writes down what the startup gave it.
struct Seen(Arc<Mutex<Option<Vec<u32>>>>);

impl std::fmt::Debug for Seen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Seen")
    }
}

impl DirectHook for Seen {
    fn name(&self) -> &'static str {
        "seen"
    }
    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> agentd::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            *self.0.lock().unwrap() = Some(ctx.runtime_pids.clone());
            Ok(())
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b765_the_startup_hands_the_runtime_pids_to_the_modules() {
    let d = Direct::start(Opts {
        edit_cfg: Some(with_engine_user()),
        ..Opts::default()
    })
    .await;
    let seen = Arc::new(Mutex::new(None));
    let hook: Arc<dyn DirectHook> = Arc::new(Seen(seen.clone()));
    let deps = Deps {
        containers: Arc::new(FakeLauncher {
            pids: vec![4101, 4102],
        }),
        hooks: Some(Hooks::new(vec![hook])),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(*seen.lock().unwrap(), Some(vec![4101, 4102]));
}

#[tokio::test(flavor = "multi_thread")]
async fn b765_a_run_without_a_runtime_gives_the_modules_no_pids() {
    let d = Direct::start(Opts::default()).await;
    let seen = Arc::new(Mutex::new(None));
    let hook: Arc<dyn DirectHook> = Arc::new(Seen(seen.clone()));
    let deps = Deps {
        containers: Arc::new(FakeLauncher { pids: vec![4101] }),
        hooks: Some(Hooks::new(vec![hook])),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(*seen.lock().unwrap(), Some(Vec::new()));
}

#[tokio::test(flavor = "multi_thread")]
async fn b765_the_messages_module_keeps_the_runtime_processes_out_of_its_reap() {
    // The product's own modules: the messages module reads the pids from the startup and protects the ones it can read.
    let d = Direct::start(Opts {
        edit_cfg: Some(with_engine_user()),
        ..Opts::default()
    })
    .await;
    let mut real = std::process::Command::new("sleep")
        .arg("300")
        .stdin(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deps = Deps {
        containers: Arc::new(FakeLauncher {
            pids: vec![real.id(), 4_000_000],
        }),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert!(
        real.try_wait().unwrap().is_none(),
        "the process the launcher named is still running after the run"
    );
    let _ = real.kill();
    let _ = real.wait();
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let log = d.log.lines();
    assert!(
        log.iter()
            .any(|l| l == "direct turn messages: kept 1 of 2 runtime process(es) out of the reap"),
        "{log:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b765_a_run_with_no_runtime_processes_logs_no_protection_line() {
    let d = Direct::start(Opts::default()).await;
    let deps = Deps {
        containers: Arc::new(FakeLauncher { pids: Vec::new() }),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        !d.log.lines().iter().any(|l| l.contains("out of the reap")),
        "{:#?}",
        d.log.lines()
    );
}

// ---- the wiring: what the run hands over is what the engine-stop reap spares (review finding F3) -----------------------------------

/// A child that is killed when it goes out of scope.
struct Guard(std::process::Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn tagged_sleep(tag: &str) -> Guard {
    Guard(
        std::process::Command::new("sleep")
            .arg("300")
            .env(TAG_ENV, tag)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn b765_the_run_spares_the_runtime_the_launcher_named_when_the_engine_stops() {
    // The whole path: the launcher names a pid, the run hands it to the messages module at startup, the module marks it in the reaper it
    // keeps, and the reap at `EngineStopped` leaves it alone while it stops the other process of the run's tag. Two real processes carry the
    // run's tag (the module's own tag, read from the module): `runtime` stands for a front that the user sweep would have found, `tool`
    // for a process the engine's tool left behind.
    let d = Direct::start(Opts {
        edit_cfg: Some(with_engine_user()),
        ..Opts::default()
    })
    .await;
    let hook = MessagesHook::new(MemoryStore::new(), Settings::for_tests());
    let tag = hook.tag().to_string();
    let mut runtime = tagged_sleep(&tag);
    let mut tool = tagged_sleep(&tag);
    let (runtime_pid, tool_pid) = (runtime.0.id(), tool.0.id());
    let deps = Deps {
        containers: Arc::new(FakeLauncher {
            pids: vec![runtime_pid],
        }),
        hooks: Some(Hooks::new(vec![hook as Arc<dyn DirectHook>])),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let log = d.log.lines();
    assert!(
        tool.0.try_wait().unwrap().is_some(),
        "TRIGGER PRESENT: the process the engine left was stopped by the reap: {log:#?}"
    );
    assert!(
        runtime.0.try_wait().unwrap().is_none(),
        "the runtime process the launcher named was stopped by the engine-stop reap: {log:#?}"
    );
    let want = format!(
        "direct messages: reaped 1 process(es) the engine left ({tool_pid} sleep), 0 remain"
    );
    assert!(log.iter().any(|l| l == &want), "{want}: {log:#?}");
    assert!(
        !log.iter()
            .any(|l| l.contains(&format!("{runtime_pid} sleep"))),
        "{log:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b765_without_the_pids_the_same_reap_stops_the_runtime_process_as_well() {
    // The control of the test above: the launcher names nothing, so the process that stands for the runtime is in the family and is stopped.
    let d = Direct::start(Opts {
        edit_cfg: Some(with_engine_user()),
        ..Opts::default()
    })
    .await;
    let hook = MessagesHook::new(MemoryStore::new(), Settings::for_tests());
    let tag = hook.tag().to_string();
    let mut runtime = tagged_sleep(&tag);
    let deps = Deps {
        containers: Arc::new(FakeLauncher { pids: Vec::new() }),
        hooks: Some(Hooks::new(vec![hook as Arc<dyn DirectHook>])),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        runtime.0.try_wait().unwrap().is_some(),
        "the reap finds the tagged process when nothing protects it: {:#?}",
        d.log.lines()
    );
}
