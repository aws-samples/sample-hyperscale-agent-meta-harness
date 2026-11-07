//! The events client against a server that behaves like the real one: a shard returns the events whose ids are above `after`. The late
//! `session.started` has a smaller id than an event the client already read (the live A1 proof, T-A1 textstats leg, 1 in 10): without a
//! re-read from the start the card never names the model. `Api::poll_with` runs the plan's cursors, so the sweep finds the event.
#![allow(clippy::unwrap_used)]

use ham::api::{Api, Retry};
use ham::http::{ControlPlane, Response, TransportError};
use ham_core::poll::{PollPlan, SWEEP_UNKNOWN_EVERY};
use ham_core::state::RunState;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const SONNET: &str = "global.anthropic.claude-sonnet-5-5";

fn ev(id: &str, ty: &str, payload: Value) -> Value {
    json!({
        "schemaVersion": 1, "eventId": id, "runId": RUN, "seq": 1,
        "ts": "2026-10-16T08:15:00Z", "type": ty, "authority": "harness",
        "payload": {"kind": "inline", "value": payload}
    })
}

#[derive(Default)]
struct Server {
    /// The events of shard 0 as the store holds them, in the order they were written.
    shard0: Mutex<Vec<Value>>,
    reads: Mutex<Vec<String>>,
}

impl ControlPlane for Server {
    async fn call(
        &self,
        _method: &str,
        path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.reads.lock().unwrap().push(path.to_string());
        let query = path.split_once('?').map_or("", |(_, q)| q);
        let param = |k: &str| {
            query
                .split('&')
                .find_map(|p| p.strip_prefix(&format!("{k}=")))
                .map(str::to_string)
        };
        let shard: u8 = param("shard").and_then(|s| s.parse().ok()).unwrap_or(0);
        let after = param("after");
        let mut events: Vec<Value> = if shard == 0 {
            self.shard0.lock().unwrap().clone()
        } else {
            Vec::new()
        };
        // The store's order and the cursor's meaning: sorted by id, only the ids above `after`.
        events.sort_by(|a, b| a["eventId"].as_str().cmp(&b["eventId"].as_str()));
        events.retain(|e| {
            after
                .as_deref()
                .is_none_or(|a| e["eventId"].as_str().unwrap() > a)
        });
        let next_after = events.last().map(|e| e["eventId"].clone());
        let body = json!({"events": events, "nextAfter": next_after});
        Ok(Response {
            status: 200,
            headers: BTreeMap::new(),
            body: serde_json::to_vec(&body).unwrap(),
        })
    }
    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        unreachable!("these calls fetch nothing")
    }
}

#[tokio::test]
async fn a_session_started_with_a_smaller_id_than_the_cursor_is_found_within_a_few_cycles() {
    let server = Arc::new(Server::default());
    server.shard0.lock().unwrap().extend([
        ev(
            "01M5A2R7K3C9XJ4D8E6F000010",
            "run.created",
            json!({"target": {"mode": "s3-only"}}),
        ),
        ev(
            "01M5A2R7K3C9XJ4D8E6F000030",
            "goal.status",
            json!({"status": "active", "phase": "green"}),
        ),
    ]);
    let api = Api::new(server.clone()).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    let mut state = RunState::new(RUN);
    let mut plan = PollPlan::new(Duration::from_secs(1));
    // The first cycle reads what is there and moves the cursor to the largest id.
    api.poll_with(&mut state, &mut plan, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(state.cursor(0), Some("01M5A2R7K3C9XJ4D8E6F000030"));
    // Then agentd's session.started lands with an id below it.
    server.shard0.lock().unwrap().push(ev(
        "01M5A2R7K3C9XJ4D8E6F000020",
        "session.started",
        json!({"agentId": "direct", "engine": "cline", "modelId": SONNET}),
    ));
    let mut found_at = None;
    for cycle in 1..=SWEEP_UNKNOWN_EVERY * 2 {
        api.poll_with(&mut state, &mut plan, Duration::from_secs(u64::from(cycle)))
            .await
            .unwrap();
        if state.view().direct_model_id().is_some() {
            found_at = Some(cycle);
            break;
        }
    }
    let at = found_at.expect("the sweep finds the late session.started");
    assert!(
        at <= SWEEP_UNKNOWN_EVERY,
        "within {SWEEP_UNKNOWN_EVERY} cycles, found at {at}"
    );
    assert_eq!(state.view().direct_model_id(), Some(SONNET));
    assert_eq!(
        state.cursor(0),
        Some("01M5A2R7K3C9XJ4D8E6F000030"),
        "the cursor never moves back"
    );
}
