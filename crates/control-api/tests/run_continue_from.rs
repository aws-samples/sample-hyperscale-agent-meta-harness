//! A follow-up run (`POST /v1/direct/runs` with `continueFrom`) is stored with the id of the run it continues from, and
//! `GET /v1/runs` (every row) and `GET /v1/runs/{id}` (`summary`) return it as `continueFrom`. An ordinary run has no such member. The stand-in
//! behind `Store`, `Blobs` and `Execution` is the one the other chat-client routes use; the handlers are the real ones.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, OTHER, SONNET, World, deps, get, request, run_item};
use harness_protocol::store::s3;
use serde_json::{Value, json};

const SOURCE: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const BASE: &str = "1111111111111111111111111111111111111111";

fn bundle(head: &str, run: &str) -> Vec<u8> {
    let mut b =
        format!("# v2 git bundle\n-{BASE} base subject\n{head} refs/heads/harness/{run}\n\nPACK")
            .into_bytes();
    b.extend(std::iter::repeat_n(7u8, 3000));
    b
}

/// A finished run of `ME` whose verified bundle names `HEAD`.
fn world_with_source() -> World {
    let w = World::new();
    w.add_run(run_item(
        SOURCE,
        ME,
        "complete",
        "2026-10-15T13:00:00Z",
        json!({"mode": "s3-only"}),
    ));
    w.put_object(&s3::verified_bundle_key(SOURCE), bundle(HEAD, SOURCE));
    w
}

fn body() -> Value {
    json!({"prompt": "Add percent_change(a, b) with tests", "target": {"mode": "s3-only"}, "model": SONNET, "rules": []})
}

fn follow_up(run: &str) -> Value {
    let mut b = body();
    b["continueFrom"] = json!({"runId": run});
    b
}

async fn start(w: &World, key: &str, b: &Value) -> String {
    let mut r = request(ME, "POST", "/v1/direct/runs", &[], &b.to_string());
    r.headers.insert("idempotency-key".into(), key.into());
    let out = control_api::handler::handle(r, &deps(w)).await;
    let v: Value = serde_json::from_str(&out.body).unwrap();
    assert_eq!(out.status, 201, "{v}");
    v["runId"].as_str().unwrap().to_string()
}

fn row<'a>(list: &'a Value, id: &str) -> &'a Value {
    list["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["runId"] == id)
        .unwrap_or_else(|| panic!("run {id} is not in {list}"))
}

#[tokio::test]
async fn a_follow_up_run_is_stored_with_the_run_it_continues_from() {
    let w = world_with_source();
    let id = start(&w, "k-1", &follow_up(SOURCE)).await;
    let stored = w.runs.lock().unwrap().get(&id).cloned().unwrap();
    assert_eq!(stored.continue_from.as_deref(), Some(SOURCE));
    assert_eq!(
        w.runs.lock().unwrap().get(SOURCE).unwrap().continue_from,
        None,
        "the run it continues from is an ordinary run"
    );
}

#[tokio::test]
async fn the_run_list_names_the_base_run_of_a_follow_up_and_no_other_row() {
    let w = world_with_source();
    let id = start(&w, "k-1", &follow_up(SOURCE)).await;
    let (status, list) = get(&w, ME, "/v1/runs", &[]).await;
    assert_eq!(status, 200, "{list}");
    assert_eq!(row(&list, &id)["continueFrom"], json!(SOURCE), "{list}");
    assert!(
        row(&list, SOURCE).get("continueFrom").is_none(),
        "an ordinary run has no member: {list}"
    );
}

#[tokio::test]
async fn the_run_detail_names_the_base_run_in_its_summary() {
    let w = world_with_source();
    let id = start(&w, "k-1", &follow_up(SOURCE)).await;
    let (status, detail) = get(&w, ME, &format!("/v1/runs/{id}"), &[]).await;
    assert_eq!(status, 200, "{detail}");
    assert_eq!(detail["summary"]["continueFrom"], json!(SOURCE), "{detail}");
    assert!(
        detail.get("continueFrom").is_none(),
        "the detail carries it only through its summary: {detail}"
    );

    let (status, base) = get(&w, ME, &format!("/v1/runs/{SOURCE}"), &[]).await;
    assert_eq!(status, 200, "{base}");
    assert!(
        base["summary"].get("continueFrom").is_none(),
        "an ordinary run's detail has no member: {base}"
    );
}

#[tokio::test]
async fn an_ordinary_run_started_now_has_no_member_in_the_list_or_the_detail() {
    let w = world_with_source();
    let id = start(&w, "k-1", &body()).await;
    assert_eq!(w.runs.lock().unwrap().get(&id).unwrap().continue_from, None);
    let (_, list) = get(&w, ME, "/v1/runs", &[]).await;
    assert!(row(&list, &id).get("continueFrom").is_none(), "{list}");
    let (_, detail) = get(&w, ME, &format!("/v1/runs/{id}"), &[]).await;
    assert!(detail["summary"].get("continueFrom").is_none(), "{detail}");
}

#[tokio::test]
async fn a_follow_up_of_a_follow_up_names_its_own_base_only() {
    let w = world_with_source();
    let second = start(&w, "k-1", &follow_up(SOURCE)).await;
    // The second run finishes and delivers a head of its own; a third run continues from it.
    w.runs.lock().unwrap().get_mut(&second).unwrap().status =
        serde_json::from_value(json!("complete")).unwrap();
    w.put_object(&s3::verified_bundle_key(&second), bundle(HEAD, &second));
    let third = start(&w, "k-2", &follow_up(&second)).await;
    let (_, list) = get(&w, ME, "/v1/runs", &[]).await;
    assert_eq!(row(&list, &second)["continueFrom"], json!(SOURCE), "{list}");
    assert_eq!(row(&list, &third)["continueFrom"], json!(second), "{list}");
    assert!(row(&list, SOURCE).get("continueFrom").is_none(), "{list}");
}

#[tokio::test]
async fn another_callers_list_never_shows_the_follow_up_or_its_base() {
    let w = world_with_source();
    let id = start(&w, "k-1", &follow_up(SOURCE)).await;
    let (status, list) = get(&w, OTHER, "/v1/runs", &[]).await;
    assert_eq!(status, 200, "{list}");
    assert!(list["runs"].as_array().unwrap().is_empty(), "{list}");
    let (status, _) = get(&w, OTHER, &format!("/v1/runs/{id}"), &[]).await;
    assert_eq!(status, 404);
}
