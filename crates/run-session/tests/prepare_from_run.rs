//! `prepare` for a follow-up run: the stored request names the source run and the head control-api resolved, and the
//! `stage` payload for the repo-gateway carries `fromRun` instead of a base. run-session checks again what control-api checked, because
//! the stored request is a file the control plane wrote: the source run must exist and belong to the same principal, the target must be
//! `s3-only`, and the head must be there. Nothing is staged by `prepare`; a refusal ends the run with the reason.
#![allow(clippy::unwrap_used)]

mod support;

use delivery::gateway::{FromRun, GatewayRequest, StageRequest, StageTarget};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::ContinueFrom;
use harness_protocol::store::RunItem;
use harness_protocol::target::RunTarget;
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

const SOURCE: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X4Z";
const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";

async fn prepare(mem: &Mem) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(json!({"op": "prepare", "runId": RUN})).unwrap();
    handle(&mem.deps(), &op).await
}

fn source_run(owner: &str, status: &str) -> RunItem {
    serde_json::from_value(json!({
        "runId": SOURCE, "ownerPrincipalArn": owner, "promptSha256": "cd".repeat(32),
        "target": {"mode": "s3-only"}, "status": status, "phase": "green",
        "enginePolicyVersion": 0, "createdAt": "2026-10-13T17:00:00Z", "version": 3
    }))
    .unwrap()
}

/// An s3-only run whose stored request continues from `SOURCE`, with the source run in the store.
fn follow_up(mem: &Mem, c: Option<ContinueFrom>) {
    let mut request: StartDirectRunRequest =
        mem.seed(RunTarget::S3Only, "add a second file", MODEL);
    request.continue_from = c;
    mem.replace_request(serde_json::to_vec(&request).unwrap());
    mem.0
        .runs
        .lock()
        .unwrap()
        .insert(SOURCE.into(), source_run(OWNER, "complete"));
}

fn named(head: Option<&str>) -> Option<ContinueFrom> {
    Some(ContinueFrom {
        run_id: SOURCE.into(),
        head_sha: head.map(str::to_string),
    })
}

fn invalid(r: Result<Value, RunSessionError>) -> String {
    match r {
        Err(RunSessionError::Invalid(m)) => m,
        other => panic!("expected an invalid run, got {other:?}"),
    }
}

#[tokio::test]
async fn a_follow_up_stages_from_the_source_head_and_has_no_base() {
    let mem = Mem::default();
    follow_up(&mem, named(Some(HEAD)));
    let out = prepare(&mem).await.unwrap();
    assert_eq!(out["stage"]["operation"], "stage");
    assert_eq!(out["stage"]["runId"], RUN);
    assert_eq!(out["stage"]["target"], json!({"mode": "s3-only"}));
    assert_eq!(
        out["stage"]["fromRun"],
        json!({"runId": SOURCE, "headSha": HEAD})
    );
    assert!(out["stage"].get("baseRef").is_none(), "{out}");
    // The gateway parses exactly this payload.
    let GatewayRequest::Stage(StageRequest {
        from_run, base_ref, ..
    }) = serde_json::from_value::<GatewayRequest>(out["stage"].clone()).unwrap()
    else {
        panic!("a stage request")
    };
    assert_eq!(
        from_run,
        Some(FromRun {
            run_id: SOURCE.into(),
            head_sha: HEAD.into()
        })
    );
    assert!(base_ref.is_none());
    assert!(out.get("newRepo").is_none());
}

#[tokio::test]
async fn a_run_without_continue_from_is_staged_as_before() {
    let mem = Mem::default();
    mem.seed(RunTarget::S3Only, "p", MODEL);
    let out = prepare(&mem).await.unwrap();
    assert!(out["stage"].get("fromRun").is_none(), "{out}");
}

#[tokio::test]
async fn a_source_run_of_another_principal_is_refused() {
    let mem = Mem::default();
    follow_up(&mem, named(Some(HEAD)));
    mem.0.runs.lock().unwrap().insert(
        SOURCE.into(),
        source_run(
            "arn:aws:sts::700000000011:assumed-role/Admin/someone",
            "complete",
        ),
    );
    let m = invalid(prepare(&mem).await);
    assert!(m.contains("continueFrom") && m.contains(SOURCE), "{m}");
    assert!(
        !m.contains("someone"),
        "no name of the other principal: {m}"
    );
}

#[tokio::test]
async fn a_missing_source_run_is_refused() {
    let mem = Mem::default();
    follow_up(&mem, named(Some(HEAD)));
    mem.0.runs.lock().unwrap().remove(SOURCE);
    let m = invalid(prepare(&mem).await);
    assert!(m.contains("continueFrom") && m.contains(SOURCE), "{m}");
}

#[tokio::test]
async fn a_follow_up_without_a_resolved_head_is_refused() {
    let mem = Mem::default();
    follow_up(&mem, named(None));
    let m = invalid(prepare(&mem).await);
    assert!(m.contains("head"), "{m}");
}

/// A repository run of `OWNER` for the source: `name` is the repository its target names.
fn source_repo_run(name: &str) -> RunItem {
    let mut r = source_run(OWNER, "complete");
    r.target =
        serde_json::from_value(json!({"mode": "repo", "repo": format!("cc:{name}")})).unwrap();
    r
}

/// A repository follow-up: the stored request targets `cc:hamh-e2e-python-calc` and names `SOURCE`, whose own target is `source`.
fn repo_follow_up(mem: &Mem, base_ref: Option<&str>, source: Option<RunItem>) {
    let mut request: StartDirectRunRequest =
        mem.seed_with(repo_target(), "add a second file", MODEL, base_ref);
    request.continue_from = named(Some(HEAD));
    mem.replace_request(serde_json::to_vec(&request).unwrap());
    if let Some(s) = source {
        mem.0.runs.lock().unwrap().insert(SOURCE.into(), s);
    }
}

#[tokio::test]
async fn a_repository_follow_up_stages_from_the_source_head_on_the_same_repository() {
    let mem = Mem::default();
    repo_follow_up(&mem, None, Some(source_repo_run("hamh-e2e-python-calc")));
    let out = prepare(&mem).await.unwrap();
    assert_eq!(out["stage"]["target"], json!("cc:hamh-e2e-python-calc"));
    assert_eq!(
        out["stage"]["fromRun"],
        json!({"runId": SOURCE, "headSha": HEAD})
    );
    assert!(out["stage"].get("baseRef").is_none(), "{out}");
    assert_eq!(out["repo"], "hamh-e2e-python-calc");
    // The gateway parses exactly this payload.
    let GatewayRequest::Stage(StageRequest {
        from_run, target, ..
    }) = serde_json::from_value::<GatewayRequest>(out["stage"].clone()).unwrap()
    else {
        panic!("a stage request")
    };
    assert_eq!(from_run.unwrap().head_sha, HEAD);
    assert!(matches!(target, StageTarget::Repo(_)), "{target:?}");
}

#[tokio::test]
async fn a_repository_follow_up_may_follow_a_run_that_made_its_repository() {
    // The source ran with --new-repo; the follow-up names the repository it made.
    let mem = Mem::default();
    let mut source = source_run(OWNER, "complete");
    source.target =
        serde_json::from_value(json!({"mode": "new-repo", "repo": "cc:hamh-e2e-python-calc"}))
            .unwrap();
    repo_follow_up(&mem, None, Some(source));
    prepare(&mem).await.unwrap();
}

#[tokio::test]
async fn a_repository_follow_up_names_no_base_ref_but_its_sources_branch() {
    for (base, ok) in [
        (Some(format!("harness/{SOURCE}")), true),
        (Some(format!("refs/heads/harness/{SOURCE}")), true),
        (Some("main".to_string()), false),
        (
            Some("harness/01K5ZQ4T8M3N6P7R9S0V1W2X5A".to_string()),
            false,
        ),
    ] {
        let mem = Mem::default();
        repo_follow_up(
            &mem,
            base.as_deref(),
            Some(source_repo_run("hamh-e2e-python-calc")),
        );
        let r = prepare(&mem).await;
        if ok {
            let out = r.unwrap();
            assert_eq!(out["stage"]["baseRef"], json!(base.unwrap()));
        } else {
            let m = invalid(r);
            assert!(
                m.contains("baseRef") && m.contains(&format!("harness/{SOURCE}")),
                "{m}"
            );
        }
    }
}

#[tokio::test]
async fn a_repository_follow_up_of_another_repository_or_an_s3_only_run_is_refused() {
    for (what, source) in [
        ("another repository", source_repo_run("hamh-e2e-calc-two")),
        ("an s3-only run", source_run(OWNER, "complete")),
    ] {
        let mem = Mem::default();
        repo_follow_up(&mem, None, Some(source));
        let m = invalid(prepare(&mem).await);
        assert!(
            m.contains("continueFrom") && m.contains(SOURCE),
            "{what}: {m}"
        );
        assert!(
            m.contains("hamh-e2e-python-calc"),
            "{what}: names the repository: {m}"
        );
    }
}

#[tokio::test]
async fn a_repository_follow_up_of_another_principal_is_refused() {
    let mem = Mem::default();
    let mut other = source_repo_run("hamh-e2e-python-calc");
    other.owner_principal_arn = "arn:aws:sts::700000000011:assumed-role/Admin/someone".into();
    repo_follow_up(&mem, None, Some(other));
    let m = invalid(prepare(&mem).await);
    assert!(
        m.contains("continueFrom") && m.contains("owner") && !m.contains("someone"),
        "{m}"
    );
}

#[tokio::test]
async fn a_follow_up_on_a_new_repository_target_is_refused() {
    let mem = Mem::default();
    let target: RunTarget =
        serde_json::from_value(json!({"mode": "new-repo", "repo": "cc:hamh-e2e-new-20261017"}))
            .unwrap();
    let mut request: StartDirectRunRequest = mem.seed(target, "p", MODEL);
    request.continue_from = named(Some(HEAD));
    mem.replace_request(serde_json::to_vec(&request).unwrap());
    mem.0
        .runs
        .lock()
        .unwrap()
        .insert(SOURCE.into(), source_run(OWNER, "complete"));
    let m = invalid(prepare(&mem).await);
    assert!(m.contains("new-repo") || m.contains("--new-repo"), "{m}");
}

#[tokio::test]
async fn an_s3_only_follow_up_of_a_repository_run_is_still_an_s3_only_follow_up() {
    let mem = Mem::default();
    follow_up(&mem, named(Some(HEAD)));
    mem.0
        .runs
        .lock()
        .unwrap()
        .insert(SOURCE.into(), source_repo_run("hamh-e2e-python-calc"));
    let out = prepare(&mem).await.unwrap();
    assert_eq!(out["stage"]["target"], json!({"mode": "s3-only"}));
    assert!(out["stage"].get("fromRun").is_some());
}

#[tokio::test]
async fn an_s3_only_follow_up_with_a_base_ref_is_still_refused() {
    let mem = Mem::default();
    let mut request: StartDirectRunRequest =
        mem.seed_with(RunTarget::S3Only, "p", MODEL, Some("main"));
    request.continue_from = named(Some(HEAD));
    mem.replace_request(serde_json::to_vec(&request).unwrap());
    mem.0
        .runs
        .lock()
        .unwrap()
        .insert(SOURCE.into(), source_run(OWNER, "complete"));
    let m = invalid(prepare(&mem).await);
    assert!(m.contains("s3-only") && m.contains("baseRef"), "{m}");
}

// ---- start-session: the staged base is the delivered head -----------------------------------------------------------------------

async fn start_with(mem: &Mem, staged: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(
        json!({"op": "start-session", "runId": RUN, "taskToken": "token-A", "staged": staged}),
    )
    .unwrap();
    handle(&mem.deps(), &op).await
}

/// A repo-gateway image that predates `fromRun` ignores the member and stages the empty root commit; the follow-up would then run on an
/// empty tree and be delivered with no error. The start refuses it before the microVM is invoked.
#[tokio::test]
async fn a_follow_up_whose_staged_base_is_not_the_delivered_head_does_not_start() {
    let mem = Mem::default();
    follow_up(&mem, named(Some(HEAD)));
    let m = invalid(start_with(&mem, staged()).await);
    assert!(m.contains("continueFrom") && m.contains(SOURCE), "{m}");
    assert!(
        m.contains(&HEAD[..12]) && m.contains(&BASE[..12]),
        "names the head it should have and the base it got: {m}"
    );
    assert!(mem.invocations().is_empty(), "the microVM was not invoked");
    assert!(mem.agent().is_none(), "no agent was bound");
}

#[tokio::test]
async fn a_follow_up_with_no_staged_base_does_not_start() {
    let mem = Mem::default();
    follow_up(&mem, named(Some(HEAD)));
    let mut st = staged();
    st.as_object_mut().unwrap().remove("baseSha");
    let m = invalid(start_with(&mem, st).await);
    assert!(m.contains("continueFrom"), "{m}");
    assert!(mem.invocations().is_empty());
}

#[tokio::test]
async fn a_follow_up_staged_on_its_head_starts_with_that_base() {
    let mem = Mem::default();
    follow_up(&mem, named(Some(HEAD)));
    let mut st = staged();
    st["baseSha"] = json!(HEAD);
    let ack = start_with(&mem, st).await.unwrap();
    assert_eq!(ack["alreadyInvoked"], false);
    assert_eq!(mem.invocations().len(), 1);
    assert_eq!(mem.turn().unwrap().base_sha.as_deref(), Some(HEAD));
}

#[tokio::test]
async fn a_run_that_is_not_a_follow_up_is_not_held_to_a_head() {
    let mem = Mem::default();
    mem.seed(RunTarget::S3Only, "p", MODEL);
    start_with(&mem, staged()).await.unwrap();
    assert_eq!(mem.turn().unwrap().base_sha.as_deref(), Some(BASE));
}
