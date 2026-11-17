//! How the run delivers reaches the worker without a member of the session request.
//!
//! A run with the zip target left out a workflow file its prompt asked for: the rules every run gets say that a pull request refuses such
//! a file and that a zip takes it, and the engine could not tell which of the two its run was. Protocol 1.61.0 had `start-session` write the
//! fact into the invocation of every session (`delivery`). A worker image of an earlier release refuses an invocation with a member it does
//! not know, so no run started after a rollback of the worker image alone.
//!
//! Protocol 1.62.0: the worker reads how the run delivers from the target of the stored request, which every worker receives, and
//! `start-session` writes no `delivery` member. The session request of a run that keeps its defaults holds only members the worker of
//! protocol 1.60.0 knows. The user's switch (`deliveryNote: false`) is a member of the stored request, not of the invocation.
//!
//! The delivery plan tells the bundle validator the target of the Run item; the fakes seed the Run item and the stored request from one
//! target, as control-api writes them (`crates/control-api/tests/b850_delivery_note.rs`).
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{
    DirectDelivery, DirectDeliveryKind, DirectInvocationRequest, delivery_to_say, s3,
    validate_direct_invocation,
};
use harness_protocol::target::RunTarget;
use run_session::ops::{Op, handle};
use run_session::slices::{Restore, RestoreKind, SlicePlan};
use serde::Deserialize;
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

/// The stored start request of the run, as the worker parses it.
fn stored_request(mem: &Mem) -> StartDirectRunRequest {
    serde_json::from_slice(
        &mem.blob(&s3::request_key(RUN))
            .expect("the request is stored"),
    )
    .unwrap()
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

/// The members of `DirectInvocationRequest` in protocol 1.60.0, read as that worker reads them: a
/// member that is not listed here fails the parse. The nested objects are taken as they come; the top level is where the released
/// workers refuse.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(dead_code)]
struct InvocationOf160 {
    kind: String,
    run_id: String,
    agent_id: String,
    turn: u32,
    runtime_session_id: String,
    credentials: Value,
    nonce: String,
    internal_api_url: String,
    region: String,
    engine: String,
    model: Value,
    #[serde(default)]
    failover: Vec<Value>,
    request: Value,
    repo: Value,
    timeout_s: u32,
    #[serde(default)]
    provider_silence_s: Option<u32>,
    #[serde(default)]
    snapshot_every_s: Option<u32>,
    #[serde(default)]
    packages_token: Option<Value>,
    #[serde(default)]
    packages_token_expires_at: Option<u64>,
    #[serde(default)]
    input_wait_s: Option<u32>,
    #[serde(default)]
    slice: Option<Value>,
}

#[test]
fn b850_the_stand_in_for_the_earlier_worker_refuses_a_member_it_does_not_know() {
    // The trigger of the tests below: this is what the worker of protocol 1.60.0 did with the invocation of protocol 1.61.0.
    let with_delivery = json!({
        "kind": "direct", "runId": RUN, "agentId": "direct", "turn": 1, "runtimeSessionId": "s", "credentials": {}, "nonce": "n",
        "internalApiUrl": "https://x", "region": "us-east-1", "engine": "cline", "model": {}, "request": {}, "repo": {}, "timeoutS": 1,
        "delivery": {"kind": "zip"}
    });
    let e = serde_json::from_value::<InvocationOf160>(with_delivery.clone()).unwrap_err();
    assert!(e.to_string().contains("unknown field `delivery`"), "{e}");
    let mut without = with_delivery;
    without.as_object_mut().unwrap().remove("delivery");
    serde_json::from_value::<InvocationOf160>(without)
        .expect("the same body without the member parses");
}

#[tokio::test]
async fn b850_no_session_request_names_the_delivery() {
    for target in [RunTarget::S3Only, repo_target(), new_repo_target()] {
        let mem = Mem::default();
        mem.seed(target.clone(), PROMPT, MODEL);
        start(&mem, 1)
            .await
            .unwrap_or_else(|e| panic!("{target:?}: the session starts: {e}"));
        assert!(
            mem.invocations()[0].get("delivery").is_none(),
            "{target:?}: {}",
            mem.invocations()[0]
        );
        assert_eq!(invocation(&mem, 0).delivery, None, "{target:?}");
    }
}

#[tokio::test]
async fn b850_a_worker_of_the_release_before_the_member_reads_every_session_request_of_a_default_run()
 {
    for target in [RunTarget::S3Only, repo_target()] {
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
        println!(
            "B850 TRIGGER PRESENT: the run is in its second session: turn {}",
            bodies[1]["turn"]
        );
        for (n, body) in bodies.iter().enumerate() {
            let read: InvocationOf160 = serde_json::from_value(body.clone()).unwrap_or_else(|e| {
                panic!(
                    "{target:?}: the worker of protocol 1.60.0 reads session {}: {e}",
                    n + 1
                )
            });
            assert_eq!(read.turn as usize, n + 1);
            assert_eq!(
                read.snapshot_every_s, None,
                "a default run names no interval"
            );
        }
    }
}

#[tokio::test]
async fn b850_the_worker_reads_how_the_run_delivers_from_the_stored_request() {
    for (target, kind, named) in [
        (RunTarget::S3Only, DirectDeliveryKind::Zip, None),
        (
            repo_target(),
            DirectDeliveryKind::PullRequest,
            Some("cc:hamh-e2e-python-calc"),
        ),
        (
            new_repo_target(),
            DirectDeliveryKind::PullRequest,
            Some("cc:hamh-e2e-new-20261111"),
        ),
    ] {
        let mem = Mem::default();
        mem.seed(target.clone(), PROMPT, MODEL);
        start(&mem, 1).await.unwrap();
        let request = stored_request(&mem);
        let said = delivery_to_say(
            invocation(&mem, 0).delivery.as_ref(),
            &request.target,
            request.delivery_note,
        )
        .unwrap_or_else(|| panic!("{target:?}: the worker has a delivery to say"));
        assert_eq!(said.kind, kind, "{target:?}");
        assert_eq!(said.repo.as_deref(), named, "{target:?}");
    }
}

#[tokio::test]
async fn b850_the_switch_rides_in_the_stored_request_and_not_in_the_session_request() {
    let mem = Mem::default();
    mem.seed_with_members(
        RunTarget::S3Only,
        PROMPT,
        MODEL,
        json!({"deliveryNote": false}),
    );
    start(&mem, 1).await.unwrap();
    let body = mem.invocations()[0].clone();
    assert!(body.get("delivery").is_none(), "{body}");
    assert!(body.get("deliveryNote").is_none(), "{body}");
    serde_json::from_value::<InvocationOf160>(body).expect("the invocation holds no new member");
    let request = stored_request(&mem);
    assert_eq!(request.delivery_note, Some(false));
    assert_eq!(
        delivery_to_say(None, &request.target, request.delivery_note),
        None,
        "the worker says nothing"
    );
}

/// A run that switched the note off and goes on into a second session (its first reached its time limit or was lost): the session
/// request of every session names neither the delivery nor the switch, a worker of the release before the member reads each of
/// them, and the stored request, which every session's worker reads, still holds the switch. So the setting holds in every later
/// session of the run.
#[tokio::test]
async fn b842_b850_every_session_of_a_run_with_the_note_off_gets_a_request_without_the_delivery_and_the_switch_stays_stored()
 {
    for target in [RunTarget::S3Only, repo_target()] {
        let mem = Mem::default();
        mem.set_now_ms(T0);
        mem.set_slice_deadline(SLICE_S);
        mem.seed_with_members(
            target.clone(),
            PROMPT,
            MODEL,
            json!({"maxRunS": 4_500, "deliveryNote": false}),
        );
        start(&mem, 1)
            .await
            .unwrap_or_else(|e| panic!("{target:?}: slice 1 starts: {e}"));
        mem.put_blob(&s3::slice_record_key(RUN, 2), plan_for(2));
        start(&mem, 2)
            .await
            .unwrap_or_else(|e| panic!("{target:?}: slice 2 starts: {e}"));
        let bodies = mem.invocations();
        assert_eq!(bodies.len(), 2, "{target:?}");
        println!(
            "B842-B850 TRIGGER PRESENT: the run with the note off is in its second session: turn {}",
            bodies[1]["turn"]
        );
        for (n, body) in bodies.iter().enumerate() {
            assert!(body.get("delivery").is_none(), "{target:?}: {body}");
            assert!(body.get("deliveryNote").is_none(), "{target:?}: {body}");
            let read: InvocationOf160 = serde_json::from_value(body.clone()).unwrap_or_else(|e| {
                panic!(
                    "{target:?}: the worker of protocol 1.60.0 reads session {}: {e}",
                    n + 1
                )
            });
            assert_eq!(read.turn as usize, n + 1, "{target:?}");
        }
        let request = stored_request(&mem);
        assert_eq!(request.delivery_note, Some(false), "{target:?}");
        assert_eq!(
            delivery_to_say(None, &request.target, request.delivery_note),
            None,
            "{target:?}: the worker of every session says nothing"
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
/// worker says `zip`, and reads every other run as a pull request (`delivery::validator::DeliveryTarget`). The packager is told the
/// same, so that EVIDENCE.md names a workflow file a zip holds.
#[tokio::test]
async fn b847_the_word_to_the_engine_and_the_target_of_the_delivery_check_agree() {
    for target in [RunTarget::S3Only, repo_target(), new_repo_target()] {
        let mem = Mem::default();
        mem.seed(target.clone(), PROMPT, MODEL);
        let word = DirectDelivery::of_target(&stored_request(&mem).target);
        assert!(
            harness_protocol::direct::check_delivery(&word),
            "{target:?}: the worker's check takes the word: {word:?}"
        );
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
            "{target:?}: the engine is told {word:?} and the validator is asked {}",
            p["validate"]
        );
        assert_eq!(
            word.kind == DirectDeliveryKind::PullRequest,
            validate.target == delivery::validator::DeliveryTarget::PullRequest,
            "{target:?}"
        );
        assert_eq!(
            p["package"].get("target") == Some(&json!("zip")),
            checked_as_zip,
            "{target:?}: the packager is told what the validator is told: {}",
            p["package"]
        );
    }
}

/// What the worker recorded as told to the engine reaches the packager with the evidence, so EVIDENCE.md can show it.
#[tokio::test]
async fn b850_the_note_of_the_summary_reaches_the_evidence_the_packager_renders() {
    const NOTE: &str =
        "How this run delivers (written by the harness): this run delivers only a zip.";
    let mem = Mem::default();
    mem.seed(RunTarget::S3Only, PROMPT, MODEL);
    mem.put_bundle();
    let mut s = summary(
        harness_protocol::direct::DirectOutcome::ReportedDone,
        Some("Added pct(a, b)."),
        true,
    );
    s.delivery_note = Some(NOTE.to_string());
    mem.put_summary(&s);
    let p = plan_delivery(&mem).await;
    assert_eq!(
        p["package"]["direct"]["deliveryNote"],
        json!(NOTE),
        "{}",
        p["package"]
    );
    // evidence/direct.json holds the whole summary, so the record in the zip has the note too.
    let record: Value = serde_json::from_slice(
        &mem.blob(&s3::evidence_key(RUN))
            .expect("the plan wrote its record"),
    )
    .unwrap();
    assert_eq!(record["summary"]["deliveryNote"], json!(NOTE), "{record}");
}

// ---- the copy interval beside it ----------------------------------------------------------------------------------------------------

/// A run whose request names a copy every 15 seconds: the invocation of its session carries the interval and no delivery, for a zip-only
/// run and for a run to a repository, and the worker's check takes it.
#[tokio::test]
async fn b823_b850_a_request_with_a_copy_every_15_seconds_reaches_the_worker_with_the_interval_and_no_delivery()
 {
    for target in [RunTarget::S3Only, repo_target()] {
        let mem = Mem::default();
        mem.seed_with_members(target.clone(), PROMPT, MODEL, json!({"snapshotEveryS": 15}));
        start(&mem, 1)
            .await
            .unwrap_or_else(|e| panic!("{target:?}: the session starts: {e}"));
        let body = mem.invocations()[0].clone();
        assert_eq!(body["snapshotEveryS"], json!(15), "{target:?}");
        assert!(body.get("delivery").is_none(), "{target:?}: {body}");
        let req = invocation(&mem, 0);
        assert_eq!(req.snapshot_every_s, Some(15), "{target:?}");
    }
}
