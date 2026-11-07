//! The direct driver and the nobody route: a Landlock run starts its engine through the helper by default, a helper that fails its check or an
//! identity word that is not one ends the run before anything is fetched, and `AGENTD_ENGINE_IDENTITY=agentd` brings back the command line of before.
//!
//! The kernel's Landlock and user namespaces are not needed: the launcher and the helper are replaced by scripts that keep their arguments and run the program
//! unconfined, so the command line and the policy the driver composed can be read back on any machine.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::config::Config;
use agentd::direct::Deps;
use agentd::sandbox::{
    Identity, IdentityPreflight, IdentitySetting, Mode, Preflight, SandboxError, Setting,
    route_of_this_process,
};
use common::scratch;
use direct_support::{CfgEdit, Direct, Opts};
use harness_protocol::direct::DirectOutcome;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct AllowLandlock;

impl Preflight for AllowLandlock {
    fn check(&self) -> Result<(), SandboxError> {
        Ok(())
    }
}

struct FakeIdentityCheck(Result<(), &'static str>);

impl IdentityPreflight for FakeIdentityCheck {
    fn check(&self, _helper: &Path) -> Result<(), SandboxError> {
        self.0.map_err(|why| SandboxError::Helper(why.to_string()))
    }
}

fn script(dir: &Path, name: &str, text: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    let mut perm = std::fs::metadata(&p).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(&p, perm).unwrap();
    p
}

/// The stand-in launcher: `$1` is `sandbox-exec`, `$3` the policy, the program follows `--`.
fn stand_in_launcher(dir: &Path) -> PathBuf {
    script(
        dir,
        "launcher.sh",
        "#!/bin/sh\nprintf '%s\\n' \"$3\" > \"$HOME/policy.json\"\nshift 4\nexec \"$@\"\n",
    )
}

/// The stand-in helper: it writes its arguments, one per line, to `$HOME/helper-args.txt`, drops the `--` and runs the rest.
fn stand_in_helper(dir: &Path) -> PathBuf {
    script(
        dir,
        "helper.sh",
        "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$HOME/helper-args.txt\"\nshift 1\nexec \"$@\"\n",
    )
}

fn edit(f: impl FnOnce(&mut Config) + 'static) -> Option<CfgEdit> {
    Some(Box::new(f))
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
}

async fn run(
    identity: IdentitySetting,
    helper: IdentityCheck,
    with_helper_script: bool,
) -> (Direct, PathBuf, Vec<String>, Value) {
    let dir = scratch("nobody-driver");
    let launcher = stand_in_launcher(&dir);
    let launcher_path = launcher.clone();
    let helper_path = if with_helper_script {
        stand_in_helper(&dir)
    } else {
        dir.join("no-such-helper")
    };
    let d = Direct::start(Opts {
        edit_cfg: edit(move |cfg| {
            cfg.engine_sandbox = Setting::Mode(Mode::Landlock);
            cfg.engine_identity = identity;
            cfg.agentd_exe = launcher;
            cfg.identity_helper = helper_path;
            cfg.engine_path.push(common::fake_engine_dir());
        }),
        ..Opts::default()
    })
    .await;
    let deps = Deps {
        preflight: Arc::new(AllowLandlock),
        identity: Arc::new(FakeIdentityCheck(helper)),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    d.wait_healthy().await;
    let home = d.agent_data().join("home");
    let args = std::fs::read_to_string(home.join("helper-args.txt"))
        .map(|t| t.lines().map(str::to_string).collect())
        .unwrap_or_default();
    let policy = std::fs::read_to_string(home.join("policy.json"))
        .map(|t| serde_json::from_str(&t).unwrap())
        .unwrap_or(Value::Null);
    (d, launcher_path, args, policy)
}

type IdentityCheck = Result<(), &'static str>;

#[tokio::test(flavor = "multi_thread")]
async fn a_nobody_run_puts_the_helper_in_front_of_the_launcher_and_names_agentds_namespaces_in_the_policy()
 {
    let (d, launcher, args, policy) = run(IdentitySetting::default(), Ok(()), true).await;
    let launcher = launcher.display().to_string();
    assert_eq!(
        args.first().map(String::as_str),
        Some("--"),
        "the helper's own arguments: {args:?}"
    );
    assert_eq!(
        args.get(1),
        Some(&launcher),
        "the launcher follows the helper: {args:?}"
    );
    assert_eq!(
        args.get(2).map(String::as_str),
        Some("sandbox-exec"),
        "{args:?}"
    );
    let route = route_of_this_process().unwrap();
    assert_eq!(
        policy["nobody"]["agentdIpc"],
        Value::String(route.agentd_ipc),
        "{policy}"
    );
    assert_eq!(
        policy["nobody"]["agentdUser"],
        Value::String(route.agentd_user),
        "{policy}"
    );
    let log = d.log.lines().join("\n");
    assert!(
        log.contains("identity nobody"),
        "the run says who the engine is: {log}"
    );
    assert_eq!(
        d.summary().engine.identity.as_deref(),
        Some("nobody"),
        "the summary records it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_helper_that_fails_its_check_ends_the_run_before_anything_is_fetched() {
    let d = Direct::start(Opts {
        edit_cfg: edit(|cfg| cfg.engine_sandbox = Setting::Mode(Mode::Landlock)),
        ..Opts::default()
    })
    .await;
    let deps = Deps {
        preflight: Arc::new(AllowLandlock),
        identity: Arc::new(FakeIdentityCheck(Err("injected: no namespaces"))),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_refused_before_anything(&d, "injected: no namespaces");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_identity_word_that_is_not_one_ends_the_run_before_anything_is_fetched() {
    let d = Direct::start(Opts {
        edit_cfg: edit(|cfg| {
            cfg.engine_sandbox = Setting::Mode(Mode::Landlock);
            cfg.engine_identity = IdentitySetting::from_value(Some("root"));
        }),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    assert_refused_before_anything(&d, "AGENTD_ENGINE_IDENTITY");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agentd_identity_brings_back_the_command_line_of_before_and_never_asks_the_helper() {
    let (d, _, args, policy) = run(
        IdentitySetting::Identity(Identity::Agentd),
        Err("must not be asked"),
        false,
    )
    .await;
    assert!(args.is_empty(), "the helper did not run: {args:?}");
    assert!(
        policy.get("nobody").is_none(),
        "no route in the policy: {policy}"
    );
    assert!(policy["version"].is_number(), "the launcher ran: {policy}");
    let log = d.log.lines().join("\n");
    assert!(log.contains("identity agentd"), "{log}");
    assert_eq!(
        d.summary().engine.identity.as_deref(),
        Some("agentd"),
        "the summary records it"
    );
}
