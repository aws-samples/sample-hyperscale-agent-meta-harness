//! Adaptive polling against the fake stage (client polish B2): a quiet watcher reads far fewer event shards than the old loop of
//! 32 reads every cycle, and a new event after a burst is still on screen within two seconds. The plan itself is tested with a
//! virtual clock in `ham-core/tests/poll.rs`; here the real `watch` and the real TUI app talk to the local `/v1` API.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::tui::{App, Effect, Msg};
use ham::watch::{WatchOptions, watch};
use ham_core::state::RunState;
use harness_protocol::api::v1::EventsPage;
use harness_protocol::envelope::EventType;
use serde_json::json;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::{Fake, REGION, creds};

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 4,
        base: Duration::from_millis(10),
    })
}

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Shared {
    fn lines(&self) -> usize {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .count()
    }
}

async fn new_run(fake: &Fake, prompt: &str) -> String {
    let req = serde_json::from_value::<harness_protocol::api::v1::StartDirectRunRequest>(
        json!({"prompt": prompt, "target": {"mode": "s3-only"}, "model": support::MODEL}),
    )
    .unwrap();
    api(fake)
        .start_direct_run(&req, &format!("k-{}", prompt.replace(' ', "-")))
        .await
        .unwrap()
        .run_id
}

/// The event reads of one run so far.
fn reads(fake: &Fake, run: &str) -> usize {
    let needle = format!("/runs/{run}/events");
    fake.mem
        .seen()
        .iter()
        .filter(|s| s.path.contains(&needle))
        .count()
}

fn spawn_watch(
    fake: &Fake,
    run: &str,
    interval: Duration,
) -> (Shared, tokio::task::JoinHandle<()>) {
    let out = Shared::default();
    let (api, run, o) = (api(fake), run.to_string(), out.clone());
    let opts = WatchOptions {
        interval,
        json: false,
        download_dir: None,
    };
    let h = tokio::spawn(async move {
        let (mut out, mut err) = (o, Vec::new());
        watch(&api, &run, &opts, &mut out, &mut err).await.unwrap();
    });
    (out, h)
}

async fn finish(fake: &Fake, run: &str, h: tokio::task::JoinHandle<()>) {
    fake.mem
        .harness(run, EventType::RunCompleted, json!({"status": "blocked"}));
    tokio::time::timeout(Duration::from_secs(10), h)
        .await
        .expect("the watch ends on run.completed")
        .unwrap();
}

#[tokio::test]
async fn a_quiet_watch_reads_at_least_five_times_fewer_shards_than_the_all_shards_loop() {
    let fake = Fake::start().await;
    let window = Duration::from_millis(1500);
    let base = Duration::from_millis(20);

    // The old loop: every shard on every cycle, the same base wait.
    let old_run = new_run(&fake, "quiet old loop").await;
    let (a, mut state) = (api(&fake), RunState::new(&old_run));
    let started = Instant::now();
    while started.elapsed() < window {
        a.poll(&mut state).await.unwrap();
        tokio::time::sleep(base).await;
    }
    let old = reads(&fake, &old_run);

    // The watch over the same quiet window.
    let run = new_run(&fake, "quiet watch").await;
    let (_out, h) = spawn_watch(&fake, &run, base);
    tokio::time::sleep(window).await;
    let new = reads(&fake, &run);
    finish(&fake, &run, h).await;

    eprintln!("quiet window: the old loop read {old} times, the watch {new} times");
    assert!(
        old >= 32 * 20,
        "the old loop read {old} times in the window"
    );
    assert!(
        new * 5 <= old,
        "the quiet watch read {new} times; the old loop read {old}"
    );
    assert!(new >= 32, "the first cycle still reads every shard: {new}");
}

/// Appends a status change and returns when the watch has printed a new line for it.
async fn latency_of_next_line(fake: &Fake, run: &str, out: &Shared, status: &str) -> Duration {
    let before = out.lines();
    let at = Instant::now();
    fake.mem.harness(
        run,
        EventType::GoalStatus,
        json!({"status": status, "phase": "green"}),
    );
    while out.lines() == before {
        assert!(
            at.elapsed() < Duration::from_secs(10),
            "the new event never reached the screen"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    at.elapsed()
}

#[tokio::test]
async fn after_a_quiet_stretch_a_new_event_is_printed_within_the_ceiling_of_the_wait() {
    let fake = Fake::start().await;
    let base = Duration::from_millis(20);
    let run = new_run(&fake, "quiet then event").await;
    let (out, h) = spawn_watch(&fake, &run, base);
    // Long enough for the wait to reach its ceiling (five bases) and for a cold sweep.
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let l = latency_of_next_line(&fake, &run, &out, "paused").await;
    assert!(
        l < base * 5 + Duration::from_millis(400),
        "printed {l:?} after the event"
    );
    finish(&fake, &run, h).await;
}

#[tokio::test]
async fn after_a_burst_a_new_event_is_printed_within_two_seconds_at_the_default_rate() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "burst then event").await;
    let (out, h) = spawn_watch(&fake, &run, Duration::from_secs(1));
    // A burst: an event every second for four seconds.
    for status in ["active", "paused", "active", "paused"] {
        let l = latency_of_next_line(&fake, &run, &out, status).await;
        assert!(l < Duration::from_secs(2), "{status}: {l:?}");
        tokio::time::sleep(Duration::from_millis(1000).saturating_sub(l)).await;
    }
    // Then a short silence, then the next event.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let l = latency_of_next_line(&fake, &run, &out, "active").await;
    assert!(l < Duration::from_secs(2), "after the burst: {l:?}");
    finish(&fake, &run, h).await;
}

// ---- the TUI --------------------------------------------------------------------------------------------------------------

const RUN: &str = "01K6A0000000000000000000R1";

fn empty_answer(cursors: &[(u8, Option<String>)]) -> Msg {
    Msg::Pages(Ok(cursors
        .iter()
        .map(|(s, _)| {
            (
                *s,
                EventsPage {
                    events: vec![],
                    next_after: None,
                },
            )
        })
        .collect()))
}

/// The cursors one tick asks to read (none when the tick polls nothing). Opening a run also asks for its detail (the prompt the
/// chat view shows first); that request is not a read of the shards and is skipped here.
fn polled(effects: &[Effect]) -> Option<&Vec<(u8, Option<String>)>> {
    let reads: Vec<&Effect> = effects
        .iter()
        // Opening a run also asks for the stored conversation and the messages (`Effect::Ext`), and a run that shows events and no model reads
        // the run list once to get it (`Effect::RefreshRuns`); a run whose name is not a model's reads its row at open and every fifth tick:
        // those are not reads of the event shards.
        .filter(|e| {
            !matches!(
                e,
                Effect::LoadDetail { .. }
                    | Effect::Ext(_)
                    | Effect::RefreshRuns(_)
                    | Effect::ReadRunName { .. }
            )
        })
        .collect();
    match reads.as_slice() {
        [Effect::Poll { cursors, .. }] => Some(cursors),
        [] => None,
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_tui_reads_every_shard_once_and_then_only_the_shards_that_matter_less_often() {
    let mut app = App::new_direct("n".into(), None, ham::tui::DirectSettings::default());
    let first = app.open_run(RUN);
    let first = polled(&first)
        .expect("opening the run polls at once")
        .clone();
    assert_eq!(first.len(), 32, "the first read covers every shard");
    app.update(empty_answer(&first));
    // A quiet minute of 1 s ticks.
    let mut total = 0usize;
    let mut cycles = 0usize;
    for _ in 0..60 {
        let effects = app.tick();
        if let Some(cursors) = polled(&effects) {
            total += cursors.len();
            cycles += 1;
            app.update(empty_answer(cursors));
        }
    }
    let old = 60 * 32;
    eprintln!("tui quiet minute: {total} shard reads in {cycles} cycles against {old}");
    assert!(
        total * 5 <= old,
        "{total} shard reads in a quiet minute against {old}"
    );
    assert!(
        (10..=30).contains(&cycles),
        "{cycles} cycles: the wait backs off to 5 s but never stops"
    );
}

#[test]
fn the_tui_polls_every_tick_while_events_flow() {
    let mut app = App::new_direct("n".into(), None, ham::tui::DirectSettings::default());
    let first = app.open_run(RUN);
    let first = polled(&first).unwrap().clone();
    app.update(empty_answer(&first));
    let mut n = 0u32;
    for _ in 0..10 {
        let effects = app.tick();
        if let Some(cursors) = polled(&effects) {
            // An event on the harness shard every time.
            let ev = support_event(n);
            n += 1;
            let mut pages: Vec<(u8, EventsPage)> = cursors
                .iter()
                .map(|(s, _)| {
                    (
                        *s,
                        EventsPage {
                            events: vec![],
                            next_after: None,
                        },
                    )
                })
                .collect();
            pages[0].1 = EventsPage {
                next_after: Some(ev.event_id.clone()),
                events: vec![ev],
            };
            app.update(Msg::Pages(Ok(pages)));
        }
    }
    assert_eq!(n, 10, "every tick of a busy run polls");
}

fn support_event(n: u32) -> harness_protocol::envelope::Envelope {
    use harness_protocol::envelope::{Authority, Envelope, Payload, SCHEMA_VERSION};
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{:04}", n + 1),
        run_id: RUN.into(),
        task_id: None,
        agent_id: None,
        seq: 0,
        ts: "2026-10-14T20:00:00Z".into(),
        event_type: EventType::GoalStatus,
        authority: Authority::Harness,
        payload: Payload::Inline {
            value: json!({"status": "active", "phase": "green"}),
        },
    }
}
