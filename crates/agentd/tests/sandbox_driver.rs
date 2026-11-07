//! The direct driver and the confinement mode: a run whose mode is not one, whose engine user conflicts with it or whose kernel cannot
//! give the Landlock domain ends before anything is fetched, and a Landlock run starts its engine through the launcher.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::config::Config;
use agentd::direct::Deps;
use agentd::sandbox::{Identity, IdentitySetting, Mode, Preflight, SandboxError, Setting};
use direct_support::{CfgEdit, Direct, Opts};
use harness_protocol::direct::DirectOutcome;
use harness_protocol::runtime::TurnOutcome;

struct Refuse(&'static str);

impl Preflight for Refuse {
    fn check(&self) -> Result<(), SandboxError> {
        Err(SandboxError::Enforce {
            stage: "create".to_string(),
            detail: self.0.to_string(),
        })
    }
}

struct Allow;

impl Preflight for Allow {
    fn check(&self) -> Result<(), SandboxError> {
        Ok(())
    }
}

fn edit(f: impl FnOnce(&mut Config) + 'static) -> Option<CfgEdit> {
    Some(Box::new(f))
}

fn kernel_has_abi_six() -> bool {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let mut parts = release.trim().split(['.', '-']);
    let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (major, minor) >= (6, 12)
}

fn assert_refused_before_anything(d: &Direct, needle: &str) {
    assert!(d.prompts_received().is_empty(), "no model call");
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed, "{}", s.label);
    let detail = s.detail.unwrap_or_default();
    assert!(detail.contains("engine sandbox"), "{detail}");
    assert!(detail.contains(needle), "{detail}");
    assert!(
        !d.worktree().exists(),
        "the check came before the work tree was made"
    );
    // The summary of the refused run is written in the run's own folder, not where the process happens to stand.
    assert!(
        d.agent_data().join("turns/1/summary.json").is_file(),
        "no summary.json in {}",
        d.agent_data().display()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mode_that_is_not_one_ends_the_run_before_anything_is_fetched() {
    let d = Direct::start(Opts {
        edit_cfg: edit(|cfg| cfg.engine_sandbox = Setting::from_value(None)),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    assert_refused_before_anything(&d, "AGENTD_ENGINE_SANDBOX");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_landlock_mode_with_an_engine_user_is_a_conflict_that_ends_the_run() {
    let d = Direct::start(Opts {
        edit_cfg: edit(|cfg| {
            cfg.engine_sandbox = Setting::Mode(Mode::Landlock);
            cfg.engine_user = Some("agent".to_string());
        }),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    assert_refused_before_anything(&d, "AGENTD_ENGINE_USER");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_kernel_that_cannot_give_the_domain_ends_the_run_with_the_reason() {
    let d = Direct::start(Opts {
        edit_cfg: edit(|cfg| cfg.engine_sandbox = Setting::Mode(Mode::Landlock)),
        ..Opts::default()
    })
    .await;
    let deps = Deps {
        preflight: std::sync::Arc::new(Refuse("injected: no Landlock")),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_refused_before_anything(&d, "injected: no Landlock");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_landlock_run_starts_the_engine_through_the_launcher_and_the_launcher_decides() {
    let d = Direct::start(Opts {
        edit_cfg: edit(|cfg| {
            cfg.engine_sandbox = Setting::Mode(Mode::Landlock);
            // The engine as agentd's own user: the nobody route has its own driver tests (sandbox_nobody_driver.rs).
            cfg.engine_identity = IdentitySetting::Identity(Identity::Agentd);
            // The test engine is a script that execs the real fake engine in the build folder: both folders are granted to run programs from.
            cfg.engine_path.push(common::fake_engine_dir());
        }),
        ..Opts::default()
    })
    .await;
    let deps = Deps {
        preflight: std::sync::Arc::new(Allow),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    let log = format!(
        "{}\nsummary detail: {:?}",
        d.log.lines().join("\n"),
        d.summary().detail
    );
    assert!(
        log.contains("direct turn engine sandbox: landlock"),
        "the run says which mode it is in: {log}"
    );
    if kernel_has_abi_six() {
        // The engine ran inside the domain: this is the positive path, run on the Instances machine.
        assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}\n{log}");
    } else {
        // On an older kernel the launcher refuses and no engine process runs: its words reach the summary through the rules preflight, the first
        // process the driver starts in the sandbox.
        let detail = d.summary().detail.unwrap_or_default();
        assert!(
            detail.contains("agentd sandbox-exec: refused"),
            "the launcher's refusal is in the summary: {detail:?}\n{log}"
        );
        assert!(
            d.prompts_received().is_empty(),
            "no prompt reached an engine"
        );
        eprintln!(
            "NOT RUN: the positive path needs Linux 6.12 or newer (Landlock ABI 6); it runs on the Instances machine"
        );
    }
}
