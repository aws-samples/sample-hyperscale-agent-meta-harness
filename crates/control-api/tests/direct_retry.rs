//! `POST /v1/runs/{runId}/retry`: a run that ended `blocked` is started again with the request it was started with. Nothing in the
//! client holds that request, so control-api reads it from the stored `runs/<id>/input/request.json`, checks it the way a fresh start is
//! checked, and starts a new run whose stored request is the same bytes. The new run's Run item names the blocked run (`continueFrom` in the
//! run rows), which is a link and nothing more: the stored request keeps its own `continueFrom` (with its head) when the blocked run was a
//! follow-up, and carries none when it was an ordinary run.
//!
//! The key of an idempotent retry is chosen by the client (`retry-<blocked run>`), so a repeat, or a second press, returns the run the first
//! one started. The claim hashes the blocked run's id with the request, so one key cannot return the retry of another run.
//!
//! The stand-in behind `Store`, `Blobs` and `Execution` is the one the other chat-client routes use; the handlers are the real ones.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, OTHER, SONNET, World, deps, get, request, run_item};
use harness_protocol::store::s3;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

const BASE_RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const BASE: &str = "1111111111111111111111111111111111111111";
const REPO: &str = "cc:hamh-e2e-python-calc";

fn bundle(head: &str, run: &str) -> Vec<u8> {
    let mut b =
        format!("# v2 git bundle\n-{BASE} base subject\n{head} refs/heads/harness/{run}\n\nPACK")
            .into_bytes();
    b.extend(std::iter::repeat_n(7u8, 3000));
    b
}

/// A request with a member of nearly every kind, so "the same request" is a strong claim.
fn rich_body() -> Value {
    let rule = "Write the test first.\n";
    json!({
        "prompt": "Add percent_change(a, b) with tests\n\n  keep   the spacing",
        "target": {"mode": "s3-only"},
        "model": SONNET,
        "rules": [{"path": "tdd.md", "sha256": control_api::ids::sha256_hex(rule.as_bytes()), "content": rule}],
        "inputWaitS": 1800,
        "goal": {"enabled": false},
        "research": {"enabled": false},
        "thirdParty": "allow",
        "reportTests": false
    })
}

async fn start(w: &World, key: &str, b: &Value) -> String {
    let mut r = request(ME, "POST", "/v1/direct/runs", &[], &b.to_string());
    r.headers.insert("idempotency-key".into(), key.into());
    let out = control_api::handler::handle(r, &deps(w)).await;
    let v: Value = serde_json::from_str(&out.body).unwrap();
    assert_eq!(out.status, 201, "{v}");
    v["runId"].as_str().unwrap().to_string()
}

/// A run of `ME` started through the real handler (so its request was stored by the real code) and then ended blocked.
async fn blocked_run(w: &World, key: &str, b: &Value) -> String {
    let id = start(w, key, b).await;
    block(w, &id);
    id
}

fn set_status(w: &World, id: &str, status: &str) {
    w.runs.lock().unwrap().get_mut(id).unwrap().status =
        serde_json::from_value(json!(status)).unwrap();
}

fn block(w: &World, id: &str) {
    set_status(w, id, "blocked");
}

async fn retry_as(w: &World, caller: &str, run: &str, key: Option<&str>) -> (u16, Value) {
    let mut r = request(caller, "POST", &format!("/v1/runs/{run}/retry"), &[], "{}");
    if let Some(k) = key {
        r.headers.insert("idempotency-key".into(), k.into());
    }
    let out = control_api::handler::handle(r, &deps(w)).await;
    (
        out.status,
        serde_json::from_str(&out.body).unwrap_or(Value::Null),
    )
}

async fn retry(w: &World, run: &str) -> (u16, Value) {
    retry_as(w, ME, run, Some(&format!("retry-{run}"))).await
}

fn stored_bytes(w: &World, run: &str) -> Vec<u8> {
    w.objects.lock().unwrap()[&s3::request_key(run)].clone()
}

fn code(v: &Value) -> &str {
    v["code"]
        .as_str()
        .unwrap_or_else(|| panic!("no error code in {v}"))
}

/// What a refused retry leaves behind: the runs, claims, executions and blobs it found.
struct Mark {
    runs: usize,
    claims: usize,
    started: usize,
    blobs: usize,
}

fn mark(w: &World) -> Mark {
    Mark {
        runs: w.runs.lock().unwrap().len(),
        claims: w.idem.lock().unwrap().len(),
        started: w.started.lock().unwrap().len(),
        blobs: w.count("blob_put"),
    }
}

fn unchanged(w: &World, before: &Mark) {
    let now = mark(w);
    assert_eq!(now.runs, before.runs, "no run was created");
    assert_eq!(now.claims, before.claims, "no claim was written");
    assert_eq!(now.started, before.started, "no execution started");
    assert_eq!(now.blobs, before.blobs, "no blob was written");
}

// ---- the retry starts a new run with the same request ----------------------------------------------------------------------------

#[tokio::test]
async fn a_blocked_run_is_retried_as_a_new_run_with_the_request_it_was_started_with() {
    let w = World::new();
    let id = blocked_run(&w, "k-1", &rich_body()).await;
    let (status, v) = retry(&w, &id).await;
    assert_eq!(status, 201, "{v}");
    let new = v["runId"].as_str().unwrap();
    assert_ne!(new, id, "a new run, not the blocked one");
    assert_eq!(v["status"], json!("active"), "{v}");
    // The bytes are the blocked run's own, member for member: the prompt as typed, the rules, the switches, the model.
    assert_eq!(stored_bytes(&w, new), stored_bytes(&w, &id));
    let stored: Value = serde_json::from_slice(&stored_bytes(&w, new)).unwrap();
    assert_eq!(stored["prompt"], rich_body()["prompt"]);
    assert_eq!(stored["model"], json!(SONNET));
    assert_eq!(stored["goal"], json!({"enabled": false}));
    assert_eq!(stored["thirdParty"], json!("allow"));
    assert!(
        stored.get("continueFrom").is_none(),
        "an ordinary run has no member to carry over: {stored}"
    );
    // One execution per run, the new one named after the new run.
    assert_eq!(
        *w.started.lock().unwrap(),
        vec![id.clone(), new.to_string()]
    );
    // The blocked run is not touched.
    let runs = w.runs.lock().unwrap();
    assert_eq!(
        runs[&id].status,
        serde_json::from_value(json!("blocked")).unwrap()
    );
    assert_eq!(runs[&id].continue_from, None);
    // The new run is the caller's, on the same target, and names the blocked run.
    assert_eq!(runs[new].owner_principal_arn, ME);
    assert_eq!(runs[new].target, runs[&id].target);
    assert_eq!(runs[new].continue_from.as_deref(), Some(id.as_str()));
}

#[tokio::test]
async fn the_run_rows_and_the_detail_name_the_blocked_run() {
    let w = World::new();
    let id = blocked_run(&w, "k-1", &rich_body()).await;
    let (_, v) = retry(&w, &id).await;
    let new = v["runId"].as_str().unwrap().to_string();
    let (status, list) = get(&w, ME, "/v1/runs", &[]).await;
    assert_eq!(status, 200, "{list}");
    let rows = list["runs"].as_array().unwrap();
    let row = |run: &str| rows.iter().find(|r| r["runId"] == run).unwrap();
    assert_eq!(row(&new)["continueFrom"], json!(id), "{list}");
    assert!(row(&id).get("continueFrom").is_none(), "{list}");
    let (status, detail) = get(&w, ME, &format!("/v1/runs/{new}"), &[]).await;
    assert_eq!(status, 200, "{detail}");
    assert_eq!(detail["summary"]["continueFrom"], json!(id), "{detail}");
}

#[tokio::test]
async fn the_new_run_has_its_own_search_item() {
    let w = World::new();
    let id = blocked_run(&w, "k-1", &rich_body()).await;
    let (_, v) = retry(&w, &id).await;
    let new = v["runId"].as_str().unwrap();
    let indexed: BTreeSet<String> = w.indexed(ME).into_iter().map(|i| i.run_id).collect();
    assert!(indexed.contains(new), "{indexed:?}");
    assert!(indexed.contains(&id), "{indexed:?}");
}

#[tokio::test]
async fn a_repository_run_is_retried_on_its_repository_and_base() {
    let w = World::new();
    let mut b = rich_body();
    b["target"] = json!({"mode": "repo", "repo": REPO});
    b["baseRef"] = json!("main");
    let id = blocked_run(&w, "k-1", &b).await;
    let (status, v) = retry(&w, &id).await;
    assert_eq!(status, 201, "{v}");
    let new = v["runId"].as_str().unwrap();
    let stored: Value = serde_json::from_slice(&stored_bytes(&w, new)).unwrap();
    assert_eq!(stored["target"], json!({"mode": "repo", "repo": REPO}));
    assert_eq!(stored["baseRef"], json!("main"));
    let runs = w.runs.lock().unwrap();
    assert_eq!(runs[new].target, runs[&id].target);
}

#[tokio::test]
async fn a_retry_of_a_follow_up_keeps_the_head_it_started_from_and_names_the_blocked_run() {
    let w = World::new();
    w.add_run(run_item(
        BASE_RUN,
        ME,
        "complete",
        "2026-10-15T13:00:00Z",
        json!({"mode": "s3-only"}),
    ));
    w.put_object(&s3::verified_bundle_key(BASE_RUN), bundle(HEAD, BASE_RUN));
    let mut b = rich_body();
    b["continueFrom"] = json!({"runId": BASE_RUN});
    let follow_up = blocked_run(&w, "k-1", &b).await;
    let (status, v) = retry(&w, &follow_up).await;
    assert_eq!(status, 201, "{v}");
    let new = v["runId"].as_str().unwrap();
    let stored: Value = serde_json::from_slice(&stored_bytes(&w, new)).unwrap();
    // The request still seeds from the run it always seeded from, with the head it resolved then.
    assert_eq!(
        stored["continueFrom"],
        json!({"runId": BASE_RUN, "headSha": HEAD}),
        "{stored}"
    );
    // The link of the Run item is the blocked run.
    assert_eq!(
        w.runs.lock().unwrap()[new].continue_from.as_deref(),
        Some(follow_up.as_str())
    );
}

#[tokio::test]
async fn a_retry_of_a_follow_up_whose_base_run_lost_its_bundle_starts_nothing() {
    let w = World::new();
    w.add_run(run_item(
        BASE_RUN,
        ME,
        "complete",
        "2026-10-15T13:00:00Z",
        json!({"mode": "s3-only"}),
    ));
    w.put_object(&s3::verified_bundle_key(BASE_RUN), bundle(HEAD, BASE_RUN));
    let mut b = rich_body();
    b["continueFrom"] = json!({"runId": BASE_RUN});
    let follow_up = blocked_run(&w, "k-1", &b).await;
    w.objects
        .lock()
        .unwrap()
        .remove(&s3::verified_bundle_key(BASE_RUN));
    let before = mark(&w);
    let (status, v) = retry(&w, &follow_up).await;
    assert_eq!((status, code(&v)), (409, "no-delivered-head"), "{v}");
    unchanged(&w, &before);
}

// ---- idempotency ----------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_same_key_returns_the_run_the_first_retry_started() {
    let w = World::new();
    let id = blocked_run(&w, "k-1", &rich_body()).await;
    let (_, first) = retry(&w, &id).await;
    let before = mark(&w);
    for _ in 0..3 {
        let (status, again) = retry(&w, &id).await;
        assert_eq!(status, 201, "{again}");
        assert_eq!(again["runId"], first["runId"], "{again}");
    }
    let now = mark(&w);
    assert_eq!(
        (now.runs, now.claims, now.blobs),
        (before.runs, before.claims, before.blobs)
    );
    let started: BTreeSet<String> = w.started.lock().unwrap().iter().cloned().collect();
    assert_eq!(
        started.len(),
        2,
        "the blocked run and one retry: {started:?}"
    );
}

#[tokio::test]
async fn one_key_never_returns_the_retry_of_another_run() {
    let w = World::new();
    // Two blocked runs with byte-for-byte the same request.
    let a = blocked_run(&w, "k-1", &rich_body()).await;
    let b = blocked_run(&w, "k-2", &rich_body()).await;
    assert_eq!(stored_bytes(&w, &a), stored_bytes(&w, &b));
    let (status, first) = retry_as(&w, ME, &a, Some("shared-key")).await;
    assert_eq!(status, 201, "{first}");
    let before = mark(&w);
    let (status, second) = retry_as(&w, ME, &b, Some("shared-key")).await;
    assert_eq!(
        (status, code(&second)),
        (422, "idempotency-key-reused"),
        "{second}"
    );
    let now = mark(&w);
    assert_eq!(
        (now.runs, now.started, now.blobs),
        (before.runs, before.started, before.blobs)
    );
}

#[tokio::test]
async fn a_start_key_cannot_be_replayed_as_a_retry() {
    let w = World::new();
    let id = blocked_run(&w, "retry-key", &rich_body()).await;
    // The first start used the key `retry-key`; the same key on the retry route is another request.
    let before = mark(&w);
    let (status, v) = retry_as(&w, ME, &id, Some("retry-key")).await;
    assert_eq!((status, code(&v)), (422, "idempotency-key-reused"), "{v}");
    unchanged(&w, &before);
}

// ---- who may retry what ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn only_a_blocked_run_is_retried() {
    for (state, want_status, want_code) in [
        ("active", 409, "run-active"),
        ("complete", 409, "run-not-blocked"),
        ("cleared", 409, "run-not-blocked"),
        ("paused", 409, "run-not-blocked"),
        ("usage_limited", 409, "run-not-blocked"),
        ("budget_limited", 409, "run-not-blocked"),
    ] {
        let w = World::new();
        let id = start(&w, "k-1", &rich_body()).await;
        set_status(&w, &id, state);
        let before = mark(&w);
        let (status, v) = retry(&w, &id).await;
        assert_eq!((status, code(&v)), (want_status, want_code), "{state}: {v}");
        assert!(
            v["message"].as_str().unwrap().contains(&id),
            "the words name the run: {v}"
        );
        unchanged(&w, &before);
    }
}

#[tokio::test]
async fn another_callers_run_and_a_missing_run_answer_alike_and_nothing_is_read() {
    let w = World::new();
    let id = blocked_run(&w, "k-1", &rich_body()).await;
    // The control: the owner's retry of a run of its own works, so the 404s below are the route's refusals and not a missing route.
    let own = blocked_run(&w, "k-0", &rich_body()).await;
    let (status, v) = retry(&w, &own).await;
    assert_eq!(status, 201, "{v}");
    let reads_before = w.count("read_prefix");
    let before = mark(&w);
    let bad_ids = [
        id.clone(),
        "01K6A0000000000000000000ZZ".to_string(),
        "not-a-run-id".to_string(),
        "..".to_string(),
    ];
    for (caller, run) in [
        (OTHER, bad_ids[0].as_str()),
        (ME, bad_ids[1].as_str()),
        (ME, bad_ids[2].as_str()),
        (ME, bad_ids[3].as_str()),
    ] {
        let (status, v) = retry_as(&w, caller, run, Some("k")).await;
        assert_eq!(
            (status, code(&v)),
            (404, "not-found"),
            "{caller} {run}: {v}"
        );
    }
    assert_eq!(
        w.count("read_prefix"),
        reads_before,
        "the stored request is read only after the caller's ownership is known"
    );
    unchanged(&w, &before);
}

#[tokio::test]
async fn the_route_needs_a_valid_idempotency_key_and_takes_no_other_method() {
    let w = World::new();
    let id = blocked_run(&w, "k-1", &rich_body()).await;
    let before = mark(&w);
    let (status, v) = retry_as(&w, ME, &id, None).await;
    assert_eq!((status, code(&v)), (400, "idempotency-key-required"), "{v}");
    let (status, v) = retry_as(&w, ME, &id, Some("has a space")).await;
    assert_eq!((status, code(&v)), (400, "bad-request"), "{v}");
    let (status, v) = retry_as(&w, ME, &id, Some(&"k".repeat(129))).await;
    assert_eq!((status, code(&v)), (400, "bad-request"), "{v}");
    unchanged(&w, &before);
    let (status, v) = get(&w, ME, &format!("/v1/runs/{id}/retry"), &[]).await;
    assert_eq!((status, code(&v)), (405, "method-not-allowed"), "{v}");
}

// ---- the stored request is checked like a fresh one ---------------------------------------------------------------------------

#[tokio::test]
async fn a_model_the_stage_no_longer_offers_is_refused_with_the_words_of_a_fresh_start() {
    let w = World::new();
    let id = blocked_run(&w, "k-1", &rich_body()).await;
    // The stage's model list moved on since the run was started: its stored request names a model that is not there.
    let mut stored: Value = serde_json::from_slice(&stored_bytes(&w, &id)).unwrap();
    stored["model"] = json!("global.example.retired-model-1");
    w.put_object(&s3::request_key(&id), serde_json::to_vec(&stored).unwrap());
    let before = mark(&w);
    let (status, v) = retry(&w, &id).await;
    assert_eq!((status, code(&v)), (422, "unknown-model"), "{v}");
    unchanged(&w, &before);
}

#[tokio::test]
async fn a_stored_request_that_is_missing_or_unreadable_is_refused_in_plain_words_and_leaks_nothing()
 {
    let secret = "SECRET-PROMPT-TEXT-DO-NOT-ECHO";
    for stored in [
        None,
        Some(b"{ this is not json".to_vec()),
        Some(
            serde_json::to_vec(&json!({"prompt": secret, "target": {"mode": "s3-only"},
                "model": SONNET, "somethingNew": 1}))
            .unwrap(),
        ),
        Some(vec![b' '; 9 * 1024 * 1024]),
    ] {
        let w = World::new();
        let id = blocked_run(&w, "k-1", &rich_body()).await;
        match stored {
            Some(bytes) => w.put_object(&s3::request_key(&id), bytes),
            None => {
                w.objects.lock().unwrap().remove(&s3::request_key(&id));
            }
        }
        let before = mark(&w);
        let (status, v) = retry(&w, &id).await;
        assert_eq!(
            (status, code(&v)),
            (422, "stored-request-unreadable"),
            "{v}"
        );
        assert!(v["message"].as_str().unwrap().contains(&id), "{v}");
        assert!(
            !v.to_string().contains(secret),
            "no stored content in the answer: {v}"
        );
        unchanged(&w, &before);
    }
}

#[tokio::test]
async fn a_valid_request_with_blanks_after_it_past_the_read_limit_is_a_cut_read_and_is_refused() {
    // The route reads the first 8 MiB and compares them with the object's size. Here the first 8 MiB are a request that parses by itself, and
    // blanks follow it up to 9 MiB, so the parse alone would accept it: only the size says that the read was cut.
    let w = World::new();
    let id = blocked_run(&w, "k-1", &rich_body()).await;
    let mut bytes = stored_bytes(&w, &id);
    assert!(bytes.len() < 8 * 1024 * 1024);
    bytes.resize(9 * 1024 * 1024, b' ');
    w.put_object(&s3::request_key(&id), bytes);
    let before = mark(&w);
    let (status, v) = retry(&w, &id).await;
    assert_eq!(
        (status, code(&v)),
        (422, "stored-request-unreadable"),
        "{v}"
    );
    unchanged(&w, &before);
}

#[tokio::test]
async fn a_store_that_cannot_be_read_is_a_retry_later_and_claims_nothing() {
    let w = World::new();
    let id = blocked_run(&w, "k-1", &rich_body()).await;
    w.objects_down.store(true, Ordering::SeqCst);
    let before = mark(&w);
    let (status, v) = retry(&w, &id).await;
    assert_eq!((status, code(&v)), (503, "unavailable"), "{v}");
    assert!(v["message"].as_str().unwrap().contains("retry"), "{v}");
    unchanged(&w, &before);
    // The store comes back: the same key now starts the retry.
    w.objects_down.store(false, Ordering::SeqCst);
    let (status, v) = retry(&w, &id).await;
    assert_eq!(status, 201, "{v}");
}

#[tokio::test]
async fn a_role_without_the_grant_to_read_the_request_is_a_retry_later_and_claims_nothing() {
    // S3 answers a GET of a key the role may not read (and of an absent key, for a role that may not list the bucket) with 403; the adapter
    // reports it as unavailable. The retry must not claim the key, so the same key works once the grant is there.
    let w = World::new();
    let id = blocked_run(&w, "k-1", &rich_body()).await;
    w.objects.lock().unwrap().remove(&s3::request_key(&id));
    w.no_list_permission.store(true, Ordering::SeqCst);
    let before = mark(&w);
    let (status, v) = retry(&w, &id).await;
    assert_eq!((status, code(&v)), (503, "unavailable"), "{v}");
    unchanged(&w, &before);
}
