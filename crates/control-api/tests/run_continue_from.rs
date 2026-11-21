//! A follow-up run (`POST /v1/direct/runs` with `continueFrom`) is stored with the id of the run it continues from, and
//! `GET /v1/runs` (every row) and `GET /v1/runs/{id}` (`summary`) return it as `continueFrom`. An ordinary run has no such member. The stand-in
//! behind `Store`, `Blobs` and `Execution` is the one the other chat-client routes use; the handlers are the real ones.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, OTHER, SONNET, World, deps, get, request, run_item};
use harness_protocol::direct::{DirectOutcome, s3 as direct_s3};
use harness_protocol::store::s3;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

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

// ----: whether a follow-up starts without the lifecycle records of the run it continues ---------------------------------------------
//
// The worker takes the earlier records out of the work tree only when the stored request holds `continueFrom` and `removeEarlierRecords:
// true`. The service is the only writer of `true`, and it decides where the source run is read: `true` is stored only for the start of a
// follow-up whose source run is `complete` and whose evidence (`evidence/direct.json`, written once by run-session) says `reported_done`
// for this run and for the head the delivered bundle names. The user's own `false` is stored as sent. Every other request is stored
// without the member, as release 5.24 stored it, and the worker keeps the records: a source run that did not report done, and evidence
// that is absent or that the store refuses (HTTP 403 or 404). Only a failure of the evidence read that may pass on a retry (a fault of the
// store, a timeout, a throttle) starts nothing: a behaviour is never guessed from an outage.

/// A text that only the evidence holds: no log line and no reply may carry it.
const EVIDENCE_ONLY: &str = "LABEL-ONLY-THE-EVIDENCE-HOLDS";

fn evidence(run: &str, outcome: &str, head: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schemaVersion": 1, "runId": run, "outcome": outcome, "label": EVIDENCE_ONLY, "detail": null,
        "headSource": "final", "headSha": head, "baseSha": BASE,
        "plannedAt": "2026-10-15T13:20:00Z", "plannedAtMs": 1_760_534_400_000u64, "failure": null, "summary": null
    }))
    .unwrap()
}

fn word(outcome: DirectOutcome) -> String {
    serde_json::to_value(outcome)
        .unwrap()
        .as_str()
        .unwrap()
        .to_string()
}

fn done() -> String {
    word(DirectOutcome::ReportedDone)
}

/// A source run of `ME` in `status` whose verified bundle names `HEAD`, with the evidence object when one is given.
fn world_with(status: &str, evidence: Option<Vec<u8>>) -> World {
    let w = World::new();
    w.add_run(run_item(
        SOURCE,
        ME,
        status,
        "2026-10-15T13:00:00Z",
        json!({"mode": "s3-only"}),
    ));
    w.put_object(&s3::verified_bundle_key(SOURCE), bundle(HEAD, SOURCE));
    if let Some(bytes) = evidence {
        w.put_object(&direct_s3::evidence_key(SOURCE), bytes);
    }
    w
}

fn stored_bytes(w: &World, run: &str) -> Vec<u8> {
    w.objects.lock().unwrap()[&s3::request_key(run)].clone()
}

fn stored(w: &World, run: &str) -> Value {
    serde_json::from_slice(&stored_bytes(w, run)).unwrap()
}

/// How often the evidence object of the source run was asked for.
fn evidence_reads(w: &World) -> usize {
    w.count(&format!(
        "read_or_refused {}",
        direct_s3::evidence_key(SOURCE)
    ))
}

async fn start_logged(w: &World, key: &str, b: &Value) -> (u16, Value, Vec<String>) {
    let mut r = request(ME, "POST", "/v1/direct/runs", &[], &b.to_string());
    r.headers.insert("idempotency-key".into(), key.into());
    let (out, logs) = control_api::handler::handle_logged(r, &deps(w)).await;
    (
        out.status,
        serde_json::from_str(&out.body).unwrap_or(Value::Null),
        logs,
    )
}

/// The log line that says why the earlier records stay; it names the source run and carries nothing of the evidence.
fn kept_line(name: &str, logs: &[String], reply: &Value) -> String {
    let line = logs
        .iter()
        .find(|l| l.contains("earlier records"))
        .unwrap_or_else(|| panic!("{name}: no log line in {logs:?}"));
    assert!(line.contains(SOURCE), "{name}: {line}");
    assert!(
        !logs.iter().any(|l| l.contains(EVIDENCE_ONLY))
            && !reply.to_string().contains(EVIDENCE_ONLY),
        "{name}: {logs:?}"
    );
    line.clone()
}

#[tokio::test]
async fn b866_the_source_runs_end_decides() {
    // The one case in which the worker takes the earlier records out: the service stores `true`.
    let w = world_with("complete", Some(evidence(SOURCE, &done(), HEAD)));
    let (status, v, logs) = start_logged(&w, "k-done", &follow_up(SOURCE)).await;
    assert_eq!(status, 201, "{v}");
    let s = stored(&w, v["runId"].as_str().unwrap());
    assert_eq!(s["removeEarlierRecords"], json!(true), "{s}");
    assert_eq!(s["continueFrom"], json!({"runId": SOURCE, "headSha": HEAD}));
    assert!(
        !logs.iter().any(|l| l.contains("earlier records")),
        "nothing to say about a done source: {logs:?}"
    );
    assert_eq!(evidence_reads(&w), 1);
    println!(
        "B866 TRIGGER PRESENT a source run that reported done: the service stores removeEarlierRecords true"
    );

    // A complete run that ended any other way keeps its records: every outcome word but the one stores no member.
    let mut kept = 0;
    for outcome in DirectOutcome::ALL {
        if outcome == DirectOutcome::ReportedDone {
            continue;
        }
        let w = world_with("complete", Some(evidence(SOURCE, &word(outcome), HEAD)));
        let (status, v, logs) = start_logged(&w, "k-1", &follow_up(SOURCE)).await;
        assert_eq!(status, 201, "{outcome:?}: {v}");
        let s = stored(&w, v["runId"].as_str().unwrap());
        assert!(s.get("removeEarlierRecords").is_none(), "{outcome:?}: {s}");
        kept_line(&format!("{outcome:?}"), &logs, &v);
        kept += 1;
    }
    assert_eq!(kept, DirectOutcome::ALL.len() - 1);

    // `ham continue` on a run that stopped delivered: the task is open, whatever its evidence says.
    for status in ["paused", "stalled"] {
        let w = world_with(status, Some(evidence(SOURCE, &done(), HEAD)));
        let id = start(&w, "k-1", &follow_up(SOURCE)).await;
        let s = stored(&w, &id);
        assert!(s.get("removeEarlierRecords").is_none(), "{status}: {s}");
        assert_eq!(
            evidence_reads(&w),
            0,
            "{status}: the evidence of a run that is not complete is not read"
        );
    }
}

#[tokio::test]
async fn b866_evidence_that_cannot_be_used_keeps_the_records() {
    let other_run = "01K5ZQ4T8M3N6P7R9S0V1W2X3Z";
    let mut over_the_limit = evidence(SOURCE, &done(), HEAD);
    over_the_limit.extend(std::iter::repeat_n(b' ', 2 * 1024 * 1024));
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("no JSON", b"reported_done".to_vec()),
        ("no JSON object", b"[\"reported_done\"]".to_vec()),
        (
            "the evidence of another run",
            evidence(other_run, &done(), HEAD),
        ),
        (
            "another head than the delivered one",
            evidence(SOURCE, &done(), BASE),
        ),
        (
            "no head",
            json!({"runId": SOURCE, "outcome": done()})
                .to_string()
                .into_bytes(),
        ),
        (
            "an outcome word this build does not know",
            evidence(SOURCE, "reported_done_and_checked", HEAD),
        ),
        (
            "an outcome that is no word",
            json!({"runId": SOURCE, "outcome": true, "headSha": HEAD})
                .to_string()
                .into_bytes(),
        ),
        ("larger than the route reads", over_the_limit),
    ];
    for (name, bytes) in cases {
        let w = world_with("complete", Some(bytes));
        let (status, v, logs) = start_logged(&w, "k-1", &follow_up(SOURCE)).await;
        assert_eq!(status, 201, "{name}: {v}");
        let s = stored(&w, v["runId"].as_str().unwrap());
        assert!(s.get("removeEarlierRecords").is_none(), "{name}: {s}");
        // The log says why, names the source run and carries nothing of the evidence.
        kept_line(name, &logs, &v);
    }
}

/// HTTP 403: the role may not list the bucket, so the store answers the read of an absent key (and of a key it may not read) with
/// AccessDenied. That is how a real stage answers. The follow-up starts, as on release 5.24, and the records stay.
#[tokio::test]
async fn b866_r2_evidence_the_store_refuses_with_403_starts_the_follow_up_and_stores_no_member() {
    let w = world_with("complete", None);
    w.no_list_permission.store(true, Ordering::SeqCst);
    let (status, v, logs) = start_logged(&w, "k-1", &follow_up(SOURCE)).await;
    assert_eq!(
        status, 201,
        "a follow-up never fails to start for this: {v}"
    );
    let id = v["runId"].as_str().unwrap();
    let s = stored(&w, id);
    assert!(s.get("removeEarlierRecords").is_none(), "{s}");
    assert_eq!(s["continueFrom"], json!({"runId": SOURCE, "headSha": HEAD}));
    assert_eq!(w.started.lock().unwrap().as_slice(), [id.to_string()]);
    assert_eq!(evidence_reads(&w), 1, "TRIGGER: the evidence was asked for");
    let line = kept_line("403", &logs, &v);
    assert!(
        line.contains("403"),
        "the log names the store's answer: {line}"
    );
    // Every repeat answers the same: the same key returns the same run.
    let (status, again, _) = start_logged(&w, "k-1", &follow_up(SOURCE)).await;
    assert_eq!((status, again["runId"].as_str()), (201, Some(id)));
}

/// HTTP 404: a store that tells an absent key apart says that the object is not there.
#[tokio::test]
async fn b866_r2_evidence_that_is_absent_with_404_starts_the_follow_up_and_stores_no_member() {
    let w = world_with("complete", None);
    let (status, v, logs) = start_logged(&w, "k-1", &follow_up(SOURCE)).await;
    assert_eq!(status, 201, "{v}");
    let id = v["runId"].as_str().unwrap();
    let s = stored(&w, id);
    assert!(s.get("removeEarlierRecords").is_none(), "{s}");
    assert_eq!(w.started.lock().unwrap().as_slice(), [id.to_string()]);
    assert_eq!(evidence_reads(&w), 1, "TRIGGER: the evidence was asked for");
    kept_line("404", &logs, &v);
}

/// A failure of the evidence read that may pass on a retry answers 503 with the retry words and writes nothing; `fault` is what the store
/// said. When the read answers again, the same key starts the run.
async fn a_read_that_may_pass_answers_retry(fault: &str) {
    let w = world_with("complete", Some(evidence(SOURCE, &done(), HEAD)));
    let key = direct_s3::evidence_key(SOURCE);
    w.read_faults
        .lock()
        .unwrap()
        .insert(key.clone(), fault.to_string());
    let before = (w.runs.lock().unwrap().len(), w.idem.lock().unwrap().len());
    let (status, v, logs) = start_logged(&w, "k-1", &follow_up(SOURCE)).await;
    assert_eq!(status, 503, "{fault}: {v}");
    assert_eq!(v["code"], json!("unavailable"), "{fault}: {v}");
    assert!(
        v["message"].as_str().unwrap().contains("retry"),
        "{fault}: {v}"
    );
    assert_eq!(evidence_reads(&w), 1, "TRIGGER: the evidence read failed");
    assert!(
        logs.iter().any(|l| l.contains(fault)),
        "the store's reason goes to the log: {logs:?}"
    );
    assert_eq!(
        (w.runs.lock().unwrap().len(), w.idem.lock().unwrap().len()),
        before,
        "{fault}: no run and no claim"
    );
    assert!(
        w.started.lock().unwrap().is_empty(),
        "{fault}: nothing started"
    );
    assert!(
        !w.objects
            .lock()
            .unwrap()
            .keys()
            .any(|k| k.ends_with("/input/request.json")),
        "{fault}: no request was stored"
    );
    // The read answers again: the same key starts the run, and the rule decides.
    w.read_faults.lock().unwrap().remove(&key);
    let (status, v, _) = start_logged(&w, "k-1", &follow_up(SOURCE)).await;
    assert_eq!(status, 201, "{fault}: {v}");
    assert_eq!(
        stored(&w, v["runId"].as_str().unwrap())["removeEarlierRecords"],
        json!(true)
    );
}

#[tokio::test]
async fn b866_r2_a_5xx_answer_of_the_evidence_read_answers_retry_and_writes_nothing() {
    a_read_that_may_pass_answers_retry("s3 GET returned 500 InternalError").await;
}

#[tokio::test]
async fn b866_r2_a_timeout_of_the_evidence_read_answers_retry_and_writes_nothing() {
    a_read_that_may_pass_answers_retry("the read of the object timed out").await;
}

#[tokio::test]
async fn b866_r2_only_the_service_writes_true_and_the_users_false_is_kept() {
    // A client's `true` is never copied: the rule decides. A source run that reported done gets the service's `true`.
    let mut on = follow_up(SOURCE);
    on["removeEarlierRecords"] = json!(true);
    let w = world_with("complete", Some(evidence(SOURCE, &done(), HEAD)));
    let id = start(&w, "k-1", &on).await;
    assert_eq!(stored(&w, &id)["removeEarlierRecords"], json!(true));
    // A client's `true` for a source run that did not report done stores no member.
    let stopped = word(DirectOutcome::StoppedByUser);
    for (status, said) in [
        ("complete", stopped.clone()),
        ("paused", done()),
        ("stalled", done()),
    ] {
        let w = world_with(status, Some(evidence(SOURCE, &said, HEAD)));
        let id = start(&w, "k-1", &on).await;
        let s = stored(&w, &id);
        assert!(s.get("removeEarlierRecords").is_none(), "{status}: {s}");
    }
    // A client's `true` cannot stand in for evidence the store refuses or does not have.
    let w = world_with("complete", None);
    w.no_list_permission.store(true, Ordering::SeqCst);
    let id = start(&w, "k-1", &on).await;
    assert!(stored(&w, &id).get("removeEarlierRecords").is_none());

    // The user's `false` (run.remove_earlier_records = false) is stored as sent, and the evidence is not read for it.
    let mut off = follow_up(SOURCE);
    off["removeEarlierRecords"] = json!(false);
    let w = world_with("complete", Some(evidence(SOURCE, &done(), HEAD)));
    let id = start(&w, "k-1", &off).await;
    assert_eq!(stored(&w, &id)["removeEarlierRecords"], json!(false));
    assert_eq!(evidence_reads(&w), 0);
    // Without `continueFrom` the member means nothing and is never stored, whatever the client sent.
    let mut plain = body();
    plain["removeEarlierRecords"] = json!(false);
    let id = start(&w, "k-2", &plain).await;
    assert!(stored(&w, &id).get("removeEarlierRecords").is_none());
    plain["removeEarlierRecords"] = json!(true);
    let id = start(&w, "k-3", &plain).await;
    assert!(stored(&w, &id).get("removeEarlierRecords").is_none());
    // A first request stored without the member is byte for byte the request the service stored before the member existed.
    let id = start(&w, "k-4", &body()).await;
    assert_eq!(stored(&w, &id), body());
}

#[tokio::test]
async fn b866_a_repeat_of_the_key_returns_the_same_run_and_the_same_stored_bytes() {
    for evidence_of in [done(), word(DirectOutcome::StoppedProviderError)] {
        let w = world_with("complete", Some(evidence(SOURCE, &evidence_of, HEAD)));
        let first = start(&w, "k-1", &follow_up(SOURCE)).await;
        let bytes = stored_bytes(&w, &first);
        let again = start(&w, "k-1", &follow_up(SOURCE)).await;
        assert_eq!(again, first, "{evidence_of}");
        assert_eq!(stored_bytes(&w, &again), bytes, "{evidence_of}");
        assert_eq!(
            w.runs.lock().unwrap().len(),
            2,
            "the source run and one follow-up"
        );
    }
}

/// The claim of an Idempotency-Key is compared without the member: a start whose first call stored no member (a service of release 5.24,
/// or evidence the store did not have then) and whose repeat would store `true` is the same request, and the repeat returns the run the
/// first call started.
#[tokio::test]
async fn b866_r2_a_repeat_of_a_start_whose_first_call_stored_no_member_returns_the_same_run() {
    let w = world_with("complete", None);
    let first = start(&w, "k-1", &follow_up(SOURCE)).await;
    let bytes = stored_bytes(&w, &first);
    assert!(
        stored(&w, &first).get("removeEarlierRecords").is_none(),
        "the first call stored the request as release 5.24 stored it"
    );
    // The rule now gives `true` for the same request.
    w.put_object(
        &direct_s3::evidence_key(SOURCE),
        evidence(SOURCE, &done(), HEAD),
    );
    let (status, v, _) = start_logged(&w, "k-1", &follow_up(SOURCE)).await;
    assert_eq!(status, 201, "the repeat is the same request: {v}");
    assert_eq!(v["runId"].as_str(), Some(first.as_str()));
    assert_eq!(
        stored_bytes(&w, &first),
        bytes,
        "the stored request of the run is the first call's"
    );
    assert_eq!(w.runs.lock().unwrap().len(), 2);
    // TRIGGER: a new key reads the evidence and stores the service's `true`.
    let second = start(&w, "k-2", &follow_up(SOURCE)).await;
    assert_eq!(stored(&w, &second)["removeEarlierRecords"], json!(true));
    // Another request under the first key is still refused.
    let mut other = follow_up(SOURCE);
    other["prompt"] = json!("another request");
    let (status, v, _) = start_logged(&w, "k-1", &other).await;
    assert_eq!(
        (status, v["code"].as_str()),
        (422, Some("idempotency-key-reused")),
        "{v}"
    );
}
