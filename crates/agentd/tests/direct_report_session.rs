//! The report step in the session of a run: the whole agent runs in process against the fake AWS and the fake
//! ACP engine, with a fake package proxy and a fake container runtime in the places of the real ones. The commands the fake engine leaves in
//! `.ham/test-command` check the environment they find: a step that needs the Maven settings file Cline's session has, a step that needs
//! the container variables, one that needs neither, and one that needs a runtime the session does not have. The in-VM report of the
//! enterprise sample was red for the first two and for the fourth, though Cline's own tests had passed.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::config::Config;
use agentd::direct::hooks::{DirectHook, Hooks};
use agentd::direct::report::{ReportHook, Settings};
use agentd::direct::{
    ContainersLauncher, ContainersRun, ContainersStart, Deps, ProxyLauncher, ProxyRun, ProxyStart,
};
use agentd::packages::PackagesIndex;
use direct_support::{Direct, Opts, RUN};
use harness_protocol::direct::{
    DirectOutcome, DirectPackages, TestCounts, TestReport, TestReportStatus, s3 as keys,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

struct FakeProxy;
struct FakeProxyRun;

impl ProxyRun for FakeProxyRun {
    fn env(&self) -> BTreeMap<String, String> {
        [
            ("PIP_INDEX_URL", "http://127.0.0.1:3128/pypi/simple/"),
            ("npm_config_registry", "http://127.0.0.1:3128/npm/"),
            ("GOPROXY", "http://127.0.0.1:3128/go"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }
    fn maven_settings_xml(&self) -> String {
        "<settings><mirrors><mirror><id>loop</id><mirrorOf>*</mirrorOf><url>http://127.0.0.1:3128/maven/</url></mirror></mirrors></settings>".to_string()
    }
    fn cargo_config_toml(&self) -> String {
        "[source.crates-io]\nreplace-with = \"hamh-proxy\"\n".to_string()
    }
    fn stats(&self) -> DirectPackages {
        DirectPackages::default()
    }
    fn stop(self: Box<Self>) -> agentd::BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

impl ProxyLauncher for FakeProxy {
    fn start(
        &self,
        _start: ProxyStart,
    ) -> agentd::BoxFuture<'_, Result<Box<dyn ProxyRun>, String>> {
        Box::pin(async { Ok(Box::new(FakeProxyRun) as Box<dyn ProxyRun>) })
    }
}

/// A runtime that starts, or does not.
struct FakeContainers {
    up: bool,
}
struct FakeRuntime;

impl ContainersRun for FakeRuntime {
    fn env(&self) -> BTreeMap<String, String> {
        agentd::direct::containers_env()
    }
    fn stop(self: Box<Self>) -> agentd::BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

impl ContainersLauncher for FakeContainers {
    fn start(
        &self,
        _s: ContainersStart,
    ) -> agentd::BoxFuture<'_, Result<Box<dyn ContainersRun>, String>> {
        let up = self.up;
        Box::pin(async move {
            if up {
                Ok(Box::new(FakeRuntime) as Box<dyn ContainersRun>)
            } else {
                Err("no runtime in this test".to_string())
            }
        })
    }
}

fn session_config(c: &mut Config) {
    c.engine_user = Some("agent".to_string());
    c.packages = Some(PackagesIndex {
        domain: "d".into(),
        owner: "111111111111".into(),
        repository: "packages".into(),
        api_endpoint: None,
    });
}

async fn run_session(scenario: &'static str, runtime_up: bool) -> Direct {
    let d = Direct::start(Opts {
        scenario,
        packages_token: true,
        edit_cfg: Some(Box::new(session_config)),
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    let hooks = Hooks::new(vec![Arc::new(ReportHook::new(Settings {
        enabled: true,
        budget: Duration::from_secs(60),
        step: Duration::from_secs(30),
        tail: harness_protocol::direct::DEFAULT_REPORT_TAIL_BYTES,
    })) as Arc<dyn DirectHook>]);
    let deps = Deps {
        proxy: Arc::new(FakeProxy),
        containers: Arc::new(FakeContainers { up: runtime_up }),
        hooks: Some(hooks),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    d
}

fn report_of(d: &Direct) -> TestReport {
    let bytes = d
        .aws
        .get(&keys::test_report_key(RUN))
        .expect("a report was uploaded");
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_step_that_needs_the_maven_settings_of_the_session_finds_them() {
    let d = run_session("report_needs_maven", true).await;
    let r = report_of(&d);
    assert_eq!(r.status, TestReportStatus::Ran, "{r:?}");
    let s = &r.steps[0];
    assert_eq!(s.exit_code, Some(0), "{}", s.output_tail);
    assert_eq!(
        s.counts,
        Some(TestCounts {
            passed: 3,
            failed: 0,
            skipped: 0
        })
    );
    assert_eq!(
        r.summary().claim().as_deref(),
        Some("repository tests pass in a clean checkout: 3 of 3")
    );
}

/// The report step gets the cargo config the package proxy gave the run (`Handle::cargo_config_toml`), through its own `CARGO_HOME`.
#[tokio::test(flavor = "multi_thread")]
async fn a_step_that_needs_the_cargo_config_of_the_session_finds_the_proxys() {
    let d = run_session("report_needs_cargo", true).await;
    let r = report_of(&d);
    assert_eq!(r.status, TestReportStatus::Ran, "{r:?}");
    let s = &r.steps[0];
    assert_eq!(s.exit_code, Some(0), "{}", s.output_tail);
    assert_eq!(
        s.counts,
        Some(TestCounts {
            passed: 2,
            failed: 0,
            skipped: 0
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_step_that_needs_the_container_variables_of_the_session_finds_them() {
    let d = run_session("report_needs_docker", true).await;
    let r = report_of(&d);
    assert_eq!(r.status, TestReportStatus::Ran, "{r:?}");
    let s = &r.steps[0];
    assert_eq!(s.exit_code, Some(0), "{}", s.output_tail);
    assert_eq!(
        s.counts,
        Some(TestCounts {
            passed: 5,
            failed: 0,
            skipped: 0
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_step_that_needs_neither_passes_with_or_without_a_runtime() {
    for up in [true, false] {
        let d = run_session("report_green", up).await;
        let r = report_of(&d);
        assert_eq!(r.steps[0].exit_code, Some(0), "up={up}: {r:?}");
        assert!(r.summary().claim().is_some(), "up={up}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_step_that_needs_a_runtime_the_session_does_not_have_is_marked_not_run_and_the_report_is_not_red()
 {
    let d = run_session("report_docker_down", false).await;
    let r = report_of(&d);
    assert_eq!(r.status, TestReportStatus::Ran, "{r:?}");
    assert_eq!(r.steps.len(), 2, "{r:?}");
    let first = &r.steps[0];
    assert_eq!(first.counts_from.as_deref(), Some("not run"), "{first:?}");
    assert_eq!(
        first.output_tail,
        "NOT RUN: the session has no container runtime"
    );
    assert_eq!(first.exit_code, None);
    // The other command ran and passed.
    assert_eq!(r.steps[1].exit_code, Some(0));
    let s = r.summary();
    // Not red: nothing failed, no exit code other than 0, no failing command. And not a claim either: a command did not run.
    assert_eq!(s.failed_command, None, "{s:?}");
    assert_eq!(s.exit_code, Some(0));
    assert!(!s.setup_failed);
    assert_eq!(s.claim(), None);
    assert!(s.sentence().starts_with("NOT RUN: "), "{}", s.sentence());
    assert!(
        s.sentence().contains("docker compose up"),
        "{}",
        s.sentence()
    );
    assert!(
        s.sentence()
            .contains("the session has no container runtime"),
        "{}",
        s.sentence()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_step_that_was_not_run_records_no_limit_and_one_that_ran_records_the_one_it_had() {
    // The first command needs a runtime the session does not have: agentd does not start it, so no time limit applied to it and the report
    // must not say one did (`TestStep.limitS` is absent for a command never started). The second command ran with the module's 30 s.
    let d = run_session("report_docker_down", false).await;
    let r = report_of(&d);
    assert_eq!(r.steps.len(), 2, "{r:?}");
    assert!(
        r.steps[0].is_not_run(),
        "TRIGGER PRESENT: the first step is not run: {:?}",
        r.steps[0]
    );
    assert_eq!(r.steps[0].limit_s, None, "{:?}", r.steps[0]);
    assert_eq!(r.steps[1].limit_s, Some(30), "{:?}", r.steps[1]);
    // The rest of the report is what it was: not red, no timed-out command, the same sentence.
    let s = r.summary();
    assert_eq!(s.timed_out, None);
    assert!(s.sentence().starts_with("NOT RUN: "), "{}", s.sentence());
}
