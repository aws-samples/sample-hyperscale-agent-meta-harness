//! The events client against a server that behaves like the real one (a shard returns the events whose ids are above `after`), when the
//! harness commits its COMPLETION events with ids below the cursor. The A1 proof lost `session.started` that way; the same
//! hole could hide `delivery.ready` (the zip link) and `run.completed` (the label) of a run that is cancelled while it is delivered, or
//! any event whose id is dated earlier than the moment the Lambda writes it. While the run is open the client reads the harness shard
//! from its start on every cycle, so the next `Api::poll_with` finds them.
#![allow(clippy::unwrap_used)]

use ham::api::{Api, Retry};
use ham::http::{ControlPlane, Response, TransportError};
use ham_core::poll::PollPlan;
use ham_core::state::RunState;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RUN: &str = "01M54NV6WAC7ADQMC8WXQJ5EQ2";

fn ev(id: &str, ty: &str, payload: Value) -> Value {
    json!({
        "schemaVersion": 1, "eventId": id, "runId": RUN, "seq": 0,
        "ts": "2026-10-17T10:18:02Z", "type": ty, "authority": "harness",
        "payload": {"kind": "inline", "value": payload}
    })
}

#[derive(Default)]
struct Server {
    shard0: Mutex<Vec<Value>>,
}

impl ControlPlane for Server {
    async fn call(
        &self,
        _method: &str,
        path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
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
async fn a_zip_link_and_a_label_committed_below_the_cursor_are_read_on_the_next_cycle() {
    let server = Arc::new(Server::default());
    // A cancel-like writer took an id above the delivery events the run-session Lambda writes a little later (ids dated at the plan).
    server.shard0.lock().unwrap().extend([
        ev(
            "01M54NV70TDDBYBP3FF9SJ9HY3",
            "run.created",
            json!({"target": {"mode": "s3-only"}}),
        ),
        ev(
            "01M54NWP1ZZZZZZZZZZZZZZZZZ",
            "goal.status",
            json!({"status": "active", "phase": "delivering"}),
        ),
    ]);
    let api = Api::new(server.clone()).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    let mut state = RunState::new(RUN);
    let mut plan = PollPlan::new(Duration::from_secs(1));
    api.poll_with(&mut state, &mut plan, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(state.cursor(0), Some("01M54NWP1ZZZZZZZZZZZZZZZZZ"));
    assert!(!state.view().completed);
    // The delivery events land with smaller ids.
    server.shard0.lock().unwrap().extend([
        ev(
            "01M54NWP1K469Q31MJ2NG4YT60",
            "delivery.ready",
            json!({"artifactId": "zip-0123456789abcdef0123", "sha256": "ab".repeat(32), "sizeBytes": 4096}),
        ),
        ev(
            "01M54NWP1NX6WTJJX1MW2DNYDA",
            "run.completed",
            json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
        ),
    ]);
    // One cycle later (the very next one, not the third or the thirtieth) the run card has its end.
    api.poll_with(&mut state, &mut plan, Duration::from_secs(1))
        .await
        .unwrap();
    let view = state.view();
    assert!(view.completed, "run.completed is read on the next cycle");
    assert_eq!(view.result.as_ref().unwrap().label, "Cline reported done");
    assert_eq!(
        state.cursor(0),
        Some("01M54NWP1ZZZZZZZZZZZZZZZZZ"),
        "the cursor never moves back"
    );
}
