//! `plan-delivery` for a follow-up run on a repository: the `push-pr` payload carries the earlier run, the commit it
//! delivered and the link of its pull request (read from that run's `delivery.pr` event), so the repo-gateway can aim the new pull request
//! and link the two. The description of the pull request names the run it continues. The zip, the validation and the package are the new
//! run's, with the delivered commit as their base.
#![allow(clippy::unwrap_used)]

mod support;

use delivery::checks::pr_console_url;
use delivery::follow_up::FollowUp;
use delivery::gateway::GatewayRequest;
use delivery::packager::PackageRequest;
use delivery::validator::ValidateRequest;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{ContinueFrom, DirectOutcome};
use harness_protocol::store::RunItem;
use harness_protocol::target::RunTarget;
use run_session::error::RunSessionError;
use run_session::events;
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

const SOURCE: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X4Z";
/// The commit the source run delivered: the base of this run.
const SOURCE_HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const REPO: &str = "hamh-e2e-python-calc";

fn old_pr() -> String {
    pr_console_url(REPO, "7")
}

fn source_run() -> RunItem {
    serde_json::from_value(json!({
        "runId": SOURCE, "ownerPrincipalArn": OWNER, "promptSha256": "cd".repeat(32),
        "target": {"mode": "repo", "repo": format!("cc:{REPO}")}, "status": "complete", "phase": "green",
        "enginePolicyVersion": 0, "createdAt": "2026-10-13T17:00:00Z", "version": 3
    }))
    .unwrap()
}

/// A repository run that continues `SOURCE`; the source run is in the store, and its `delivery.pr` event when `link` is given.
fn seeded(link: Option<&str>) -> Mem {
    let mem = Mem::default();
    let mut request: StartDirectRunRequest =
        mem.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
    request.continue_from = Some(ContinueFrom {
        run_id: SOURCE.into(),
        head_sha: Some(SOURCE_HEAD.into()),
    });
    mem.replace_request(serde_json::to_vec(&request).unwrap());
    mem.0
        .runs
        .lock()
        .unwrap()
        .insert(SOURCE.into(), source_run());
    if let Some(url) = link {
        let e = events::delivery_pr(SOURCE, 1_790_000_000_000, url);
        mem.0.events.lock().unwrap().insert(e.event_id.clone(), e);
    }
    mem.put_bundle();
    mem.put_summary(&summary(
        DirectOutcome::ReportedDone,
        Some("Added pct(a, b)."),
        true,
    ));
    mem
}

fn staged_on_the_source_head() -> Value {
    let mut st = staged();
    st["baseSha"] = json!(SOURCE_HEAD);
    st
}

async fn plan(mem: &Mem, staged: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(json!({
        "op": "plan-delivery", "runId": RUN, "staged": staged,
        "session": {"turn": turn_output(true), "failure": {}}
    }))
    .unwrap();
    handle(&mem.deps(), &op).await
}

fn push_pr(p: &Value) -> delivery::gateway::PushPrRequest {
    let GatewayRequest::PushPr(r) = serde_json::from_value(p["pushPr"].clone()).unwrap() else {
        panic!("push-pr expected")
    };
    r
}

#[tokio::test]
async fn the_pull_request_names_the_run_it_continues_the_commit_and_the_old_pull_request() {
    let mem = seeded(Some(&old_pr()));
    let p = plan(&mem, staged_on_the_source_head()).await.unwrap();
    assert_eq!(p["deliver"], true);
    assert_eq!(
        p["pushPr"]["followUp"],
        json!({"runId": SOURCE, "headSha": SOURCE_HEAD, "pullRequestUrl": old_pr()})
    );
    // The gateway parses exactly this payload.
    let r = push_pr(&p);
    assert_eq!(
        r.follow_up,
        Some(FollowUp {
            run_id: SOURCE.into(),
            head_sha: SOURCE_HEAD.into(),
            pull_request_url: Some(old_pr()),
        })
    );
    assert!(
        r.description.contains(&format!(
            "Follow-up of run {SOURCE}; it continues the pull request {}.",
            old_pr()
        )),
        "{}",
        r.description
    );
    assert!(r.description.chars().count() <= 10_240);
}

#[tokio::test]
async fn the_zip_the_validation_and_the_package_are_the_new_runs_with_the_delivered_commit_as_base()
{
    let mem = seeded(Some(&old_pr()));
    let p = plan(&mem, staged_on_the_source_head()).await.unwrap();
    let pkg: PackageRequest = serde_json::from_value(p["package"].clone()).unwrap();
    assert_eq!(
        pkg.run_id, RUN,
        "the zip is this run's, not the source run's"
    );
    assert_eq!(pkg.repo, REPO);
    assert_eq!(pkg.head_sha, HEAD);
    assert_eq!(pkg.base_sha.as_deref(), Some(SOURCE_HEAD));
    let v: ValidateRequest = serde_json::from_value(p["validate"].clone()).unwrap();
    assert_eq!(v.run_id, RUN);
    assert_eq!(v.base_sha.as_deref(), Some(SOURCE_HEAD));
    assert!(
        v.source_bundle_uri
            .as_deref()
            .unwrap()
            .contains(&format!("runs/{RUN}/")),
        "{v:?}"
    );
    assert!(
        !p.to_string().contains(&format!("runs/{SOURCE}/")),
        "no object of the source run is named: {p}"
    );
    let r = push_pr(&p);
    assert_eq!(r.run_id, RUN);
}

#[tokio::test]
async fn a_source_that_opened_no_pull_request_leaves_the_link_out_and_says_so() {
    let mem = seeded(None);
    let p = plan(&mem, staged_on_the_source_head()).await.unwrap();
    assert_eq!(
        p["pushPr"]["followUp"],
        json!({"runId": SOURCE, "headSha": SOURCE_HEAD})
    );
    let r = push_pr(&p);
    assert!(
        r.description.contains(&format!(
            "Follow-up of run {SOURCE}; that run opened no pull request."
        )),
        "{}",
        r.description
    );
}

#[tokio::test]
async fn a_run_that_continues_nothing_has_no_follow_up_member_and_the_description_it_always_had() {
    let mem = Mem::default();
    mem.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
    mem.put_bundle();
    mem.put_summary(&summary(
        DirectOutcome::ReportedDone,
        Some("Added pct(a, b)."),
        true,
    ));
    let p = plan(&mem, staged()).await.unwrap();
    assert!(p["pushPr"].get("followUp").is_none(), "{p}");
    assert!(!push_pr(&p).description.contains("Follow-up"));
}

#[tokio::test]
async fn a_follow_up_that_changed_nothing_opens_no_pull_request() {
    // The final head is the commit the follow-up started from.
    let mem = seeded(Some(&old_pr()));
    let mut st = staged();
    st["baseSha"] = json!(HEAD);
    let p = plan(&mem, st).await.unwrap();
    assert!(p.get("pushPr").is_none(), "{p}");
    assert_eq!(p["noPrReason"], "Cline left the repository unchanged");
}

#[tokio::test]
async fn an_s3_only_follow_up_opens_no_pull_request_and_reads_no_old_link() {
    let mem = Mem::default();
    let mut request: StartDirectRunRequest = mem.seed(RunTarget::S3Only, "p", MODEL);
    request.continue_from = Some(ContinueFrom {
        run_id: SOURCE.into(),
        head_sha: Some(SOURCE_HEAD.into()),
    });
    mem.replace_request(serde_json::to_vec(&request).unwrap());
    mem.0
        .runs
        .lock()
        .unwrap()
        .insert(SOURCE.into(), source_run());
    mem.put_bundle();
    mem.put_summary(&summary(DirectOutcome::ReportedDone, Some("m"), true));
    // A store that cannot answer the link is not asked: an s3-only run has no pull request.
    mem.fail(
        "delivery_pr_url",
        RunSessionError::Unavailable("not asked".into()),
    );
    let p = plan(&mem, staged_on_the_source_head()).await.unwrap();
    assert!(p.get("pushPr").is_none(), "{p}");
}

#[tokio::test]
async fn a_store_that_cannot_read_the_old_link_fails_the_op_so_the_machine_retries_it() {
    let mem = seeded(Some(&old_pr()));
    mem.fail(
        "delivery_pr_url",
        RunSessionError::Unavailable("throttled".into()),
    );
    let e = plan(&mem, staged_on_the_source_head()).await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionUnavailable");
    // The retry gets the link: a missing link is never guessed.
    let p = plan(&mem, staged_on_the_source_head()).await.unwrap();
    assert!(
        p["pushPr"]["followUp"].get("pullRequestUrl").is_some(),
        "{p}"
    );
}
