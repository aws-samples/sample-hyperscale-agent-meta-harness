//! What run-session does with the `research` member of `summary.json`: the counters of the research lane that
//! agentd writes through its `on_summary` hook. `plan-delivery` reads the summary strictly and copies it into `evidence/direct.json`, which
//! the zip carries, so the member has to survive that read and write. The bytes are `fixtures/research/summary-research.json`, the one set
//! agentd's writer (`crates/agentd/tests/direct_research_hook.rs`) and every reader are tested against.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::direct::{DirectOutcome, s3};
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

const GOLDEN: &str = include_str!("../../../fixtures/research/summary-research.json");

async fn plan(mem: &Mem) -> Value {
    let op: Op = serde_json::from_value(json!({
        "op": "plan-delivery", "runId": RUN, "staged": staged(),
        "session": {"turn": turn_output(true), "failure": {}}
    }))
    .unwrap();
    handle(&mem.deps(), &op).await.unwrap()
}

/// A started run whose session wrote the final bundle and the given `summary.json` body.
async fn started_with_summary(summary_body: Vec<u8>) -> Mem {
    let mem = Mem::default();
    mem.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
    mem.put_bundle();
    mem.put_blob(&s3::summary_key(RUN), summary_body);
    let op: Op = serde_json::from_value(
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .unwrap();
    handle(&mem.deps(), &op).await.unwrap();
    mem.clear_events();
    mem
}

fn summary_value() -> Value {
    serde_json::to_value(summary(
        DirectOutcome::ReportedDone,
        Some("Added pct."),
        true,
    ))
    .unwrap()
}

fn evidence(mem: &Mem) -> Value {
    serde_json::from_slice(&mem.blob(&s3::evidence_key(RUN)).unwrap()).unwrap()
}

#[tokio::test]
async fn plan_delivery_keeps_the_research_note_of_the_summary_in_the_evidence_file() {
    let golden: Value = serde_json::from_str(GOLDEN).unwrap();
    let mut v = summary_value();
    v["research"] = golden["research"].clone();
    let mem = started_with_summary(serde_json::to_vec(&v).unwrap()).await;
    let p = plan(&mem).await;
    assert_eq!(p["deliver"], true, "{p}");
    assert_eq!(p["outcome"], "reported_done", "the summary was read: {p}");
    assert_eq!(
        evidence(&mem)["summary"]["research"],
        golden["research"],
        "the counters reach the evidence file of the zip"
    );
}

#[tokio::test]
async fn a_summary_without_the_member_gives_an_evidence_file_without_it() {
    let mem = started_with_summary(serde_json::to_vec(&summary_value()).unwrap()).await;
    let p = plan(&mem).await;
    assert_eq!(p["outcome"], "reported_done", "{p}");
    let e = evidence(&mem);
    assert!(e["summary"].is_object(), "{e}");
    assert!(e["summary"].get("research").is_none(), "{e}");
}
