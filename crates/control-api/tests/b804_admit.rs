//! The stage starts no new runs while a deploy holds its deploy lock (the SSM parameter `<deployment prefix>/deploy-lock`).
//!
//! The three routes that start a run (`POST /v1/direct/runs`, with `continueFrom` too, `POST /v1/runs/{runId}/retry` and
//! `POST /v1/runs/{runId}/resume`) end in one function, which reads the lock right before the write that would create the run. A lock closes the
//! stage while it exists, has no line `deploys=no`, and was written by SSM at most [`control_api::admission::STALE_AFTER_S`] seconds ago. The
//! answer is then 409 `stage-updating` with `Retry-After`, and nothing but the idempotency claim is written. A lock nobody renews, and a read
//! that the stage's own setup refuses, let the run start and log one line; a read that may pass on a retry is the 503 of the read beside it.
//! A repeat of a start whose run was launched returns that run without a look at the lock. Every other route stays open.
//!
//! The stand-in behind `Store`, `Blobs` and `Execution` is the one the other chat-client routes use; the handlers are the real ones.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, SONNET, World, deps, request, run_item};
use control_api::admission::{MESSAGE, RETRY_AFTER_S, STAGE_UPDATING, STALE_AFTER_S};
use control_api::handler::{DeployLock, StoreError, V1Response, handle_logged};
use harness_protocol::goal::ResumeMode;
use harness_protocol::store::s3;
use serde_json::{Value, json};

const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const BASE: &str = "1111111111111111111111111111111111111111";
const SOURCE: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";

/// The lock as setup writes it for a deploy (contract C6): no `deploys` line. Its token is a short stand-in: the service never reads it.
const LOCK: &str = "token=tok-b804\nholder=laptop:4242\nidentity=arn:aws:sts::123456789012:assumed-role/Admin/operator\nroute=runner\nstarted=2026-11-11T16:00:00Z\ncommit=8dd391d43f9011813f3c5f8988ec562436ff4b2e\ninputs=aa\nbuild=";

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A lock with `value` that SSM wrote `age_s` seconds ago.
fn lock_aged(value: &str, age_s: i64) -> DeployLock {
    DeployLock {
        value: value.to_string(),
        written_epoch_s: Some(now() - age_s),
    }
}

/// A deploy took the lock a minute ago and renews it.
fn close(w: &World) {
    *w.deploy_lock.lock().unwrap() = Some(Ok(lock_aged(LOCK, 60)));
}

fn open(w: &World) {
    *w.deploy_lock.lock().unwrap() = None;
}

fn body() -> Value {
    json!({"prompt": "Add percent_change(a, b) with tests", "target": {"mode": "s3-only"}, "model": SONNET, "rules": []})
}

fn bundle(head: &str, run: &str) -> Vec<u8> {
    let mut b =
        format!("# v2 git bundle\n-{BASE} base subject\n{head} refs/heads/harness/{run}\n\nPACK")
            .into_bytes();
    b.extend(std::iter::repeat_n(7u8, 3000));
    b
}

async fn post(w: &World, path: &str, key: &str, body: &str) -> (V1Response, Value, Vec<String>) {
    let mut r = request(ME, "POST", path, &[], body);
    r.headers.insert("idempotency-key".into(), key.into());
    let (out, logs) = handle_logged(r, &deps(w)).await;
    let v = serde_json::from_str(&out.body).unwrap_or(Value::Null);
    (out, v, logs)
}

async fn start(w: &World, key: &str) -> (V1Response, Value, Vec<String>) {
    post(w, "/v1/direct/runs", key, &body().to_string()).await
}

/// A run of `ME` started through the real handler while the stage is open.
async fn started_run(w: &World, key: &str) -> String {
    let (out, v, _) = start(w, key).await;
    assert_eq!(out.status, 201, "{v}");
    v["runId"].as_str().unwrap().to_string()
}

fn set_status(w: &World, id: &str, status: &str) {
    w.runs.lock().unwrap().get_mut(id).unwrap().status =
        serde_json::from_value(json!(status)).unwrap();
}

/// The first session of the run pinned the worker runtime on its Run item, as `run-session` does when the execution starts.
fn launch(w: &World, id: &str) {
    let mut runs = w.runs.lock().unwrap();
    let run = runs.get_mut(id).unwrap();
    run.runtime_version = Some("7".into());
    run.runtime_endpoint = Some("v7".into());
}

fn assert_refused(out: &V1Response, v: &Value) {
    assert_eq!(out.status, 409, "{v}");
    assert_eq!(v["code"], json!(STAGE_UPDATING), "{v}");
    assert_eq!(v["message"], json!(MESSAGE), "{v}");
    assert_eq!(
        out.retry_after_s,
        Some(RETRY_AFTER_S),
        "the answer names a wait"
    );
}

fn lock_reads(w: &World) -> usize {
    w.count("deploy_lock")
}

#[tokio::test]
async fn b804_admit_a_start_is_refused_while_a_renewed_lock_is_held() {
    let w = World::new();
    close(&w);
    let (out, v, logs) = start(&w, "k-1").await;
    assert_refused(&out, &v);
    assert_eq!(STAGE_UPDATING, "stage-updating");
    assert_eq!(
        MESSAGE,
        "This stage is being updated, so it starts no new runs. Try again in a few minutes."
    );
    assert_eq!(RETRY_AFTER_S, 120);
    assert!(w.runs.lock().unwrap().is_empty(), "no run was created");
    assert!(w.started.lock().unwrap().is_empty(), "no execution started");
    assert_eq!(w.count("blob_put"), 0, "no request was stored");
    assert_eq!(
        w.idem.lock().unwrap().len(),
        1,
        "only the claim of the key was written"
    );
    assert_eq!(logs.len(), 1, "one line for the refusal: {logs:?}");
    assert!(
        logs[0].contains("deploy lock") && logs[0].contains("60 s"),
        "the line names the lock and its age: {logs:?}"
    );
    assert!(
        !logs[0].contains("tok-b804"),
        "the token of the lock stays out of the log: {logs:?}"
    );
}

#[tokio::test]
async fn b804_admit_a_follow_up_a_retry_and_a_resume_are_refused_too() {
    let w = World::new();
    // a finished run to follow up, a blocked run to retry and a paused run to resume, all made while the stage is open
    w.add_run(run_item(
        SOURCE,
        ME,
        "complete",
        "2026-10-15T13:00:00Z",
        json!({"mode": "s3-only"}),
    ));
    w.put_object(&s3::verified_bundle_key(SOURCE), bundle(HEAD, SOURCE));
    let blocked = started_run(&w, "k-blocked").await;
    set_status(&w, &blocked, "blocked");
    let paused = started_run(&w, "k-paused").await;
    {
        let mut runs = w.runs.lock().unwrap();
        let run = runs.get_mut(&paused).unwrap();
        run.status = serde_json::from_value(json!("paused")).unwrap();
        run.resume_mode = Some(ResumeMode::FollowUp);
    }
    w.put_object(&s3::verified_bundle_key(&paused), bundle(HEAD, &paused));
    let runs_before = w.runs.lock().unwrap().len();
    let started_before = w.started.lock().unwrap().len();
    close(&w);

    let mut follow = body();
    follow["continueFrom"] = json!({"runId": SOURCE});
    let (out, v, _) = post(&w, "/v1/direct/runs", "k-follow", &follow.to_string()).await;
    assert_refused(&out, &v);
    let (out, v, _) = post(&w, &format!("/v1/runs/{blocked}/retry"), "k-retry", "{}").await;
    assert_refused(&out, &v);
    let (out, v, _) = post(&w, &format!("/v1/runs/{paused}/resume"), "k-resume", "{}").await;
    assert_refused(&out, &v);

    assert_eq!(
        w.runs.lock().unwrap().len(),
        runs_before,
        "no run was created"
    );
    assert_eq!(
        w.started.lock().unwrap().len(),
        started_before,
        "no execution started"
    );

    // the same three requests start their runs once the deploy has ended
    open(&w);
    let (out, v, _) = post(&w, "/v1/direct/runs", "k-follow", &follow.to_string()).await;
    assert_eq!(out.status, 201, "{v}");
    let (out, v, _) = post(&w, &format!("/v1/runs/{blocked}/retry"), "k-retry", "{}").await;
    assert_eq!(out.status, 201, "{v}");
    let (out, v, _) = post(&w, &format!("/v1/runs/{paused}/resume"), "k-resume", "{}").await;
    assert_eq!(out.status, 201, "{v}");
    assert_eq!(w.runs.lock().unwrap().len(), runs_before + 3);
}

#[tokio::test]
async fn b804_admit_a_lock_nobody_renews_does_not_refuse_and_logs_one_line() {
    let w = World::new();
    let age = STALE_AFTER_S as i64 + 5;
    *w.deploy_lock.lock().unwrap() = Some(Ok(lock_aged(LOCK, age)));
    let (out, v, logs) = start(&w, "k-1").await;
    assert_eq!(out.status, 201, "{v}");
    assert_eq!(out.retry_after_s, None);
    assert_eq!(w.started.lock().unwrap().len(), 1, "the run started");
    let about: Vec<&String> = logs.iter().filter(|l| l.contains("deploy lock")).collect();
    assert_eq!(about.len(), 1, "one line about the lock: {logs:?}");
    assert!(
        about[0].contains("nobody renews it") && about[0].contains("the run starts"),
        "{logs:?}"
    );
}

#[tokio::test]
async fn b804_admit_a_lock_written_at_the_limit_still_refuses() {
    let w = World::new();
    // ten seconds inside the limit, so the seconds this test takes do not move it over
    *w.deploy_lock.lock().unwrap() = Some(Ok(lock_aged(LOCK, STALE_AFTER_S as i64 - 10)));
    let (out, v, _) = start(&w, "k-1").await;
    assert_refused(&out, &v);
}

#[tokio::test]
async fn b804_admit_a_lock_without_a_write_time_does_not_refuse_and_logs_one_line() {
    let w = World::new();
    *w.deploy_lock.lock().unwrap() = Some(Ok(DeployLock {
        value: LOCK.into(),
        written_epoch_s: None,
    }));
    let (out, v, logs) = start(&w, "k-1").await;
    assert_eq!(out.status, 201, "{v}");
    assert_eq!(
        logs.iter().filter(|l| l.contains("deploy lock")).count(),
        1,
        "{logs:?}"
    );
}

#[tokio::test]
async fn b804_admit_a_read_that_may_pass_on_a_retry_is_the_503_of_the_read_beside_it() {
    let w = World::new();
    *w.deploy_lock.lock().unwrap() = Some(Err(StoreError::Unavailable(
        "ThrottlingException: SENTINEL-detail-7a1c".into(),
    )));
    let (out, v, logs) = start(&w, "k-1").await;
    assert_eq!(out.status, 503, "{v}");
    assert_eq!(v["code"], json!("unavailable"));
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("retry with the same Idempotency-Key"),
        "{v}"
    );
    assert!(
        !out.body.contains("SENTINEL"),
        "the reason goes to the log only: {}",
        out.body
    );
    assert!(
        logs.iter().any(|l| l.contains("SENTINEL-detail-7a1c")),
        "{logs:?}"
    );
    assert!(w.runs.lock().unwrap().is_empty(), "no run was created");
    assert!(w.started.lock().unwrap().is_empty(), "no execution started");
}

#[tokio::test]
async fn b804_admit_a_read_the_stage_refuses_lets_the_run_start_and_logs_one_line() {
    let w = World::new();
    *w.deploy_lock.lock().unwrap() = Some(Err(StoreError::NotSetUp(
        "SENTINEL-detail-9e3b: the role may not read the parameter".into(),
    )));
    let (out, v, logs) = start(&w, "k-1").await;
    assert_eq!(out.status, 201, "{v}");
    assert_eq!(w.started.lock().unwrap().len(), 1, "the run started");
    let about: Vec<&String> = logs.iter().filter(|l| l.contains("deploy lock")).collect();
    assert_eq!(about.len(), 1, "one line about the lock: {logs:?}");
    assert!(
        about[0].contains("could not be read") && about[0].contains("SENTINEL-detail-9e3b"),
        "{logs:?}"
    );
    assert!(!out.body.contains("SENTINEL"), "{}", out.body);
}

#[tokio::test]
async fn b804_admit_a_lock_that_deploys_nothing_does_not_refuse() {
    let w = World::new();
    *w.deploy_lock.lock().unwrap() = Some(Ok(lock_aged(&format!("{LOCK}\ndeploys=no"), 60)));
    let (out, v, logs) = start(&w, "k-1").await;
    assert_eq!(out.status, 201, "{v}");
    assert!(
        !logs.iter().any(|l| l.contains("deploy lock")),
        "a hold that deploys nothing is the normal case of a client update: {logs:?}"
    );
}

#[tokio::test]
async fn b804_admit_only_the_exact_line_opens_a_held_lock() {
    for value in [
        format!("{LOCK}\ndeploys=yes"),
        format!("{LOCK}\ndeploys=no thanks"),
        format!("{LOCK}\ndeploys=NO"),
        format!("{LOCK}\nnote=deploys=no"),
        format!("{LOCK}\n deploys=no"),
        "not a lock at all".to_string(),
        String::new(),
    ] {
        let w = World::new();
        *w.deploy_lock.lock().unwrap() = Some(Ok(lock_aged(&value, 60)));
        let (out, v, _) = start(&w, "k-1").await;
        assert_eq!(out.status, 409, "{value:?} closes the stage: {v}");
    }
    // the line may stand anywhere, and a line end of either kind is no part of it
    for value in [
        format!("deploys=no\n{LOCK}"),
        format!("{LOCK}\r\ndeploys=no\r\nparties=a:1@2026-11-11T16:00:00Z"),
    ] {
        let w = World::new();
        *w.deploy_lock.lock().unwrap() = Some(Ok(lock_aged(&value, 60)));
        let (out, v, _) = start(&w, "k-1").await;
        assert_eq!(out.status, 201, "{value:?} leaves the stage open: {v}");
    }
}

#[tokio::test]
async fn b804_admit_a_repeat_of_a_launched_run_returns_its_run_and_reads_no_lock() {
    let w = World::new();
    let id = started_run(&w, "k-1").await;
    launch(&w, &id);
    close(&w);
    let reads = lock_reads(&w);
    let (out, v, _) = start(&w, "k-1").await;
    assert_eq!(out.status, 201, "{v}");
    assert_eq!(v["runId"], json!(id), "the same run");
    assert_eq!(
        lock_reads(&w),
        reads,
        "a repeat of a launched run does not look at the lock"
    );
    assert_eq!(w.runs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn b804_admit_a_repeat_of_a_run_that_was_not_launched_is_refused() {
    let w = World::new();
    // the Run item exists, and nothing shows that its execution started (the first StartExecution may have failed)
    let id = started_run(&w, "k-1").await;
    let started = w.started.lock().unwrap().len();
    close(&w);
    let (out, v, _) = start(&w, "k-1").await;
    assert_refused(&out, &v);
    assert_eq!(
        w.started.lock().unwrap().len(),
        started,
        "no StartExecution is sent while the stage is closed"
    );
    // after the deploy the same key returns the same run
    open(&w);
    let (out, v, _) = start(&w, "k-1").await;
    assert_eq!(out.status, 201, "{v}");
    assert_eq!(v["runId"], json!(id));
}

#[tokio::test]
async fn b804_admit_a_refused_key_starts_its_run_after_the_update() {
    let w = World::new();
    close(&w);
    let (out, v, _) = start(&w, "k-1").await;
    assert_refused(&out, &v);
    let claimed = w
        .idem
        .lock()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .run_id
        .clone();
    open(&w);
    let (out, v, _) = start(&w, "k-1").await;
    assert_eq!(out.status, 201, "{v}");
    assert_eq!(
        v["runId"],
        json!(claimed),
        "the run has the id the key claimed"
    );
    assert_eq!(w.started.lock().unwrap().as_slice(), [claimed]);
}

#[tokio::test]
async fn b804_admit_an_open_stage_reads_the_lock_once_for_a_new_run_and_logs_nothing() {
    let w = World::new();
    let (out, v, logs) = start(&w, "k-1").await;
    assert_eq!(out.status, 201, "{v}");
    assert_eq!(
        lock_reads(&w),
        1,
        "one read, right before the run is created"
    );
    assert!(!logs.iter().any(|l| l.contains("deploy lock")), "{logs:?}");
    // the lock is read after the claim and before the request is stored
    let calls = w.calls.lock().unwrap().clone();
    let at = |name: &str| {
        calls
            .iter()
            .position(|c| c.starts_with(name))
            .unwrap_or_else(|| panic!("no {name} in {calls:?}"))
    };
    assert!(at("deploy_lock") < at("blob_put"), "{calls:?}");
}

#[tokio::test]
async fn b804_admit_a_request_that_is_refused_for_its_own_reasons_never_reads_the_lock() {
    let w = World::new();
    close(&w);
    // no key, a body that is no request, a model the catalog does not list
    let r = request(ME, "POST", "/v1/direct/runs", &[], &body().to_string());
    let (out, _) = handle_logged(r, &deps(&w)).await;
    assert_eq!(out.status, 400);
    let (out, _, _) = post(&w, "/v1/direct/runs", "k-1", "{").await;
    assert_eq!(out.status, 400);
    let mut b = body();
    b["model"] = json!("no-such-model");
    let (out, _, _) = post(&w, "/v1/direct/runs", "k-2", &b.to_string()).await;
    assert_eq!(out.status, 422);
    assert_eq!(lock_reads(&w), 0, "the person's own mistake is said first");
}

#[tokio::test]
async fn b804_admit_the_other_routes_answer_while_the_stage_is_closed() {
    let w = World::new();
    let id = started_run(&w, "k-1").await;
    launch(&w, &id);
    close(&w);
    let reads = lock_reads(&w);
    let gets = [
        "/v1/runs".to_string(),
        format!("/v1/runs/{id}"),
        format!("/v1/runs/{id}/events"),
        format!("/v1/runs/{id}/artifacts"),
        format!("/v1/runs/{id}/messages"),
        format!("/v1/runs/{id}/transcript"),
        format!("/v1/runs/{id}/head"),
        "/v1/direct/models".to_string(),
        "/v1/models".to_string(),
        "/v1/whoami".to_string(),
        "/v1/search".to_string(),
    ];
    for path in gets {
        let query: &[(&str, &str)] = if path.ends_with("/events") {
            &[("shard", "28")]
        } else if path == "/v1/search" {
            &[("q", "percent")]
        } else {
            &[]
        };
        let (out, _) = handle_logged(request(ME, "GET", &path, query, ""), &deps(&w)).await;
        // a 409 of the route's own (a run that has not finished has no head) is no refusal of the stage
        assert!(
            !out.body.contains(STAGE_UPDATING),
            "GET {path}: {}",
            out.body
        );
        assert_eq!(out.retry_after_s, None, "GET {path}");
    }
    let posts = [
        (format!("/v1/runs/{id}/input"), json!({"requestId": "01M5A2R7K30000000000000001", "action": "finish"}).to_string()),
        (format!("/v1/runs/{id}/messages"), json!({"messageId": "01M5A2R7K30000000000000002", "text": "use the smaller fixture", "delivery": "next"}).to_string()),
        (format!("/v1/runs/{id}/snapshot/download-url"), String::new()),
        ("/v1/runs/index".to_string(), json!({"runIds": [id]}).to_string()),
        ("/v1/tags/edit".to_string(), json!({"runIds": [id], "add": ["release"]}).to_string()),
        (format!("/v1/runs/{id}/cancel"), String::new()),
    ];
    for (path, body) in posts {
        let (out, _, _) = post(&w, &path, "k-other", &body).await;
        assert!(
            !out.body.contains(STAGE_UPDATING),
            "POST {path} is not a start: {}",
            out.body
        );
        assert_eq!(out.retry_after_s, None, "POST {path}");
    }
    assert_eq!(
        lock_reads(&w),
        reads,
        "no route but a start looks at the lock"
    );
}

#[test]
fn b804_admit_the_stale_number_is_the_one_line_of_the_file_setup_reads() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/lib/deploy-lock-stale-seconds.txt");
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text, "1800\n", "one integer line");
    assert_eq!(STALE_AFTER_S, 1800);
    // deploy.py and setup write the lock at least every 600 seconds; the limit leaves room for two missed writes
    const { assert!(STALE_AFTER_S >= 3 * 600) };
}

#[test]
fn b804_admit_the_judgement_of_a_lock_by_its_text_and_its_age() {
    use control_api::admission::{Verdict, judge};
    let at = 1_800_000_000;
    let lock = |value: &str, written: Option<i64>| DeployLock {
        value: value.into(),
        written_epoch_s: written,
    };
    assert_eq!(
        judge(&lock(LOCK, Some(at - 60)), at),
        Verdict::Closed { age_s: 60 }
    );
    assert_eq!(
        judge(&lock(LOCK, Some(at)), at),
        Verdict::Closed { age_s: 0 }
    );
    // a write time ahead of this clock (two clocks a second apart) is a fresh write
    assert_eq!(
        judge(&lock(LOCK, Some(at + 2)), at),
        Verdict::Closed { age_s: 0 }
    );
    assert_eq!(
        judge(&lock(LOCK, Some(at - 1800)), at),
        Verdict::Closed { age_s: 1800 },
        "the limit itself is still renewed"
    );
    assert_eq!(
        judge(&lock(LOCK, Some(at - 1801)), at),
        Verdict::NotRenewed { age_s: Some(1801) }
    );
    assert_eq!(
        judge(&lock(LOCK, None), at),
        Verdict::NotRenewed { age_s: None }
    );
    assert_eq!(
        judge(&lock(&format!("{LOCK}\ndeploys=no"), Some(at - 60)), at),
        Verdict::DeploysNothing
    );
}
