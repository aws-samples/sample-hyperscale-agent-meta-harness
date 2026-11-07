//! The `on_report` hook of the module seam: the step between "the delivered head is bundled,
//! uploaded and summarised" and "the turn ends". A module that works here can take minutes, so the tests pin where the call is made
//! (the head and bundle are in the bucket, `turn.ended` is not recorded, nothing is posted to the run machine), that heartbeats go on
//! while it works, that its limit is a ceiling the driver enforces, and that a module which fails or hangs changes nothing about how
//! the run ends. The default, with no module, is pinned by the golden streams of `direct_golden.rs`.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::BoxFuture;
use agentd::direct::hooks::{DirectHook, Hooks, Limits, Report};
use agentd::direct::{ContainersLauncher, ContainersRun, ContainersStart, Deps};
use direct_support::{Direct, FakeAws, Opts, RUN};
use harness_protocol::direct::{DirectOutcome, s3 as keys};
use harness_protocol::envelope::EventType;
use harness_protocol::runtime::{InvocationResponse, TurnOutcome};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A module that only has an `on_report`.
struct ReportProbe {
    log: Arc<Mutex<Vec<String>>>,
    world: FakeAws,
    /// How long the hook works.
    work: Duration,
    panic: bool,
}

impl ReportProbe {
    fn new(d: &Direct, work: Duration) -> Arc<Self> {
        Arc::new(Self {
            log: Arc::new(Mutex::new(Vec::new())),
            world: FakeAws {
                base: d.aws.base.clone(),
                state: d.aws.state.clone(),
            },
            work,
            panic: false,
        })
    }

    fn panicking(d: &Direct) -> Arc<Self> {
        Arc::new(Self {
            log: Arc::new(Mutex::new(Vec::new())),
            world: FakeAws {
                base: d.aws.base.clone(),
                state: d.aws.state.clone(),
            },
            work: Duration::ZERO,
            panic: true,
        })
    }

    fn lines(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl DirectHook for ReportProbe {
    fn name(&self) -> &'static str {
        "reportprobe"
    }

    fn on_report<'a>(&'a self, r: &'a Report<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if self.panic {
                panic!("a module bug at report");
            }
            let heartbeats = self.world.heartbeats().len();
            let events = self.world.events();
            let ended = events.iter().any(|(t, _)| *t == EventType::TurnEnded);
            let summary: Value = self
                .world
                .get(&keys::summary_key(RUN))
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or(Value::Null);
            self.log.lock().unwrap().push(format!(
                "report head={} bundle-file={} bundle-bytes={} bundle-uploaded={} summary-uploaded={} turn-ended-recorded={ended} \
                 complete-posted={} outcome={} uri={} time-left-s={} summary-outcome={} summary-head={}",
                r.head_sha,
                r.bundle.is_file(),
                std::fs::metadata(r.bundle).map_or(0, |m| m.len()),
                self.world.get(&keys::bundle_key(RUN)).is_some(),
                self.world.get(&keys::summary_key(RUN)).is_some(),
                self.world.complete().len(),
                r.classified.outcome.as_str(),
                r.bundle_uri,
                r.time_left.as_secs(),
                summary["outcome"].as_str().unwrap_or("-"),
                summary["git"]["headSha"].as_str().unwrap_or("-")
            ));
            tokio::time::sleep(self.work).await;
            let after = self.world.heartbeats().len();
            self.log
                .lock()
                .unwrap()
                .push(format!("worked heartbeats-during={}", after - heartbeats));
        })
    }
}

async fn run_with(d: &Direct, hooks: Hooks) -> InvocationResponse {
    let deps = Deps {
        hooks: Some(hooks),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await
}

#[test]
fn the_default_ceiling_of_the_report_step_is_480_seconds() {
    assert_eq!(Limits::default().report, Duration::from_secs(480));
    // The other limits keep their values: the new step must not widen the old ones.
    assert_eq!(Limits::default().finalize, Duration::from_secs(15));
    assert_eq!(Limits::default().shutdown, Duration::from_secs(15));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hook_is_called_once_the_head_is_uploaded_and_before_the_turn_ends() {
    let d = Direct::start(Opts::default()).await;
    let p = ReportProbe::new(&d, Duration::ZERO);
    let resp = run_with(&d, Hooks::new(vec![p.clone() as Arc<dyn DirectHook>])).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let lines = p.lines();
    assert_eq!(lines.len(), 2, "{lines:?}");
    let head = resp.last_commit_sha.clone().expect("a delivered head");
    assert!(lines[0].contains(&format!("head={head} ")), "{}", lines[0]);
    assert!(lines[0].contains("bundle-file=true"), "{}", lines[0]);
    assert!(lines[0].contains("bundle-uploaded=true"), "{}", lines[0]);
    assert!(lines[0].contains("summary-uploaded=true"), "{}", lines[0]);
    assert!(
        lines[0].contains("turn-ended-recorded=false"),
        "{}",
        lines[0]
    );
    assert!(lines[0].contains("complete-posted=0"), "{}", lines[0]);
    assert!(lines[0].contains("outcome=reported_done"), "{}", lines[0]);
    // The summary is durable before the module works: it already names the label's outcome and the head the module is given.
    assert!(
        lines[0].contains("summary-outcome=reported_done"),
        "{}",
        lines[0]
    );
    assert!(
        lines[0].contains(&format!("summary-head={head}")),
        "{}",
        lines[0]
    );
    assert!(
        lines[0].contains(&format!(
            "uri=s3://{}/{}",
            direct_support::BUCKET,
            keys::bundle_key(RUN)
        )),
        "{}",
        lines[0]
    );
    // The run still ends as it would have without the module.
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    assert_eq!(d.aws.complete().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_time_left_is_what_remains_of_the_runs_deadline() {
    let d = Direct::start(Opts {
        timeout_s: 120,
        ..Opts::default()
    })
    .await;
    let p = ReportProbe::new(&d, Duration::ZERO);
    run_with(&d, Hooks::new(vec![p.clone() as Arc<dyn DirectHook>])).await;
    let line = p.lines().remove(0);
    let left: u64 = line
        .rsplit("time-left-s=")
        .next()
        .unwrap()
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!((60..=120).contains(&left), "{line}");
}

#[tokio::test(flavor = "multi_thread")]
async fn heartbeats_go_on_while_the_hook_works() {
    // The flusher posts a heartbeat every 300 ms in these tests; the hook works for 1.5 s.
    let d = Direct::start(Opts::default()).await;
    let p = ReportProbe::new(&d, Duration::from_millis(1500));
    let resp = run_with(&d, Hooks::new(vec![p.clone() as Arc<dyn DirectHook>])).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let lines = p.lines();
    let during: usize = lines[1]
        .rsplit("heartbeats-during=")
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        during >= 2,
        "the flusher must still run during the report step: {lines:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hook_that_panics_is_switched_off_and_the_run_ends_the_same() {
    let d = Direct::start(Opts::default()).await;
    let p = ReportProbe::panicking(&d);
    let resp = run_with(&d, Hooks::new(vec![p as Arc<dyn DirectHook>])).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    assert_eq!(d.aws.complete().len(), 1);
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("reportprobe") && l.contains("disabled") && l.contains("panicked")),
        "{:?}",
        d.log.lines()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hook_that_outlasts_its_limit_is_cut_off_and_the_run_ends_the_same() {
    let d = Direct::start(Opts::default()).await;
    let p = ReportProbe::new(&d, Duration::from_secs(60));
    let limits = Limits {
        report: Duration::from_millis(400),
        ..Limits::default()
    };
    let hooks = Hooks::new(vec![p.clone() as Arc<dyn DirectHook>]).with_limits(limits);
    let started = Instant::now();
    let resp = run_with(&d, hooks).await;
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the run waited for a hook past its limit"
    );
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    assert_eq!(d.aws.complete().len(), 1);
    assert!(
        d.log.lines().iter().any(|l| l.contains("reportprobe")
            && l.contains("disabled")
            && l.contains("no answer")),
        "{:?}",
        d.log.lines()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn no_head_no_call() {
    // The repository bundle is missing from the bucket: the run does not start, nothing is delivered, the module is not asked.
    let d = Direct::start(Opts {
        no_bundle: true,
        ..Opts::default()
    })
    .await;
    let p = ReportProbe::new(&d, Duration::ZERO);
    run_with(&d, Hooks::new(vec![p.clone() as Arc<dyn DirectHook>])).await;
    assert!(p.lines().is_empty(), "{:?}", p.lines());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_label_does_not_depend_on_the_module() {
    let plain = Direct::start(Opts::default()).await;
    let with = Direct::start(Opts::default()).await;
    let p = ReportProbe::new(&with, Duration::from_millis(50));
    run_with(&plain, Hooks::none()).await;
    run_with(&with, Hooks::new(vec![p as Arc<dyn DirectHook>])).await;
    let (a, b) = (plain.summary(), with.summary());
    assert_eq!(a.outcome, b.outcome);
    assert_eq!(a.label, b.label);
    assert_eq!(a.detail, b.detail);
    assert_eq!(a.stop_reason, b.stop_reason);
    assert_eq!(a.git.files_changed, b.git.files_changed);
}

// ---- which ends of a run get the report step ----------------------------------------------------------
//
// The report step can take minutes. A user who sends `ham cancel` and a run that ran into its deadline are ended without it: the head is
// delivered as it stands, the label is the cancel's or the deadline's, and nothing waits for a test run nobody asked for at that point.

#[tokio::test(flavor = "multi_thread")]
async fn a_run_the_user_cancelled_is_wound_up_without_the_report_step() {
    let d = Direct::start(Opts {
        scenario: "end_turn_done",
        engine_env: vec![("FAKE_ACP_HOLD_PROMPTS", "1".into())],
        timeout_s: 60,
        ..Opts::default()
    })
    .await;
    d.aws.cancel_on(EventType::TurnStarted);
    let p = ReportProbe::new(&d, Duration::from_secs(30));
    let started = Instant::now();
    let resp = run_with(&d, Hooks::new(vec![p.clone() as Arc<dyn DirectHook>])).await;
    let summary = d.summary();
    // TRIGGER PRESENT: the run was cancelled by the user and a head was delivered, so the step would have been asked.
    assert_eq!(summary.outcome, DirectOutcome::StoppedByUser, "{resp:?}");
    assert!(resp.last_commit_sha.is_some(), "{resp:?}");
    assert!(p.lines().is_empty(), "the report step ran: {:?}", p.lines());
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "the cancel waited for the step: {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_missed_its_deadline_is_wound_up_without_the_report_step() {
    let d = Direct::start(Opts {
        scenario: "end_turn_done",
        engine_env: vec![("FAKE_ACP_HOLD_PROMPTS", "1".into())],
        timeout_s: 15,
        ..Opts::default()
    })
    .await;
    let p = ReportProbe::new(&d, Duration::ZERO);
    let resp = run_with(&d, Hooks::new(vec![p.clone() as Arc<dyn DirectHook>])).await;
    let summary = d.summary();
    // TRIGGER PRESENT: the deadline ended the run and a head was delivered.
    assert_eq!(summary.outcome, DirectOutcome::StoppedTimeout, "{resp:?}");
    assert!(resp.last_commit_sha.is_some(), "{resp:?}");
    assert!(p.lines().is_empty(), "the report step ran: {:?}", p.lines());
}

// ---- what is still running when the module works ---------------------------------------------------------------------------------

/// A container runtime that counts its stops.
struct CountingContainers(Arc<AtomicU32>);
struct CountingRun(Arc<AtomicU32>);

impl ContainersRun for CountingRun {
    fn env(&self) -> BTreeMap<String, String> {
        BTreeMap::new()
    }
    fn stop(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
        })
    }
}

impl ContainersLauncher for CountingContainers {
    fn start(&self, _s: ContainersStart) -> BoxFuture<'_, Result<Box<dyn ContainersRun>, String>> {
        let stops = self.0.clone();
        Box::pin(async move { Ok(Box::new(CountingRun(stops)) as Box<dyn ContainersRun>) })
    }
}

/// Reads the stop counter of the container runtime at the moment the hook runs.
struct StopWatcher {
    stops: Arc<AtomicU32>,
    seen: Mutex<Option<u32>>,
}

impl DirectHook for StopWatcher {
    fn name(&self) -> &'static str {
        "stopwatcher"
    }

    fn on_report<'a>(&'a self, _r: &'a Report<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            *self.seen.lock().unwrap() = Some(self.stops.load(Ordering::SeqCst));
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_container_runtime_is_still_up_when_the_hook_runs_and_stops_after_it() {
    let d = Direct::start(Opts {
        edit_cfg: Some(Box::new(|c| c.engine_user = Some("agent".to_string()))),
        ..Opts::default()
    })
    .await;
    let stops = Arc::new(AtomicU32::new(0));
    let watcher = Arc::new(StopWatcher {
        stops: stops.clone(),
        seen: Mutex::new(None),
    });
    let deps = Deps {
        hooks: Some(Hooks::new(vec![watcher.clone() as Arc<dyn DirectHook>])),
        containers: Arc::new(CountingContainers(stops.clone())),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(
        *watcher.seen.lock().unwrap(),
        Some(0),
        "the runtime was stopped before the module worked on the head"
    );
    assert_eq!(
        stops.load(Ordering::SeqCst),
        1,
        "and it is stopped once the run ends"
    );
}
