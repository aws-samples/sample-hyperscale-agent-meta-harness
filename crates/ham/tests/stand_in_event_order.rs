//! The order of the events of the stand-in stage. The stand-in gives the events its tests play (`Mem::ulid`) a time one second ahead of
//! now, so that they sort after the `run.created` id the real handler makes. An event the handler writes later (the status event of a
//! cancel of a run that has no live agent yet) used a plain ULID of the real time and sorted BEFORE the events already played: a client
//! that had read up to the played events never saw it (its cursor was past it).
#![allow(clippy::unwrap_used)]

mod support;

use control_api::handler::Store;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::store::{GoalItem, RunItem};
use serde_json::json;
use support::Mem;

const RUN: &str = "01M54ZXEW3YZKDBFHQ870V4Q23";

fn run_item(version: u64, status: &str) -> RunItem {
    serde_json::from_value(json!({
        "runId": RUN, "ownerPrincipalArn": support::ME, "promptSha256": "cd".repeat(32),
        "target": {"mode": "s3-only"}, "status": status, "phase": "green",
        "enginePolicyVersion": 0, "createdAt": "2026-10-17T13:13:13Z", "version": version
    }))
    .unwrap()
}

fn goal_item(status: &str) -> GoalItem {
    serde_json::from_value(json!({
        "runId": RUN, "objective": "x", "requirements": [], "specSha256": "", "acceptanceSha256": "",
        "status": status, "phase": "green", "lastBlockerFingerprint": null, "sameBlockerStreak": 0,
        "emptyTurnStreak": 0, "execFailureStreak": 0, "ladderRung": 0, "evaluatorVerdicts": [],
        "tokensUsed": 0, "timeUsedS": 0, "turnCount": 0, "version": 1
    }))
    .unwrap()
}

/// What the real handler writes with a run update: a harness status event with a ULID of the real time.
fn status_event(id: String) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: id,
        run_id: RUN.into(),
        task_id: None,
        agent_id: None,
        seq: 0,
        ts: "2026-10-17T13:13:14Z".into(),
        event_type: EventType::GoalStatus,
        authority: Authority::Harness,
        payload: Payload::Inline {
            value: json!({"status": "cleared", "phase": "green"}),
        },
    }
}

#[tokio::test]
async fn a_status_event_written_with_a_run_update_sorts_after_the_events_already_played() {
    let mem = Mem::default();
    mem.inner
        .lock()
        .unwrap()
        .runs
        .insert(RUN.to_string(), run_item(3, "active"));
    // The player's first events, then a client read that ends on the last of them.
    mem.harness(
        RUN,
        EventType::GoalStatus,
        json!({"status": "active", "phase": "green"}),
    );
    mem.harness(RUN, EventType::SessionStarted, json!({"agentId": "direct"}));
    let first = mem.events(RUN, 0, None, 100).await.unwrap();
    assert_eq!(first.len(), 2);
    let cursor = first.last().unwrap().event_id.clone();
    // The cancel of a run with no live agent: the handler updates the run and writes the status event, its id from the real clock.
    let event = status_event(control_api::ids::new_ulid());
    mem.update_run(
        &run_item(4, "cleared"),
        Some(&goal_item("cleared")),
        Some(&event),
    )
    .await
    .unwrap();
    let later = mem.events(RUN, 0, Some(&cursor), 100).await.unwrap();
    assert_eq!(
        later.len(),
        1,
        "the client read up to {cursor} and the status event of the update must come after it; the store holds {:?}",
        mem.inner
            .lock()
            .unwrap()
            .events
            .iter()
            .map(|e| e.event_id.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(later[0].event_type, EventType::GoalStatus);
}

#[tokio::test]
async fn events_the_stand_in_plays_still_come_in_the_order_they_were_played() {
    let mem = Mem::default();
    for i in 0..5 {
        mem.harness(
            RUN,
            EventType::GoalStatus,
            json!({"status": "active", "phase": "green", "n": i}),
        );
    }
    let all = mem.events(RUN, 0, None, 100).await.unwrap();
    let ns: Vec<i64> = all
        .iter()
        .map(|e| match &e.payload {
            Payload::Inline { value } => value["n"].as_i64().unwrap(),
            Payload::ClaimCheck { .. } => -1,
        })
        .collect();
    assert_eq!(ns, [0, 1, 2, 3, 4]);
}
