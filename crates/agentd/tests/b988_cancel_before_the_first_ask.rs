//! The user's cancel is up when the answer of the first prompt has been read, before the modules are asked at the first stop.
//!
//! Before this item the driver asked no module once the cancel was up. At the first stop it had held the run at no stop yet, so it
//! had heard nothing about the engine: the run ended with the outcome of the answered prompt, nothing told the engine, and no
//! module heard of the cancel, also when the engine was at work in a follow-up round of its own loop. Repaired the same
//! symptom for a cancel that comes up while the modules are asked. The time between the answer and the ask is not small: the
//! driver measures the work tree and writes the digest of the stop in it.
//!
//! Now the driver asks at the first stop of a run, when the user's cancel is up, whether a round of the engine's own loop is open
//! (`DirectHook::round_open`; the goal module reads the observer's rounds log for it and changes nothing). If one is, the engine is
//! told once and the run ends as the user's stop, as it does after a hold. If none is, the run ends with the outcome of the
//! answered prompt, as before. No vote is asked for once the cancel is up: a module that would only keep the run open (the goal
//! module while it waits to see whether the engine claimed the goal) does not make the stop the user's.
//!
//! The real goal module and the real message module run in the real driver against the engine simulator: the scenario
//! `goal_message_in_round` of `b857_now_in_follow_up_rounds.rs` for an engine that goes on by itself, `goal_not_claimed` for an
//! engine that never claims the goal. The rule is also held on a stopped clock, with scripted modules, by the five `b988_` tests
//! of `direct_hold.rs`, and the look at the rounds log by the six `b988_` tests of `direct_goal_files.rs`.
#![allow(clippy::unwrap_used)]
#![cfg(target_os = "linux")]

mod common;
mod direct_messages_world;
mod direct_support;

use agentd::BoxFuture;
use agentd::direct::goal::GoalHook;
use agentd::direct::goal::settings::GoalSettings;
use agentd::direct::hooks::{DirectHook, Event, TurnEnd, TurnVote};
use agentd::direct::messages::{Mode, State};
use common::plugin_folder;
use direct_messages_world::*;
use direct_support::{Direct, RUN, checkout_of_copy};
use harness_protocol::direct::{DirectOutcome, s3 as keys};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

const SCENARIO: &str = "goal_message_in_round";
const TEXT: &str = "Please also add a README line.";

/// The goal module's settings of `b857_now_in_follow_up_rounds.rs`: the plugin folder of the repository and times short enough to
/// run in seconds.
fn goal_settings() -> GoalSettings {
    let m: BTreeMap<String, String> = [
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
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    GoalSettings::from_lookup(&|k| m.get(k).cloned())
}

/// How often the engine received `session/cancel`.
fn cancels_written(w: &World) -> usize {
    w.log()
        .lines()
        .filter(|l| *l == "notification session/cancel")
        .count()
}

/// How many engine sessions the run had.
fn engine_sessions(w: &World) -> usize {
    w.log()
        .lines()
        .filter_map(|l| l.strip_prefix("in "))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["method"] == "session/new")
        .count()
}

/// Whether the engine was inside a round of its own loop when `session/cancel` reached it: a round had started and had not ended.
/// `None` when no cancel reached the engine.
fn in_a_round_at_the_cancel(w: &World) -> Option<bool> {
    let mut in_round = false;
    for line in w.log().lines() {
        if line.starts_with("round ") && line.ends_with(" starts") {
            in_round = true;
        } else if line.starts_with("round ") && line.contains(" ends") {
            in_round = false;
        } else if line == "notification session/cancel" {
            return Some(in_round);
        }
    }
    None
}

/// `snapshot.json` as the bucket holds it now: the record of the saved copy of the work.
fn saved_copy(d: &Direct) -> Option<serde_json::Value> {
    d.aws
        .get(&keys::snapshot_json_key(RUN))
        .map(|b| serde_json::from_slice(&b).unwrap())
}

/// Delivers the user's cancel when the answer of the first prompt has been read, which is before the modules are asked at the
/// first stop, and counts how often it is asked afterwards.
struct CancelAtTheAnswer {
    canceller: Arc<Canceller>,
    done: AtomicBool,
    stop_reason: std::sync::Mutex<Option<String>>,
    asked: AtomicU32,
    /// The engine's own log, for an engine that goes on by itself: the cancel waits until a round of the engine's own loop has
    /// started, so the engine is at work when the cancel comes up. The driver stands in this event meanwhile and asks no module.
    engine_log: Option<std::path::PathBuf>,
}

impl CancelAtTheAnswer {
    fn new(canceller: Arc<Canceller>, engine_log: Option<std::path::PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            canceller,
            done: AtomicBool::new(false),
            stop_reason: std::sync::Mutex::new(None),
            asked: AtomicU32::new(0),
            engine_log,
        })
    }
}

impl DirectHook for CancelAtTheAnswer {
    fn name(&self) -> &'static str {
        "cancel-at-the-answer"
    }

    fn on_event(&self, event: &Event<'_>) {
        if let Event::PromptReplied(r) = event
            && r.no == 1
            && !self.done.swap(true, Ordering::SeqCst)
        {
            *self.stop_reason.lock().unwrap() = r.stop_reason.map(str::to_string);
            if let Some(log) = &self.engine_log {
                // The cap is long on purpose: the wait ends as soon as the round has started.
                let until = std::time::Instant::now() + Duration::from_secs(60);
                let started = || {
                    std::fs::read_to_string(log)
                        .unwrap_or_default()
                        .lines()
                        .any(|l| l.starts_with("round ") && l.ends_with(" starts"))
                };
                while !started() && std::time::Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            self.canceller.cancel();
        }
    }

    fn on_turn_end<'a>(&'a self, _end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { TurnVote::Done })
    }

    fn on_seal<'a>(&'a self, _end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { TurnVote::Done })
    }
}

/// An engine that goes on by itself after its first answer (the goal loop's follow-up rounds). The user's cancel is up when the
/// answer has been read, while a round of the engine's own loop is open. The run ends as the user's stop, the engine is told
/// once, and the copy of the end holds the work.
#[tokio::test(flavor = "multi_thread")]
async fn b988_a_cancel_before_the_first_ask_while_the_engine_works_by_itself_is_the_users_stop_and_the_engine_is_told()
 {
    let w = World::start_scenario(SCENARIO, quick(), &pid_dir()).await;
    let goal = GoalHook::for_engine("cline", goal_settings())
        .expect("the plugin folder is in the repository");
    let at = CancelAtTheAnswer::new(
        w.canceller.clone(),
        Some(w.d.agent_data().join("data/fake-acp.log")),
    );
    let run = w.run_after(vec![
        goal as Arc<dyn DirectHook>,
        at.clone() as Arc<dyn DirectHook>,
    ]);
    // A message of the user that waits for the turn's end: it must be neither claimed nor sent once the cancel is up.
    w.pump();
    w.send("m1", TEXT, Mode::Next).await;
    let _ = run.await.unwrap();
    let agentd_log = w.d.log.lines();
    let s = w.d.summary();
    println!(
        "an engine that goes on by itself: outcome {:?}, label {:?}, detail {:?}, session/cancel written {} time(s), in a round at the cancel: {:?}, modules asked {} time(s), claims {}, message {:?}",
        s.outcome,
        s.label,
        s.detail,
        cancels_written(&w),
        in_a_round_at_the_cancel(&w),
        at.asked.load(Ordering::SeqCst),
        w.hook.stats().claims,
        w.store.state("m1")
    );
    // TRIGGER PRESENT: the cancel was delivered when the first prompt had been answered with the engine's own end of turn, and no
    // other prompt went out.
    assert_eq!(
        at.stop_reason.lock().unwrap().as_deref(),
        Some("end_turn"),
        "the cancel was not delivered at the answer of the first prompt: the test proves nothing\n{agentd_log:?}"
    );
    assert_eq!(w.d.prompts_received().len(), 1, "{agentd_log:?}");
    // What must hold: the engine was at work, so the stop is the user's and the engine is told once.
    assert_eq!(
        s.outcome,
        DirectOutcome::StoppedByUser,
        "the run kept the outcome of the answered prompt though the engine was at work (label {:?})\n{}\n{agentd_log:?}",
        s.label,
        w.d.engine_trace()
    );
    assert_eq!(s.label, "Cline stopped: you stopped the run");
    assert_eq!(
        cancels_written(&w),
        1,
        "the cancel is written to the engine once\n{}",
        w.log()
    );
    // The engine was inside a round of its own when the cancel reached it: the user's stop ended work in flight.
    assert_eq!(
        in_a_round_at_the_cancel(&w),
        Some(true),
        "the engine was not at work when the cancel reached it: the test proves nothing\n{}",
        w.log()
    );
    assert_eq!(
        engine_sessions(&w),
        1,
        "no fresh engine session after the cancel: {agentd_log:?}"
    );
    assert_eq!(w.hook.stats().tripwires, 0, "{agentd_log:?}");
    assert_eq!(
        at.asked.load(Ordering::SeqCst),
        0,
        "no vote is asked for once the user's cancel is up: {agentd_log:?}"
    );
    assert_eq!(w.hook.stats().claims, 0, "{agentd_log:?}");
    assert!(
        !matches!(w.store.state("m1"), Some(State::Delivered { .. })),
        "{:?}",
        w.store.state("m1")
    );
    // Root invariant 9: the copy of the end is stored whatever the outcome reads, and it holds what the engine wrote.
    let copy = saved_copy(&w.d).expect("the end of the run saved a copy of the work");
    assert_eq!(copy["final"], true, "{copy}");
    let clone = checkout_of_copy(
        &w.d.aws,
        &w.d.dir,
        &keys::snapshot_bundle_key(RUN),
        "b988-copy",
    );
    assert!(
        std::fs::read_to_string(clone.join("src/calc.py"))
            .unwrap()
            .contains("percent_change"),
        "the copy of the end does not hold the engine's work"
    );
}

/// An engine that ended its turn and does nothing more, and a message of the user that waits for the turn's end. The user's
/// cancel is up when the answer has been read. No module says that the engine works by itself: the run ends with the outcome of
/// that answer, as before, nothing is written to the engine, and the message is neither claimed nor sent.
#[tokio::test(flavor = "multi_thread")]
async fn b988_a_cancel_before_the_first_ask_of_an_engine_that_is_not_at_work_keeps_the_engines_outcome()
 {
    let w = World::start(&directive(r#"{"work_ms": 800}"#), &[], quick(), &pid_dir()).await;
    let at = CancelAtTheAnswer::new(w.canceller.clone(), None);
    let run = w.run_after(vec![at.clone() as Arc<dyn DirectHook>]);
    w.pump();
    w.send("m1", TEXT, Mode::Next).await;
    let _ = run.await.unwrap();
    let agentd_log = w.d.log.lines();
    let s = w.d.summary();
    println!(
        "an engine that is not at work: outcome {:?}, label {:?}, session/cancel written {} time(s), modules asked {} time(s), claims {}, message {:?}",
        s.outcome,
        s.label,
        cancels_written(&w),
        at.asked.load(Ordering::SeqCst),
        w.hook.stats().claims,
        w.store.state("m1")
    );
    // TRIGGER PRESENT: the cancel was delivered when the first prompt had been answered with the engine's own end of turn.
    assert_eq!(
        at.stop_reason.lock().unwrap().as_deref(),
        Some("end_turn"),
        "the cancel was not delivered at the answer of the first prompt: the test proves nothing\n{agentd_log:?}"
    );
    // What a user sees: the engine's own end of its turn, and nothing was written to the engine.
    assert_eq!(s.outcome, DirectOutcome::ReportedDone, "{agentd_log:?}");
    assert_eq!(s.label, "Cline reported done");
    assert_eq!(cancels_written(&w), 0, "{}", w.log());
    // The user's message: nothing is claimed once the cancel is up, and nothing goes out after it.
    assert_eq!(w.hook.stats().claims, 0, "{agentd_log:?}");
    assert_eq!(
        w.d.prompts_received().len(),
        1,
        "no prompt goes out after the user's cancel: {agentd_log:?}"
    );
    assert!(
        !matches!(w.store.state("m1"), Some(State::Delivered { .. })),
        "{:?}",
        w.store.state("m1")
    );
    assert_eq!(w.hook.stats().tripwires, 0, "{agentd_log:?}");
}

/// An engine that never claims the goal and does nothing after its answer. The goal module would keep the run open if it were
/// asked for a vote (it waits to see whether the engine claims the goal), but no round of the engine's own is open. The user's
/// cancel is up when the answer has been read: the run ends with the outcome of that answer, and nothing is written to the engine.
#[tokio::test(flavor = "multi_thread")]
async fn b988_a_cancel_before_the_first_ask_of_an_engine_that_never_claimed_the_goal_keeps_the_engines_outcome()
 {
    let w = World::start_scenario("goal_not_claimed", quick(), &pid_dir()).await;
    let goal = GoalHook::for_engine("cline", goal_settings())
        .expect("the plugin folder is in the repository");
    let at = CancelAtTheAnswer::new(w.canceller.clone(), None);
    let run = w.run_after(vec![
        goal as Arc<dyn DirectHook>,
        at.clone() as Arc<dyn DirectHook>,
    ]);
    let _ = run.await.unwrap();
    let agentd_log = w.d.log.lines();
    let s = w.d.summary();
    println!(
        "an engine that never claimed the goal: outcome {:?}, label {:?}, session/cancel written {} time(s), modules asked {} time(s)",
        s.outcome,
        s.label,
        cancels_written(&w),
        at.asked.load(Ordering::SeqCst)
    );
    // TRIGGER PRESENT: the cancel was delivered when the first prompt had been answered with the engine's own end of turn, and
    // the engine started no round of its own.
    assert_eq!(
        at.stop_reason.lock().unwrap().as_deref(),
        Some("end_turn"),
        "the cancel was not delivered at the answer of the first prompt: the test proves nothing\n{agentd_log:?}"
    );
    assert_eq!(w.d.prompts_received().len(), 1, "{agentd_log:?}");
    assert!(
        !w.log()
            .lines()
            .any(|l| l.starts_with("round ") && l.ends_with(" starts")),
        "the engine went on by itself: the test proves nothing\n{}",
        w.log()
    );
    assert_eq!(s.outcome, DirectOutcome::ReportedDone, "{agentd_log:?}");
    assert_eq!(s.label, "Cline reported done");
    assert_eq!(cancels_written(&w), 0, "{}", w.log());
    assert_eq!(at.asked.load(Ordering::SeqCst), 0, "{agentd_log:?}");
}
