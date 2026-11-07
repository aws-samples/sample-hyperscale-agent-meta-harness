//! A long command that the model waits for, and the last turn of a brake, whole: the real `goal` module in the real driver against the fake ACP
//! engine. The tests run as one user, so the evidence that a command is running comes from a probe the test names
//! (`GoalHook::for_engine_with_probe`); the reading of the process table has its own tests (`direct_goal_command.rs`).
//!
//! Each test that proves "the run goes on" has its twin that proves the trigger: the same polls with nothing running end the run.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::Deps;
use agentd::direct::goal::command::CommandProbe;
use agentd::direct::goal::settings::GoalSettings;
use agentd::direct::goal::watch::Command;
use agentd::direct::goal::{FINAL_PROMPT, GoalHook};
use agentd::direct::hooks::{DirectHook, Hooks};
use common::{plugin_folder, scratch};
use direct_support::{Direct, Opts, PROMPT, RUN, sha};
use harness_protocol::direct::{DirectOutcome, s3 as keys};
use harness_protocol::runtime::{InvocationResponse, TurnOutcome};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A job the model started minutes ago: the sessions that use the CPU are older than every call of these runs (about 12 s long).
const JOB: Command = Command::Active {
    age: Duration::from_secs(3600),
};

/// A command the call itself started: the sessions that use the CPU are one second old.
const OWN_COMMAND: Command = Command::Active {
    age: Duration::from_secs(1),
};

/// The label of a stop by the brake inside a round.
const BRAKE_LABEL: &str = "Cline stopped: the harness ended the run (Cline made no progress in ";

/// The evidence a test decides: what every sample answers, and how many baselines were taken.
#[derive(Debug)]
struct Scripted {
    answer: Mutex<Command>,
    samples: AtomicU32,
    baselines: AtomicU32,
}

impl Scripted {
    fn new(answer: Command) -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(answer),
            samples: AtomicU32::new(0),
            baselines: AtomicU32::new(0),
        })
    }
}

impl CommandProbe for Scripted {
    fn rebaseline(&self) {
        self.baselines.fetch_add(1, Ordering::SeqCst);
    }
    fn sample(&self, _now: Instant) -> Command {
        self.samples.fetch_add(1, Ordering::SeqCst);
        *self.answer.lock().unwrap()
    }
}

/// A plugin folder like the image's (`common::plugin_folder`: the goal plugin is fetched at image build, not stored in the repository) and times
/// short enough to run in seconds. The window cannot be below 10 s, so the polls of the scenario
/// (20 calls, 600 ms apart) take longer than it.
fn settings(extra: &[(&str, &str)]) -> GoalSettings {
    let mut m: BTreeMap<String, String> = [
        (
            "AGENTD_GOAL_PLUGIN_DIR",
            plugin_folder().display().to_string(),
        ),
        ("AGENTD_GOAL_POLL_MS", "20".to_string()),
        ("AGENTD_GOAL_SETTLE_MS", "150".to_string()),
        ("AGENTD_GOAL_DEAD_GRACE_MS", "150".to_string()),
        ("AGENTD_GOAL_KICK_AFTER_S", "1".to_string()),
        ("AGENTD_GOAL_DRAIN_S", "3".to_string()),
        ("AGENTD_GOAL_CLEARED_CONFIRM_MS", "200".to_string()),
        ("AGENTD_GOAL_FALLBACK_QUIET_S", "1".to_string()),
        ("AGENTD_GOAL_CLAIM_WAIT_MS", "300".to_string()),
        ("AGENTD_GOAL_EVENTS_EVERY_S", "0".to_string()),
        ("AGENTD_GOAL_LOOK_EVERY_MS", "50".to_string()),
        ("AGENTD_GOAL_ROUND_WINDOW_S", "10".to_string()),
        ("AGENTD_GOAL_ROUND_MIN_CALLS", "3".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    for (k, v) in extra {
        m.insert((*k).to_string(), (*v).to_string());
    }
    GoalSettings::from_lookup(&|k| m.get(k).cloned())
}

/// Puts members into the request the run fetches, and the hash the invocation carries.
fn with_members(d: &mut Direct, members: Value) {
    let mut v: Value = serde_json::from_slice(&d.request_json).unwrap();
    for (k, val) in members.as_object().unwrap() {
        v[k] = val.clone();
    }
    let bytes = serde_json::to_vec(&v).unwrap();
    d.aws.put(&keys::request_key(RUN), bytes.clone());
    d.req.request.sha256 = sha(&bytes);
    d.request_json = bytes;
}

async fn run(d: &Direct, s: GoalSettings, probe: Arc<Scripted>) -> InvocationResponse {
    let goal = GoalHook::for_engine_with_probe("cline", s, probe)
        .expect("the plugin folder is in the repository");
    run_hook(d, goal).await
}

async fn run_hook(d: &Direct, goal: Arc<GoalHook>) -> InvocationResponse {
    let deps = Deps {
        hooks: Some(Hooks::new(vec![goal as Arc<dyn DirectHook>])),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await
}

async fn start(scenario: &'static str, extra_env: Vec<(&'static str, String)>) -> Direct {
    Direct::start(Opts {
        scenario,
        timeout_s: 90,
        engine_env: extra_env,
        ..Opts::default()
    })
    .await
}

fn phases(d: &Direct) -> Vec<String> {
    d.aws
        .progress("goal-loop")
        .into_iter()
        .map(|(_, v)| v["phase"].as_str().unwrap().to_string())
        .collect()
}

fn cancels_written(d: &Direct) -> usize {
    d.fake_engine_log()
        .lines()
        .filter(|l| *l == "notification session/cancel")
        .count()
}

fn brake_rule(d: &Direct) -> Value {
    d.aws
        .progress("goal-loop")
        .into_iter()
        .map(|(_, v)| v)
        .find(|v| v["phase"] == "brake")
        .expect("a brake event")["rule"]
        .clone()
}

// ---- a command is running: the polls are waiting ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn polling_a_running_command_for_longer_than_the_window_does_not_end_the_run() {
    let d = start("goal_command_polling", vec![]).await;
    let probe = Scripted::new(JOB);
    let t0 = Instant::now();
    let resp = run(&d, settings(&[]), probe.clone()).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}\n{:?}\n{:#?}",
        d.engine_trace(),
        phases(&d),
        d.log.lines()
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::ReportedDone,
        "{}",
        d.engine_trace()
    );
    // TRIGGER PRESENT: 20 polls over 12 seconds with no file changed until the end: more than the window of 10 s and more than its 3 calls
    assert!(s.tool_calls >= 20, "{}", s.tool_calls);
    assert!(t0.elapsed() > Duration::from_secs(10), "{:?}", t0.elapsed());
    assert!(
        probe.samples.load(Ordering::SeqCst) >= 3,
        "the evidence was sampled while the polls came"
    );
    assert!(
        probe.baselines.load(Ordering::SeqCst) >= 1,
        "a baseline is taken when the session begins"
    );
    assert!(
        !phases(&d).contains(&"brake".to_string()),
        "{:?}",
        phases(&d)
    );
    assert!(d.worktree().join("notes/result.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_polls_with_nothing_running_end_the_run_and_cline_writes_its_final_message() {
    let d = start("goal_command_polling_brake", vec![]).await;
    let probe = Scripted::new(Command::Idle);
    let resp = run(&d, settings(&[]), probe).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}\n{:?}\n{:#?}",
        d.engine_trace(),
        phases(&d),
        d.log.lines()
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::StoppedOther,
        "{}",
        d.engine_trace()
    );
    // the label is the brake's, as before: the last turn does not change what ended the run
    assert!(s.label.starts_with(BRAKE_LABEL), "{}", s.label);
    assert!(
        s.label.contains("in 10 seconds: no file changed and "),
        "{}",
        s.label
    );
    assert_eq!(brake_rule(&d), "round-no-progress");
    // TRIGGER PRESENT: the polls ran past the window with no file changed
    assert!(s.tool_calls >= 5, "{}", s.tool_calls);
    assert!(!d.worktree().join("notes/result.txt").exists());
    // the last turn: one prompt after the user's, the same session, and Cline's answer to it is the run's final message
    assert_eq!(
        d.prompts_received(),
        [PROMPT.to_string(), FINAL_PROMPT.to_string()],
        "{}",
        d.engine_trace()
    );
    assert_eq!(
        s.cline_message.as_deref(),
        Some(
            "I stopped while the full test run was still going. Nothing is left to do but read its result."
        )
    );
    let p = phases(&d);
    let at = |w: &str| p.iter().position(|x| x == w);
    assert!(at("brake") < at("final") && at("final").is_some(), "{p:?}");
    assert_eq!(
        cancels_written(&d),
        1,
        "only the first prompt was cancelled"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_polls_while_the_model_runs_its_own_commands_end_the_run_as_well() {
    // TRIGGER PRESENT: the processes use the CPU the whole time (the evidence is Active at every sample), but they are younger than the
    // calls that started them: a loop of commands the model runs itself and waits for is not a wait for a job. The run ends as it does
    // when nothing runs.
    let d = start("goal_command_polling_brake", vec![]).await;
    let probe = Scripted::new(OWN_COMMAND);
    let resp = run(&d, settings(&[]), probe.clone()).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}\n{:?}",
        d.engine_trace(),
        phases(&d)
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::StoppedOther,
        "{}",
        d.engine_trace()
    );
    assert!(s.label.starts_with(BRAKE_LABEL), "{}", s.label);
    assert!(
        s.label.contains("in 10 seconds: no file changed and "),
        "{}",
        s.label
    );
    assert_eq!(brake_rule(&d), "round-no-progress");
    assert!(
        probe.samples.load(Ordering::SeqCst) >= 3,
        "the evidence was sampled while the polls came"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sixty_calls_of_commands_the_model_runs_itself_end_the_run_at_thirty_calls_in_a_row() {
    // TRIGGER PRESENT: the scenario of `a_round_in_flight_that_ends_inside_the_drain_gives_the_last_turn` (60 varied calls inside one
    // follow-up round, no file changed, no text), and at every sample the processes that use the CPU began after the call that started
    // them. The calls rule ends the run as it does when nothing runs; the review's example was 40 such calls that ran to the grace.
    let d = start("goal_round_polling_ends", vec![]).await;
    let probe = Scripted::new(OWN_COMMAND);
    let resp = run(&d, settings(&[]), probe.clone()).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}\n{:?}",
        d.engine_trace(),
        phases(&d)
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::StoppedOther,
        "{}",
        d.engine_trace()
    );
    assert!(s.label.starts_with(BRAKE_LABEL), "{}", s.label);
    assert!(s.label.contains("tool calls in a row"), "{}", s.label);
    assert_eq!(brake_rule(&d), "round-no-progress");
    assert!(
        probe.samples.load(Ordering::SeqCst) >= 3,
        "the evidence was sampled while the calls came"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn evidence_that_could_not_be_read_does_not_protect_the_polls() {
    let d = start("goal_command_polling_brake", vec![]).await;
    let resp = run(&d, settings(&[]), Scripted::new(Command::Unknown)).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(d.summary().outcome, DirectOutcome::StoppedOther);
    assert_eq!(brake_rule(&d), "round-no-progress");
}

// ---- the last turn -----------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_user_can_switch_the_last_turn_off_for_a_run() {
    let mut d = start("goal_command_polling_brake", vec![]).await;
    with_members(
        &mut d,
        json!({"goal": {"enabled": true, "finalMessage": false}}),
    );
    let resp = run(&d, settings(&[]), Scripted::new(Command::Idle)).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedOther);
    assert!(s.label.starts_with(BRAKE_LABEL), "{}", s.label);
    assert_eq!(d.prompts_received(), [PROMPT.to_string()], "no last turn");
    assert!(!phases(&d).contains(&"final".to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stage_can_switch_the_last_turn_off_for_every_run() {
    let d = start("goal_command_polling_brake", vec![]).await;
    let resp = run(
        &d,
        settings(&[("AGENTD_GOAL_FINAL", "off")]),
        Scripted::new(Command::Idle),
    )
    .await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(d.prompts_received(), [PROMPT.to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_last_turn_that_does_not_end_is_cut_short_by_its_window_and_the_run_still_ends_with_the_brake()
 {
    let d = start("goal_command_polling_final_hangs", vec![]).await;
    let t0 = Instant::now();
    let resp = run(
        &d,
        settings(&[("AGENTD_GOAL_FINAL_S", "5")]),
        Scripted::new(Command::Idle),
    )
    .await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::StoppedOther,
        "{}",
        d.engine_trace()
    );
    assert!(s.label.starts_with(BRAKE_LABEL), "{}", s.label);
    // TRIGGER PRESENT: the engine was still working on the last turn (its scenario holds it for a minute) and ignored the cancel
    assert_eq!(d.prompts_received().len(), 2);
    assert_eq!(
        cancels_written(&d),
        2,
        "the first prompt, then the last turn"
    );
    assert!(t0.elapsed() < Duration::from_secs(60), "{:?}", t0.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_round_in_flight_that_ends_inside_the_drain_gives_the_last_turn() {
    // The engine's own follow-up round polls (60 varied calls, no file changed); the brake of 30 calls in a row fires inside it, the plugin's
    // state file is cleared, the round ends by itself 0.3 s after its last call, and the engine is idle: its last turn. (The existing polling
    // test of `direct_goal.rs` holds its round for 8 s against a drain of 3 s, so there the run ends at once.)
    let d = start("goal_round_polling_ends", vec![]).await;
    let resp = run(&d, settings(&[]), Scripted::new(Command::Idle)).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}\n{:?}",
        d.engine_trace(),
        phases(&d)
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::StoppedOther,
        "{}",
        d.engine_trace()
    );
    assert!(s.label.starts_with(BRAKE_LABEL), "{}", s.label);
    assert!(s.label.contains("tool calls in a row"), "{}", s.label);
    // TRIGGER PRESENT: the calls were made inside a follow-up round (the first run ended, the second ended by itself), not inside the first prompt
    assert_eq!(
        cancels_written(&d),
        0,
        "the round was not cancelled: it ignores the cancel and the module waited for it"
    );
    assert_eq!(
        d.prompts_received(),
        [PROMPT.to_string(), FINAL_PROMPT.to_string()],
        "{}",
        d.engine_trace()
    );
    assert_eq!(
        s.cline_message.as_deref(),
        Some("The polling round has finished. I changed nothing else.")
    );
}

// ---- the limits are settings -------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_limit_in_the_request_replaces_the_stage_setting_for_the_run() {
    // five polls in a row end the run, long before the window and the 30 calls of the stage
    let mut d = start("goal_command_polling_brake", vec![]).await;
    with_members(&mut d, json!({"goal": {"enabled": true, "roundCalls": 5}}));
    let t0 = Instant::now();
    let resp = run(&d, settings(&[]), Scripted::new(Command::Idle)).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d.summary();
    assert!(
        s.label.contains("tool calls in a row"),
        "the calls rule, not the window: {}",
        s.label
    );
    assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_limit_outside_its_range_ends_the_run_before_any_model_call() {
    let mut d = start("goal_command_polling_brake", vec![]).await;
    with_members(&mut d, json!({"goal": {"enabled": true, "roundCalls": 1}}));
    let resp = run(&d, settings(&[]), Scripted::new(Command::Idle)).await;
    assert_eq!(resp.outcome, TurnOutcome::Failed, "{resp:?}");
    let error = resp.error.clone().unwrap_or_default();
    assert!(
        error.contains("roundCalls") && error.contains("5 to 1000"),
        "{error}"
    );
    assert!(
        !error.contains(": 1"),
        "the value is never repeated: {error}"
    );
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed, "{}", s.label);
    assert!(d.prompts_received().is_empty(), "no model call");
}

// ---- the production path: the engine user, its process table, the baseline ---------------------------------------------------------------
//
// The tests above name the evidence. These choose it as the product does: from the engine user (`StartupCtx.engine_uid`) and the process
// table, over a `/proc` tree of the test's own (`GoalHook::for_engine_over_proc`; a real engine user needs a root agentd). A slip in the
// wiring (the user and group swapped, the probe refused, no baseline taken when the session begins, the evidence off by default) passes
// every test above.

/// The engine user and group of these runs: two different numbers, so that a swap of the two is seen.
const ENGINE_UID: u32 = 4242;
const ENGINE_GID: u32 = 4343;
/// The uptime of the fake machine, in ticks, when a test begins.
const BOOT: u64 = 5_000_000;

fn write_atomic(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

/// One process of the engine user in the fake `/proc`: its own session, `ticks` of CPU, started at `start` ticks of uptime.
fn write_proc(root: &Path, pid: u32, start: u64, ticks: u64) {
    let dir = root.join(pid.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    // pid (comm) state ppid pgrp session tty tpgid flags minflt cminflt majflt cmajflt utime stime cutime cstime prio nice threads itreal starttime ...
    let stat = format!(
        "{pid} (x) S 1 {pid} {pid} 0 -1 4194560 10 0 0 0 {ticks} 0 0 0 20 0 1 0 {start} 123456 77 18446744073709551615"
    );
    write_atomic(&dir.join("stat"), stat.as_bytes());
    let status = format!(
        "Name:\tx\nUid:\t{ENGINE_UID}\t{ENGINE_UID}\t{ENGINE_UID}\t{ENGINE_UID}\nGid:\t{ENGINE_GID}\t{ENGINE_GID}\t{ENGINE_GID}\t{ENGINE_GID}\n"
    );
    write_atomic(&dir.join("status"), status.as_bytes());
}

/// The engine user's process table while a run goes on: a helper of the engine (pid 800) that was there before the session began and
/// uses a whole CPU for ever, and, when `job` is set, a job of a session of its own (pid 900, which began a minute before: an earlier call
/// started it) that uses a whole CPU from the moment the engine receives its first prompt, which is after the session began.
struct FakeProc {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeProc {
    fn start(root: &Path, engine_log: PathBuf, job: bool) -> Self {
        std::fs::create_dir_all(root).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let root = root.to_path_buf();
        let table = move |up: u64, helper: u64, job: Option<u64>| {
            write_atomic(
                &root.join("uptime"),
                format!("{}.{:02} 1.00\n", up / 100, up % 100).as_bytes(),
            );
            write_proc(&root, 800, BOOT - 400_000, helper);
            if let Some(ticks) = job {
                write_proc(&root, 900, BOOT - 6_000, ticks);
            }
        };
        // the helper is there before the run begins
        table(BOOT, 0, None);
        let thread = std::thread::spawn(move || {
            let t0 = Instant::now();
            let mut began: Option<Instant> = None;
            while !flag.load(Ordering::Relaxed) {
                if job
                    && began.is_none()
                    && std::fs::read_to_string(&engine_log)
                        .is_ok_and(|l| l.contains("session/prompt"))
                {
                    began = Some(Instant::now());
                }
                let ticks = |since: Instant| {
                    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX) / 10
                };
                table(BOOT + ticks(t0), ticks(t0), began.map(ticks));
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for FakeProc {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn evidence_line(d: &Direct, what: &str) -> bool {
    let want = format!("direct turn goal loop: command evidence {what}");
    d.log.lines().contains(&want)
}

fn own_uid() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|l| l.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap()
}

async fn production_run(
    scenario: &'static str,
    uid: Option<(u32, u32)>,
    job: bool,
) -> (Direct, InvocationResponse, Instant) {
    let d = start(scenario, vec![]).await;
    let root = scratch("fake-proc").join("proc");
    let table = FakeProc::start(&root, d.agent_data().join("data/fake-acp.log"), job);
    let goal = GoalHook::for_engine_over_proc("cline", settings(&[]), uid, root)
        .expect("the plugin folder is in the repository");
    let t0 = Instant::now();
    let resp = run_hook(&d, goal).await;
    drop(table);
    (d, resp, t0)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_engine_users_process_table_shows_the_job_and_the_polls_do_not_end_the_run() {
    // The late scenario holds the first tool call for 600 ms after the prompt, so the job is in the table before the first sample: only a
    // baseline taken when the session begins (and not at the first sample) leaves it out of the baseline.
    let (d, resp, t0) = production_run(
        "goal_command_polling_late",
        Some((ENGINE_UID, ENGINE_GID)),
        true,
    )
    .await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}\n{:?}\n{:#?}",
        d.engine_trace(),
        phases(&d),
        d.log.lines()
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::ReportedDone,
        "{}",
        d.engine_trace()
    );
    assert!(
        evidence_line(&d, "on: the processes of user 4242"),
        "the user, not the group: {:#?}",
        d.log.lines()
    );
    // TRIGGER PRESENT: 20 polls over 12 seconds with no file changed until the end: more than the window of 10 s and more than its 3 calls
    assert!(s.tool_calls >= 20, "{}", s.tool_calls);
    assert!(t0.elapsed() > Duration::from_secs(10), "{:?}", t0.elapsed());
    assert!(
        !phases(&d).contains(&"brake".to_string()),
        "{:?}",
        phases(&d)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_helper_that_was_there_when_the_session_began_protects_nothing() {
    // the same polls, the process table is busy all the time, but with a helper of the engine from before the session: the baseline
    let (d, resp, _) = production_run(
        "goal_command_polling_brake",
        Some((ENGINE_UID, ENGINE_GID)),
        false,
    )
    .await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(evidence_line(&d, "on: the processes of user 4242"));
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedOther, "{}", s.label);
    assert!(s.label.starts_with(BRAKE_LABEL), "{}", s.label);
    assert_eq!(brake_rule(&d), "round-no-progress");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_whose_engine_has_no_user_of_its_own_has_no_evidence_whatever_the_process_table_shows()
 {
    let (d, resp, _) = production_run("goal_command_polling_brake", None, true).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        evidence_line(&d, "off: the engine has no user of its own"),
        "{:#?}",
        d.log.lines()
    );
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedOther, "{}", s.label);
    assert!(s.label.starts_with(BRAKE_LABEL), "{}", s.label);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_engine_user_that_is_agentds_own_is_refused_and_the_brake_works_as_it_did() {
    let (d, resp, _) = production_run(
        "goal_command_polling_brake",
        Some((own_uid(), ENGINE_GID)),
        true,
    )
    .await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        evidence_line(&d, "off: the engine user cannot be told from agentd's"),
        "{:#?}",
        d.log.lines()
    );
    assert_eq!(d.summary().outcome, DirectOutcome::StoppedOther);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stage_switch_turns_the_evidence_off_even_with_an_engine_user() {
    let d = start("goal_command_polling_brake", vec![]).await;
    let root = scratch("fake-proc-off").join("proc");
    let table = FakeProc::start(&root, d.agent_data().join("data/fake-acp.log"), true);
    let goal = GoalHook::for_engine_over_proc(
        "cline",
        settings(&[("AGENTD_GOAL_COMMAND", "off")]),
        Some((ENGINE_UID, ENGINE_GID)),
        root,
    )
    .expect("the plugin folder is in the repository");
    let resp = run_hook(&d, goal).await;
    drop(table);
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(evidence_line(&d, "off: switched off"));
    assert_eq!(d.summary().outcome, DirectOutcome::StoppedOther);
}

// ---- jobs whose CPU is not a steady load on one process ------------------------------------
//
// The run of 2026-10-23 was ended by the brake while its test run was still going: the job was a python process that ran the tests one after the
// other, each test in children that lived a fraction of a second, and it waited for timeouts for minutes at a time. The probe judged every
// 5 s interval alone and read most of them Idle. These whole runs use the same engine user and the same fake `/proc` as the tests above, with
// jobs that behave like that.

/// How the CPU of one process of the fake table grows with the time since the engine received its first prompt.
#[derive(Clone, Copy)]
enum Cpu {
    /// A whole CPU for the rest of the run.
    Always,
    /// A whole CPU for `busy_ms` at the start of every `period_ms`.
    Bursts { busy_ms: u64, period_ms: u64 },
}

impl Cpu {
    fn ticks(self, since_ms: u64) -> u64 {
        match self {
            Cpu::Always => since_ms / 10,
            Cpu::Bursts { busy_ms, period_ms } => {
                (since_ms / period_ms) * (busy_ms / 10) + (since_ms % period_ms).min(busy_ms) / 10
            }
        }
    }
}

/// Which counters of the process's `stat` the CPU is in: its own (`utime`) or those of the children it waited for (`cutime`).
#[derive(Clone, Copy)]
enum Counted {
    Own,
    Waited,
}

/// A job of its own session that began a minute before the first prompt (so an earlier call started it) and uses the CPU as `cpu` says.
#[derive(Clone, Copy)]
struct Job {
    pid: u32,
    cpu: Cpu,
    counted: Counted,
}

/// The commands the model runs itself: every `period_ms` a new process in a session of its own begins, uses a whole CPU for `life_ms` and ends.
#[derive(Clone, Copy)]
struct Foreground {
    period_ms: u64,
    life_ms: u64,
}

fn write_job(root: &Path, pid: u32, start: u64, utime: u64, cutime: u64) {
    let dir = root.join(pid.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let stat = format!(
        "{pid} (x) S 1 {pid} {pid} 0 -1 4194560 10 0 0 0 {utime} 0 {cutime} 0 20 0 1 0 {start} 123456 77 18446744073709551615"
    );
    write_atomic(&dir.join("stat"), stat.as_bytes());
    let status = format!(
        "Name:\tx\nUid:\t{ENGINE_UID}\t{ENGINE_UID}\t{ENGINE_UID}\t{ENGINE_UID}\nGid:\t{ENGINE_GID}\t{ENGINE_GID}\t{ENGINE_GID}\t{ENGINE_GID}\n"
    );
    write_atomic(&dir.join("status"), status.as_bytes());
}

/// The engine user's process table for a run: a helper of the engine from before the session (the baseline), the `jobs` from the moment the
/// engine receives its first prompt, and the `foreground` commands.
struct FakeJobs {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeJobs {
    fn start(
        root: &Path,
        engine_log: PathBuf,
        jobs: Vec<Job>,
        foreground: Option<Foreground>,
    ) -> Self {
        std::fs::create_dir_all(root).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let root = root.to_path_buf();
        let uptime = |root: &Path, up: u64| {
            write_atomic(
                &root.join("uptime"),
                format!("{}.{:02} 1.00\n", up / 100, up % 100).as_bytes(),
            );
        };
        // the helper is there before the run begins
        uptime(&root, BOOT);
        write_job(&root, 800, BOOT - 400_000, 0, 0);
        let thread = std::thread::spawn(move || {
            let t0 = Instant::now();
            let mut began: Option<Instant> = None;
            let mut live: Option<u32> = None;
            while !flag.load(Ordering::Relaxed) {
                if began.is_none()
                    && std::fs::read_to_string(&engine_log)
                        .is_ok_and(|l| l.contains("session/prompt"))
                {
                    began = Some(Instant::now());
                }
                let ms =
                    |since: Instant| u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX);
                uptime(&root, BOOT + ms(t0) / 10);
                write_job(&root, 800, BOOT - 400_000, ms(t0) / 10, 0);
                if let Some(b) = began {
                    for j in &jobs {
                        let ticks = j.cpu.ticks(ms(b));
                        let (own, waited) = match j.counted {
                            Counted::Own => (ticks, 0),
                            Counted::Waited => (0, ticks),
                        };
                        write_job(&root, j.pid, BOOT - 6_000, own, waited);
                    }
                    if let Some(f) = foreground {
                        let e = ms(b);
                        let pid = 1000 + u32::try_from(e / f.period_ms).unwrap();
                        let into = e % f.period_ms;
                        if into < f.life_ms {
                            if let Some(old) = live.filter(|l| *l != pid) {
                                let _ = std::fs::remove_dir_all(root.join(old.to_string()));
                            }
                            let born = BOOT + ms(t0) / 10 - into / 10;
                            write_job(&root, pid, born, into / 10, 0);
                            live = Some(pid);
                        } else if let Some(old) = live.take() {
                            let _ = std::fs::remove_dir_all(root.join(old.to_string()));
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for FakeJobs {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

async fn run_over_jobs(
    scenario: &'static str,
    jobs: Vec<Job>,
    foreground: Option<Foreground>,
    extra: &[(&str, &str)],
) -> (Direct, InvocationResponse, Instant) {
    let d = start(scenario, vec![]).await;
    let root = scratch("fake-jobs").join("proc");
    let table = FakeJobs::start(
        &root,
        d.agent_data().join("data/fake-acp.log"),
        jobs,
        foreground,
    );
    let goal = GoalHook::for_engine_over_proc(
        "cline",
        settings(extra),
        Some((ENGINE_UID, ENGINE_GID)),
        root,
    )
    .expect("the plugin folder is in the repository");
    let t0 = Instant::now();
    let resp = run_hook(&d, goal).await;
    drop(table);
    (d, resp, t0)
}

fn assert_polls_went_on(d: &Direct, resp: &InvocationResponse, t0: Instant) {
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}\n{:?}\n{:#?}",
        d.engine_trace(),
        phases(d),
        d.log.lines()
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::ReportedDone,
        "{}\n{}\n{:?}",
        s.label,
        d.engine_trace(),
        phases(d)
    );
    // TRIGGER PRESENT: 20 polls over 12 seconds with no file changed until the end: more than the window of 10 s and more than its 3 calls
    assert!(s.tool_calls >= 20, "{}", s.tool_calls);
    assert!(t0.elapsed() > Duration::from_secs(10), "{:?}", t0.elapsed());
    assert!(!phases(d).contains(&"brake".to_string()), "{:?}", phases(d));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_that_works_in_bursts_is_waited_for_and_the_polls_do_not_end_the_run() {
    // TRIGGER PRESENT: the job uses a whole CPU for 0.2 s in every 5 s (4 percent; the stage rate is 2 percent here). The samples taken in the
    // quiet stretches read Idle when each interval is judged alone: under a second of the run was credited, and the rest ended it at the
    // window. Two bursts are always inside the 10 s window: 40 ticks against the 20 that 2 percent of 10 s needs.
    let (d, resp, t0) = run_over_jobs(
        "goal_command_polling_late",
        vec![Job {
            pid: 900,
            cpu: Cpu::Bursts {
                busy_ms: 200,
                period_ms: 5000,
            },
            counted: Counted::Own,
        }],
        None,
        &[("AGENTD_GOAL_COMMAND_CPU_PCT", "2")],
    )
    .await;
    assert_polls_went_on(&d, &resp, t0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_whose_cpu_is_in_children_that_exit_is_waited_for_too() {
    // TRIGGER PRESENT: the job's own counters stay at zero; a whole CPU is used by children it waited for (as a test run does that starts a
    // process per test). No sample sees the children alive.
    let (d, resp, t0) = run_over_jobs(
        "goal_command_polling_late",
        vec![Job {
            pid: 900,
            cpu: Cpu::Always,
            counted: Counted::Waited,
        }],
        None,
        &[],
    )
    .await;
    assert_polls_went_on(&d, &resp, t0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_loop_of_short_commands_the_model_runs_itself_still_ends_the_run() {
    // TRIGGER PRESENT: every 0.6 s, with each poll, a process in a session of its own uses a whole CPU for 0.5 s and ends. The evidence is
    // Active at most samples, and the sessions are as young as the calls that started them, and gone a moment later: the window does not
    // lend their CPU to the next call. Nothing runs that is older than the calls.
    let (d, resp, _) = run_over_jobs(
        "goal_command_polling_brake",
        vec![],
        Some(Foreground {
            period_ms: 600,
            life_ms: 500,
        }),
        &[],
    )
    .await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(evidence_line(&d, "on: the processes of user 4242"));
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedOther, "{}", s.label);
    assert!(s.label.starts_with(BRAKE_LABEL), "{}", s.label);
    assert!(
        s.label.contains("in 10 seconds: no file changed and "),
        "{}",
        s.label
    );
    assert_eq!(brake_rule(&d), "round-no-progress");
    assert!(
        !phases(&d).contains(&"hold".to_string()),
        "no time was held back: {:?}",
        phases(&d)
    );
}

// ---- the run's events show the hold ---------------------------------------------------------------------------------------------------

/// Evidence that says a job is running for the first `runs[0]` samples, nothing for the next `runs[1]`, a job again for the next `runs[2]`, and so
/// on; after the last run nothing.
#[derive(Debug)]
struct Phased {
    runs: Vec<u32>,
    samples: AtomicU32,
}

impl Phased {
    fn new(runs: &[u32]) -> Arc<Self> {
        Arc::new(Self {
            runs: runs.to_vec(),
            samples: AtomicU32::new(0),
        })
    }
}

impl CommandProbe for Phased {
    fn rebaseline(&self) {}
    fn sample(&self, _now: Instant) -> Command {
        let mut left = self.samples.fetch_add(1, Ordering::SeqCst);
        for (i, len) in self.runs.iter().enumerate() {
            if left < *len {
                return if i % 2 == 0 { JOB } else { Command::Idle };
            }
            left -= len;
        }
        Command::Idle
    }
}

fn goal_events(d: &Direct) -> Vec<Value> {
    d.aws
        .progress("goal-loop")
        .into_iter()
        .map(|(_, v)| v)
        .collect()
}

/// The long scenario (40 polls over 24 seconds, which the 10 s window cannot take unless time is held back) over evidence that follows `runs`. The
/// brake ends the run: the job is seen for a few seconds only.
async fn run_phased(runs: &[u32]) -> (Direct, InvocationResponse) {
    let d = start("goal_command_polling_long", vec![]).await;
    let goal = GoalHook::for_engine_with_probe("cline", settings(&[]), Phased::new(runs))
        .expect("the plugin folder is in the repository");
    let resp = run_hook(&d, goal).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d.summary();
    // TRIGGER PRESENT: the job was seen for a few seconds only, the polls went on for twenty-four: the brake ended the run
    assert_eq!(s.outcome, DirectOutcome::StoppedOther, "{}", s.label);
    (d, resp)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pause_longer_than_the_release_time_ends_the_hold_and_the_next_burst_begins_another() {
    // Six sample intervals (the release time of this run: 300 ms) without credit end a hold. The pause here is 10 samples, at least 500 ms, so
    // it does; a release time of a second or more would not end it, and the two bursts would be one hold.
    let (d, _) = run_phased(&[20, 10, 20]).await;
    let seen: Vec<String> = phases(&d)
        .into_iter()
        .filter(|p| matches!(p.as_str(), "hold" | "release" | "brake"))
        .collect();
    assert_eq!(
        seen,
        ["hold", "release", "hold", "release", "brake"],
        "{:?}",
        phases(&d)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_run_s_events_say_when_the_brake_was_held_back_when_it_was_released_and_for_how_long() {
    // The long scenario: 40 polls over 24 seconds. (The 20 polls of the others last 12 seconds, and the seconds before the first sample are
    // credited to the first Active sample, so the brake would not reach its 10 s window.) The job is Active for the first 60 samples, 50 ms
    // apart at the least, so at least 2.95 s are credited; the polls then go on for about 20 s, which the 10 s window cannot take.
    let (d, _) = run_phased(&[60]).await;
    let events = goal_events(&d);
    let at = |phase: &str| events.iter().position(|e| e["phase"] == phase);
    let (hold, release, brake) = (at("hold"), at("release"), at("brake"));
    assert!(
        hold.is_some() && release.is_some() && brake.is_some(),
        "{:?}",
        phases(&d)
    );
    assert!(hold < release && release < brake, "{:?}", phases(&d));
    let hold = &events[hold.unwrap()];
    let text = hold["text"].as_str().unwrap();
    assert!(
        text.contains("held back") && text.contains("began"),
        "what it is and why: {text}"
    );
    // The hold is also granted by the rate over the window, so the command may be waiting at that moment: the text says what the harness saw.
    assert!(
        text.contains("has used the CPU lately") && !text.contains("is using the CPU"),
        "the evidence is the CPU used lately, not the CPU used now: {text}"
    );
    assert_eq!(hold["ageS"], 3600, "the age of the command, in seconds");
    let released = &events[release.unwrap()];
    assert!(
        released["text"].as_str().unwrap().contains("counts"),
        "{released}"
    );
    let released_after = released["heldS"]
        .as_u64()
        .expect("the release says how long the brake was held back");
    assert!(
        released_after >= 2,
        "60 samples at least 50 ms apart were credited: {released}"
    );
    let brake = &events[brake.unwrap()];
    let held = brake["heldS"]
        .as_u64()
        .expect("the brake says how long it was held back");
    assert_eq!(brake["rule"], "round-no-progress", "{brake}");
    assert!(
        (2..=10).contains(&held),
        "the seconds of the job and the start-up before the first sample were held back, and fewer than the 10 s window: {brake}"
    );
}
