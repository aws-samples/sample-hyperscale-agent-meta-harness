//! run-session tells the worker how the run delivers. A run with the zip target left out a workflow file its prompt asked for: the
//! rules every run gets say that a pull request refuses such a file and that a zip takes it, and only the service knows which of the two a
//! run is (its target). `start-session` now writes the fact into the invocation of every session of the run (`delivery`, protocol 1.61.0):
//! a zip alone for the zip target, else a pull request to the repository as the user named it. The worker says it to the engine at the
//! start of every session.
//!
//! A follow-up run (`ham continue`), a resumed run (`ham resume`) and a run started again are new runs with their own Run item, so the
//! invocation of each says how that run delivers.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::direct::{
    DirectDelivery, DirectDeliveryKind, DirectInvocationRequest, s3, validate_direct_invocation,
};
use harness_protocol::target::RunTarget;
use run_session::ops::{Op, handle};
use run_session::slices::{Restore, RestoreKind, SlicePlan};
use serde_json::{Value, json};
use support::fakes::*;

const PROMPT: &str = "Add percent_change(a, b) with tests and a workflow file";
const SLICE_S: u32 = 600;

async fn start(mem: &Mem, slice: u32) -> Result<Value, run_session::RunSessionError> {
    let op = Op::StartSession {
        run_id: RUN.to_string(),
        task_token: "task-token".to_string(),
        staged: staged(),
        slice: Some(slice),
    };
    handle(&mem.deps(), &op).await
}

fn invocation(mem: &Mem, n: usize) -> DirectInvocationRequest {
    let req: DirectInvocationRequest =
        serde_json::from_value(mem.invocations()[n].clone()).unwrap();
    validate_direct_invocation(&req).unwrap();
    req
}

fn new_repo_target() -> RunTarget {
    serde_json::from_value(json!({"mode": "new-repo", "repo": "cc:hamh-e2e-new-20261111"})).unwrap()
}

/// The record run-session reads for slice `n`: the plan `next-slice` writes when the session before it handed over.
fn plan_for(n: u32) -> Vec<u8> {
    serde_json::to_vec_pretty(&SlicePlan {
        slice: n,
        engine_s: SLICE_S,
        last: false,
        restore: Restore {
            kind: RestoreKind::Base,
            key: s3::repo_bundle_key(RUN),
            head: Some(BASE.to_string()),
        },
        base_sha: Some(BASE.to_string()),
        sessions_before: n - 1,
        brief: None,
        planned_at_ms: T0,
    })
    .unwrap()
}

#[tokio::test]
async fn b847_a_zip_only_run_tells_the_worker_that_it_delivers_a_zip() {
    let mem = Mem::default();
    mem.seed(RunTarget::S3Only, PROMPT, MODEL);
    start(&mem, 1).await.unwrap();
    assert_eq!(invocation(&mem, 0).delivery, Some(DirectDelivery::zip()));
    assert_eq!(mem.invocations()[0]["delivery"], json!({"kind": "zip"}));
}

#[tokio::test]
async fn b847_a_repository_run_names_the_repository_as_the_user_named_it() {
    for (target, named) in [
        (repo_target(), "cc:hamh-e2e-python-calc"),
        (new_repo_target(), "cc:hamh-e2e-new-20261111"),
    ] {
        let mem = Mem::default();
        mem.seed(target.clone(), PROMPT, MODEL);
        start(&mem, 1)
            .await
            .unwrap_or_else(|e| panic!("{target:?}: the session starts: {e}"));
        assert_eq!(
            invocation(&mem, 0).delivery,
            Some(DirectDelivery::pull_request(named)),
            "{target:?}"
        );
        assert_eq!(
            mem.invocations()[0]["delivery"],
            json!({"kind": "pull-request", "repo": named}),
            "{target:?}"
        );
    }
}

#[tokio::test]
async fn b847_every_session_of_the_run_is_told_how_it_delivers() {
    for (target, kind) in [
        (RunTarget::S3Only, DirectDeliveryKind::Zip),
        (repo_target(), DirectDeliveryKind::PullRequest),
    ] {
        let mem = Mem::default();
        mem.set_now_ms(T0);
        mem.set_slice_deadline(SLICE_S);
        mem.seed_with_members(target.clone(), PROMPT, MODEL, json!({"maxRunS": 4_500}));
        start(&mem, 1)
            .await
            .unwrap_or_else(|e| panic!("slice 1 starts: {e}"));
        mem.put_blob(&s3::slice_record_key(RUN, 2), plan_for(2));
        start(&mem, 2)
            .await
            .unwrap_or_else(|e| panic!("slice 2 starts: {e}"));
        let bodies = mem.invocations();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[1]["turn"], json!(2));
        println!(
            "B847 TRIGGER PRESENT: the run is in its second session: turn {}",
            bodies[1]["turn"]
        );
        let first = invocation(&mem, 0).delivery;
        assert_eq!(first.as_ref().map(|d| d.kind), Some(kind), "{target:?}");
        assert_eq!(
            invocation(&mem, 1).delivery,
            first,
            "{target:?}: the session a run renews itself into is told what the first was told"
        );
    }
}

// ---- the word and the check read one target ------------------------------------------------------------------------------------------

async fn plan_delivery(mem: &Mem) -> Value {
    let op: Op = serde_json::from_value(
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged(),
        "session": {"turn": turn_output(true), "failure": {}}}),
    )
    .unwrap();
    handle(&mem.deps(), &op).await.unwrap()
}

/// What the engine is told and what its work meets cannot differ: the bundle validator is told `target: zip` for exactly the runs whose
/// worker is told `zip`, and reads every other run as a pull request (`delivery::validator::DeliveryTarget`).
#[tokio::test]
async fn b847_the_word_to_the_worker_and_the_target_of_the_delivery_check_agree() {
    for target in [RunTarget::S3Only, repo_target(), new_repo_target()] {
        let word = run_session::start_session::delivery_of(&target);
        assert!(
            harness_protocol::direct::check_delivery(&word),
            "{target:?}: the worker's check takes the word: {word:?}"
        );
        let mem = Mem::default();
        mem.seed(target.clone(), PROMPT, MODEL);
        mem.put_bundle();
        mem.put_summary(&summary(
            harness_protocol::direct::DirectOutcome::ReportedDone,
            Some("Added pct(a, b)."),
            true,
        ));
        let p = plan_delivery(&mem).await;
        let validate: delivery::validator::ValidateRequest =
            serde_json::from_value(p["validate"].clone()).unwrap();
        let checked_as_zip = validate.target == delivery::validator::DeliveryTarget::Zip;
        assert_eq!(
            word.kind == DirectDeliveryKind::Zip,
            checked_as_zip,
            "{target:?}: the worker is told {word:?} and the validator is asked {}",
            p["validate"]
        );
        assert_eq!(
            word.kind == DirectDeliveryKind::PullRequest,
            validate.target == delivery::validator::DeliveryTarget::PullRequest,
            "{target:?}"
        );
    }
}

// ---- the copy interval and the delivery in one invocation (the two changes of protocol 1.61.0 together) ------------------------------

/// A run whose request names a copy every 15 seconds: the invocation of its session carries the interval and how the run delivers, for
/// a zip-only run and for a run to a repository, and the worker's check takes it.
#[tokio::test]
async fn b823_b847_a_request_with_a_copy_every_15_seconds_reaches_the_worker_with_the_interval_and_the_delivery()
 {
    for (target, delivery) in [
        (RunTarget::S3Only, json!({"kind": "zip"})),
        (
            repo_target(),
            json!({"kind": "pull-request", "repo": "cc:hamh-e2e-python-calc"}),
        ),
    ] {
        let mem = Mem::default();
        mem.seed_with_members(target.clone(), PROMPT, MODEL, json!({"snapshotEveryS": 15}));
        start(&mem, 1)
            .await
            .unwrap_or_else(|e| panic!("{target:?}: the session starts: {e}"));
        let body = mem.invocations()[0].clone();
        assert_eq!(body["snapshotEveryS"], json!(15), "{target:?}");
        assert_eq!(body["delivery"], delivery, "{target:?}");
        // `invocation` runs the worker's own check on the body.
        assert_eq!(invocation(&mem, 0).snapshot_every_s, Some(15), "{target:?}");
    }
}

/// A run whose request names no interval: the invocation leaves the interval to the worker and still says how the run delivers. So the
/// invocation of this version always carries a member that a worker of the release before does not know, whatever the interval: leaving
/// `snapshotEveryS` out does not make it an invocation such a worker reads, and the service and the worker image roll back together.
#[tokio::test]
async fn b823_b847_a_request_without_an_interval_still_tells_the_worker_how_the_run_delivers() {
    for (target, delivery) in [
        (RunTarget::S3Only, json!({"kind": "zip"})),
        (
            repo_target(),
            json!({"kind": "pull-request", "repo": "cc:hamh-e2e-python-calc"}),
        ),
    ] {
        let mem = Mem::default();
        mem.seed(target.clone(), PROMPT, MODEL);
        start(&mem, 1)
            .await
            .unwrap_or_else(|e| panic!("{target:?}: the session starts: {e}"));
        let body = mem.invocations()[0].clone();
        assert!(
            body.get("snapshotEveryS").is_none(),
            "{target:?}: the interval is the worker's own: {body}"
        );
        assert_eq!(body["delivery"], delivery, "{target:?}");
        assert_eq!(invocation(&mem, 0).snapshot_every_s, None, "{target:?}");
    }
}
