//! Defect 1: a user's message to a run whose engine works through the goal loop's follow-up rounds. The REAL message module and the
//! REAL goal module run together in the real driver against the engine simulator (`goal.json` of the scenario `goal_message_in_round`: after
//! the first prompt the engine goes on by itself with follow-up rounds, ignores `session/cancel`, and answers a prompt that arrives during a
//! round at once and queues it behind the round; its first follow-up round is silent for three seconds, so a prompt that arrives then is
//! answered with no update of the round in between).
//!
//! No harness prompt is open in that state, so a `/now` message starts no interrupt: it goes out as the next prompt and the engine queues
//! it. Two things must hold. The user is told that the turn was not cancelled, in the words of every other interrupt that could not
//! cancel: "could not interrupt; delivered after this turn". And the engine that queued the message as designed is not replaced: the
//! answer of the queued prompt (`end_turn`, no usage, at once, no update since the prompt) has the shape the message module's tripwire
//! reads as a fault, and a replaced engine is a fresh session that never reads the message.
//!
//! The message reaches the module at a chosen moment: [`Feeder`] is a module registered between the goal module and the message module,
//! and hands the pending message out inside the poll of a turn end, just before the message module is asked. So the two orders the driver
//! can ask in are both run, each time the same way: the message is claimed at the FIRST ask of a stop (the goal module voted `Wait` a
//! moment before, in the same poll), or at a LATER ask of the same stop (the driver held the run and asks again).
#![allow(clippy::unwrap_used)]
#![cfg(target_os = "linux")]

mod common;
mod direct_messages_world;
mod direct_support;

use agentd::BoxFuture;
use agentd::direct::goal::GoalHook;
use agentd::direct::goal::settings::GoalSettings;
use agentd::direct::hooks::{DirectHook, TurnEnd, TurnVote};
use agentd::direct::messages::{MemoryStore, MessagesHook, Mode, State};
use common::plugin_folder;
use direct_messages_world::*;
use harness_protocol::direct::DirectOutcome;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const SCENARIO: &str = "goal_message_in_round";
/// The same first prompt, and one follow-up round that is silent for three seconds and then completes the goal: the engine's loop ends
/// with the round a message arrives in.
const LAST_ROUND: &str = "goal_message_in_last_round";
const TEXT: &str = "Please also add a README line.";
const NOTICE: &str = "could not interrupt; delivered after this turn";

/// The goal module's settings of `direct_goal.rs`: the plugin folder of the repository and times short enough to run in seconds.
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

/// When [`Feeder`] hands the message out.
#[derive(Clone, Copy, PartialEq, Eq)]
enum At {
    /// At the first ask of the first stop: the goal module has voted in this poll and the driver has not held the run yet.
    FirstAsk,
    /// At an ask of the first stop that follows a hold, once a follow-up round of the engine's own loop has started.
    LaterAsk,
}

/// Hands the pending message of the store to the message module inside a poll, before the message module is asked.
struct Feeder {
    at: At,
    hook: Arc<MessagesHook>,
    store: Arc<MemoryStore>,
    engine_log: PathBuf,
    asks: AtomicU32,
    fed: AtomicBool,
    /// The number of the ask (1 for the first ask of the first stop) at which the message was handed out.
    fed_at_ask: AtomicU32,
}

impl Feeder {
    fn round_started(&self) -> bool {
        std::fs::read_to_string(&self.engine_log)
            .unwrap_or_default()
            .lines()
            .any(|l| l.starts_with("round ") && l.ends_with(" starts"))
    }
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
            let now = match self.at {
                At::FirstAsk => {
                    // The control plane accepts the message as soon as the session exists; the first ask waits for it (a moment), so
                    // the order of the two does not decide the case.
                    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    while ask == 1
                        && self.store.handout().is_empty()
                        && std::time::Instant::now() < end
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                    ask == 1
                }
                // Two holds after the round began: what the engine wrote before its silence has been read from the pipe.
                At::LaterAsk => ask >= 3 && self.round_started(),
            };
            if now && !self.store.handout().is_empty() {
                self.fed.store(true, Ordering::SeqCst);
                self.fed_at_ask.store(ask, Ordering::SeqCst);
                self.hook
                    .on_controls(&notice_controls(&self.store.handout()));
            }
            TurnVote::Done
        })
    }
}

/// What a run left for the checks.
struct Ran {
    w: World,
    fed_at_ask: u32,
}

/// The run: the goal module first, the feeder, and the message module last (its place in the product). The message is accepted by the
/// control plane as soon as the session exists and handed out by the feeder; the run is awaited to its end.
async fn message_in_the_follow_up_rounds(mode: Mode, at: At) -> Ran {
    message_in(SCENARIO, mode, at).await
}

/// As [`message_in_the_follow_up_rounds`], on the scenario named.
async fn message_in(scenario: &'static str, mode: Mode, at: At) -> Ran {
    let w = World::start_scenario(scenario, quick(), &pid_dir()).await;
    let goal = GoalHook::for_engine("cline", goal_settings())
        .expect("the plugin folder is in the repository");
    let feeder = Arc::new(Feeder {
        at,
        hook: w.hook.clone(),
        store: w.store.clone(),
        engine_log: w.d.agent_data().join("data/fake-acp.log"),
        asks: AtomicU32::new(0),
        fed: AtomicBool::new(false),
        fed_at_ask: AtomicU32::new(0),
    });
    let run = w.run_after(vec![
        goal as Arc<dyn DirectHook>,
        feeder.clone() as Arc<dyn DirectHook>,
    ]);
    // No pump: the feeder is the only hand-out, so the moment is the test's.
    w.send("m1", TEXT, mode).await;
    let _ = run.await.unwrap();
    Ran {
        fed_at_ask: feeder.fed_at_ask.load(Ordering::SeqCst),
        w,
    }
}

/// The notices of the run that say a message did not interrupt, as the terminal app receives them.
fn not_interrupted(w: &World) -> Vec<serde_json::Value> {
    w.d.aws
        .progress("notify")
        .into_iter()
        .map(|(_, v)| v)
        .filter(|v| v["reason"] == "message-not-interrupted")
        .collect()
}

/// TRIGGER PRESENT, for every case: the message was handed out at the ask the case names, it went out while the engine worked by itself,
/// the engine queued it behind its round, nothing was cancelled for it, and the control plane reads it delivered.
fn the_engine_queued_the_message(r: &Ran, at: At) {
    let log = r.w.log();
    match at {
        At::FirstAsk => assert_eq!(r.fed_at_ask, 1, "handed out at the first ask of the stop"),
        At::LaterAsk => assert!(
            r.fed_at_ask >= 3,
            "handed out at a later ask of the stop, after a hold: ask {}",
            r.fed_at_ask
        ),
    }
    assert!(
        log.contains("prompt queued during a round"),
        "the message did not arrive while the engine worked by itself: the test proves nothing\n{log}"
    );
    assert!(
        r.w.d.prompts_received().contains(&TEXT.to_string()),
        "{:?}",
        r.w.d.prompts_received()
    );
    assert!(
        !log.lines().any(|l| l == "notification session/cancel"),
        "a cancel was written for a message that had no prompt to interrupt\n{log}"
    );
    assert!(
        matches!(r.w.store.state("m1"), Some(State::Delivered { .. })),
        "{:?}",
        r.w.store.state("m1")
    );
}

/// The engine that queued the message is the engine that ends the run: the tripwire did not read the queued prompt's answer as a fault,
/// nothing was killed, no second session began, the engine ran the message's own round and the loop went on to its end.
fn the_engine_was_not_replaced(r: &Ran) {
    let agentd_log = r.w.d.log.lines();
    assert!(
        !agentd_log.iter().any(|l| l.contains("tripwire")),
        "the tripwire read the queued message as a fault: {agentd_log:?}"
    );
    assert!(
        !agentd_log
            .iter()
            .any(|l| l.contains("engine and tools killed")),
        "the engine was replaced: {agentd_log:?}"
    );
    assert_eq!(r.w.hook.stats().tripwires, 0);
    let sessions =
        r.w.log()
            .lines()
            .filter_map(|l| l.strip_prefix("in "))
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["method"] == "session/new")
            .count();
    assert_eq!(sessions, 1, "one engine session for the whole run");
    assert_eq!(
        r.w.d.prompts_received(),
        [direct_support::PROMPT.to_string(), TEXT.to_string()],
        "the first prompt and the message, each once, and no prompt of a second session"
    );
    let log = r.w.log();
    // The first follow-up round, the round of the queued message and the round that completes the goal.
    assert_eq!(
        log.lines()
            .filter(|l| l.starts_with("round ") && l.ends_with(" ends completed"))
            .count(),
        3,
        "the engine ran the message's round behind the round in flight\n{log}"
    );
    assert_eq!(
        r.w.d.summary().outcome,
        DirectOutcome::ReportedDone,
        "{}\n{agentd_log:?}",
        r.w.d.engine_trace()
    );
    assert_eq!(
        r.w.d.summary().cline_message.as_deref(),
        Some("Goal marked complete.")
    );
}

/// The user is told, once, in the words of every other interrupt that could not cancel.
fn the_user_is_told_once(r: &Ran) {
    let told = not_interrupted(&r.w);
    assert_eq!(
        told.len(),
        1,
        "one notice for the message that did not interrupt: {told:?}\nagentd log: {:?}",
        r.w.d.log.lines()
    );
    assert_eq!(told[0]["detail"], NOTICE, "{told:?}");
    assert_eq!(told[0]["messageId"], "m1", "{told:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn b857_now_claimed_at_a_later_ask_of_a_stop_cancels_nothing_the_user_is_told_and_the_engine_is_kept()
 {
    let r = message_in_the_follow_up_rounds(Mode::Now, At::LaterAsk).await;
    the_engine_queued_the_message(&r, At::LaterAsk);
    the_user_is_told_once(&r);
    the_engine_was_not_replaced(&r);
}

#[tokio::test(flavor = "multi_thread")]
async fn b857_now_claimed_at_the_first_ask_of_a_stop_cancels_nothing_the_user_is_told_and_the_engine_is_kept()
 {
    let r = message_in_the_follow_up_rounds(Mode::Now, At::FirstAsk).await;
    the_engine_queued_the_message(&r, At::FirstAsk);
    the_user_is_told_once(&r);
    the_engine_was_not_replaced(&r);
}

#[tokio::test(flavor = "multi_thread")]
async fn b857_a_next_message_during_the_follow_up_rounds_is_queued_by_the_engine_and_the_engine_is_kept()
 {
    let r = message_in_the_follow_up_rounds(Mode::Next, At::LaterAsk).await;
    the_engine_queued_the_message(&r, At::LaterAsk);
    // A `next` message never asked for an interrupt: no notice.
    let told = not_interrupted(&r.w);
    assert!(told.is_empty(), "{told:?}");
    the_engine_was_not_replaced(&r);
}

/// The round a message arrives in can be the last of the engine's loop: it completes the goal, and the engine starts no run after it. The
/// message it took behind that round is then never read, while the control plane shows it delivered. The message module knows what it
/// handed to a working engine: when the engine is idle again and began no run after it took the message, the message goes out once more,
/// as an ordinary prompt, and the engine reads it.
#[tokio::test(flavor = "multi_thread")]
async fn b857_a_message_taken_behind_the_round_that_ends_the_loop_is_sent_again_to_the_idle_engine_and_read()
 {
    let r = message_in(LAST_ROUND, Mode::Next, At::LaterAsk).await;
    let log = r.w.log();
    let agentd_log = r.w.d.log.lines();
    // TRIGGER PRESENT: the message went out while the engine worked by itself, the engine queued it behind its round, and that round
    // completed the goal.
    assert!(
        r.fed_at_ask >= 3,
        "handed out at a later ask of the stop: ask {}",
        r.fed_at_ask
    );
    assert!(
        log.contains("prompt queued during a round"),
        "the message did not arrive while the engine worked by itself: the test proves nothing\n{log}"
    );
    assert!(
        agentd_log
            .iter()
            .any(|l| l.contains("the engine took it behind its round and is kept")),
        "{agentd_log:?}"
    );
    assert!(
        agentd_log
            .iter()
            .any(|l| l.contains("called the goal-complete tool")),
        "the round in flight did not complete the goal: the test proves nothing: {agentd_log:?}"
    );
    // The engine read the message: it went out a second time, to an engine that had ended its loop, and its own run followed.
    assert_eq!(
        r.w.d.prompts_received(),
        [
            direct_support::PROMPT.to_string(),
            TEXT.to_string(),
            TEXT.to_string()
        ],
        "the first prompt, the message while the engine worked, and the message again to the idle engine\n{log}\n{agentd_log:?}"
    );
    assert!(
        log.contains("prompt runs on a dead loop"),
        "the second send did not reach an idle engine\n{log}"
    );
    assert_eq!(
        log.lines()
            .filter(|l| l.starts_with("round ") && l.ends_with(" ends completed"))
            .count(),
        2,
        "the round that completed the goal and the run of the message\n{log}"
    );
    assert_eq!(
        r.w.d.summary().cline_message.as_deref(),
        Some("Added the README line."),
        "the run ends with what the engine said about the message"
    );
    assert_eq!(
        r.w.d.summary().outcome,
        DirectOutcome::ReportedDone,
        "{}\n{agentd_log:?}",
        r.w.d.engine_trace()
    );
    // One delivery for the control plane and for the record of the run, and the engine was kept.
    assert!(
        matches!(r.w.store.state("m1"), Some(State::Delivered { .. })),
        "{:?}",
        r.w.store.state("m1")
    );
    assert_eq!(r.w.hook.stats().delivered, 1, "{agentd_log:?}");
    assert_eq!(
        agentd::direct::messages::earlier::read_local(&r.w.d.agent_data())
            .messages
            .len(),
        1
    );
    assert_eq!(r.w.hook.stats().tripwires, 0, "{agentd_log:?}");
    assert!(
        !agentd_log
            .iter()
            .any(|l| l.contains("engine and tools killed")),
        "{agentd_log:?}"
    );
    let sessions = log
        .lines()
        .filter_map(|l| l.strip_prefix("in "))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["method"] == "session/new")
        .count();
    assert_eq!(sessions, 1, "one engine session for the whole run");
}

// ---- the same round, with the user's cancel and with a request to end the session ----------------------------------

/// As [`message_in_the_follow_up_rounds`] with the message handed out at a later ask of the stop; once the engine has queued the
/// message behind its round, `act` is done. The round in flight is then the round the message arrived in.
async fn message_in_a_round_and_then(mode: Mode, act: impl FnOnce(&World)) -> Ran {
    let w = World::start_scenario(SCENARIO, quick(), &pid_dir()).await;
    let goal = GoalHook::for_engine("cline", goal_settings())
        .expect("the plugin folder is in the repository");
    let feeder = Arc::new(Feeder {
        at: At::LaterAsk,
        hook: w.hook.clone(),
        store: w.store.clone(),
        engine_log: w.d.agent_data().join("data/fake-acp.log"),
        asks: AtomicU32::new(0),
        fed: AtomicBool::new(false),
        fed_at_ask: AtomicU32::new(0),
    });
    let run = w.run_after(vec![
        goal as Arc<dyn DirectHook>,
        feeder.clone() as Arc<dyn DirectHook>,
    ]);
    w.send("m1", TEXT, mode).await;
    w.until("the engine queued the message behind its round", 60, |w| {
        w.log().contains("prompt queued during a round")
    })
    .await;
    act(&w);
    let _ = run.await.unwrap();
    Ran {
        fed_at_ask: feeder.fed_at_ask.load(Ordering::SeqCst),
        w,
    }
}

/// The engine sessions the run began.
fn engine_sessions(w: &World) -> usize {
    w.log()
        .lines()
        .filter_map(|l| l.strip_prefix("in "))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["method"] == "session/new")
        .count()
}

/// The user's cancel in the round a message arrived in. The run ends as the user's stop. The message that the engine had queued is
/// not read as a fault, the engine is not replaced by a fresh session that would work on, and the message is not sent a second time.
#[tokio::test(flavor = "multi_thread")]
async fn b853_b857_a_cancel_in_the_round_a_message_arrived_in_ends_the_run_as_the_users_stop() {
    let r = message_in_a_round_and_then(Mode::Now, |w| w.canceller.cancel()).await;
    let agentd_log = r.w.d.log.lines();
    the_engine_queued_the_message_before(&r);
    assert_eq!(
        r.w.d.summary().outcome,
        DirectOutcome::StoppedByUser,
        "{}\n{agentd_log:?}",
        r.w.d.engine_trace()
    );
    assert_eq!(r.w.hook.stats().tripwires, 0, "{agentd_log:?}");
    assert!(
        !agentd_log.iter().any(|l| l.contains("tripwire")),
        "the tripwire read the queued message as a fault: {agentd_log:?}"
    );
    assert_eq!(
        engine_sessions(&r.w),
        1,
        "no fresh engine session after the cancel: {agentd_log:?}"
    );
    assert_eq!(
        r.w.d.prompts_received(),
        [direct_support::PROMPT.to_string(), TEXT.to_string()],
        "the message went out once, before the cancel, and nothing after it"
    );
    the_user_is_told_once(&r);
}

/// A request to end the session from outside (the platform stops the machine) in the round a message arrived in. The worker tells the
/// service at once, which is what ends the service's wait and starts its read of the session's storage. Until the machine is gone
/// the run goes on as it was: the engine that queued the message is kept, and the message is delivered once.
#[tokio::test(flavor = "multi_thread")]
async fn b854_b857_a_request_to_end_in_the_round_a_message_arrived_in_is_told_to_the_service_and_the_engine_is_kept()
 {
    let r = message_in_a_round_and_then(Mode::Next, |w| {
        w.d.app.ending.ask();
    })
    .await;
    the_engine_queued_the_message_before(&r);
    let notices = r.w.d.aws.ending_notices();
    assert_eq!(
        notices.len(),
        1,
        "one notice of the end for the session: {:?}",
        r.w.d.log.lines()
    );
    assert_eq!(notices[0].0.ending, Some(true));
    assert!(
        matches!(r.w.store.state("m1"), Some(State::Delivered { .. })),
        "{:?}",
        r.w.store.state("m1")
    );
    assert_eq!(r.w.hook.stats().delivered, 1);
    // In the test nothing stops the machine, so the run goes on to its end with the engine it had.
    the_engine_was_not_replaced(&r);
}

/// TRIGGER PRESENT for the two cases above: the message went out while the engine worked by itself, and the engine queued it behind
/// its round before the cancel or the request to end.
fn the_engine_queued_the_message_before(r: &Ran) {
    let log = r.w.log();
    assert!(
        r.fed_at_ask >= 3,
        "handed out at a later ask of the stop: ask {}",
        r.fed_at_ask
    );
    assert!(
        log.contains("prompt queued during a round"),
        "the message did not arrive while the engine worked by itself: the test proves nothing\n{log}"
    );
    assert!(
        r.w.d.prompts_received().contains(&TEXT.to_string()),
        "{:?}",
        r.w.d.prompts_received()
    );
}
