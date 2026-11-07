//! What run-session does with the `summary.json` of a run that was asked a question or restarted from the work tree:
//! attempts of kind `fresh_session` and `instruction`, `interventions[]` and `freshSessions[]`. `plan-delivery` reads the
//! summary strictly and treats one it cannot read as no summary at all, so a Lambda that predates the new values would end such a run
//! as a lost session. That is why run-session deploys before the runtime image; the last test pins the strictness.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::direct::{DirectOutcome, s3};
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

async fn call(mem: &Mem, v: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(v).unwrap();
    handle(&mem.deps(), &op).await
}

async fn plan(mem: &Mem) -> Value {
    call(
        mem,
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged(),
            "session": {"turn": turn_output(true), "failure": {}}}),
    )
    .await
    .unwrap()
}

/// A started run whose session wrote the final bundle and the given `summary.json` body.
async fn started_with_summary(summary_body: Vec<u8>) -> Mem {
    let mem = Mem::default();
    mem.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
    mem.put_bundle();
    mem.put_blob(&s3::summary_key(RUN), summary_body);
    call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
    mem.clear_events();
    mem
}

/// The summary a finished run writes, as JSON, for `edit` to extend.
fn summary_value() -> Value {
    serde_json::to_value(summary(
        DirectOutcome::ReportedDone,
        Some("Added pct."),
        true,
    ))
    .unwrap()
}

fn attempt(n: u32, kind: &str) -> Value {
    json!({
        "n": n, "kind": kind,
        "startedAt": "2026-10-13T18:00:00Z", "endedAt": "2026-10-13T18:01:00Z",
        "result": "ok"
    })
}

fn intervention(n: u32) -> Value {
    json!({
        "n": n, "requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT1N", "reason": "engine_stop",
        "openedAt": "2026-10-13T18:41:07Z", "closedAt": "2026-10-13T18:42:40Z", "waitedS": 93,
        "outcome": "answered", "action": "new_session_with_prompt", "origin": "user",
        "textBytes": 88, "atAttempt": 3
    })
}

fn fresh_session(n: u32) -> Value {
    json!({
        "n": n, "trigger": "user", "reason": "engine_stop", "requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT1N",
        "startedAt": "2026-10-13T18:42:41Z", "restartedProcess": false, "digestBytes": 18432,
        "digestSha256": "9f".repeat(32), "atAttempt": 3
    })
}

/// `fixtures/direct-input/summary-interventions.json`: the new members of `summary.json`, the one set of bytes agentd's writer and
/// every reader are tested against (package P).
const GOLDEN_MEMBERS: &str =
    include_str!("../../../fixtures/direct-input/summary-interventions.json");

fn evidence(mem: &Mem) -> Value {
    serde_json::from_slice(&mem.blob(&s3::evidence_key(RUN)).unwrap()).unwrap()
}

#[tokio::test]
async fn plan_delivery_reads_the_new_attempt_kinds_interventions_and_fresh_sessions() {
    let mut v = summary_value();
    v["attempts"] = json!([
        attempt(1, "prompt"),
        attempt(2, "resume"),
        attempt(3, "fresh_session"),
        attempt(4, "instruction")
    ]);
    let golden: Value = serde_json::from_str(GOLDEN_MEMBERS).unwrap();
    v["interventions"] = golden["interventions"].clone();
    v["freshSessions"] = golden["freshSessions"].clone();
    v["usage"] = golden["usage"].clone();
    let mem = started_with_summary(serde_json::to_vec(&v).unwrap()).await;
    let p = plan(&mem).await;
    assert_eq!(p["deliver"], true, "{p}");
    assert_eq!(p["outcome"], "reported_done", "the summary was read: {p}");
    let e = evidence(&mem);
    let s = &e["summary"];
    assert!(s.is_object(), "the summary reached the evidence file: {e}");
    let kinds: Vec<&str> = s["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["prompt", "resume", "fresh_session", "instruction"]);
    assert_eq!(s["interventions"], golden["interventions"]);
    assert_eq!(s["freshSessions"], golden["freshSessions"]);
    assert_eq!(s["usage"], golden["usage"], "the summed usage is kept");
}

#[tokio::test]
async fn the_largest_summary_agentd_may_write_is_still_read() {
    // 24 attempts (the protocol cap) with every detail at its limit, 8 interventions and 8 fresh sessions (the caps of the new lists).
    let mut v = summary_value();
    let long = "x".repeat(300);
    let kinds = ["prompt", "resume", "fresh_session", "instruction"];
    v["attempts"] = Value::Array(
        (1..=24u32)
            .map(|n| {
                let kind = if n == 1 {
                    kinds[0]
                } else {
                    kinds[(n as usize) % 4]
                };
                let mut a = attempt(n, kind);
                a["result"] = json!("provider_error");
                a["retryClass"] = json!("transient");
                a["detail"] = json!(long.clone());
                a["backoffS"] = json!(30);
                a["modelId"] = json!(MODEL);
                a
            })
            .collect(),
    );
    v["interventions"] = Value::Array((1..=8u32).map(intervention).collect());
    v["freshSessions"] = Value::Array((2..=9u32).map(fresh_session).collect());
    let body = serde_json::to_vec(&v).unwrap();
    assert!(body.len() < 64 * 1024, "{} bytes", body.len());
    let mem = started_with_summary(body).await;
    let p = plan(&mem).await;
    assert_eq!(p["outcome"], "reported_done", "{p}");
    let e = evidence(&mem);
    assert_eq!(e["summary"]["attempts"].as_array().unwrap().len(), 24);
    assert_eq!(e["summary"]["interventions"].as_array().unwrap().len(), 8);
    assert_eq!(e["summary"]["freshSessions"].as_array().unwrap().len(), 8);
}

#[tokio::test]
async fn a_summary_without_the_new_members_is_read_and_written_back_unchanged() {
    let mem = started_with_summary(serde_json::to_vec(&summary_value()).unwrap()).await;
    let p = plan(&mem).await;
    assert_eq!(p["outcome"], "reported_done", "{p}");
    let s = &evidence(&mem)["summary"];
    assert!(s.is_object());
    assert!(s.get("interventions").is_none(), "{s}");
    assert!(s.get("freshSessions").is_none(), "{s}");
}

#[tokio::test]
async fn a_summary_with_an_attempt_kind_nobody_knows_is_not_read_so_readers_deploy_first() {
    // Today's strictness, pinned: a body `plan-delivery` cannot parse is no summary, and the run ends as a lost session. A writer
    // that adds a kind before the reader knows it would end every such run that way.
    let mut v = summary_value();
    v["attempts"] = json!([attempt(1, "prompt"), attempt(2, "teleport")]);
    let mem = started_with_summary(serde_json::to_vec(&v).unwrap()).await;
    let p = plan(&mem).await;
    assert_eq!(p["outcome"], "stopped_session_lost", "{p}");
    assert!(evidence(&mem)["summary"].is_null());
}
