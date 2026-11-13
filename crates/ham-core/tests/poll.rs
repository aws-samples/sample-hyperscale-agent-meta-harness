//! Adaptive polling of the event shards (client polish B2): a watcher used to read all 32 shards about 1.3 times a second, 64,000
//! requests in 26 minutes, of which 30 shards of 32 were always empty for a direct run. The plan reads the shards that matter (the
//! harness shard 0 and the shard of each agent the run has started), reads the rest rarely, and waits longer while nothing arrives.
//! The plan is pure: the tests drive it with a virtual clock.

#![allow(clippy::unwrap_used)]

use ham_core::poll::PollPlan;
use harness_protocol::api::v1::EventsPage;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use harness_protocol::store::{EVENT_SHARDS, event_shard};
use serde_json::json;
use std::time::Duration;

const BASE: Duration = Duration::from_secs(1);

fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s)
}

fn envelope(n: u32, t: EventType, agent: Option<&str>, v: serde_json::Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: format!("01J9ZZZZZZ0000000000{n:06}"),
        run_id: "R1".into(),
        task_id: None,
        agent_id: agent.map(str::to_string),
        seq: u64::from(n),
        ts: "2026-10-14T02:00:00Z".into(),
        event_type: t,
        authority: if agent.is_some() {
            Authority::Agent
        } else {
            Authority::Harness
        },
        payload: Payload::Inline { value: v },
    }
}

fn page(events: Vec<Envelope>) -> EventsPage {
    let next_after = events.last().map(|e| e.event_id.clone());
    EventsPage { events, next_after }
}

/// What a read of `shards` returns: nothing, except the pages `with` names.
fn answer(shards: &[u8], with: &[(u8, EventsPage)]) -> Vec<(u8, EventsPage)> {
    shards
        .iter()
        .map(|s| {
            let p = with
                .iter()
                .find(|(w, _)| w == s)
                .map_or_else(|| page(vec![]), |(_, p)| p.clone());
            (*s, p)
        })
        .collect()
}

fn session_started(n: u32, agent: &str) -> Envelope {
    envelope(
        n,
        EventType::SessionStarted,
        None,
        json!({"agentId": agent, "engine": "cline", "modelId": "m"}),
    )
}

/// One cycle at `now`: the shards read and the pages they returned recorded; the wait before the next cycle.
fn cycle(plan: &mut PollPlan, now: Duration, with: &[(u8, EventsPage)]) -> (Vec<u8>, Duration) {
    let shards = plan.shards(now);
    plan.record(now, &answer(&shards, with));
    (shards, plan.interval())
}

#[test]
fn the_first_cycle_reads_every_shard_so_a_rehydration_misses_nothing() {
    let mut plan = PollPlan::new(BASE);
    let (shards, _) = cycle(&mut plan, Duration::ZERO, &[]);
    assert_eq!(shards, (0..EVENT_SHARDS).collect::<Vec<u8>>());
}

#[test]
fn after_that_a_quiet_run_reads_the_harness_shard_and_the_started_agents_shard() {
    let mut plan = PollPlan::new(BASE);
    let direct = event_shard(Authority::Agent, Some("direct"));
    assert_ne!(direct, 0);
    // Cycle 1 finds run.created and session.started on shard 0.
    cycle(
        &mut plan,
        Duration::ZERO,
        &[(
            0,
            page(vec![
                envelope(1, EventType::RunCreated, None, json!({})),
                session_started(2, "direct"),
            ]),
        )],
    );
    let (shards, _) = cycle(&mut plan, secs(1.0), &[]);
    assert_eq!(shards, vec![0, direct]);
}

#[test]
fn the_wait_stays_at_the_base_for_three_empty_cycles_then_doubles_to_the_ceiling_and_an_event_resets_it()
 {
    let mut plan = PollPlan::new(BASE);
    let mut now = Duration::ZERO;
    let mut waits = Vec::new();
    for _ in 0..8 {
        let (_, wait) = cycle(&mut plan, now, &[]);
        waits.push(wait.as_secs());
        now += wait;
    }
    assert_eq!(
        waits,
        [1, 1, 1, 2, 4, 5, 5, 5],
        "base, base, base, then 2 s, 4 s and the 5 s ceiling"
    );
    // One event on the harness shard: the next wait is the base again.
    let (_, wait) = cycle(
        &mut plan,
        now,
        &[(
            0,
            page(vec![envelope(
                9,
                EventType::GoalStatus,
                None,
                json!({"status": "active"}),
            )]),
        )],
    );
    assert_eq!(wait, BASE);
}

#[test]
fn a_quiet_minute_costs_at_least_five_times_fewer_reads_than_all_shards_every_second() {
    // The old client: 60 cycles of 32 reads in a quiet minute (a cycle every second at best; it was about 1.3 a second).
    let old = 60 * u64::from(EVENT_SHARDS);
    let mut plan = PollPlan::new(BASE);
    let mut now = Duration::ZERO;
    let mut reads = 0u64;
    let mut first = true;
    while now < Duration::from_secs(60) {
        let with = if first {
            vec![(
                0,
                page(vec![
                    envelope(1, EventType::RunCreated, None, json!({})),
                    session_started(2, "direct"),
                ]),
            )]
        } else {
            vec![]
        };
        first = false;
        let (shards, wait) = cycle(&mut plan, now, &with);
        reads += shards.len() as u64;
        now += wait;
    }
    eprintln!("quiet minute: {reads} shard reads against {old}");
    assert!(
        reads * 5 <= old,
        "{reads} reads in a quiet minute against {old}"
    );
    assert!(
        reads >= 60,
        "the harness shard is still read every cycle: {reads}"
    );
}

#[test]
fn an_event_on_a_shard_nobody_expected_is_found_by_the_cold_sweep_and_the_shard_turns_hot() {
    let mut plan = PollPlan::new(BASE);
    cycle(&mut plan, Duration::ZERO, &[]);
    let odd = 17u8;
    let mut now = secs(1.0);
    let mut found_at = None;
    while now < Duration::from_secs(120) {
        let shards = plan.shards(now);
        let with: Vec<(u8, EventsPage)> = if shards.contains(&odd) {
            vec![(
                odd,
                page(vec![envelope(
                    5,
                    EventType::TurnProgress,
                    Some("x"),
                    json!({"text": "t"}),
                )]),
            )]
        } else {
            vec![]
        };
        if !with.is_empty() && found_at.is_none() {
            found_at = Some(now);
        }
        plan.record(now, &answer(&shards, &with));
        now += plan.interval();
        if found_at.is_some() {
            break;
        }
    }
    let found = found_at.expect("the cold sweep reads every shard");
    assert!(found <= Duration::from_secs(40), "found at {found:?}");
    // It is hot now: the next cycle reads it again.
    assert!(plan.shards(now).contains(&odd));
}

#[test]
fn after_a_burst_the_next_event_is_seen_within_two_seconds_unless_the_silence_was_long() {
    // Events every second for 5 s, then silence; an event arrives `gap` after the last one. The reader sees it at its next cycle.
    let latency = |gap: f64| {
        let mut plan = PollPlan::new(BASE);
        let mut now = Duration::ZERO;
        let mut last_event = Duration::ZERO;
        // The burst.
        for i in 0..5 {
            let (_, wait) = cycle(
                &mut plan,
                now,
                &[(
                    0,
                    page(vec![envelope(
                        10 + i,
                        EventType::GoalStatus,
                        None,
                        json!({"status": "active"}),
                    )]),
                )],
            );
            last_event = now;
            now += wait;
        }
        let new_event_at = last_event + secs(gap);
        // Silence until the event exists, then the first cycle at or after it reads it.
        loop {
            let with = if now >= new_event_at {
                vec![(
                    0,
                    page(vec![envelope(
                        99,
                        EventType::GoalStatus,
                        None,
                        json!({"status": "paused"}),
                    )]),
                )]
            } else {
                vec![]
            };
            let (_, wait) = cycle(&mut plan, now, &with);
            if now >= new_event_at {
                return now - new_event_at;
            }
            now += wait;
        }
    };
    for gap in [0.5, 1.0, 1.5, 2.0, 2.9] {
        let l = latency(gap);
        assert!(
            l < Duration::from_secs(2),
            "gap {gap} s: seen {l:?} after it happened"
        );
    }
    // After a long silence the wait is the ceiling: at most 5 s.
    assert!(latency(30.0) <= Duration::from_secs(5));
}

#[test]
fn a_failed_read_of_a_shard_leaves_it_due_and_a_cycle_with_no_pages_does_not_count_as_quiet() {
    let mut plan = PollPlan::new(BASE);
    // Only shards 0 and 3 answered the first cycle; the rest failed.
    let shards = plan.shards(Duration::ZERO);
    let ok: Vec<u8> = shards
        .iter()
        .copied()
        .filter(|s| *s == 0 || *s == 3)
        .collect();
    plan.record(Duration::ZERO, &answer(&ok, &[]));
    let next = plan.shards(secs(1.0));
    assert!(
        next.contains(&5),
        "a shard that was never read is still due: {next:?}"
    );
    assert!(next.contains(&0), "the harness shard is always read");
    // A cycle with no pages at all (every read failed) leaves the wait where it was.
    let mut plan = PollPlan::new(BASE);
    plan.record(Duration::ZERO, &[]);
    assert_eq!(plan.interval(), BASE);
}

#[test]
fn the_plan_scales_with_the_base_so_a_test_can_run_it_fast() {
    let mut plan = PollPlan::new(Duration::from_millis(20));
    let mut now = Duration::ZERO;
    let mut last = Duration::ZERO;
    for _ in 0..12 {
        let (_, wait) = cycle(&mut plan, now, &[]);
        last = wait;
        now += wait;
    }
    assert_eq!(
        last,
        Duration::from_millis(100),
        "the ceiling is five times the base"
    );
}
