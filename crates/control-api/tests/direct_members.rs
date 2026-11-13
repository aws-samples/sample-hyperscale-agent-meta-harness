//! `POST /v1/direct/runs` with the start-request members of package REQ1: `goal`, `research` and `thirdParty`,
//! and `attachments`.
//!
//! A member of the wrong type or with an unknown key is a 400 `bad-request` before anything is written (the request type refuses unknown
//! members). The attachments pass the one validator (`harness_protocol::direct::check_attachments`) that `ham` and agentd also call, so a bad
//! attachment is a 400 `invalid-attachments` with the words of the validator and never its content. A good member is stored as sent, so the
//! worker (agentd) and run-session read exactly what the user asked for.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, SONNET, World, deps, request};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{Attachment, GoalOptions, check_attachments};
use harness_protocol::research::{ResearchOptions, ThirdPartyMode};
use harness_protocol::store::s3;
use serde_json::{Value, json};

fn body() -> Value {
    json!({
        "prompt": "Add percent_change(a, b) with tests",
        "target": {"mode": "s3-only"},
        "model": SONNET,
        "rules": []
    })
}

fn with(member: &str, value: Value) -> Value {
    let mut b = body();
    b[member] = value;
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

fn stored(w: &World, run_id: &str) -> StartDirectRunRequest {
    let g = w.objects.lock().unwrap();
    serde_json::from_slice(&g[&s3::request_key(run_id)]).unwrap()
}

fn stored_text(w: &World, run_id: &str) -> String {
    let g = w.objects.lock().unwrap();
    String::from_utf8(g[&s3::request_key(run_id)].clone()).unwrap()
}

// ---- goal, research, thirdParty ----------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_goal_research_and_third_party_members_are_stored_as_sent() {
    let w = World::new();
    let mut b = body();
    b["goal"] = json!({"enabled": false});
    b["research"] = json!({"enabled": false});
    b["thirdParty"] = json!("allow");
    let (status, v) = start(&w, "k-1", &b).await;
    assert_eq!(status, 201, "{v}");
    let s = stored(&w, v["runId"].as_str().unwrap());
    assert_eq!(s.goal, Some(GoalOptions::OFF));
    assert_eq!(s.research, Some(ResearchOptions { enabled: false }));
    assert_eq!(s.third_party, Some(ThirdPartyMode::Allow));
}

#[tokio::test]
async fn a_request_without_the_members_stores_the_bytes_it_always_did() {
    let w = World::new();
    let (status, v) = start(&w, "k-1", &body()).await;
    assert_eq!(status, 201, "{v}");
    let text = stored_text(&w, v["runId"].as_str().unwrap());
    for member in ["goal", "research", "thirdParty", "attachments"] {
        assert!(!text.contains(member), "{member} in {text}");
    }
    // An explicit empty list is the same request: the same stored bytes, and a retry with either form is one run.
    let (status, again) = start(&w, "k-1", &with("attachments", json!([]))).await;
    assert_eq!(status, 201, "{again}");
    assert_eq!(again["runId"], v["runId"]);
}

#[tokio::test]
async fn a_member_of_the_wrong_type_or_with_an_unknown_key_is_a_400_and_nothing_is_written() {
    let cases = [
        ("goal", json!("off")),
        ("goal", json!({"enabled": "no"})),
        ("goal", json!({})),
        ("goal", json!({"enabled": false, "rounds": 3})),
        ("research", json!({"enabled": "false"})),
        ("research", json!({})),
        ("research", json!(false)),
        ("research", json!({"enabled": false, "provider": "tavily"})),
        ("thirdParty", json!("maybe")),
        ("thirdParty", json!(true)),
        ("thirdParty", json!("ALLOW")),
        ("attachments", json!("notes.md")),
        ("attachments", json!([{"name": "a.md"}])),
        (
            "attachments",
            json!([{"name": "a.md", "sha256": "x", "size": 1, "content": "x", "path": "/etc/hosts"}]),
        ),
    ];
    for (member, value) in cases {
        let w = World::new();
        let (status, v) = start(&w, "k-1", &with(member, value.clone())).await;
        assert_eq!(status, 400, "{member} {value}: {v}");
        assert_eq!(v["code"], "bad-request", "{member} {value}: {v}");
        untouched(&w);
    }
}

// ---- attachments --------------------------------------------------------------------------------------------------------------

fn items() -> Vec<Attachment> {
    vec![
        Attachment::new("api-notes.md", "The API takes a page token.\n"),
        Attachment::new("data.csv", "a,b\n1,2\n"),
    ]
}

#[tokio::test]
async fn good_attachments_are_stored_whole() {
    let w = World::new();
    let (status, v) = start(&w, "k-1", &with("attachments", json!(items()))).await;
    assert_eq!(status, 201, "{v}");
    let s = stored(&w, v["runId"].as_str().unwrap());
    assert_eq!(s.attachments, items());
    assert_eq!(check_attachments(&s.attachments), Ok(()));
}

#[tokio::test]
async fn every_bad_attachment_is_400_with_the_validators_words_and_nothing_is_written() {
    type Edit = Box<dyn Fn(&mut Vec<Attachment>)>;
    let cases: Vec<(&str, Edit)> = vec![
        ("bad_name", Box::new(|a| a[0].name = "../escape.md".into())),
        ("bad_name", Box::new(|a| a[0].name = "my notes.md".into())),
        (
            "duplicate_name",
            Box::new(|a| a[1].name = "API-NOTES.md".into()),
        ),
        ("empty", Box::new(|a| a[0] = Attachment::new("e.md", ""))),
        ("sha_mismatch", Box::new(|a| a[0].sha256 = "00".repeat(32))),
        ("size_mismatch", Box::new(|a| a[0].size += 1)),
        ("nul", Box::new(|a| a[0] = Attachment::new("n.md", "a\0b"))),
    ];
    for (want, edit) in cases {
        let w = World::new();
        let mut a = items();
        edit(&mut a);
        let expected = check_attachments(&a).unwrap_err();
        assert_eq!(
            expected.problem.code(),
            want,
            "the case is the problem it names"
        );
        let (status, v) = start(&w, "k-1", &with("attachments", json!(a))).await;
        assert_eq!(status, 400, "{want}: {v}");
        assert_eq!(v["code"], "invalid-attachments", "{want}: {v}");
        assert_eq!(v["message"], expected.to_string(), "{want}");
        untouched(&w);
    }
}

#[tokio::test]
async fn b776_attachments_over_the_old_caps_are_stored_whole() {
    // The old limits were 32 files, 256 KiB a file and 2 MiB in all; the request as a whole is held to the platform's bound instead.
    let w = World::new();
    let many: Vec<Attachment> = (0..40)
        .map(|i| Attachment::new(&format!("f{i}.txt"), &"x".repeat(100 * 1024)))
        .collect();
    let big = Attachment::new("big.txt", &"y".repeat(1024 * 1024));
    let all: Vec<Attachment> = many.into_iter().chain([big]).collect();
    let (status, v) = start(&w, "k-1", &with("attachments", json!(all))).await;
    assert_eq!(status, 201, "{v}");
    let s = stored(&w, v["runId"].as_str().unwrap());
    assert_eq!(s.attachments, all);
}

#[tokio::test]
async fn the_refusal_never_repeats_the_content_of_the_file() {
    let w = World::new();
    let mut a = items();
    a[0].size += 7;
    let (status, v) = start(&w, "k-1", &with("attachments", json!(a))).await;
    assert_eq!(status, 400, "{v}");
    let text = v.to_string();
    assert!(text.contains("api-notes.md"), "{text}");
    assert!(!text.contains("page token"), "{text}");
    untouched(&w);
}
