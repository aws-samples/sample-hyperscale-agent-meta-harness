//! `POST /v1/direct/runs` with the `limits` member: the one check (`check_limits`) that `ham` and agentd also call. A number
//! outside its range is a 400 `invalid-limits` that names the member and the range and never the value, before anything is written. A good member is
//! stored as sent, so agentd reads exactly what the user set.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, SONNET, World, deps, request};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::RunLimits;
use harness_protocol::store::s3;
use serde_json::{Value, json};

fn body(limits: Value) -> Value {
    json!({
        "prompt": "Add percent_change(a, b) with tests",
        "target": {"mode": "s3-only"},
        "model": SONNET,
        "rules": [],
        "limits": limits
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
async fn b776w_c9_a_limit_outside_its_range_is_refused_with_the_member_and_the_range_and_nothing_is_written()
 {
    for (member, value, range) in [
        ("interrupts", json!(1001), "0 to 1000"),
        ("questions", json!(1001), "0 to 1000"),
        ("freshSessions", json!(1001), "0 to 1000"),
        ("researchFileBytes", json!(1000), "65536 to 67108864"),
        (
            "researchUnpackedBytes",
            json!(5_000_000_000u64),
            "1048576 to 1073741824",
        ),
        ("researchRunBytes", json!(0), "1048576 to 2147483648"),
    ] {
        let w = World::new();
        let (status, v) = start(&w, "k-2", &body(json!({member: value}))).await;
        assert_eq!(status, 400, "{member}: {v}");
        assert_eq!(v["code"], "invalid-limits", "{member}: {v}");
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
async fn b776w_c9_a_limit_of_the_wrong_type_or_an_unknown_member_is_a_bad_request() {
    for limits in [json!({"interrupts": "5"}), json!({"maxTurns": 3})] {
        let w = World::new();
        let (status, v) = start(&w, "k-3", &body(limits)).await;
        assert_eq!(status, 400, "{v}");
        assert_eq!(v["code"], "bad-request");
        untouched(&w);
    }
}

#[tokio::test]
async fn b776w_c9_good_limits_are_stored_as_sent_and_a_request_without_them_stores_none() {
    let w = World::new();
    let sent = json!({"interrupts": 20, "questions": 8, "freshSessions": 6, "researchFileBytes": 4_194_304u64, "researchRunBytes": 1_073_741_824u64});
    let (status, v) = start(&w, "k-4", &body(sent)).await;
    assert_eq!(status, 201, "{v}");
    let got = stored(&w, v["runId"].as_str().unwrap()).limits.unwrap();
    assert_eq!(
        got,
        RunLimits {
            interrupts: Some(20),
            questions: Some(8),
            fresh_sessions: Some(6),
            research_file_bytes: Some(4_194_304),
            research_unpacked_bytes: None,
            research_run_bytes: Some(1_073_741_824),
        }
    );
    let w2 = World::new();
    let plain = json!({"prompt": "Add percent_change(a, b) with tests", "target": {"mode": "s3-only"}, "model": SONNET, "rules": []});
    let (status, v) = start(&w2, "k-5", &plain).await;
    assert_eq!(status, 201, "{v}");
    assert_eq!(stored(&w2, v["runId"].as_str().unwrap()).limits, None);
}
