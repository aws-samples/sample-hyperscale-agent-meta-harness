//! The copy interval of a run reaches every session of the run. The start request may name `snapshotEveryS` (the user's
//! `run.copy_every_minutes`, 60 to 3,600 seconds); run-session copies it into the invocation of each session, the first one and every one the run
//! renews itself into, where the protocol's own check reads it again. A request that names none gets the default, one minute, as every run did
//! since the copies exist.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::direct::{
    DEFAULT_SNAPSHOT_EVERY_S, DirectInvocationRequest, s3, validate_direct_invocation,
};
use harness_protocol::target::RunTarget;
use run_session::ops::{Op, handle};
use run_session::slices::{Restore, RestoreKind, SlicePlan};
use serde_json::{Value, json};
use support::fakes::*;

const PROMPT: &str = "Add percent_change(a, b) with tests";
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
        recovery: None,
        base_sha: Some(BASE.to_string()),
        sessions_before: n - 1,
        brief: None,
        planned_at_ms: T0,
    })
    .unwrap()
}

#[tokio::test]
async fn b841_copy_a_request_without_the_member_leaves_the_timer_to_the_worker() {
    // The worker's default is 15 seconds, and a worker image of an earlier release refuses a number below 60. The deploy guard keeps
    // a deploy off the worker runtime while a run is active, so the case is a worker image rolled back alone: the invocation names no
    // interval unless the request did, and such an image then keeps its own default instead of refusing the invocation.
    let mem = Mem::default();
    mem.seed(RunTarget::S3Only, PROMPT, MODEL);
    start(&mem, 1).await.unwrap();
    assert_eq!(DEFAULT_SNAPSHOT_EVERY_S, 15);
    assert_eq!(invocation(&mem, 0).snapshot_every_s, None);
    assert!(mem.invocations()[0].get("snapshotEveryS").is_none());
}

#[tokio::test]
async fn b841_copy_the_interval_of_the_request_reaches_the_invocation() {
    for s in [15u32, 59, 60, 600, 3_600] {
        let mem = Mem::default();
        mem.seed_with_members(
            RunTarget::S3Only,
            PROMPT,
            MODEL,
            json!({"snapshotEveryS": s}),
        );
        start(&mem, 1).await.unwrap_or_else(|e| {
            panic!("a stored request with snapshotEveryS {s} starts its session: {e}")
        });
        assert_eq!(invocation(&mem, 0).snapshot_every_s, Some(s));
    }
}

#[tokio::test]
async fn b841_copy_every_session_of_the_run_keeps_the_interval() {
    let mem = Mem::default();
    mem.set_now_ms(T0);
    mem.set_slice_deadline(SLICE_S);
    mem.seed_with_members(
        RunTarget::S3Only,
        PROMPT,
        MODEL,
        json!({"maxRunS": 4_500, "snapshotEveryS": 600}),
    );
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
        "B841-COPY TRIGGER PRESENT: the run is in its second session: turn {}",
        bodies[1]["turn"]
    );
    assert_eq!(invocation(&mem, 0).snapshot_every_s, Some(600));
    assert_eq!(
        invocation(&mem, 1).snapshot_every_s,
        Some(600),
        "the session a run renews itself into copies as often as the first"
    );
}

#[tokio::test]
async fn b841_copy_a_stored_interval_no_request_may_name_ends_the_start_and_invokes_nothing() {
    // control-api refuses such a value; a request stored by another version, or changed in the bucket, is refused here once more
    for s in [0u32, 14, 3_601] {
        let mem = Mem::default();
        mem.seed_with_members(
            RunTarget::S3Only,
            PROMPT,
            MODEL,
            json!({"snapshotEveryS": s}),
        );
        let e = start(&mem, 1).await.unwrap_err();
        assert_eq!(e.error_type(), "RunSessionInvalid", "{s}: {e}");
        assert!(e.to_string().contains("snapshotEveryS"), "{s}: {e}");
        assert!(mem.invocations().is_empty(), "{s}: nothing was invoked");
    }
}
