//! Two cases beside the cancel, at the first stop of a run. Each test shows what a user of the run sees.
//!
//! Is the user's cancel that comes up while the modules are asked at the first stop, in the poll in which the goal module
//! says that the engine still works by itself and the message module votes the user's message as the next prompt: the prompt is
//! dropped, the engine is told once and the run ends as the user's stop.
//!
//! Case 1: the same poll, and the run's time limit comes up in it where has the cancel. Before this item the voted prompt was
//! dropped and nothing was kept of what the poll heard: nothing told the engine, and the run ended with the outcome of the last
//! answered prompt, though the engine was still working when the time ran out. Now the run is held as for the wait the poll
//! heard, and that hold ends it for the time limit at once: the engine is told once and the run reads as a run that reached its
//! time limit.
//!
//! Case 2: the user's cancel is up before the modules are asked at the first stop: the answer of the first prompt has been read,
//! and no module has voted. The driver then asks no module, and the run ends with the outcome of the prompt the engine answered
//! before the cancel was read. Two tests show what a user sees: an engine that ended its turn and does nothing more, and an engine
//! that goes on by itself after its answer (the goal loop's follow-up rounds). Nothing is changed for this case.
//!
//! The real message module and the real goal module run in the real driver against the engine simulator: for case 2 the scenario
//! `goal_message_in_round` of `b857_now_in_follow_up_rounds.rs`, for case 1 the scenario `goal_silent_round`, whose one follow-up
//! round lasts 30 s, so the engine is still in that round when the run's time limit of 20 s passes. The rule of case 1 is also
//! held on a stopped clock, with scripted modules, by the two `b980_` tests of `direct_hold.rs`.
#![allow(clippy::unwrap_used)]
#![cfg(target_os = "linux")]

mod common;
mod direct_messages_world;
mod direct_support;

use agentd::BoxFuture;
use agentd::direct::goal::GoalHook;
use agentd::direct::goal::settings::GoalSettings;
use agentd::direct::hooks::{DirectHook, Event, TurnEnd, TurnVote};
use agentd::direct::messages::{MemoryStore, MessagesHook, Mode};
use common::plugin_folder;
use direct_messages_world::*;
use direct_support::{Direct, Opts};
use harness_protocol::direct::DirectOutcome;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

const SCENARIO: &str = "goal_message_in_round";
const TEXT: &str = "Please also add a README line.";

/// The scenario of case 1: after the first prompt the engine goes on by itself with one follow-up round that is silent for 30 s.
const WORKS_ON: &str = "goal_silent_round";

/// The run's time limit in the test of case 1, and the whole length of that test. The session has to start and the first prompt
/// has to be answered inside it, also on a crowded host (alone that takes about 2 s): when they are not, the test says that it
/// proves nothing. It is shorter than the engine's round, so the engine is at work when the time runs out.
const TIME_LIMIT_S: u32 = 20;

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

/// Whether a follow-up round of the engine's own loop had started when the run ended.
fn a_round_started(w: &World) -> bool {
    w.log()
        .lines()
        .any(|l| l.starts_with("round ") && l.ends_with(" starts"))
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

// ---- case 1: the run's time limit comes up in the first poll of the first stop -------------------------------------------------

/// Hands the pending message of the store to the message module inside the first poll of the first stop, before the message
/// module is asked (the feeder of `b857_now_in_follow_up_rounds.rs`, at its first ask).
struct Feeder {
    hook: Arc<MessagesHook>,
    store: Arc<MemoryStore>,
    asks: AtomicU32,
    fed: AtomicBool,
    fed_at_ask: AtomicU32,
}

impl DirectHook for Feeder {
    fn name(&self) -> &'static str {
        "feeder"
    }

    fn on_turn_end<'a>(&'a self, end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move {
            if end.n != 1 || self.fed.load(Ordering::SeqCst) {
                return TurnVote::Done;
            }
            let ask = self.asks.fetch_add(1, Ordering::SeqCst) + 1;
            // The control plane accepts the message as soon as the session exists; the first ask waits for it (a moment).
            let until = std::time::Instant::now() + Duration::from_secs(5);
            while ask == 1 && self.store.handout().is_empty() && std::time::Instant::now() < until {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if ask == 1 && !self.store.handout().is_empty() {
                self.fed.store(true, Ordering::SeqCst);
                self.fed_at_ask.store(ask, Ordering::SeqCst);
                self.hook
                    .on_controls(&notice_controls(&self.store.handout()));
            }
            TurnVote::Done
        })
    }
}

/// Asked after the message module. In the poll in which the feeder handed the message out, it notes what the poll told it about
/// the engine and stays in the poll until the run's time limit has passed: the time limit comes up inside that poll, after every
/// module has voted.
struct TimeLimitInPoll {
    feeder: Arc<Feeder>,
    done: AtomicBool,
    engine_working: AtomicBool,
    at_ask: AtomicU32,
}

impl DirectHook for TimeLimitInPoll {
    fn name(&self) -> &'static str {
        "time-limit-in-poll"
    }

    fn on_turn_end<'a>(&'a self, end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move {
            if end.n == 1
                && self.feeder.fed.load(Ordering::SeqCst)
                && !self.done.swap(true, Ordering::SeqCst)
            {
                self.engine_working
                    .store(end.engine_working, Ordering::SeqCst);
                self.at_ask
                    .store(self.feeder.asks.load(Ordering::SeqCst), Ordering::SeqCst);
                // What was left of the run's time when the poll began, and a moment more.
                tokio::time::sleep(end.time_left + Duration::from_millis(50)).await;
            }
            TurnVote::Done
        })
    }
}

/// Case 1. The poll, with the run's time limit where has the user's cancel: the goal module says that the engine
/// still works by itself, the message module votes the user's message as the next prompt, and the time limit passes before the
/// poll ends. The prompt is not sent after the time limit. The engine is told once, and the run reads as a run that reached its
/// time limit: before, nothing told the engine and the run kept the outcome of the last answered prompt.
#[tokio::test(flavor = "multi_thread")]
async fn b980_the_time_limit_in_the_first_poll_of_the_first_stop_ends_the_run_as_its_time_limit() {
    let d = Direct::start(Opts {
        scenario: WORKS_ON,
        timeout_s: TIME_LIMIT_S,
        ..Opts::default()
    })
    .await;
    let w = World::around(d, MemoryStore::new(), quick(), &pid_dir());
    let goal = GoalHook::for_engine("cline", goal_settings())
        .expect("the plugin folder is in the repository");
    let feeder = Arc::new(Feeder {
        hook: w.hook.clone(),
        store: w.store.clone(),
        asks: AtomicU32::new(0),
        fed: AtomicBool::new(false),
        fed_at_ask: AtomicU32::new(0),
    });
    let late = Arc::new(TimeLimitInPoll {
        feeder: feeder.clone(),
        done: AtomicBool::new(false),
        engine_working: AtomicBool::new(false),
        at_ask: AtomicU32::new(0),
    });
    let run = w.run_around(
        vec![
            goal as Arc<dyn DirectHook>,
            feeder.clone() as Arc<dyn DirectHook>,
        ],
        vec![late.clone() as Arc<dyn DirectHook>],
    );
    w.send("m1", TEXT, Mode::Now).await;
    let _ = run.await.unwrap();
    let agentd_log = w.d.log.lines();
    let s = w.d.summary();
    // What a user sees.
    println!(
        "case 1: outcome {:?}, label {:?}, detail {:?}, session/cancel written {} time(s), a follow-up round had started: {}",
        s.outcome,
        s.label,
        s.detail,
        cancels_written(&w),
        a_round_started(&w)
    );
    // TRIGGER PRESENT: the message was claimed at the first ask of the first stop, a module had said in that poll that the engine
    // works by itself, the time limit passed in that poll, and the claimed message did not go out as a prompt after it.
    assert_eq!(
        feeder.fed_at_ask.load(Ordering::SeqCst),
        1,
        "the message was not handed out at the first ask: the test proves nothing\n{agentd_log:?}"
    );
    assert_eq!(
        late.at_ask.load(Ordering::SeqCst),
        1,
        "the time limit did not pass in the first poll: the test proves nothing\n{agentd_log:?}"
    );
    assert!(
        late.engine_working.load(Ordering::SeqCst),
        "no module said in that poll that the engine works by itself: the test proves nothing\n{agentd_log:?}"
    );
    assert_eq!(
        w.hook.stats().claims,
        1,
        "the message module did not claim the message in that poll: {agentd_log:?}"
    );
    assert_eq!(
        w.d.prompts_received(),
        [direct_support::PROMPT.to_string()],
        "no prompt goes out after the run's time limit"
    );
    // What must hold: the engine was working when the time ran out, so the run reads as its time limit and the engine is told.
    assert_eq!(
        s.outcome,
        DirectOutcome::StoppedTimeout,
        "the run kept the outcome of the last answered prompt (label {:?})\n{}\n{agentd_log:?}",
        s.label,
        w.d.engine_trace()
    );
    assert_eq!(s.label, "Cline stopped: the run reached its time limit");
    assert_eq!(
        cancels_written(&w),
        1,
        "the cancel is written to the engine once\n{}",
        w.log()
    );
    // The engine was inside its own round when the cancel of the time limit reached it: the time limit ended work in flight.
    assert_eq!(
        in_a_round_at_the_cancel(&w),
        Some(true),
        "the engine was not at work when the time ran out: the test proves nothing\n{}",
        w.log()
    );
    assert_eq!(
        engine_sessions(&w),
        1,
        "no fresh engine session after the time limit: {agentd_log:?}"
    );
    assert_eq!(w.hook.stats().tripwires, 0, "{agentd_log:?}");
}

// ---- case 2: the user's cancel is up before the modules are asked at the first stop -------------------------------------------

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

/// Case 2, an engine that ended its turn and does nothing more. The user's cancel is up when the answer has been read. The run
/// ends with the outcome of that answer: the engine had ended its turn before the cancel was read.
#[tokio::test(flavor = "multi_thread")]
async fn b980_a_cancel_after_the_answer_of_an_engine_that_ended_its_turn_keeps_the_engines_outcome()
{
    let w = World::start(&directive(r#"{"work_ms": 100}"#), &[], quick(), &pid_dir()).await;
    let at = CancelAtTheAnswer::new(w.canceller.clone(), None);
    let run = w.run_after(vec![at.clone() as Arc<dyn DirectHook>]);
    let _ = run.await.unwrap();
    let agentd_log = w.d.log.lines();
    let s = w.d.summary();
    println!(
        "case 2, an engine that ended its turn: outcome {:?}, label {:?}, detail {:?}, session/cancel written {} time(s), modules asked {} time(s)",
        s.outcome,
        s.label,
        s.detail,
        cancels_written(&w),
        at.asked.load(Ordering::SeqCst)
    );
    // TRIGGER PRESENT: the cancel was delivered when the first prompt had been answered with the engine's own end of turn.
    assert_eq!(
        at.stop_reason.lock().unwrap().as_deref(),
        Some("end_turn"),
        "the cancel was not delivered at the answer of the first prompt: the test proves nothing\n{agentd_log:?}"
    );
    assert_eq!(w.d.prompts_received().len(), 1, "{agentd_log:?}");
    // What a user sees: the engine's own end of its turn. No module was asked, and nothing was written to the engine.
    assert_eq!(s.outcome, DirectOutcome::ReportedDone, "{agentd_log:?}");
    assert_eq!(s.label, "Cline reported done");
    assert_eq!(at.asked.load(Ordering::SeqCst), 0, "{agentd_log:?}");
    assert_eq!(cancels_written(&w), 0, "{}", w.log());
}

/// Case 2, an engine that goes on by itself after its answer (the goal loop's follow-up rounds). The user's cancel is up when the
/// answer has been read, and no module is asked about the stop.
#[tokio::test(flavor = "multi_thread")]
async fn b980_a_cancel_after_the_answer_of_an_engine_that_goes_on_by_itself_shows_what_a_user_sees()
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
    let _ = run.await.unwrap();
    let agentd_log = w.d.log.lines();
    let s = w.d.summary();
    println!(
        "case 2, an engine that goes on by itself: outcome {:?}, label {:?}, detail {:?}, session/cancel written {} time(s), modules asked {} time(s), a follow-up round had started: {}",
        s.outcome,
        s.label,
        s.detail,
        cancels_written(&w),
        at.asked.load(Ordering::SeqCst),
        a_round_started(&w)
    );
    // TRIGGER PRESENT: the cancel was delivered when the first prompt had been answered with the engine's own end of turn, and
    // the engine was at work in a round of its own when the run ended.
    assert_eq!(
        at.stop_reason.lock().unwrap().as_deref(),
        Some("end_turn"),
        "the cancel was not delivered at the answer of the first prompt: the test proves nothing\n{agentd_log:?}"
    );
    assert_eq!(w.d.prompts_received().len(), 1, "{agentd_log:?}");
    assert!(
        a_round_started(&w),
        "the engine did not go on by itself: the test proves nothing\n{}",
        w.log()
    );
    // What a user sees, as it is: no module is asked once the cancel is up, so the driver has not heard that the engine goes on
    // by itself. The run keeps the outcome of the answer the engine gave before the cancel was read, and the engine is not told.
    assert_eq!(at.asked.load(Ordering::SeqCst), 0, "{agentd_log:?}");
    assert_eq!(s.outcome, DirectOutcome::ReportedDone, "{agentd_log:?}");
    assert_eq!(s.label, "Cline reported done");
    assert_eq!(cancels_written(&w), 0, "{}", w.log());
}
