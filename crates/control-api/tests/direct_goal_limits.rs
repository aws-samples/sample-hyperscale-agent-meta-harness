//! `POST /v1/direct/runs` with the stall limits of the `goal` member. The members pass the one check (`check_goal`)
//! that `ham` and agentd also call: a number outside its range is a 400 `invalid-goal` that names the member and the range and never the value,
//! before anything is written. A good member is stored as sent, so agentd reads exactly what the user set.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, SONNET, World, deps, request};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::GoalOptions;
use harness_protocol::store::s3;
use serde_json::{Value, json};

fn body(goal: Value) -> Value {
    json!({
        "prompt": "Add percent_change(a, b) with tests",
        "target": {"mode": "s3-only"},
        "model": SONNET,
        "rules": [],
        "goal": goal
    })
}

async fn start(w: &World, key: &str, b: &Value) -> (u16, Value) {
    let mut r = request(ME, "POST", "/v1/direct/runs", &[], &b.to_string());
    r.headers.insert("idempotency-key".into(), key.into());
    let out = control_api::handler::handle(r, &deps(w)).await;
    (
        out.status,
        serde_json::from_str(&out.body).unwrap_or(Value::Null),
    )
}

fn untouched(w: &World) {
    assert!(w.runs.lock().unwrap().is_empty(), "no run was created");
    assert!(w.idem.lock().unwrap().is_empty(), "no claim was written");
    assert!(w.started.lock().unwrap().is_empty(), "no execution started");
    assert_eq!(w.count("blob_put"), 0, "no blob was written");
}

fn stored(w: &World, run_id: &str) -> StartDirectRunRequest {
    let g = w.objects.lock().unwrap();
    serde_json::from_slice(&g[&s3::request_key(run_id)]).unwrap()
}

#[tokio::test]
async fn the_limits_are_stored_as_sent() {
    let w = World::new();
    let sent = json!({"enabled": true, "roundCalls": 45, "roundWindowS": 1800, "roundMinCalls": 12, "commandGraceS": 3600, "finalMessage": false});
    let (status, v) = start(&w, "k-1", &body(sent)).await;
    assert_eq!(status, 201, "{v}");
    let goal = stored(&w, v["runId"].as_str().unwrap()).goal.unwrap();
    assert_eq!(
        goal,
        GoalOptions {
            round_calls: Some(45),
            round_window_s: Some(1800),
            round_min_calls: Some(12),
            command_grace_s: Some(3600),
            final_message: Some(false),
            ..GoalOptions::switch(true)
        }
    );
}

#[tokio::test]
async fn a_limit_outside_its_range_is_refused_with_the_member_and_the_range_and_nothing_is_written()
{
    for (member, value, range) in [
        ("roundCalls", 4, "5 to 1000"),
        ("roundWindowS", 9, "10 to 14400"),
        ("roundMinCalls", 0, "1 to 100"),
        ("commandGraceS", 59, "60 to 86400"),
        ("newCalls", 1001, "1 to 1000"),
    ] {
        let w = World::new();
        let (status, v) = start(&w, "k-2", &body(json!({"enabled": true, member: value}))).await;
        assert_eq!(status, 400, "{member}: {v}");
        assert_eq!(v["code"], "invalid-goal", "{member}: {v}");
        let message = v["message"].as_str().unwrap();
        assert!(
            message.contains(member) && message.contains(range),
            "{message}"
        );
        assert!(!message.contains(&format!(": {value}")), "{message}");
        untouched(&w);
    }
}

#[tokio::test]
async fn a_limit_of_the_wrong_type_is_a_bad_request() {
    let w = World::new();
    let (status, v) = start(
        &w,
        "k-3",
        &body(json!({"enabled": true, "roundCalls": "30"})),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["code"], "bad-request");
    untouched(&w);
}

#[tokio::test]
async fn a_plain_switch_is_stored_as_it_always_was() {
    let w = World::new();
    let (status, v) = start(&w, "k-4", &body(json!({"enabled": false}))).await;
    assert_eq!(status, 201, "{v}");
    let text = {
        let g = w.objects.lock().unwrap();
        String::from_utf8(g[&s3::request_key(v["runId"].as_str().unwrap())].clone()).unwrap()
    };
    assert!(text.contains(r#""goal":{"enabled":false}"#), "{text}");
}

#[tokio::test]
async fn the_allowance_of_new_calls_is_stored_as_sent_and_zero_stays_zero() {
    for (key, sent) in [("k-5", 150u32), ("k-6", 0)] {
        let w = World::new();
        let (status, v) = start(&w, key, &body(json!({"enabled": true, "newCalls": sent}))).await;
        assert_eq!(status, 201, "{sent}: {v}");
        let goal = stored(&w, v["runId"].as_str().unwrap()).goal.unwrap();
        assert_eq!(
            goal,
            GoalOptions {
                new_calls: Some(sent),
                ..GoalOptions::switch(true)
            }
        );
    }
}
