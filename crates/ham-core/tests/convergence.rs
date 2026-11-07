//! The reducer converges for any delivery across the 32 shard cursors:
//! each shard's events arrive as pages in any interleaving with the other shards, pages
//! are re-delivered (duplicates), and some pages arrive only on a later pass (gaps). The view
//! equals the view of every event applied once in event-id order, and each shard's cursor ends at
//! the last event id delivered for it.

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::state::RunState;
use harness_protocol::api::v1::EventsPage;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use harness_protocol::store::{EVENT_SHARDS, event_shard};
use proptest::prelude::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

const RUN: &str = "01JABCDEFGHJKMNPQRSTVWXYZ0";
const PHASES: [&str; 4] = ["red", "green", "gating", "evaluating"];

fn ulid(n: u64) -> String {
    const A: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut s = String::from("01J9ZZZZZZ");
    let mut v = n;
    let mut tail = [b'0'; 16];
    for i in (0..16).rev() {
        tail[i] = A[(v % 32) as usize];
        v /= 32;
    }
    s.push_str(std::str::from_utf8(&tail).unwrap());
    s
}

/// One generated event: its kind, the agent it concerns and a payload number.
#[derive(Clone, Debug)]
struct Spec {
    kind: u8,
    agent: u8,
    x: u8,
}

fn agent_name(a: u8) -> String {
    format!("T{a}-a1")
}

fn build(n: u64, s: &Spec) -> Envelope {
    let agent = agent_name(s.agent);
    let task = format!("T{}", s.agent);
    let (t, harness, task_id, v): (EventType, bool, Option<String>, Value) = match s.kind % 12 {
        0 => (
            EventType::SessionStarted,
            true,
            Some(task),
            json!({"agentId": agent, "engine": if s.x.is_multiple_of(2) { "cline" } else { "opencode" }, "modelId": format!("m{}", s.x % 3)}),
        ),
        1 => (EventType::TurnStarted, false, None, json!({"turn": s.x})),
        2 => (
            EventType::ToolSummary,
            false,
            None,
            json!({"toolCallId": format!("c{}", s.x % 5), "title": format!("step {} \u{1b}[31mred", s.x), "kind": "execute", "status": "pending"}),
        ),
        3 => (
            EventType::TurnEnded,
            false,
            None,
            json!({"turn": s.x % 5, "outcome": "completed"}),
        ),
        4 => (
            EventType::GateResult,
            true,
            Some(task),
            json!({"gateId": format!("G{}", s.x % 13), "pass": !s.x.is_multiple_of(3)}),
        ),
        5 => (
            EventType::AgentFlagBlocked,
            false,
            None,
            json!({"reason": format!("needs {}", s.x), "needs": [format!("n{}", s.x % 4)]}),
        ),
        6 => (
            EventType::PermissionRequested,
            false,
            None,
            json!({"approvalId": format!("AP{}", s.x % 6), "relayed": true, "title": "t"}),
        ),
        7 => (
            EventType::PermissionRequested,
            false,
            None,
            json!({"approvalId": format!("AP{}", s.x % 6), "allowed": s.x.is_multiple_of(2), "resolvedBy": "user"}),
        ),
        8 => (
            EventType::GoalStatus,
            true,
            Some(task),
            json!({"status": "active", "phase": PHASES[usize::from(s.x % 4)]}),
        ),
        9 => (
            EventType::EvaluatorVerdict,
            true,
            None,
            json!({"modelId": format!("e{}", s.x % 3), "verdict": if s.x.is_multiple_of(2) { "met" } else { "not_met" }}),
        ),
        10 => (EventType::MergeResult, true, None, json!({"batch": s.x})),
        // An agent-authored harness-only event: rejected on every delivery.
        _ => (
            EventType::DeliveryReady,
            false,
            None,
            json!({"artifactId": "X", "sha256": "0".repeat(64), "sizeBytes": 1}),
        ),
    };
    Envelope {
        schema_version: 1,
        event_id: ulid(n),
        run_id: RUN.into(),
        task_id,
        agent_id: if harness { None } else { Some(agent) },
        seq: u64::from(s.x),
        ts: "2026-09-25T02:00:00Z".into(),
        event_type: t,
        authority: if harness {
            Authority::Harness
        } else {
            Authority::Agent
        },
        payload: Payload::Inline { value: v },
    }
}

fn events(specs: &[Spec]) -> Vec<Envelope> {
    let mut out = vec![Envelope {
        schema_version: 1,
        event_id: ulid(0),
        run_id: RUN.into(),
        task_id: None,
        agent_id: None,
        seq: 0,
        ts: "2026-09-25T02:00:00Z".into(),
        event_type: EventType::RunCreated,
        authority: Authority::Harness,
        payload: Payload::Inline { value: json!({}) },
    }];
    out.extend(
        specs
            .iter()
            .enumerate()
            .map(|(i, s)| build(i as u64 + 1, s)),
    );
    out
}

/// Each shard's events in id order, cut into pages at `cuts` (page sizes 1-5).
fn pages(evs: &[Envelope], cuts: &[u8]) -> Vec<(u8, EventsPage)> {
    let mut by: BTreeMap<u8, Vec<Envelope>> = BTreeMap::new();
    for e in evs {
        by.entry(event_shard(e.authority, e.agent_id.as_deref()))
            .or_default()
            .push(e.clone());
    }
    let mut out = Vec::new();
    let mut c = cuts.iter().cycle();
    for (shard, mut list) in by {
        list.sort_by(|a, b| a.event_id.cmp(&b.event_id));
        let mut i = 0;
        while i < list.len() {
            let n = usize::from(*c.next().unwrap_or(&1) % 5 + 1).min(list.len() - i);
            let page: Vec<Envelope> = list[i..i + n].to_vec();
            let next_after = page.last().map(|e| e.event_id.clone());
            out.push((
                shard,
                EventsPage {
                    events: page,
                    next_after,
                },
            ));
            i += n;
        }
    }
    out
}

fn spec() -> impl Strategy<Value = Spec> {
    (0u8..12, 0u8..48, any::<u8>()).prop_map(|(kind, agent, x)| Spec { kind, agent, x })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn the_view_converges_for_any_interleaving_across_32_shard_cursors(
        specs in proptest::collection::vec(spec(), 1..120),
        cuts in proptest::collection::vec(any::<u8>(), 1..16),
        order in any::<u64>(),
        dups in proptest::collection::vec(any::<u16>(), 0..12),
        held in proptest::collection::vec(any::<u16>(), 0..6),
    ) {
        let evs = events(&specs);
        let mut reference = RunState::new(RUN);
        for e in &evs {
            reference.apply(e.clone());
        }
        let want = reference.view();

        // Any interleaving of the shards: shuffle the pages, keeping each shard's own order.
        let all = pages(&evs, &cuts);
        let mut queues: BTreeMap<u8, std::collections::VecDeque<EventsPage>> = BTreeMap::new();
        for (s, p) in all.iter().cloned() {
            queues.entry(s).or_default().push_back(p);
        }
        let mut seed = order;
        let mut delivered: Vec<(u8, EventsPage)> = Vec::new();
        let mut gapped: Vec<(u8, EventsPage)> = Vec::new();
        let mut state = RunState::new(RUN);
        let mut k = 0u16;
        while queues.values().any(|q| !q.is_empty()) {
            let live: Vec<u8> = queues.iter().filter(|(_, q)| !q.is_empty()).map(|(s, _)| *s).collect();
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            let shard = live[(seed >> 33) as usize % live.len()];
            let page = queues.get_mut(&shard).and_then(|q| q.pop_front()).unwrap();
            k = k.wrapping_add(1);
            // A gap: this page is lost on this pass and arrives after everything else.
            if held.contains(&k) {
                gapped.push((shard, page));
                continue;
            }
            state.apply_page(shard, page.clone());
            delivered.push((shard, page));
            // A duplicate: an earlier page of any shard arrives again.
            if let Some(d) = dups.iter().find(|d| **d == k)
                && !delivered.is_empty()
            {
                let (s, p) = delivered[usize::from(*d) % delivered.len()].clone();
                state.apply_page(s, p);
            }
        }
        for (s, p) in gapped {
            state.apply_page(s, p);
        }
        prop_assert_eq!(state.view(), want);
        // Every shard's cursor is its last event id, whatever order its pages came in.
        for shard in 0..EVENT_SHARDS {
            let last = evs
                .iter()
                .filter(|e| event_shard(e.authority, e.agent_id.as_deref()) == shard)
                .map(|e| e.event_id.clone())
                .max();
            prop_assert_eq!(state.cursor(shard).map(str::to_string), last);
        }
    }
}

#[test]
fn many_agents_spread_over_many_shards() {
    let specs: Vec<Spec> = (0..48)
        .map(|a| Spec {
            kind: 2,
            agent: a,
            x: a,
        })
        .collect();
    let evs = events(&specs);
    let shards: std::collections::BTreeSet<u8> = evs
        .iter()
        .map(|e| event_shard(e.authority, e.agent_id.as_deref()))
        .collect();
    assert!(
        shards.len() >= 16,
        "48 agents land on {} shards",
        shards.len()
    );
}
