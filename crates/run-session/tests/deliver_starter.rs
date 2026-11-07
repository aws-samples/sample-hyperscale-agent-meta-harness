//! The harness commits the DOX starter on the repository's head before the first prompt, and agentd measures the run from that commit
//! (`summary.git.baseSha`), while the staged base stays the head the repository had. A repository run that changed nothing ends with the starter
//! commit as its head. The service must not open a pull request that holds only the starter: the rule that says "Cline left the repository
//! unchanged" compares the head with the commit agentd measured from as well. The delivered record (`baseSha` of the validate and package payloads)
//! keeps naming the repository the user supplied.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::direct::DirectOutcome;
use harness_protocol::target::RunTarget;
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

/// The starter commit: made by agentd on top of `BASE`, so it differs from both `BASE` and `HEAD`.
const STARTER: &str = "3333333333333333333333333333333333333333";

async fn plan(mem: &Mem, session: Value) -> Value {
    let op: Op = serde_json::from_value(
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged(), "session": session}),
    )
    .unwrap();
    handle(&mem.deps(), &op).await.unwrap()
}

fn seeded(target: RunTarget) -> Mem {
    let mem = Mem::default();
    mem.seed(target, "Add percent_change(a, b) with tests", MODEL);
    mem
}

/// A repository run whose final bundle is at `head`, with agentd's summary measured from `measured_from` (`None`: a summary with no base).
fn run_ending_at(head: &str, measured_from: Option<&str>, files_changed: u32) -> (Mem, Value) {
    let mem = seeded(repo_target());
    mem.put_bundle();
    let mut out = turn_output(true);
    out["lastCommitSha"] = json!(head);
    let mut s = summary(DirectOutcome::ReportedDone, Some("m"), true);
    s.git.base_sha = measured_from.map(str::to_string);
    s.git.head_sha = Some(head.to_string());
    s.git.files_changed = files_changed;
    mem.put_summary(&s);
    (mem, json!({"turn": out, "failure": {}}))
}

#[tokio::test]
async fn b766_a_repository_run_that_changed_nothing_after_the_starter_opens_no_pull_request() {
    let (mem, session) = run_ending_at(STARTER, Some(STARTER), 0);
    let p = plan(&mem, session).await;
    assert_eq!(p["deliver"], true, "{p}");
    assert_eq!(p["headSha"], STARTER);
    assert!(p.get("pushPr").is_none(), "{p}");
    assert_eq!(p["noPrReason"], "Cline left the repository unchanged");
}

#[tokio::test]
async fn b766_the_delivered_record_still_names_the_head_the_repository_had_as_its_base() {
    let (mem, session) = run_ending_at(STARTER, Some(STARTER), 0);
    let p = plan(&mem, session).await;
    assert_eq!(p["validate"]["baseSha"], BASE, "{p}");
    assert_eq!(p["package"]["baseSha"], BASE, "{p}");
}

#[tokio::test]
async fn b766_a_repository_run_that_changed_something_after_the_starter_opens_its_pull_request() {
    let (mem, session) = run_ending_at(HEAD, Some(STARTER), 3);
    let p = plan(&mem, session).await;
    assert_eq!(p["deliver"], true, "{p}");
    assert_eq!(p["pushPr"]["headSha"], HEAD, "{p}");
    assert!(p.get("noPrReason").is_none(), "{p}");
}

#[tokio::test]
async fn b766_a_summary_without_a_base_leaves_the_rule_on_the_staged_base() {
    // an image that predates the member: the rule is the one it always was
    let (mem, session) = run_ending_at(BASE, None, 0);
    let p = plan(&mem, session).await;
    assert!(p.get("pushPr").is_none(), "{p}");
    assert_eq!(p["noPrReason"], "Cline left the repository unchanged");
    let (mem, session) = run_ending_at(HEAD, None, 3);
    let p = plan(&mem, session).await;
    assert_eq!(p["pushPr"]["headSha"], HEAD, "{p}");
}

#[tokio::test]
async fn b766_a_run_that_ended_on_the_head_the_repository_had_opens_no_pull_request() {
    // the starter was skipped (the repository had a root of its own), or the engine put the tree back: the head is the staged base
    let (mem, session) = run_ending_at(BASE, Some(STARTER), 0);
    let p = plan(&mem, session).await;
    assert!(p.get("pushPr").is_none(), "{p}");
    assert_eq!(p["noPrReason"], "Cline left the repository unchanged");
}

#[tokio::test]
async fn b766_a_base_that_is_not_a_commit_id_is_not_taken_for_the_starter() {
    // a short value never matches: the rule falls back to the staged base, and a head past it opens a pull request
    let (mem, session) = run_ending_at(STARTER, Some("3333"), 0);
    let p = plan(&mem, session).await;
    assert_eq!(p["pushPr"]["headSha"], STARTER, "{p}");
    assert!(p.get("noPrReason").is_none(), "{p}");
}

#[tokio::test]
async fn b766_the_decision_is_stored_once_and_a_retry_reads_the_same_rule() {
    let (mem, session) = run_ending_at(STARTER, Some(STARTER), 0);
    let first = plan(&mem, session.clone()).await;
    let again = plan(&mem, session).await;
    assert_eq!(first["noPrReason"], again["noPrReason"], "{first} {again}");
    assert!(again.get("pushPr").is_none(), "{again}");
}

#[tokio::test]
async fn b766_a_snapshot_delivery_follows_the_same_rule() {
    // no final bundle: the last snapshot is delivered; at the starter commit nothing of Cline's is in it
    let mem = seeded(repo_target());
    mem.put_snapshot(STARTER);
    let mut s = summary(DirectOutcome::ReportedDone, Some("m"), false);
    s.git.base_sha = Some(STARTER.into());
    mem.put_summary(&s);
    let p = plan(&mem, json!({"turn": turn_output(true), "failure": {}})).await;
    assert_eq!(p["headSource"], "snapshot", "{p}");
    assert_eq!(p["headSha"], STARTER);
    assert!(p.get("pushPr").is_none(), "{p}");
    assert_eq!(p["noPrReason"], "Cline left the repository unchanged");
    // a snapshot past the starter holds work
    let mem = seeded(repo_target());
    mem.put_snapshot(HEAD);
    mem.put_summary(&s);
    let p = plan(&mem, json!({"turn": turn_output(true), "failure": {}})).await;
    assert_eq!(p["headSource"], "snapshot", "{p}");
    assert_eq!(p["pushPr"]["headSha"], HEAD, "{p}");
}

#[tokio::test]
async fn b766_the_zip_only_target_is_unaffected_by_the_starter() {
    let mem = seeded(RunTarget::S3Only);
    mem.put_bundle();
    let mut out = turn_output(true);
    out["lastCommitSha"] = json!(STARTER);
    let mut s = summary(DirectOutcome::ReportedDone, Some("m"), true);
    s.git.base_sha = Some(STARTER.into());
    s.git.head_sha = Some(STARTER.into());
    mem.put_summary(&s);
    let p = plan(&mem, json!({"turn": out, "failure": {}})).await;
    assert_eq!(p["deliver"], true, "{p}");
    assert!(
        p.get("pushPr").is_none() && p.get("noPrReason").is_none(),
        "{p}"
    );
}
