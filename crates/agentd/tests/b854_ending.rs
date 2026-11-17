//! The notice of an end (`src/events.rs`, `spawn_flusher_ending`).
//!
//! When agentd is asked to end while the engine works, it tells the service at once: one heartbeat with `ending: true` and the run's
//! completion key, after what is buffered of the events. The service then ends its wait for the session instead of waiting for its
//! own limit. A notice that fails is sent again, at most five times, one second apart. A turn that is in its end sequence sends none:
//! its turn-complete is near.
//!
//! In most tests the request to end is made as the handler of a termination signal makes it (`Ending::ask`), in the process of the
//! test. One test sends a real `agentd serve` process the signal itself.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use direct_support::{Direct, Opts, RUN, first_process_possible};
use harness_protocol::direct::s3 as keys;
use harness_protocol::runtime::{PingStatus, TurnOutcome};
use std::time::{Duration, Instant};

/// A session whose engine holds its prompt until the turn's deadline, and whose flusher posts on a timer of 30 s: an events post
/// during the test is one that an event or the notice asked for.
fn working() -> Opts {
    Opts {
        scenario: "end_turn_done",
        engine_env: vec![("FAKE_ACP_HOLD_PROMPTS", "1".into())],
        timeout_s: 15,
        edit_cfg: Some(Box::new(|cfg| {
            cfg.direct.events_every = Duration::from_secs(30);
        })),
        ..Opts::default()
    }
}

async fn until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(
            Instant::now() < deadline,
            "{what} did not happen in {limit:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Starts the turn and waits until the engine works: it has its prompt, and a copy task watches.
async fn start_working(d: &Direct) {
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    until("the engine got its prompt", Duration::from_secs(60), || {
        !d.prompts_received().is_empty() && d.app.ending.watched()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_a_termination_signal_while_the_engine_works_sends_one_ending_heartbeat_with_the_key_after_the_events()
 {
    let d = Direct::start(working()).await;
    start_working(&d).await;
    assert!(
        d.aws.ending_notices().is_empty(),
        "no notice while nobody asked agentd to end"
    );
    let posts_before = d.aws.batches().len();
    d.app.ending.ask();
    until("the notice", Duration::from_secs(10), || {
        !d.aws.ending_notices().is_empty()
    })
    .await;
    let (beat, key, posts_at) = d.aws.ending_notices()[0].clone();
    assert_eq!(
        (
            beat.run_id.as_str(),
            beat.agent_id.as_str(),
            beat.turn,
            beat.ending
        ),
        (RUN, "direct", 1, Some(true))
    );
    // It carries the key the guarded routes carry.
    let events_key = d
        .aws
        .completion_keys()
        .into_iter()
        .find_map(|(path, key)| (path == "/internal/events").then_some(key))
        .flatten()
        .expect("the events carry the run's completion key");
    assert_eq!(key, Some(events_key));
    assert!(
        posts_at > posts_before,
        "what was buffered was posted before the notice: {posts_at} posts at the notice, {posts_before} before the request"
    );
    // A plain heartbeat is the request it always was.
    until("a plain heartbeat", Duration::from_secs(5), || {
        !d.aws.heartbeats().is_empty()
    })
    .await;
    assert!(d.aws.heartbeats().iter().all(|h| h.ending.is_none()));
    // A second request to end sends no second notice.
    d.app.ending.ask();
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(
        d.aws.ending_notices().len(),
        1,
        "one notice for the session"
    );
    let resp = d.wait_result().await;
    assert_eq!(resp.outcome, TurnOutcome::TimedOut, "{resp:?}");
    d.wait_healthy().await;
    assert!(
        !d.app.ending.watched(),
        "nothing watches after the turn: the flusher never counted as a copy task"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_a_notice_that_fails_is_sent_again_one_second_apart_and_at_most_five_times() {
    let taken_third = Direct::start(working()).await;
    let never_taken = Direct::start(working()).await;
    start_working(&taken_third).await;
    start_working(&never_taken).await;
    taken_third.aws.fail_ending_notices(2);
    never_taken.aws.fail_ending_notices(u32::MAX);
    let asked = Instant::now();
    taken_third.app.ending.ask();
    never_taken.app.ending.ask();
    // Two tries fail and the third is taken, two pauses later.
    until("the third try", Duration::from_secs(10), || {
        taken_third.aws.ending_notices().len() == 3
    })
    .await;
    assert!(
        asked.elapsed() >= Duration::from_millis(1_900),
        "the tries are one second apart: {:?}",
        asked.elapsed()
    );
    // Every try fails: five tries, and no sixth.
    until("the fifth try", Duration::from_secs(12), || {
        never_taken.aws.ending_notices().len() == 5
    })
    .await;
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(never_taken.aws.ending_notices().len(), 5);
    assert_eq!(
        taken_third.aws.ending_notices().len(),
        3,
        "a notice that was taken is not sent again"
    );
    assert!(
        never_taken
            .log
            .lines()
            .iter()
            .any(|l| l.contains("ending notice failed (try 5 of 5)")),
        "{:?}",
        never_taken.log.lines()
    );
    // The session goes on to its end either way.
    assert_eq!(
        never_taken.wait_result().await.outcome,
        TurnOutcome::TimedOut
    );
    taken_third.wait_result().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_a_signal_in_the_end_sequence_sends_no_ending_heartbeat() {
    let d = Direct::start(Opts::default()).await;
    // The end sequence takes a while: the upload of the turn's bundle is slow.
    let bundle = keys::bundle_key(RUN);
    d.aws.delay_puts(&bundle, Duration::from_secs(3));
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    until(
        "the upload of the turn's bundle",
        Duration::from_secs(60),
        || d.aws.put_attempts(&bundle) >= 1,
    )
    .await;
    // The trigger: the turn is in its end sequence. The prompt phase is over, so no copy task watches, and the turn has not ended.
    assert!(!d.app.ending.watched());
    assert_eq!(d.ping().await, PingStatus::HealthyBusy);
    assert!(
        d.aws.complete().is_empty(),
        "turn-complete is still to come"
    );
    d.app.ending.ask();
    let resp = d.wait_result().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(
        d.aws.ending_notices().is_empty(),
        "a turn that is ending says so with its turn-complete: {:?}",
        d.aws.ending_notices()
    );
    assert_eq!(d.aws.complete().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_a_request_to_end_that_came_before_the_engine_worked_is_told_once_it_works() {
    let d = Direct::start(working()).await;
    // The request comes while no turn runs: nothing works, so nothing is told yet.
    d.app.ending.ask();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(d.aws.ending_notices().is_empty());
    start_working(&d).await;
    until("the notice", Duration::from_secs(10), || {
        !d.aws.ending_notices().is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let notices = d.aws.ending_notices();
    assert_eq!(notices.len(), 1, "one notice for the session");
    assert_eq!(notices[0].0.ending, Some(true));
    assert!(notices[0].1.is_some(), "with the run's completion key");
    d.wait_result().await;
}

/// The same with a real process and a real signal: `agentd serve` runs as the entry point of a session's machine runs it, its engine
/// works, and the `kill` program sends it the termination signal that a stop of the session sends. agentd is the first process of a
/// process-id namespace of its own, as in a session's microVM: it runs on after the signal, so a second signal can be sent and the
/// turn goes to its end.
///
/// A machine that cannot make such a namespace does not run the test and says so, as the tests of the copy at a stop do
/// (`b823_stop_copy.rs`). A plain process is not what a session's machine runs: it ends once its copy is saved, so the notice would
/// have to win a race against that exit. The request to end itself is covered on every machine by the tests above, in the process
/// of the test.
#[tokio::test(flavor = "multi_thread")]
async fn b854_a_termination_signal_to_the_agentd_process_sends_one_ending_heartbeat_with_the_key_after_the_events()
 {
    if !first_process_possible() {
        eprintln!(
            "SKIPPED: this machine cannot start a process as the first one of a process-id namespace (unshare --user --pid --fork)"
        );
        return;
    }
    let d = Direct::start_process(Opts {
        first_process: true,
        ..working()
    })
    .await;
    let agentd = d.process.as_ref().unwrap();
    assert_eq!(
        agentd.own_pid(),
        Some(1),
        "TRIGGER PRESENT: agentd is process 1 of its own namespace"
    );
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    until("the engine got its prompt", Duration::from_secs(60), || {
        !d.prompts_received().is_empty()
    })
    .await;
    // The copy task of the turn watches a moment after the engine has its prompt.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        d.aws.ending_notices().is_empty(),
        "no notice while nobody asked agentd to end"
    );
    let posts_before = d.aws.batches().len();
    agentd.signal("TERM");
    until("the notice", Duration::from_secs(15), || {
        !d.aws.ending_notices().is_empty()
    })
    .await;
    let log = agentd.log_text();
    assert!(
        log.contains("termination signal SIGTERM"),
        "TRIGGER PRESENT: the request to end came from the signal: {log}"
    );
    let (beat, key, posts_at) = d.aws.ending_notices()[0].clone();
    assert_eq!(
        (
            beat.run_id.as_str(),
            beat.agent_id.as_str(),
            beat.turn,
            beat.ending
        ),
        (RUN, "direct", 1, Some(true))
    );
    let events_key = d
        .aws
        .completion_keys()
        .into_iter()
        .find_map(|(path, key)| (path == "/internal/events").then_some(key))
        .flatten()
        .expect("the events carry the run's completion key");
    assert_eq!(key, Some(events_key));
    assert!(
        posts_at > posts_before,
        "what was buffered was posted before the notice: {posts_at} posts at the notice, {posts_before} before the signal"
    );
    // The first process of its machine runs on: a second signal sends no second notice, and the turn goes to its end.
    agentd.signal("TERM");
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(agentd.alive(), "process 1 runs on after the signal");
    assert_eq!(
        d.aws.ending_notices().len(),
        1,
        "one notice for the session"
    );
    let resp = d.wait_result().await;
    assert_eq!(resp.outcome, TurnOutcome::TimedOut, "{resp:?}");
}
