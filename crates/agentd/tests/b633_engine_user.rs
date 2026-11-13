//! Agentd runs as root in the microVM and starts the engine as `AGENTD_ENGINE_USER`. When that user cannot be resolved, the run ends before
//! anything is fetched ("Cline did not start", a log line naming the user); it never starts the engine as root. An empty setting and a non-root agentd
//! start the engine as before.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::BoxFuture;
use agentd::config::Config;
use agentd::direct::Deps;
use agentd::engine::{EngineUsers, SystemUsers, resolve_engine_identity};
use direct_support::{CfgEdit, Direct, Opts};
use harness_protocol::direct::DirectOutcome;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// A machine that is described, not asked: the effective uid, what the user lookup says, and how often it was asked.
struct Machine {
    euid: Result<u32, String>,
    lookup: Result<Option<(u32, u32)>, String>,
    asked: Arc<AtomicU32>,
}

impl Machine {
    fn new(euid: Result<u32, String>, lookup: Result<Option<(u32, u32)>, String>) -> Self {
        Self {
            euid,
            lookup,
            asked: Arc::new(AtomicU32::new(0)),
        }
    }
}

impl EngineUsers for Machine {
    fn euid(&self) -> Result<u32, String> {
        self.euid.clone()
    }

    fn lookup<'a>(&'a self, _user: &'a str) -> BoxFuture<'a, Result<Option<(u32, u32)>, String>> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let answer = self.lookup.clone();
        Box::pin(async move { answer })
    }
}

fn edit(f: impl FnOnce(&mut Config) + 'static) -> Option<CfgEdit> {
    Some(Box::new(f))
}

fn named_user() -> Option<CfgEdit> {
    edit(|cfg| cfg.engine_user = Some("agent".to_string()))
}

fn deps_with(machine: Machine) -> Deps {
    Deps {
        engine_users: Arc::new(machine),
        ..Deps::default()
    }
}

/// The run ended in setup, with the words a person reads, and nothing was fetched or placed.
fn assert_refused(d: &Direct, user: &str, why: &str) {
    assert!(d.prompts_received().is_empty(), "no model call");
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed, "{}", s.label);
    assert_eq!(s.label, "Cline did not start");
    let detail = s.detail.unwrap_or_default();
    assert!(detail.contains("engine user"), "{detail}");
    assert!(detail.contains(&format!("{user:?}")), "{detail}");
    assert!(detail.contains(why), "{detail}");
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("engine user") && l.contains(&format!("{user:?}"))),
        "agentd's log names the user: {:?}",
        d.log.lines()
    );
    assert!(
        !d.worktree().exists(),
        "the check came before the work tree was made"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b633_root_with_a_missing_engine_user_ends_the_run_before_anything_is_fetched() {
    let d = Direct::start(Opts {
        edit_cfg: named_user(),
        ..Opts::default()
    })
    .await;
    let deps = deps_with(Machine::new(Ok(0), Ok(None)));
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_refused(&d, "agent", "no such user");
    assert!(
        d.agent_data().join("turns/1/summary.json").is_file(),
        "the refused run still writes its summary"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b633_root_with_a_failed_user_lookup_ends_the_run_and_says_the_lookup_failed() {
    let d = Direct::start(Opts {
        edit_cfg: named_user(),
        ..Opts::default()
    })
    .await;
    let deps = deps_with(Machine::new(Ok(0), Err("id: cannot execute".to_string())));
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_refused(&d, "agent", "cannot execute");
}

#[tokio::test(flavor = "multi_thread")]
async fn b633_an_effective_uid_that_cannot_be_read_ends_the_run_when_a_user_is_named() {
    let d = Direct::start(Opts {
        edit_cfg: named_user(),
        ..Opts::default()
    })
    .await;
    let machine = Machine::new(Err("no /proc".to_string()), Ok(Some((901, 901))));
    let asked = machine.asked.clone();
    let deps = deps_with(machine);
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_refused(&d, "agent", "no /proc");
    assert_eq!(
        asked.load(Ordering::SeqCst),
        0,
        "nothing is looked up when agentd cannot say who it is"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b633_a_user_that_resolves_to_uid_zero_is_refused() {
    let d = Direct::start(Opts {
        edit_cfg: named_user(),
        ..Opts::default()
    })
    .await;
    let deps = deps_with(Machine::new(Ok(0), Ok(Some((0, 0)))));
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_refused(&d, "agent", "uid 0");
}

#[tokio::test(flavor = "multi_thread")]
async fn b633_an_empty_engine_user_setting_starts_the_engine_as_before() {
    let d = Direct::start(Opts::default()).await;
    let machine = Machine::new(Ok(0), Ok(None));
    let asked = machine.asked.clone();
    let deps = deps_with(machine);
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_ne!(d.summary().outcome, DirectOutcome::StoppedSetupFailed);
    assert!(!d.prompts_received().is_empty(), "the engine ran");
    assert_eq!(asked.load(Ordering::SeqCst), 0, "no user to look up");
}

#[tokio::test(flavor = "multi_thread")]
async fn b633_a_non_root_agentd_starts_the_engine_as_before() {
    let d = Direct::start(Opts {
        edit_cfg: named_user(),
        ..Opts::default()
    })
    .await;
    let machine = Machine::new(Ok(1000), Ok(None));
    let asked = machine.asked.clone();
    let deps = deps_with(machine);
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_ne!(d.summary().outcome, DirectOutcome::StoppedSetupFailed);
    assert!(!d.prompts_received().is_empty(), "the engine ran");
    assert_eq!(
        asked.load(Ordering::SeqCst),
        0,
        "a non-root agentd looks nobody up"
    );
}

#[tokio::test]
async fn b633_the_resolver_returns_the_ids_of_an_existing_user_when_agentd_is_root() {
    let m = Machine::new(Ok(0), Ok(Some((901, 902))));
    assert_eq!(
        resolve_engine_identity(&m, Some("agent")).await,
        Ok(Some((901, 902)))
    );
    assert_eq!(resolve_engine_identity(&m, None).await, Ok(None));
    assert_eq!(resolve_engine_identity(&m, Some("  ")).await, Ok(None));
}

#[tokio::test]
async fn b633_the_resolver_names_the_user_in_every_refusal() {
    let cases = [
        Machine::new(Ok(0), Ok(None)),
        Machine::new(Ok(0), Err("boom".to_string())),
        Machine::new(Err("unreadable".to_string()), Ok(Some((901, 901)))),
        Machine::new(Ok(0), Ok(Some((0, 0)))),
    ];
    for m in &cases {
        let why = resolve_engine_identity(m, Some("agent")).await.unwrap_err();
        assert!(why.contains("\"agent\""), "{why}");
        assert!(why.contains("root"), "{why}");
    }
}

#[tokio::test]
async fn b633_the_system_lookup_says_no_such_user_for_a_name_that_does_not_exist() {
    let found = SystemUsers.lookup("b633-no-such-user").await;
    assert_eq!(found, Ok(None));
    let me = SystemUsers.euid().unwrap();
    assert_eq!(me, rustix::process::geteuid().as_raw());
}
