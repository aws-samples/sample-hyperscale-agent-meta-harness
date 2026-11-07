//! In control-api: `POST /v1/runs/{runId}/resume` starts a follow-up run from the code a paused or stalled run delivered, with the
//! request that run was started with and a note that the new run continues; a repeat of the key returns the same run; a run that is not paused or
//! stalled is refused in its own words before anything is written. The total run time (`maxRunS`) is checked and stored, and the run rows carry the
//! reason, the resume mode and the session of a run that stopped delivered. The stand-in behind `Store`, `Blobs` and `Execution` is the one the
//! other chat-client routes use; the handlers are the real ones.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, OTHER, SONNET, World, deps, get, request};
use harness_protocol::goal::ResumeMode;
use harness_protocol::store::s3;
use serde_json::{Value, json};

const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const BASE: &str = "1111111111111111111111111111111111111111";
const REASON: &str = "total_time: the run used the total time it was given";

fn bundle(head: &str, run: &str) -> Vec<u8> {
    let mut b =
        format!("# v2 git bundle\n-{BASE} base subject\n{head} refs/heads/harness/{run}\n\nPACK")
            .into_bytes();
    b.extend(std::iter::repeat_n(7u8, 3000));
    b
}

fn body() -> Value {
    json!({
        "prompt": "Add percent_change(a, b) with tests\n\n  keep   the spacing",
        "target": {"mode": "s3-only"}, "model": SONNET, "rules": [], "inputWaitS": 1800
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

/// A run of `ME` started through the real handler, then ended in `status` with the reason and mode the service stores, and a delivered bundle.
async fn stopped_run(w: &World, key: &str, status: &str, mode: ResumeMode) -> String {
    let (code, v) = start(w, key, &body()).await;
    assert_eq!(code, 201, "{v}");
    let id = v["runId"].as_str().unwrap().to_string();
    {
        let mut runs = w.runs.lock().unwrap();
        let run = runs.get_mut(&id).unwrap();
        run.status = serde_json::from_value(json!(status)).unwrap();
        run.status_reason = Some(REASON.to_string());
        run.resume_mode = Some(mode);
        run.slice = Some(3);
    }
    w.put_object(&s3::verified_bundle_key(&id), bundle(HEAD, &id));
    id
}

async fn resume_as(w: &World, caller: &str, run: &str, key: Option<&str>) -> (u16, Value) {
    let mut r = request(caller, "POST", &format!("/v1/runs/{run}/resume"), &[], "{}");
    if let Some(k) = key {
        r.headers.insert("idempotency-key".into(), k.into());
    }
    let out = control_api::handler::handle(r, &deps(w)).await;
    (
        out.status,
        serde_json::from_str(&out.body).unwrap_or(Value::Null),
    )
}

async fn resume(w: &World, run: &str) -> (u16, Value) {
    resume_as(w, ME, run, Some(&format!("resume-{run}"))).await
}

fn stored(w: &World, run: &str) -> Value {
    let g = w.objects.lock().unwrap();
    serde_json::from_slice(&g[&s3::request_key(run)]).unwrap()
}

fn code(v: &Value) -> &str {
    v["code"]
        .as_str()
        .unwrap_or_else(|| panic!("no error code in {v}"))
}

#[tokio::test]
async fn b689_a_paused_run_is_resumed_as_a_follow_up_with_its_request_and_a_continuation_note() {
    let w = World::new();
    let id = stopped_run(&w, "k-1", "paused", ResumeMode::FollowUp).await;
    let (status, v) = resume(&w, &id).await;
    assert_eq!(status, 201, "{v}");
    let new = v["runId"].as_str().unwrap();
    assert_ne!(new, id, "a new run, not the stopped one");
    assert_eq!(v["status"], json!("active"));
    let s = stored(&w, new);
    // the person's prompt is first and as typed; the harness's paragraph follows and names the run and why it stopped
    let prompt = s["prompt"].as_str().unwrap();
    assert!(
        prompt.starts_with(body()["prompt"].as_str().unwrap()),
        "{prompt}"
    );
    assert!(prompt.contains("This is a continuation"), "{prompt}");
    assert!(
        prompt.contains(&id) && prompt.contains("the run used the total time it was given"),
        "{prompt}"
    );
    assert!(
        !prompt.contains("total_time:"),
        "the code stays out of the sentence: {prompt}"
    );
    // the delivered head of the stopped run is the base, and everything else is the stopped run's request
    assert_eq!(s["continueFrom"], json!({"runId": id, "headSha": HEAD}));
    assert_eq!(s["model"], body()["model"]);
    assert_eq!(s["inputWaitS"], json!(1800));
    // the new run names the run it continues, and the stopped run keeps its status
    assert_eq!(
        w.runs
            .lock()
            .unwrap()
            .get(new)
            .unwrap()
            .continue_from
            .as_deref(),
        Some(id.as_str())
    );
    assert_eq!(
        serde_json::to_value(w.runs.lock().unwrap().get(&id).unwrap().status).unwrap(),
        json!("paused")
    );
}

#[tokio::test]
async fn b689_a_stalled_run_is_resumed_in_the_same_way() {
    let w = World::new();
    let id = stopped_run(&w, "k-1", "stalled", ResumeMode::FollowUp).await;
    let (status, v) = resume(&w, &id).await;
    assert_eq!(status, 201, "{v}");
}

#[tokio::test]
async fn b689_a_second_press_returns_the_run_the_first_one_started() {
    let w = World::new();
    let id = stopped_run(&w, "k-1", "paused", ResumeMode::FollowUp).await;
    let (_, first) = resume(&w, &id).await;
    let runs_after_first = w.runs.lock().unwrap().len();
    let (status, second) = resume(&w, &id).await;
    assert_eq!(status, 201, "{second}");
    assert_eq!(
        second["runId"], first["runId"],
        "the same key is the same run"
    );
    assert_eq!(
        w.runs.lock().unwrap().len(),
        runs_after_first,
        "no second run was made"
    );
}

#[tokio::test]
async fn b689_a_run_that_is_not_paused_or_stalled_is_refused_in_its_own_words_and_nothing_is_written()
 {
    let w = World::new();
    for (status, want_code, want_word) in [
        ("active", "run-active", "has not ended"),
        ("complete", "run-not-resumable", "follow-up"),
        ("blocked", "run-not-resumable", "retry"),
        ("cleared", "run-not-resumable", "cleared"),
        ("usage_limited", "run-not-resumable", "limit"),
    ] {
        let id = stopped_run(&w, &format!("k-{status}"), status, ResumeMode::FollowUp).await;
        let runs = w.runs.lock().unwrap().len();
        let claims = w.idem.lock().unwrap().len();
        let (code_, v) = resume(&w, &id).await;
        assert_eq!(code_, 409, "{status}: {v}");
        assert_eq!(code(&v), want_code, "{status}: {v}");
        assert!(
            v["message"].as_str().unwrap().contains(want_word),
            "{status}: {v}"
        );
        assert_eq!(
            w.runs.lock().unwrap().len(),
            runs,
            "{status}: no run was created"
        );
        assert_eq!(
            w.idem.lock().unwrap().len(),
            claims,
            "{status}: no claim was written"
        );
    }
}

#[tokio::test]
async fn b689_a_stopped_run_that_delivered_nothing_to_continue_from_is_refused() {
    let w = World::new();
    let none = stopped_run(&w, "k-1", "stalled", ResumeMode::None).await;
    let (status, v) = resume(&w, &none).await;
    assert_eq!((status, code(&v)), (409, "run-not-resumable"), "{v}");
    // a run whose bundle is gone answers as a follow-up does
    let lost = stopped_run(&w, "k-2", "paused", ResumeMode::FollowUp).await;
    w.objects
        .lock()
        .unwrap()
        .remove(&s3::verified_bundle_key(&lost));
    let (status, v) = resume(&w, &lost).await;
    assert_eq!((status, code(&v)), (409, "no-delivered-head"), "{v}");
}

#[tokio::test]
async fn b689_another_callers_run_answers_like_a_missing_one() {
    let w = World::new();
    let id = stopped_run(&w, "k-1", "paused", ResumeMode::FollowUp).await;
    let (status, v) = resume_as(&w, OTHER, &id, Some(&format!("resume-{id}"))).await;
    assert_eq!(status, 404, "{v}");
    let (status, v) = resume_as(&w, ME, "01K5ZQ4T8M3N6P7R9S0V1W2X3Y", Some("resume-x")).await;
    assert_eq!(status, 404, "{v}");
}

#[tokio::test]
async fn b689_the_resume_needs_an_idempotency_key() {
    let w = World::new();
    let id = stopped_run(&w, "k-1", "paused", ResumeMode::FollowUp).await;
    let (status, v) = resume_as(&w, ME, &id, None).await;
    assert_eq!((status, code(&v)), (400, "idempotency-key-required"), "{v}");
}

#[tokio::test]
async fn b689_a_paused_run_takes_no_cancel() {
    let w = World::new();
    let id = stopped_run(&w, "k-1", "paused", ResumeMode::FollowUp).await;
    let mut r = request(ME, "POST", &format!("/v1/runs/{id}/cancel"), &[], "{}");
    r.headers
        .insert("idempotency-key".into(), "cancel-1".into());
    let out = control_api::handler::handle(r, &deps(&w)).await;
    assert_eq!(out.status, 409, "{}", out.body);
}

#[tokio::test]
async fn b689_the_run_list_and_the_detail_carry_the_reason_the_mode_and_the_session() {
    let w = World::new();
    let id = stopped_run(&w, "k-1", "stalled", ResumeMode::FollowUp).await;
    let (status, list) = get(&w, ME, "/v1/runs", &[]).await;
    assert_eq!(status, 200, "{list}");
    let row = list["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["runId"] == json!(id))
        .unwrap()
        .clone();
    assert_eq!(row["status"], json!("stalled"));
    assert_eq!(row["statusReason"], json!(REASON));
    assert_eq!(row["resumeMode"], json!("follow_up"));
    assert_eq!(row["session"], json!(3));
    let (status, detail) = get(&w, ME, &format!("/v1/runs/{id}"), &[]).await;
    assert_eq!(status, 200, "{detail}");
    assert_eq!(detail["summary"]["statusReason"], json!(REASON));
    assert_eq!(detail["summary"]["session"], json!(3));
}

#[tokio::test]
async fn b688_a_total_run_time_outside_the_limits_is_refused_before_anything_is_written() {
    for bad in [0u64, 1_799, 604_801, 10_000_000] {
        let w = World::new();
        let mut b = body();
        b["maxRunS"] = json!(bad);
        let (status, v) = start(&w, "k-1", &b).await;
        assert_eq!(status, 400, "{bad}: {v}");
        assert!(v["message"].as_str().unwrap().contains("maxRunS"), "{v}");
        assert!(
            w.runs.lock().unwrap().is_empty(),
            "{bad}: no run was created"
        );
        assert!(
            w.idem.lock().unwrap().is_empty(),
            "{bad}: no claim was written"
        );
    }
}

#[tokio::test]
async fn b688_a_total_run_time_is_stored_on_the_request_and_on_the_run() {
    let w = World::new();
    let mut b = body();
    b["maxRunS"] = json!(36_000);
    let (status, v) = start(&w, "k-1", &b).await;
    assert_eq!(status, 201, "{v}");
    let id = v["runId"].as_str().unwrap();
    assert_eq!(stored(&w, id)["maxRunS"], json!(36_000));
    assert_eq!(
        w.runs.lock().unwrap().get(id).unwrap().max_run_s,
        Some(36_000)
    );
    // a request that names none stores none: the default is the service's
    let (_, v2) = start(&w, "k-2", &body()).await;
    let id2 = v2["runId"].as_str().unwrap();
    assert!(stored(&w, id2).get("maxRunS").is_none());
    assert_eq!(w.runs.lock().unwrap().get(id2).unwrap().max_run_s, None);
}

#[tokio::test]
async fn b689_an_idempotency_key_that_is_not_visible_ascii_or_is_too_long_is_refused() {
    let w = World::new();
    let id = stopped_run(&w, "k-1", "paused", ResumeMode::FollowUp).await;
    for bad in ["has a space".to_string(), "k".repeat(129), String::new()] {
        let (status, v) = resume_as(&w, ME, &id, Some(&bad)).await;
        assert_eq!((status, code(&v)), (400, "bad-request"), "{bad:?}: {v}");
    }
    assert_eq!(
        w.started.lock().unwrap().len(),
        1,
        "only the original run was started"
    );
}

#[tokio::test]
async fn b689_a_stored_request_that_cannot_be_read_ends_the_resume_with_the_run_named_and_nothing_started()
 {
    let w = World::new();
    let id = stopped_run(&w, "k-1", "paused", ResumeMode::FollowUp).await;
    let started = w.started.lock().unwrap().len();
    // absent, not JSON, and larger than the route reads
    let key = s3::request_key(&id);
    let original = w.objects.lock().unwrap().remove(&key).unwrap();
    let broken: [(&str, Vec<u8>); 3] = [
        ("absent", vec![]),
        ("malformed", b"{ not json".to_vec()),
        ("too large", vec![b' '; 8 * 1024 * 1024 + 1]),
    ];
    for (what, bytes) in broken {
        if !bytes.is_empty() {
            w.put_object(&key, bytes);
        }
        let (status, v) = resume(&w, &id).await;
        assert_eq!(
            (status, code(&v)),
            (422, "stored-request-unreadable"),
            "{what}: {v}"
        );
        let text = v["message"].as_str().unwrap_or_default();
        assert!(
            text.contains(&id) && text.contains("start a new run"),
            "{what}: {text}"
        );
        assert_eq!(w.started.lock().unwrap().len(), started, "{what}");
    }
    // the same request, read again, resumes
    w.put_object(&key, original);
    let (status, v) = resume(&w, &id).await;
    assert_eq!(status, 201, "{v}");
}

#[tokio::test]
async fn b689_a_store_or_bucket_that_is_down_answers_unavailable_and_the_press_can_be_repeated() {
    let w = World::new();
    let id = stopped_run(&w, "k-1", "paused", ResumeMode::FollowUp).await;
    w.objects_down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (status, v) = resume(&w, &id).await;
    assert_eq!(status, 503, "{v}");
    w.objects_down
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let (status, v) = resume(&w, &id).await;
    assert_eq!(status, 201, "{v}");
}
