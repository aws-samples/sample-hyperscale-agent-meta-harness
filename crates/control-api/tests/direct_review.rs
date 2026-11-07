//! `POST /v1/direct/runs` with the `review` member. The member passes the one check (`check_review`) that `ham` and
//! agentd also call: a bad text, hash or guard is a 400 `invalid-review` that names the member and the rule and never the value, before anything
//! is written. A good member is stored as sent, so agentd reads exactly what the user set; a request without the member is stored as it always was.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, SONNET, World, deps, request};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{ReviewMode, ReviewOptions};
use harness_protocol::store::s3;
use serde_json::{Value, json};

fn body(review: Option<Value>) -> Value {
    let mut b = json!({
        "prompt": "Add percent_change(a, b) with tests",
        "target": {"mode": "s3-only"},
        "model": SONNET,
        "rules": []
    });
    if let Some(r) = review {
        b["review"] = r;
    }
    b
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

fn stored_text(w: &World, run_id: &str) -> String {
    let g = w.objects.lock().unwrap();
    String::from_utf8(g[&s3::request_key(run_id)].clone()).unwrap()
}

#[tokio::test]
async fn a_good_member_is_stored_as_sent() {
    let w = World::new();
    let sent = serde_json::to_value(ReviewOptions::with_text(
        "Look again at {{base}}.\n",
        Some(900),
    ))
    .unwrap();
    let (status, v) = start(&w, "k-1", &body(Some(sent.clone()))).await;
    assert_eq!(status, 201, "{v}");
    let text = stored_text(&w, v["runId"].as_str().unwrap());
    let req: StartDirectRunRequest = serde_json::from_str(&text).unwrap();
    assert_eq!(
        req.review,
        Some(ReviewOptions::with_text(
            "Look again at {{base}}.\n",
            Some(900)
        ))
    );
    assert!(
        text.contains(r#""review":{"mode":"auto","text":"Look again at {{base}}.\n""#),
        "{text}"
    );
}

#[tokio::test]
async fn b776_a_review_text_over_the_old_8_kib_cap_is_stored_whole() {
    let w = World::new();
    let text = format!("{}\n", "Look again at the diff. ".repeat(10_000));
    assert!(text.len() > 8 * 1024);
    let sent = serde_json::to_value(ReviewOptions::with_text(&text, None)).unwrap();
    let (status, v) = start(&w, "k-1", &body(Some(sent))).await;
    assert_eq!(status, 201, "{v}");
    let req: StartDirectRunRequest =
        serde_json::from_str(&stored_text(&w, v["runId"].as_str().unwrap())).unwrap();
    assert_eq!(req.review.unwrap().text.as_deref(), Some(text.as_str()));
}

#[tokio::test]
async fn the_switch_alone_is_stored_and_a_request_without_the_member_is_stored_as_before() {
    let w = World::new();
    let (status, v) = start(&w, "k-2", &body(Some(json!({"mode": "off"})))).await;
    assert_eq!(status, 201, "{v}");
    let text = stored_text(&w, v["runId"].as_str().unwrap());
    assert!(text.contains(r#""review":{"mode":"off"}"#), "{text}");
    let req: StartDirectRunRequest = serde_json::from_str(&text).unwrap();
    assert_eq!(req.review.unwrap().mode, ReviewMode::Off);

    let (status, v) = start(&w, "k-3", &body(None)).await;
    assert_eq!(status, 201, "{v}");
    assert!(
        !stored_text(&w, v["runId"].as_str().unwrap()).contains("review"),
        "a request that keeps the defaults is the request it was"
    );
}

#[tokio::test]
async fn a_member_that_breaks_a_rule_is_refused_with_its_name_and_never_its_value() {
    let mut wrong_hash = serde_json::to_value(ReviewOptions::with_text("go on", None)).unwrap();
    wrong_hash["sha256"] = json!("0".repeat(64));
    let cases: Vec<(Value, &str, &str)> = vec![
        (
            serde_json::to_value(ReviewOptions::with_text("  \n ", None)).unwrap(),
            "text",
            "must not be empty",
        ),
        (wrong_hash, "sha256", "SHA-256 of text"),
        (
            json!({"mode": "off", "sha256": "0".repeat(64)}),
            "sha256",
            "sent only with text",
        ),
        (
            serde_json::to_value(ReviewOptions::with_text("go on", Some(5))).unwrap(),
            "guardS",
            "from 60 to 28800",
        ),
    ];
    for (i, (member, key, rule)) in cases.into_iter().enumerate() {
        let w = World::new();
        let (status, v) = start(&w, &format!("k-4-{i}"), &body(Some(member.clone()))).await;
        assert_eq!(status, 400, "{i}: {v}");
        assert_eq!(v["code"], "invalid-review", "{i}: {v}");
        let message = v["message"].as_str().unwrap();
        assert!(
            message.contains(key) && message.contains(rule),
            "{i}: {message}"
        );
        assert!(
            !message.contains("go on") && !message.contains(&"0".repeat(16)),
            "{message}"
        );
        untouched(&w);
    }
}

#[tokio::test]
async fn a_member_of_the_wrong_shape_is_a_bad_request() {
    for member in [
        json!({"mode": "always"}),
        json!({"mode": "auto", "later": 1}),
        json!({"text": "go"}),
        json!("off"),
        json!({"mode": "auto", "guardS": "300"}),
    ] {
        let w = World::new();
        let (status, v) = start(&w, "k-5", &body(Some(member.clone()))).await;
        assert_eq!(status, 400, "{member}: {v}");
        assert_eq!(v["code"], "bad-request", "{member}: {v}");
        untouched(&w);
    }
}
