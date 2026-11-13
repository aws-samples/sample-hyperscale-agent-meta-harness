//! The container-runtime seam of the direct driver: `Deps.containers` starts the session's rootless container runtime
//! (`ham-containers up --user <engine user>`) before the engine, hands the engine the Docker endpoint through four environment
//! variables, and stops the runtime at the end. A runtime that is missing or fails never fails the run. The real launcher
//! (`ShellContainersLauncher`) is tested against fake scripts; no container is started here.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::{
    ContainersLauncher, ContainersRun, ContainersStart, Deps, ShellContainersLauncher,
    containers_env,
};
use agentd::logging::{LogSink, MemoryLog};
use common::scratch;
use direct_support::{Direct, Opts};
use harness_protocol::runtime::TurnOutcome;
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What the engine must see, exactly.
const FOUR: [&str; 4] = [
    "CONTAINER_HOST=unix:///var/run/docker.sock",
    "DOCKER_HOST=unix:///var/run/docker.sock",
    "TESTCONTAINERS_HOST_OVERRIDE=127.0.0.1",
    "TESTCONTAINERS_RYUK_DISABLED=true",
];

fn container_lines(probe: &str) -> Vec<String> {
    probe
        .lines()
        .filter(|l| {
            ["DOCKER_", "CONTAINER_", "TESTCONTAINERS_"]
                .iter()
                .any(|p| l.starts_with(p))
        })
        .map(str::to_string)
        .collect()
}

fn with_engine_user(user: &'static str) -> Box<dyn FnOnce(&mut agentd::config::Config)> {
    Box::new(move |c| c.engine_user = Some(user.to_string()))
}

// ---- the fake launcher -----------------------------------------------------------------------------------------------------

enum Mode {
    Ok,
    Err(&'static str),
    Panic,
}

#[derive(Default)]
struct Calls {
    starts: Mutex<Vec<String>>,
    stops: Mutex<u32>,
}

struct FakeContainers {
    calls: Arc<Calls>,
    mode: Mode,
}

struct FakeRun(Arc<Calls>, Arc<dyn LogSink>);

impl ContainersRun for FakeRun {
    fn env(&self) -> BTreeMap<String, String> {
        FOUR.iter()
            .map(|l| {
                let (k, v) = l.split_once('=').unwrap();
                (k.to_string(), v.to_string())
            })
            .collect()
    }
    fn stop(self: Box<Self>) -> agentd::BoxFuture<'static, ()> {
        Box::pin(async move {
            *self.0.stops.lock().unwrap() += 1;
            self.1.line("fake containers stopped");
        })
    }
}

impl ContainersLauncher for FakeContainers {
    fn start(
        &self,
        s: ContainersStart,
    ) -> agentd::BoxFuture<'_, Result<Box<dyn ContainersRun>, String>> {
        self.calls.starts.lock().unwrap().push(s.user.clone());
        let calls = self.calls.clone();
        let mode = &self.mode;
        let result = match mode {
            Mode::Ok => Some(Ok(())),
            Mode::Err(e) => Some(Err((*e).to_string())),
            Mode::Panic => None,
        };
        Box::pin(async move {
            match result {
                Some(Ok(())) => Ok(Box::new(FakeRun(calls, s.log)) as Box<dyn ContainersRun>),
                Some(Err(e)) => Err(e),
                None => panic!("the fake container launcher panics"),
            }
        })
    }
}

fn deps(mode: Mode) -> (Deps, Arc<Calls>) {
    let calls = Arc::new(Calls::default());
    let deps = Deps {
        containers: Arc::new(FakeContainers {
            calls: calls.clone(),
            mode,
        }),
        ..Deps::default()
    };
    (deps, calls)
}

fn no_docker(probe: &str) {
    assert!(
        container_lines(probe).is_empty(),
        "the engine saw {:?}",
        container_lines(probe)
    );
}

// ---- the run ---------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_engine_gets_exactly_the_four_docker_variables_and_the_launcher_is_called_once_with_the_engine_user()
 {
    let d = Direct::start(Opts {
        edit_cfg: Some(with_engine_user("agent")),
        ..Opts::default()
    })
    .await;
    let (deps, calls) = deps(Mode::Ok);
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(*calls.starts.lock().unwrap(), vec!["agent".to_string()]);
    assert_eq!(container_lines(&d.engine_probe()), FOUR.to_vec());
    assert!(
        !d.log
            .lines()
            .iter()
            .any(|l| l.contains("containers not available"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_launcher_that_fails_leaves_the_run_completed_and_the_engine_without_docker() {
    let d = Direct::start(Opts {
        edit_cfg: Some(with_engine_user("agent")),
        ..Opts::default()
    })
    .await;
    let (deps, calls) = deps(Mode::Err("containers: no user namespaces here"));
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    no_docker(&d.engine_probe());
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l
                == "direct turn containers not available: containers: no user namespaces here")
    );
    assert_eq!(
        *calls.stops.lock().unwrap(),
        0,
        "nothing started, nothing to stop"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_launcher_that_panics_leaves_the_run_completed() {
    let d = Direct::start(Opts {
        edit_cfg: Some(with_engine_user("agent")),
        ..Opts::default()
    })
    .await;
    let (deps, _) = deps(Mode::Panic);
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    no_docker(&d.engine_probe());
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.starts_with("direct turn containers not available:"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_runtime_is_stopped_exactly_once_after_the_engine_has_run() {
    let d = Direct::start(Opts {
        edit_cfg: Some(with_engine_user("agent")),
        ..Opts::default()
    })
    .await;
    let (deps, calls) = deps(Mode::Ok);
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(*calls.stops.lock().unwrap(), 1);
    let log = d.log.lines();
    let at = |needle: &str| log.iter().position(|l| l == needle).unwrap();
    assert!(
        at("direct turn engine starting") < at("fake containers stopped"),
        "{log:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_runtime_is_also_stopped_when_the_run_fails_after_it_started() {
    // The rules preflight fails after the container step and before any model call.
    let d = Direct::start(Opts {
        engine_env: vec![("FAKE_ACP_RULES_MODE", "fail".into())],
        edit_cfg: Some(with_engine_user("agent")),
        ..Opts::default()
    })
    .await;
    let (deps, calls) = deps(Mode::Ok);
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Failed, "{resp:?}");
    assert_eq!(*calls.starts.lock().unwrap(), vec!["agent".to_string()]);
    assert_eq!(*calls.stops.lock().unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_engine_user_or_a_root_one_the_launcher_is_never_called() {
    for user in [None, Some("root")] {
        let d = Direct::start(Opts {
            edit_cfg: user.map(with_engine_user),
            ..Opts::default()
        })
        .await;
        let (deps, calls) = deps(Mode::Ok);
        let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
        assert_eq!(resp.outcome, TurnOutcome::Completed, "{user:?}: {resp:?}");
        assert!(calls.starts.lock().unwrap().is_empty(), "{user:?}");
        assert_eq!(*calls.stops.lock().unwrap(), 0);
        no_docker(&d.engine_probe());
        assert!(
            d.log
                .lines()
                .iter()
                .any(|l| l == "direct turn containers not started: no engine user"),
            "{user:?}"
        );
    }
}

// ---- the real launcher against fake scripts ---------------------------------------------------------------------------------

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

const SECS: Duration = Duration::from_secs(10);

#[test]
fn the_environment_of_the_real_runtime_is_exactly_these_four_variables() {
    let want: BTreeMap<String, String> = FOUR
        .iter()
        .map(|l| {
            let (k, v) = l.split_once('=').unwrap();
            (k.to_string(), v.to_string())
        })
        .collect();
    assert_eq!(containers_env(), want);
}

#[test]
fn the_binary_and_the_timeouts_come_from_the_environment_with_safe_defaults() {
    assert_eq!(
        ShellContainersLauncher::bin_from(None),
        PathBuf::from("/opt/harness/bin/ham-containers")
    );
    assert_eq!(
        ShellContainersLauncher::bin_from(Some("")),
        PathBuf::from("/opt/harness/bin/ham-containers")
    );
    assert_eq!(
        ShellContainersLauncher::bin_from(Some("/x/y")),
        PathBuf::from("/x/y")
    );
    let default = (Duration::from_secs(30), Duration::from_secs(15));
    assert_eq!(ShellContainersLauncher::timeouts_from(None), default);
    for junk in ["", "junk", "0", "-3"] {
        assert_eq!(
            ShellContainersLauncher::timeouts_from(Some(junk)),
            default,
            "{junk:?}"
        );
    }
    // An override can only shorten the stop, never lengthen it past 15 s.
    assert_eq!(
        ShellContainersLauncher::timeouts_from(Some("5")),
        (Duration::from_secs(5), Duration::from_secs(5))
    );
    assert_eq!(
        ShellContainersLauncher::timeouts_from(Some("60")),
        (Duration::from_secs(60), Duration::from_secs(15))
    );
}

#[tokio::test]
async fn up_gets_the_engine_user_and_down_runs_when_the_run_stops() {
    let dir = scratch("containers-argv");
    let args = dir.join("args");
    let bin = script(
        &dir,
        "ham-containers",
        &format!("echo \"$@\" >> {}", args.display()),
    );
    let launcher = ShellContainersLauncher::new(bin, SECS, SECS);
    let (start, _) = start_for("agent");
    let run = launcher.start(start).await.unwrap();
    assert_eq!(run.env(), containers_env());
    assert_eq!(std::fs::read_to_string(&args).unwrap(), "up --user agent\n");
    run.stop().await;
    assert_eq!(
        std::fs::read_to_string(&args).unwrap(),
        "up --user agent\ndown\n"
    );
}

#[tokio::test]
async fn a_failing_up_gives_the_first_line_of_its_stderr() {
    let dir = scratch("containers-fail");
    let bin = script(
        &dir,
        "ham-containers",
        "echo 'no user namespaces here' >&2; echo 'second line' >&2; exit 1",
    );
    let launcher = ShellContainersLauncher::new(bin, SECS, SECS);
    let (start, _) = start_for("agent");
    let Err(e) = launcher.start(start).await else {
        panic!("up exited 1 and the launcher said Ok");
    };
    assert_eq!(e, "containers: no user namespaces here");
}

#[tokio::test]
async fn a_failing_up_with_no_stderr_still_says_what_happened() {
    let dir = scratch("containers-fail-quiet");
    let bin = script(&dir, "ham-containers", "exit 3");
    let launcher = ShellContainersLauncher::new(bin, SECS, SECS);
    let (start, _) = start_for("agent");
    let Err(e) = launcher.start(start).await else {
        panic!("up exited 3 and the launcher said Ok");
    };
    assert!(e.starts_with("containers: ") && e.contains('3'), "{e}");
}

#[tokio::test]
async fn a_missing_binary_is_not_installed_and_nothing_is_spawned() {
    let dir = scratch("containers-missing");
    for bin in [dir.join("nope"), dir.clone()] {
        let launcher = ShellContainersLauncher::new(bin.clone(), SECS, SECS);
        let (start, _) = start_for("agent");
        let Err(e) = launcher.start(start).await else {
            panic!("{bin:?}: said Ok");
        };
        // A directory exists but is no file: trying to run it would say "cannot run", not this.
        assert_eq!(e, "containers: ham-containers is not installed", "{bin:?}");
    }
}

#[tokio::test]
async fn an_up_that_outlasts_its_timeout_is_killed_and_is_an_error() {
    let dir = scratch("containers-timeout");
    let pid = dir.join("pid");
    let bin = script(
        &dir,
        "ham-containers",
        &format!("echo $$ > {}; exec sleep 30", pid.display()),
    );
    let launcher = ShellContainersLauncher::new(bin, Duration::from_millis(800), SECS);
    let (start, _) = start_for("agent");
    let t = Instant::now();
    let Err(e) = launcher.start(start).await else {
        panic!("up ran past its timeout and the launcher said Ok");
    };
    assert!(t.elapsed() < Duration::from_secs(8), "{:?}", t.elapsed());
    assert!(
        e.starts_with("containers: ") && e.contains("did not finish"),
        "{e}"
    );
    let pid = std::fs::read_to_string(&pid).unwrap().trim().to_string();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        if !alive {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the timed-out up (pid {pid}) is still running"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn an_up_that_leaves_a_daemon_holding_its_pipes_still_returns() {
    let dir = scratch("containers-daemon");
    let pid = dir.join("daemon.pid");
    // The script starts a background process that inherits stdout and stderr, then exits 0, as `ham-containers up` may.
    let bin = script(
        &dir,
        "ham-containers",
        &format!("sleep 30 &\necho $! > {}\nexit 0", pid.display()),
    );
    let launcher = ShellContainersLauncher::new(bin, SECS, SECS);
    let (start, _) = start_for("agent");
    let t = Instant::now();
    let run = launcher.start(start).await;
    let took = t.elapsed();
    let daemon = std::fs::read_to_string(&pid).unwrap().trim().to_string();
    let _ = std::process::Command::new("kill").arg(&daemon).status();
    assert!(run.is_ok());
    assert!(took < Duration::from_secs(5), "{took:?}");
}

#[tokio::test]
async fn the_log_keeps_at_most_the_last_2_kib_of_what_up_printed() {
    let dir = scratch("containers-log");
    let bin = script(
        &dir,
        "ham-containers",
        "head -c 3000 /dev/zero | tr '\\0' A; echo; echo LAST-LINE",
    );
    let launcher = ShellContainersLauncher::new(bin, SECS, SECS);
    let (start, log) = start_for("agent");
    launcher.start(start).await.unwrap();
    let lines = log.lines();
    assert!(lines.iter().any(|l| l.contains("LAST-LINE")), "{lines:?}");
    let logged: usize = lines.iter().map(String::len).sum();
    assert!(logged < 2048 + 400, "{logged} bytes logged");
    assert!(!lines.iter().any(|l| l.contains(&"A".repeat(2500))));
}

#[tokio::test]
async fn a_failing_down_is_logged_and_never_raised() {
    let dir = scratch("containers-down");
    let bin = script(
        &dir,
        "ham-containers",
        "case \"$1\" in down) echo 'cannot stop' >&2; exit 1;; esac",
    );
    let launcher = ShellContainersLauncher::new(bin, SECS, SECS);
    let (start, log) = start_for("agent");
    let run = launcher.start(start).await.unwrap();
    run.stop().await;
    assert!(
        log.lines()
            .iter()
            .any(|l| l.starts_with("direct turn containers stop") && l.contains("cannot stop")),
        "{:?}",
        log.lines()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_whole_run_through_the_real_launcher_and_a_script_shows_the_engine_the_four_variables() {
    let dir = scratch("containers-run");
    let args = dir.join("args");
    let bin = script(
        &dir,
        "ham-containers",
        &format!("echo \"$@\" >> {}", args.display()),
    );
    let d = Direct::start(Opts {
        edit_cfg: Some(with_engine_user("agent")),
        ..Opts::default()
    })
    .await;
    let deps = Deps {
        containers: Arc::new(ShellContainersLauncher::new(bin, SECS, SECS)),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(container_lines(&d.engine_probe()), FOUR.to_vec());
    assert_eq!(
        std::fs::read_to_string(&args).unwrap(),
        "up --user agent\ndown\n"
    );
}
