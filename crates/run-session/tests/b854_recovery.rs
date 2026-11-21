//! The op `recover` and the two states of the run machine that reach it. A session of a run left no result (its worker gave notice
//! that it is being ended, its heartbeat stopped, or the state ended another way). Before `next-slice` decides, `run-session` fences that
//! session, stops it, and calls its session id once more: a new machine with the same storage copies the work tree into one bundle. The
//! op writes what came of it under `runs/<run>/slices/` and decides nothing.
//!
//! What this file holds: a session with a result never reaches the read; the read failing or timing out never fails the run; the order is
//! fence, stop, call; one attempt per session; the answer is checked against the object in the bucket; a worker image that cannot read is
//! one miss; the whole op ends within its limit; the credentials of the read are its own.
#![allow(clippy::unwrap_used)]

mod support;

use delivery::gateway::GatewayRequest;
use harness_protocol::direct::{
    DirectOutcome, RECOVERY_CREDENTIALS_S, RECOVERY_LIMIT_S, RECOVERY_SETTLE_S,
    RECOVERY_STOP_CAP_S, RECOVERY_WORKER_BUDGET_S, RecoveryInvocationRequest, s3,
    validate_recovery_invocation,
};
use harness_protocol::goal::GoalStatus;
use harness_protocol::target::RunTarget;
use run_session::RunSessionError;
use run_session::ops::{Op, handle};
use run_session::recover::{RecoveryAttempt, RecoveryResult};
use serde_json::{Value, json};
use std::collections::VecDeque;
use support::asl::{Lambdas, Wait, direct_definition, walk};
use support::fakes::*;

fn sha(c: char) -> String {
    c.to_string().repeat(40)
}

async fn call(mem: &Mem, v: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(v).unwrap();
    handle(&mem.deps(), &op).await
}

fn seeded(extra: Value) -> Mem {
    let mem = Mem::default();
    mem.set_now_ms(T0);
    clear_pauses();
    mem.seed_with_members(
        RunTarget::S3Only,
        "Add percent_change(a, b) with tests",
        MODEL,
        extra,
    );
    mem
}

/// A run whose first session started at `T0` and saved a copy at commit `s`.
async fn started_with_copy(s: &str) -> Mem {
    let mem = seeded(json!({"maxRunS": 8_000}));
    call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
    mem.put_snapshot_t(1, s);
    mem
}

fn silence() -> Value {
    json!({"turn": {}, "failure": {"Error": "States.HeartbeatTimeout", "Cause": "heartbeat"}})
}

fn notice(at: &str) -> Value {
    json!({"turn": {"sessionEnded": {"session": 1, "cause": "end-requested", "at": at}}, "failure": {}})
}

async fn recover(mem: &Mem, session: Value) -> Value {
    call(
        mem,
        json!({"op": "recover", "runId": RUN, "session": session, "loop": {"slice": 1}}),
    )
    .await
    .unwrap()
}

fn attempt(mem: &Mem, n: u32) -> Option<RecoveryAttempt> {
    mem.blob(&s3::recovery_attempt_key(RUN, n))
        .map(|b| serde_json::from_slice(&b).unwrap())
}

fn result(mem: &Mem, n: u32) -> Option<RecoveryResult> {
    mem.blob(&s3::recovery_result_key(RUN, n))
        .map(|b| serde_json::from_slice(&b).unwrap())
}

/// What a worker answers after it stored a copy at commit `head` on top of `parent`: the marker names `marker_head`, nothing is in doubt.
fn stored(head: &str, parent: &str, marker_head: &str, sum: &str) -> Value {
    json!({
        "attemptId": "<attempt>", "runId": RUN, "session": 1, "recovered": true,
        "copy": {"headSha": head, "treeSha": sha('7'), "parentSha": parent,
                 "marker": {"schemaVersion": 1, "session": 1, "count": 5, "headSha": marker_head},
                 "stable": true, "files": 12, "bytes": 3456, "bundleBytes": 60, "bundleSha256": sum},
        "comparison": {"state": "done", "sameAsBaseline": false, "regressed": 0, "rewritten": 0},
        "uptimeS": 41, "tookMs": 2300, "agentdVersion": "0.1.0"
    })
}

// ---- the op ------------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn b854_the_fence_comes_before_the_stop_and_the_stop_before_the_call() {
    let (s, r) = (sha('5'), sha('6'));
    let mem = started_with_copy(&s).await;
    let before = mem.agent().unwrap();
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &s, &sum));

    let out = recover(&mem, silence()).await;
    assert_eq!(out, json!({"recovery": "copied"}));
    // fence, stop, mint, call: in that order, each once.
    assert_eq!(mem.recovery_calls(), ["fence", "stop", "mint", "invoke"]);
    // The fence: the generation is fenced, the credential refresh is revoked, the version moved, the completion key is kept.
    let agent = mem.agent().unwrap();
    assert_eq!(agent.fenced_generation, Some(1));
    assert_eq!(agent.session_generation, Some(1));
    assert!(agent.creds.as_ref().unwrap().revoked);
    assert_eq!(agent.version, before.version + 1);
    assert_eq!(
        agent.creds.as_ref().unwrap().completion_key_sha256,
        before.creds.as_ref().unwrap().completion_key_sha256,
        "the fence never clears the completion key: an item without one reads as one from before the key existed"
    );
    // The call: the old session id, at the run's pinned endpoint, with a body both sides accept.
    let targets = mem.recover_targets();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].0, before.runtime_arn);
    assert_eq!(targets[0].1, before.runtime_endpoint.clone().unwrap());
    assert_eq!(targets[0].2, before.runtime_session_id);
    assert_eq!(mem.stops().last().unwrap().1, before.runtime_session_id);
    let body = &mem.recover_bodies()[0];
    let req: RecoveryInvocationRequest = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(validate_recovery_invocation(&req), Ok(()));
    assert_eq!(req.session, 1);
    assert_eq!(req.runtime_session_id, before.runtime_session_id);
    assert_eq!(
        req.object,
        format!("s3://{BUCKET}/{}", s3::recovery_bundle_key(RUN, 1))
    );
    assert_eq!(req.base_sha, BASE);
    assert_eq!(req.start_head, BASE);
    assert_eq!(req.baseline.head_sha, s);
    assert_eq!(req.baseline.count, Some(5));
    assert_eq!(
        req.baseline.object.as_deref(),
        Some(format!("s3://{BUCKET}/{}", s3::snapshot_bundle_key_t(RUN, 1)).as_str()),
        "the worker fetches the saved copy with its own credentials; no presigned URL is sent"
    );
    assert!((1..=RECOVERY_WORKER_BUDGET_S).contains(&req.budget_s));
    // It was no turn: the fake counts no direct invocation beyond the session's own.
    assert_eq!(mem.invocations().len(), 1);
    // The records.
    let a = attempt(&mem, 1).unwrap();
    assert_eq!(a.attempt_id, req.attempt_id);
    assert_eq!(a.cause, "no-heartbeat");
    assert_eq!(a.end_known_at_ms, None);
    assert_eq!(a.deadline_ms, a.started_at_ms + RECOVERY_LIMIT_S * 1000);
    let res = result(&mem, 1).unwrap();
    assert_eq!(res.state, "copied");
    assert_eq!(res.attempt_id.as_deref(), Some(a.attempt_id.as_str()));
    assert_eq!(res.stop.as_ref().unwrap().stopped, Some(true));
    assert_eq!(res.baseline.as_ref().unwrap().head_sha, s);
    let copy = res.copy.as_ref().unwrap();
    assert_eq!(
        (copy.head_sha.as_str(), copy.parent_sha.as_deref()),
        (r.as_str(), Some(s.as_str()))
    );
    assert_eq!(copy.key, s3::recovery_bundle_key(RUN, 1));
    assert!(res.comparison.as_ref().unwrap().done());
    assert_eq!(res.invoke.as_ref().unwrap().tries, 1);
}

#[tokio::test]
async fn b854_a_stop_without_a_definite_answer_is_a_miss_and_the_old_session_id_is_not_called() {
    // The stop fails: no call.
    let mem = started_with_copy(&sha('5')).await;
    mem.fail(
        "stop",
        RunSessionError::Unavailable("ThrottlingException".into()),
    );
    assert_eq!(
        recover(&mem, silence()).await,
        json!({"recovery": "missed", "reason": "stop-failed"})
    );
    assert_eq!(mem.recovery_calls(), ["fence", "stop"]);
    assert!(mem.recover_bodies().is_empty());
    assert_eq!(result(&mem, 1).unwrap().stop.unwrap().stopped, None);

    // The stop never answers: given up after its cap, no call.
    let mem = started_with_copy(&sha('5')).await;
    mem.stop_hangs();
    let t = mem.now_ms();
    assert_eq!(
        recover(&mem, silence()).await,
        json!({"recovery": "missed", "reason": "stop-failed"})
    );
    assert!(mem.recover_bodies().is_empty());
    assert_eq!(mem.now_ms() - t, RECOVERY_STOP_CAP_S * 1000);

    // The session had already ended (not found): a definite answer, and the call is made.
    let mem = started_with_copy(&sha('5')).await;
    *mem.0.stop_gone.lock().unwrap() = true;
    recover(&mem, silence()).await;
    assert_eq!(mem.recovery_calls(), ["fence", "stop", "mint", "invoke"]);
    assert_eq!(result(&mem, 1).unwrap().stop.unwrap().stopped, Some(false));
}

#[tokio::test]
async fn b854_a_second_recover_call_for_one_generation_calls_nothing() {
    let (s, r) = (sha('5'), sha('6'));
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &s, &sum));
    assert_eq!(recover(&mem, silence()).await["recovery"], "copied");
    let (calls, minted, stops) = (
        mem.recovery_calls(),
        mem.0.minted.lock().unwrap().len(),
        mem.stops().len(),
    );
    let first = result(&mem, 1).unwrap();

    assert_eq!(
        recover(&mem, silence()).await,
        json!({"recovery": "duplicate"})
    );
    assert_eq!(
        mem.recovery_calls(),
        calls,
        "no second fence, stop, mint or call"
    );
    assert_eq!(mem.0.minted.lock().unwrap().len(), minted);
    assert_eq!(mem.stops().len(), stops);
    assert_eq!(
        result(&mem, 1).unwrap(),
        first,
        "the result is written once"
    );
}

#[tokio::test]
async fn b854_no_read_for_a_run_that_was_cleared_or_switched_it_off_or_runs_on_instances() {
    // Trigger: the same run with nothing in the way is read.
    let mem = started_with_copy(&sha('5')).await;
    recover(&mem, silence()).await;
    assert_eq!(
        mem.recover_bodies().len(),
        1,
        "TRIGGER PRESENT: a lost session of an active run on the microVM is read"
    );

    // Switched off by the run: the Agent item says so, written by start-session from the stored request.
    let mem = seeded(json!({"maxRunS": 8_000, "recoverAfterStop": false}));
    call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
    assert_eq!(mem.agent().unwrap().recover_after_stop, Some(false));
    assert_eq!(
        recover(&mem, silence()).await,
        json!({"recovery": "skipped", "reason": "off"})
    );
    assert!(
        mem.recovery_calls().is_empty(),
        "no fence, no stop, no call: {:?}",
        mem.recovery_calls()
    );
    assert!(attempt(&mem, 1).is_none());
    assert_eq!(mem.agent().unwrap().fenced_generation, None);

    // A run someone ended.
    let mem = started_with_copy(&sha('5')).await;
    mem.0.runs.lock().unwrap().get_mut(RUN).unwrap().status = GoalStatus::Cleared;
    assert_eq!(
        recover(&mem, silence()).await,
        json!({"recovery": "skipped", "reason": "run-ended"})
    );
    assert!(mem.recovery_calls().is_empty());

    // A run that is ended between the stop and the call: stopped, and not called.
    let mem = started_with_copy(&sha('5')).await;
    *mem.0.flip_run_after_reads.lock().unwrap() = Some((1, GoalStatus::Cleared));
    assert_eq!(
        recover(&mem, silence()).await,
        json!({"recovery": "skipped", "reason": "run-ended"})
    );
    assert_eq!(mem.recovery_calls(), ["fence", "stop"]);
    assert!(mem.recover_bodies().is_empty());

    // Instances compute: its session's volume is deleted with the session.
    let mem = started_with_copy(&sha('5')).await;
    mem.0
        .agents
        .lock()
        .unwrap()
        .get_mut("direct")
        .unwrap()
        .compute = Some("instances".into());
    assert_eq!(
        recover(&mem, silence()).await,
        json!({"recovery": "skipped", "reason": "instances"})
    );
    assert!(mem.recovery_calls().is_empty());

    // A session that ended with a result, and one that was never invoked: nothing is written at all.
    let mem = started_with_copy(&sha('5')).await;
    let done = json!({"turn": {"summary": "x", "runId": RUN, "agentId": "direct", "turn": 1, "finishReason": "end_turn", "outcome": "completed"}, "failure": {}});
    assert_eq!(recover(&mem, done).await, json!({"recovery": "not-needed"}));
    assert!(result(&mem, 1).is_none() && attempt(&mem, 1).is_none());
    let mem = seeded(json!({}));
    assert_eq!(
        recover(&mem, silence()).await,
        json!({"recovery": "not-needed"})
    );
    assert!(result(&mem, 1).is_none() && attempt(&mem, 1).is_none());
}

#[tokio::test]
async fn b854_the_answer_is_checked_against_the_object() {
    let (s, r) = (sha('5'), sha('6'));
    // The object is the one the answer describes: copied.
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &s, &sum));
    assert_eq!(recover(&mem, silence()).await["recovery"], "copied");

    // Another attempt, another run, another session; another checksum than the object's; another commit than the bundle's header names.
    let edits: [(&str, Value); 5] = [
        ("/attemptId", json!("01M6B3S8K4D0YK5E9F7GR1XV2Q")),
        ("/runId", json!("01M4X3Q7R9S0V1W2X3Y4Z5A6B8")),
        ("/session", json!(2)),
        (
            "/copy/bundleSha256",
            json!("47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="),
        ),
        ("/copy/headSha", json!(sha('9'))),
    ];
    for (pointer, value) in edits {
        let mem = started_with_copy(&s).await;
        let sum = mem.put_recovery_bundle(1, &r);
        let mut answer = stored(&r, &s, &s, &sum);
        *answer.pointer_mut(pointer).unwrap() = value;
        mem.recover_answers(&answer);
        let out = recover(&mem, silence()).await;
        assert_eq!(
            out,
            json!({"recovery": "missed", "reason": "not-whole"}),
            "{pointer}"
        );
        let res = result(&mem, 1).unwrap();
        assert!(
            res.copy.is_none(),
            "a result that is not whole names no copy"
        );
        assert!(
            mem.blob(&s3::recovery_bundle_key(RUN, 1)).is_some(),
            "a check that fails deletes nothing: the object stays in the bucket"
        );
    }
    // The worker says it stored a copy and no object is there.
    let mem = started_with_copy(&s).await;
    mem.recover_answers(&stored(
        &r,
        &s,
        &s,
        "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=",
    ));
    assert_eq!(
        recover(&mem, silence()).await,
        json!({"recovery": "missed", "reason": "not-whole"})
    );
    // The worker stored nothing and says why: the reason is kept when it is a word, and never free text.
    let mem = started_with_copy(&s).await;
    mem.recover_answers(&json!({"attemptId": "<attempt>", "runId": RUN, "session": 1, "recovered": false, "reason": "no-tree", "tookMs": 9, "agentdVersion": "0.1.0"}));
    assert_eq!(
        recover(&mem, silence()).await,
        json!({"recovery": "missed", "reason": "no-tree"})
    );
    let mem = started_with_copy(&s).await;
    mem.recover_answers(&json!({"attemptId": "<attempt>", "runId": RUN, "session": 1, "recovered": false, "reason": "Some long text\nwith a line break", "tookMs": 9, "agentdVersion": "0.1.0"}));
    assert_eq!(
        recover(&mem, silence()).await,
        json!({"recovery": "missed", "reason": "no-answer"})
    );
    // A stored copy whose comparison is unknown, and one without a parent, are still stored copies: the op reports them and decides nothing.
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    let mut answer = stored(&r, &s, &s, &sum);
    answer["comparison"] = json!({"state": "unknown", "reason": "baseline-unreadable"});
    answer["copy"].as_object_mut().unwrap().remove("parentSha");
    mem.recover_answers(&answer);
    assert_eq!(
        recover(&mem, silence()).await,
        json!({"recovery": "copied"})
    );
    let res = result(&mem, 1).unwrap();
    assert_eq!(res.copy.as_ref().unwrap().parent_sha, None);
    assert!(!res.comparison.as_ref().unwrap().done());
}

#[tokio::test]
async fn b854_an_earlier_worker_image_is_one_miss() {
    // No scripted answer: the fake refuses the body with HTTP 400, as a worker image that predates the read does.
    let mem = started_with_copy(&sha('5')).await;
    assert_eq!(
        recover(&mem, silence()).await,
        json!({"recovery": "missed", "reason": "worker-refused"})
    );
    assert_eq!(mem.recover_bodies().len(), 1, "one call, no second try");
    let res = result(&mem, 1).unwrap();
    assert_eq!(res.invoke.as_ref().unwrap().tries, 1);
    assert_eq!(res.invoke.as_ref().unwrap().last_status, Some(400));
    assert!(
        pauses().is_empty(),
        "no pause before or after a refusal: {:?}",
        pauses()
    );

    // HTTP 424 on the first call (seen once after a stop), then an answer: two tries, one pause of 3 s.
    let (s, r) = (sha('5'), sha('6'));
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_fails(RunSessionError::Invalid(
        "Received error (424) from runtime".into(),
    ));
    mem.recover_answers(&stored(&r, &s, &s, &sum));
    assert_eq!(recover(&mem, silence()).await["recovery"], "copied");
    assert_eq!(result(&mem, 1).unwrap().invoke.unwrap().tries, 2);
    assert_eq!(pauses(), [3_000]);

    // HTTP 409 until the op's time is used: the stopped session's machine still answers.
    let mem = started_with_copy(&sha('5')).await;
    for _ in 0..40 {
        mem.recover_fails(RunSessionError::Conflict(
            "Received error (409) from runtime: a turn is already running in this session".into(),
        ));
    }
    let t = mem.now_ms();
    assert_eq!(
        recover(&mem, silence()).await,
        json!({"recovery": "missed", "reason": "session-active"})
    );
    assert!(
        mem.now_ms() - t <= RECOVERY_LIMIT_S * 1000,
        "within the op's limit: {} ms",
        mem.now_ms() - t
    );
    let tries = result(&mem, 1).unwrap().invoke.unwrap().tries;
    assert!(
        (20..=23).contains(&tries),
        "a call every 5 s while more than 10 s are left: {tries}"
    );
}

#[tokio::test]
async fn b854_a_read_that_never_answers_costs_at_most_its_limit_and_the_run_goes_on_from_the_saved_copy()
 {
    let s = sha('5');
    let mem = started_with_copy(&s).await;
    mem.recover_hangs();
    let t = mem.now_ms();
    assert_eq!(
        recover(&mem, silence()).await,
        json!({"recovery": "missed", "reason": "timeout"})
    );
    assert_eq!(
        mem.now_ms() - t,
        RECOVERY_LIMIT_S * 1000,
        "the op ends at its limit by its own clock"
    );
    assert_eq!(mem.recover_bodies().len(), 1);
    // The worker was given what the op had left less the 10 s the op keeps, and at most 75 s.
    assert_eq!(mem.recover_bodies()[0]["budgetS"], RECOVERY_WORKER_BUDGET_S);
    // The run goes on from the saved copy, as it does today: the read cost time and nothing else.
    let next = call(
        &mem,
        json!({"op": "next-slice", "runId": RUN, "staged": staged(), "session": silence(), "loop": {"slice": 1}}),
    )
    .await
    .unwrap();
    assert_eq!(next["continue"], true);
    let plan: Value =
        serde_json::from_slice(&mem.blob(&s3::slice_record_key(RUN, 2)).unwrap()).unwrap();
    assert_eq!(plan["restore"]["kind"], "snapshot");
    assert_eq!(plan["restore"]["key"], s3::snapshot_bundle_key_t(RUN, 1));
    assert_eq!(plan["restore"]["head"], s);
    assert!(plan.get("recovery").is_none());
}

#[tokio::test]
async fn b854_the_settle_is_unconditional_on_a_known_end() {
    let (s, r) = (sha('5'), sha('6'));
    // A known end: the notice came 4 s before the op began. The stop waits until 15 s after the notice, however the op began.
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &s, &sum));
    let known_ms = mem.now_ms();
    let at = dispatcher::items::rfc3339(known_ms / 1000);
    mem.set_now_ms(known_ms + 4_000);
    assert_eq!(recover(&mem, notice(&at)).await["recovery"], "copied");
    let res = result(&mem, 1).unwrap();
    assert_eq!(res.cause, "end-requested");
    assert_eq!(res.end_known_at_ms, Some(known_ms));
    let stop = res.stop.unwrap();
    assert_eq!(
        stop.started_at_ms,
        known_ms + RECOVERY_SETTLE_S * 1000,
        "no stop before the settle ended"
    );
    assert_eq!(pauses(), [(RECOVERY_SETTLE_S - 4) * 1000]);
    assert_eq!(attempt(&mem, 1).unwrap().cause, "end-requested");

    // The settle is already over when the op begins: no wait.
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &s, &sum));
    let known_ms = mem.now_ms();
    let at = dispatcher::items::rfc3339(known_ms / 1000);
    mem.set_now_ms(known_ms + 40_000);
    assert_eq!(recover(&mem, notice(&at)).await["recovery"], "copied");
    assert!(pauses().is_empty(), "{:?}", pauses());

    // Silence: there is no known end and no settle.
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &s, &sum));
    assert_eq!(recover(&mem, silence()).await["recovery"], "copied");
    assert!(pauses().is_empty(), "{:?}", pauses());
    assert_eq!(result(&mem, 1).unwrap().end_known_at_ms, None);

    // A known end whose stop gives no definite answer: stop-failed, and no call, after the settle all the same.
    let mem = started_with_copy(&s).await;
    mem.fail(
        "stop",
        RunSessionError::Unavailable("InternalServerException".into()),
    );
    let known_ms = mem.now_ms();
    let at = dispatcher::items::rfc3339(known_ms / 1000);
    assert_eq!(
        recover(&mem, notice(&at)).await,
        json!({"recovery": "missed", "reason": "stop-failed"})
    );
    assert_eq!(pauses(), [RECOVERY_SETTLE_S * 1000]);
    assert!(mem.recover_bodies().is_empty());
}

#[tokio::test]
async fn b854_the_recovery_credentials_are_tagged_recovery_and_last_900_s() {
    let (s, r) = (sha('5'), sha('6'));
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &s, &sum));
    let session_mints = mem.0.minted.lock().unwrap().len();
    recover(&mem, silence()).await;
    let minted = mem.0.minted.lock().unwrap().clone();
    assert_eq!(minted.len(), session_mints + 1, "one mint for the read");
    let plan = minted.last().unwrap();
    let working = &minted[0];
    assert_eq!(plan.duration_seconds, RECOVERY_CREDENTIALS_S);
    assert_eq!(plan.duration_seconds, 900);
    assert_eq!(plan.role_session_name, format!("{RUN}.recovery"));
    assert_eq!(plan.source_identity, "recovery");
    let tag = |p: &worker_api::creds::MintPlan, k: &str| {
        p.tags
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(tag(plan, "run").as_deref(), Some(RUN));
    assert_eq!(tag(plan, "agent").as_deref(), Some("recovery"));
    // The other tags are the run's own binding.
    for k in ["tenant", "mode", "engine"] {
        assert_eq!(tag(plan, k), tag(working, k), "{k}");
        assert!(tag(plan, k).is_some(), "{k}");
    }
    assert_eq!(tag(working, "agent").as_deref(), Some("direct"));
    // The policy names the read-back key, the baseline, the bundle the run started from
    // and the two data-key actions, and nothing else.
    let policy: Value = serde_json::from_str(&plan.session_policy).unwrap();
    let text = plan.session_policy.as_str();
    assert!(text.contains(&format!(
        "arn:aws:s3:::{BUCKET}/{}",
        s3::recovery_bundle_key(RUN, 1)
    )));
    assert!(text.contains(&format!(
        "arn:aws:s3:::{BUCKET}/{}",
        s3::snapshot_bundle_key_t(RUN, 1)
    )));
    assert!(
        text.contains(&format!(
            "arn:aws:s3:::{BUCKET}/{}",
            s3::repo_bundle_key(RUN)
        )),
        "b860: the read may fetch the bundle the run started from: {text}"
    );
    assert_eq!(text.matches("s3:GetObject").count(), 2, "{text}");
    assert!(text.contains("kms:GenerateDataKey") && text.contains("kms:Decrypt"));
    for banned in [
        "execute-api",
        "bedrock",
        "s3:ListBucket",
        "dynamodb",
        "sts:",
    ] {
        assert!(!text.contains(banned), "{banned} in {text}");
    }
    assert_eq!(policy["Statement"].as_array().unwrap().len(), 4);
    // The credentials reach the worker in the body and nowhere else: no record holds them.
    let body = &mem.recover_bodies()[0];
    assert!(
        body["credentials"]["AccessKeyId"]
            .as_str()
            .unwrap()
            .starts_with("ASIA")
    );
    for key in [
        s3::recovery_attempt_key(RUN, 1),
        s3::recovery_result_key(RUN, 1),
    ] {
        let record = String::from_utf8(mem.blob(&key).unwrap()).unwrap();
        assert!(
            !record.contains("secret") && !record.contains("ASIA"),
            "{key}: {record}"
        );
    }

    // A session that saved no copy: the baseline is the commit it started on, no object is fetched, and the policy has no read at all.
    let mem = seeded(json!({"maxRunS": 8_000}));
    call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
    recover(&mem, silence()).await;
    let body = &mem.recover_bodies()[0];
    assert_eq!(body["baseline"]["headSha"], BASE);
    assert!(body["baseline"].get("object").is_none());
    let minted = mem.0.minted.lock().unwrap().clone();
    assert!(!minted.last().unwrap().session_policy.contains("GetObject"));
    // With no saved copy to fetch, the bundle the run started from is not named either.
    assert!(
        !minted
            .last()
            .unwrap()
            .session_policy
            .contains(&s3::repo_bundle_key(RUN))
    );
}

#[tokio::test]
async fn b854_a_read_that_cannot_start_is_a_miss_with_its_reason() {
    // A store that cannot answer the fence is an error of the op, which the state catches; nothing was stopped or called.
    let mem = started_with_copy(&sha('5')).await;
    mem.fail(
        "fence_agent",
        RunSessionError::Unavailable("ProvisionedThroughputExceededException".into()),
    );
    let err = call(
        &mem,
        json!({"op": "recover", "runId": RUN, "session": silence(), "loop": {"slice": 1}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.error_type(), "RunSessionUnavailable");
    assert!(mem.recover_bodies().is_empty());
    assert!(
        attempt(&mem, 1).is_some(),
        "the attempt stands, so next-slice closes it"
    );
    assert!(result(&mem, 1).is_none());

    // An Agent item with no credential binding: nothing to fence, so no stop and no call.
    let mem = started_with_copy(&sha('5')).await;
    mem.0
        .agents
        .lock()
        .unwrap()
        .get_mut("direct")
        .unwrap()
        .creds = None;
    assert_eq!(
        recover(&mem, silence()).await,
        json!({"recovery": "missed", "reason": "no-session"}),
        "an Agent item without a credential binding cannot be fenced"
    );
    assert!(mem.recover_bodies().is_empty());

    // The plan of the session names no base commit: there is no baseline to compare with, and nothing is called.
    let mem = started_with_copy(&sha('5')).await;
    let key = s3::slice_record_key(RUN, 1);
    let mut plan: Value = serde_json::from_slice(&mem.blob(&key).unwrap()).unwrap();
    plan.as_object_mut().unwrap().remove("baseSha");
    mem.put_blob(&key, serde_json::to_vec(&plan).unwrap());
    assert_eq!(
        recover(&mem, silence()).await,
        json!({"recovery": "missed", "reason": "no-baseline"})
    );
    assert_eq!(mem.recovery_calls(), ["fence", "stop"]);
}

// ---- the machine -------------------------------------------------------------------------------------------------------------------

/// How the scripted worker ends the next session.
#[derive(Clone, Debug)]
enum Ends {
    Done(String),
    Handoff(String),
    /// The heartbeat stops; a copy at this commit was saved when `Some`.
    Silent(Option<String>),
    /// The worker gives notice that the session is being ended: the task ends with `sessionEnded`.
    Notice,
    SetupFailed,
}

struct World {
    mem: Mem,
    script: VecDeque<Ends>,
    builder: tokio::runtime::Handle,
    /// What the function answers to the op `recover` instead of running it.
    recover_error: Option<(String, String)>,
    ops: Vec<String>,
}

impl World {
    fn new(mem: &Mem, script: Vec<Ends>) -> World {
        World {
            mem: mem.clone(),
            script: script.into(),
            builder: tokio::runtime::Handle::current(),
            recover_error: None,
            ops: vec![],
        }
    }
}

fn summary_n(
    n: u32,
    outcome: DirectOutcome,
    head: &str,
) -> harness_protocol::direct::DirectSummary {
    let mut v = serde_json::to_value(summary(outcome, Some("Worked on pct(a, b)."), true)).unwrap();
    v["turn"] = json!(n);
    v["git"]["headSha"] = json!(head);
    v["git"]["bundleUri"] = json!(format!("s3://{BUCKET}/{}", s3::bundle_key_t(RUN, n)));
    serde_json::from_value(v).unwrap()
}

fn turn_complete(n: u32, finish: &str, head: &str, handoff: bool) -> Value {
    let mut v = json!({
        "summary": "Worked on pct(a, b).", "runId": RUN, "agentId": "direct", "turn": n,
        "finishReason": finish, "outcome": "completed",
        "bundleUri": format!("s3://{BUCKET}/{}", s3::bundle_key_t(RUN, n)), "lastCommitSha": head,
    });
    if handoff {
        v["handoff"] = json!(true);
    }
    v
}

impl Lambdas for World {
    fn invoke(&mut self, function: &str, payload: Value) -> Result<Value, (String, String)> {
        match function {
            "run-session" => {
                let name = payload["op"].as_str().unwrap().to_string();
                self.ops.push(name.clone());
                if name == "recover"
                    && let Some(e) = self.recover_error.clone()
                {
                    return Err(e);
                }
                let op: Op = serde_json::from_value(payload)
                    .map_err(|e| ("RunSessionInvalid".to_string(), e.to_string()))?;
                let deps = self.mem.deps();
                let h = self.builder.clone();
                tokio::task::block_in_place(|| h.block_on(handle(&deps, &op)))
                    .map_err(|e| (e.error_type().to_string(), e.to_string()))
            }
            "repo-gateway" => match serde_json::from_value::<GatewayRequest>(payload).unwrap() {
                GatewayRequest::Stage(_) => Ok(staged()),
                GatewayRequest::NewRepo(_) => Ok(json!({"repo": "hamh-e2e-new-20261103"})),
                GatewayRequest::PushPr(_) => Ok(json!({"url": "https://example.invalid/pr/1"})),
            },
            "bundle-validator" => Ok(json!({"pass": true, "violations": []})),
            "packager" => Ok(
                json!({"artifactId": "zip-0123456789abcdef0123", "sha256": "ab".repeat(32), "sizeBytes": 4096, "prUrl": null}),
            ),
            other => panic!("unknown function {other}"),
        }
    }

    fn event(&mut self, function: &str, payload: Value) -> Result<(), (String, String)> {
        assert_eq!(function, "run-session");
        let op: Op = serde_json::from_value(payload).unwrap();
        let deps = self.mem.deps();
        let h = self.builder.clone();
        let _ = tokio::task::block_in_place(|| h.block_on(handle(&deps, &op)));
        Ok(())
    }

    fn wait(&mut self, _function: &str, _token: &str) -> Wait {
        let bodies = self.mem.invocations();
        let body = bodies.last().expect("start-session invoked the runtime");
        let n = u32::try_from(body["turn"].as_u64().unwrap()).unwrap();
        let more_time = body["slice"]["last"] == json!(false);
        let now = self.mem.now_ms();
        match self
            .script
            .pop_front()
            .expect("the script has an end for every session")
        {
            Ends::Done(head) => {
                self.mem.set_now_ms(now + 300_000);
                self.mem.put_bundle_t(n);
                self.mem
                    .put_summary_t(n, &summary_n(n, DirectOutcome::ReportedDone, &head));
                Wait::Success(turn_complete(n, "end_turn", &head, false))
            }
            Ends::Handoff(head) => {
                assert!(
                    more_time,
                    "the script hands session {n} over, and the invocation said it is the last"
                );
                self.mem.set_now_ms(now + 720_000);
                self.mem.put_bundle_t(n);
                self.mem
                    .put_summary_t(n, &summary_n(n, DirectOutcome::StoppedTimeout, &head));
                Wait::Success(turn_complete(n, "stopped_timeout", &head, true))
            }
            Ends::Silent(copy) => {
                self.mem.set_now_ms(now + 900_000);
                if let Some(head) = copy {
                    self.mem.put_snapshot_t(n, &head);
                }
                Wait::HeartbeatTimeout
            }
            Ends::Notice => {
                self.mem.set_now_ms(now + 200_000);
                let at = dispatcher::items::rfc3339(self.mem.now_ms() / 1000);
                Wait::Success(
                    json!({"sessionEnded": {"session": n, "cause": "end-requested", "at": at}}),
                )
            }
            Ends::SetupFailed => {
                self.mem.set_now_ms(now + 60_000);
                let mut s = summary_n(n, DirectOutcome::StoppedSetupFailed, &sha('0'));
                s.git.bundle_uri = None;
                s.git.head_sha = None;
                self.mem.put_summary_t(n, &s);
                let mut v = turn_complete(n, "stopped_setup_failed", &sha('0'), false);
                v.as_object_mut().unwrap().remove("bundleUri");
                v.as_object_mut().unwrap().remove("lastCommitSha");
                Wait::Success(v)
            }
        }
    }
}

fn machine_run() -> Mem {
    let mem = seeded(json!({"maxRunS": 6_000}));
    mem.set_slice_deadline(600);
    mem
}

fn count(trace: &[String], state: &str) -> usize {
    trace.iter().filter(|s| s.as_str() == state).count()
}

/// The states between each `StartSession` and the `NextSlice` that follows it.
fn between(trace: &[String]) -> Vec<Vec<String>> {
    let mut out = vec![];
    let mut cur: Option<Vec<String>> = None;
    for s in trace {
        match s.as_str() {
            "StartSession" => cur = Some(vec![]),
            "NextSlice" => {
                if let Some(c) = cur.take() {
                    out.push(c);
                }
            }
            other => {
                if let Some(c) = cur.as_mut() {
                    c.push(other.to_string());
                }
            }
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_a_session_that_ended_with_a_result_never_reaches_recover() {
    // Trigger first: a session that left no result goes through HasResult to Recover, once, before NextSlice.
    let mem = machine_run();
    let mut w = World::new(
        &mem,
        vec![Ends::Silent(Some(sha('a'))), Ends::Done(sha('b'))],
    );
    let out = walk(&direct_definition(), json!({"runId": RUN}), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(
        between(&out.trace),
        [
            vec!["HasResult".to_string(), "Recover".to_string()],
            vec!["HasResult".to_string()]
        ],
        "{:?}",
        out.trace
    );
    assert_eq!(count(&out.trace, "Recover"), 1);
    assert_eq!(w.ops.iter().filter(|o| *o == "recover").count(), 1);
    assert!(
        attempt(&mem, 1).is_some(),
        "TRIGGER PRESENT: the lost session was read"
    );
    // The worker's notice of an end is no result either.
    let mem = machine_run();
    let mut w = World::new(&mem, vec![Ends::Notice, Ends::Done(sha('b'))]);
    let out = walk(&direct_definition(), json!({"runId": RUN}), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(between(&out.trace)[0], ["HasResult", "Recover"]);
    assert_eq!(attempt(&mem, 1).unwrap().cause, "end-requested");

    // A session that ended with a result passes HasResult and never reaches Recover: done, handoff, a failed setup.
    for script in [
        vec![Ends::Done(sha('a'))],
        vec![Ends::Handoff(sha('a')), Ends::Done(sha('b'))],
        vec![
            Ends::Handoff(sha('a')),
            Ends::SetupFailed,
            Ends::Done(sha('c')),
        ],
    ] {
        let mem = machine_run();
        let sessions = script.len();
        let mut w = World::new(&mem, script);
        let out = walk(&direct_definition(), json!({"runId": RUN}), &mut w);
        assert!(out.failed.is_none(), "{:?}", out.failed);
        assert_eq!(count(&out.trace, "Recover"), 0, "{:?}", out.trace);
        assert_eq!(count(&out.trace, "HasResult"), sessions);
        assert_eq!(count(&out.trace, "NextSlice"), sessions);
        assert!(!w.ops.contains(&"recover".to_string()));
        assert!(
            mem.recovery_calls().iter().all(|c| c == "stop"),
            "no fence, mint or read: {:?}",
            mem.recovery_calls()
        );
        for n in 1..=u32::try_from(sessions).unwrap() {
            assert!(attempt(&mem, n).is_none() && result(&mem, n).is_none());
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_the_recover_state_failing_or_timing_out_reaches_next_slice_and_never_blocked_or_plan()
{
    for error in [
        ("RunSessionUnavailable", "the store did not answer"),
        ("RunSessionInvalid", "an op this function does not know"),
        ("States.Timeout", "the state ran 150 s"),
        ("Lambda.Unknown", "the function ended"),
    ] {
        let mem = machine_run();
        let mut w = World::new(
            &mem,
            vec![Ends::Silent(Some(sha('a'))), Ends::Done(sha('b'))],
        );
        w.recover_error = Some((error.0.to_string(), error.1.to_string()));
        let out = walk(&direct_definition(), json!({"runId": RUN}), &mut w);
        assert!(out.failed.is_none(), "{error:?}: {:?}", out.failed);
        assert!(
            out.caught
                .contains(&("Recover".to_string(), error.0.to_string())),
            "{:?}",
            out.caught
        );
        // No retry of the state, and the state after it is NextSlice: never Plan, never Blocked.
        assert!(
            out.retries.iter().all(|(s, _)| s != "Recover"),
            "{:?}",
            out.retries
        );
        let at = out.trace.iter().position(|s| s == "Recover").unwrap();
        assert_eq!(out.trace[at + 1], "NextSlice", "{error:?}: {:?}", out.trace);
        assert_eq!(count(&out.trace, "Blocked"), 0);
        // The run went on: a second session started from the saved copy and the run was delivered once.
        assert_eq!(count(&out.trace, "StartSession"), 2, "{:?}", out.trace);
        assert_eq!(count(&out.trace, "Plan"), 1);
        assert_eq!(count(&out.trace, "Finalize"), 1);
        let plan: Value =
            serde_json::from_slice(&mem.blob(&s3::slice_record_key(RUN, 2)).unwrap()).unwrap();
        assert_eq!(plan["restore"]["head"], sha('a'));
        // The catch drops the error: the state NextSlice saw holds no trace of it.
        let (_, entered) = out.entered.iter().find(|(s, _)| s == "NextSlice").unwrap();
        assert!(entered.get("Error").is_none() && entered.get("Cause").is_none());
    }
    // The state has no Retry, a timeout above the op's limit and a catch of everything into NextSlice.
    let def = direct_definition();
    let state = &def["States"]["Recover"];
    assert!(state.get("Retry").is_none());
    assert_eq!(state["TimeoutSeconds"], 150);
    assert_eq!(state["ResultPath"], Value::Null);
    assert_eq!(state["Next"], "NextSlice");
    assert_eq!(
        state["Catch"],
        json!([{"ErrorEquals": ["States.ALL"], "ResultPath": null, "Next": "NextSlice"}])
    );
    assert_eq!(state["Parameters"]["Payload"]["op"], "recover");
    assert_eq!(def["States"]["StartSession"]["HeartbeatSeconds"], 600);
    assert_eq!(def["States"]["StartSession"]["TimeoutSeconds"], 28500);
}

// ---- the decision of next-slice --------------------------------------------------------------------------------------------------------
//
// These tests read what the service stored (the plan of the next session, the end record, the event, the Run item) as JSON, so they fail on
// a service that reads the storage back and decides nothing from it, where they compile all the same.

fn record(mem: &Mem, key: &str) -> Option<Value> {
    mem.blob(key).map(|b| serde_json::from_slice(&b).unwrap())
}

fn plan_of(mem: &Mem, n: u32) -> Value {
    record(mem, &s3::slice_record_key(RUN, n)).unwrap_or_else(|| panic!("no plan for session {n}"))
}

fn end_of(mem: &Mem, n: u32) -> Value {
    record(mem, &format!("runs/{RUN}/slices/{n}-end.json"))
        .unwrap_or_else(|| panic!("no end record of session {n}"))
}

/// The payloads of the events that announce a lost session.
fn lost_events(mem: &Mem) -> Vec<Value> {
    mem.events()
        .into_iter()
        .filter(|(t, v)| t == "run.waiting" && v["kind"] == "session-lost")
        .map(|(_, v)| v)
        .collect()
}

async fn next_slice(mem: &Mem, session: Value, loop_state: Value) -> Value {
    call(
        mem,
        json!({"op": "next-slice", "runId": RUN, "staged": staged(), "session": session, "loop": loop_state}),
    )
    .await
    .unwrap()
}

/// Session 1 saved a copy at `s` and was lost; the worker of the read stored a copy at `r` on top of it and found nothing in doubt.
async fn lost_with_a_usable_copy(s: &str, r: &str) -> Mem {
    let mem = started_with_copy(s).await;
    let sum = mem.put_recovery_bundle(1, r);
    mem.recover_answers(&stored(r, s, s, &sum));
    assert_eq!(recover(&mem, silence()).await["recovery"], "copied");
    mem
}

/// **[DEFECT]**: a session is stopped from outside. Its storage holds a file written after the last saved copy. Before this
/// change the next session started from the saved copy and the file was lost, although the storage still held it.
#[tokio::test(flavor = "multi_thread")]
async fn b854_a_stopped_session_restarts_from_the_files_on_its_storage_not_from_the_last_timer_copy()
 {
    let (s, r, done) = (sha('5'), sha('6'), sha('7'));
    let mem = machine_run();
    // What the worker of the read will answer: it stored the tree as the storage held it (commit r, on top of the saved copy s); the
    // storage's marker names s, the comparison found nothing in doubt.
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &s, &sum));
    let mut w = World::new(
        &mem,
        vec![Ends::Silent(Some(s.clone())), Ends::Done(done.clone())],
    );
    let out = walk(&direct_definition(), json!({"runId": RUN}), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(between(&out.trace)[0], ["HasResult", "Recover"]);
    assert_ne!(
        s, r,
        "TRIGGER PRESENT: the storage holds another tree than the last saved copy"
    );
    assert_eq!(
        result(&mem, 1).unwrap().state,
        "copied",
        "TRIGGER PRESENT: the read stored a copy"
    );

    // The plan of session 2: `restore` is the saved copy, as it always was, and `recovery` names the copy read back.
    let plan = plan_of(&mem, 2);
    assert_eq!(plan["restore"]["kind"], "snapshot");
    assert_eq!(plan["restore"]["key"], s3::snapshot_bundle_key_t(RUN, 1));
    assert_eq!(plan["restore"]["head"], s);
    assert_eq!(
        plan["recovery"],
        json!({"key": s3::recovery_bundle_key(RUN, 1), "head": r}),
        "session 2 is planned to start from the files read back"
    );
    // Session 2 was invoked with that copy: the repository, the commit the worker must find, and the Turn item's resume head.
    let second = &mem.invocations()[1];
    assert_eq!(second["turn"], 2);
    assert_eq!(
        second["repo"]["uri"],
        format!("s3://{BUCKET}/{}", s3::recovery_bundle_key(RUN, 1))
    );
    assert_eq!(second["slice"]["restoreHead"], r);
    assert_eq!(
        mem.turn_n(2).unwrap().resume_head.as_deref(),
        Some(r.as_str())
    );
    // The decision is in the end record, and the run says which copy session 2 started from.
    let end = end_of(&mem, 1);
    assert_eq!(end["recovery"]["used"], true);
    assert_eq!(end["recovery"]["tried"], true);
    assert_eq!(end["recovery"]["head"], r);
    let events = lost_events(&mem);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["from"], "recovered");
    assert_eq!(events[0]["nextSession"], 2);
    // The run went on and was delivered once, from what session 2 left.
    assert_eq!(count(&out.trace, "StartSession"), 2);
    assert_eq!(count(&out.trace, "Finalize"), 1);
    assert_eq!(mem.run().status, GoalStatus::Complete);
}

#[tokio::test]
async fn b854_the_decision_waits_for_the_stopped_machine_in_every_branch() {
    let (s, r) = (sha('5'), sha('6'));
    // A read that looks usable, after silence: the service's own stop began at t; nothing is decided before t + 75 s.
    let mem = lost_with_a_usable_copy(&s, &r).await;
    let stop_at = result(&mem, 1).unwrap().stop.unwrap().started_at_ms;
    mem.set_now_ms(stop_at + 10_000);
    clear_pauses();
    assert_eq!(
        next_slice(&mem, silence(), json!({"slice": 1})).await["continue"],
        true
    );
    assert_eq!(pauses(), [65_000], "the usable branch waits too");
    assert_eq!(end_of(&mem, 1)["decidedAtMs"], stop_at + 75_000);
    assert_eq!(end_of(&mem, 1)["recovery"]["used"], true);

    // A read that missed, after silence: the same wait.
    let mem = started_with_copy(&s).await;
    assert_eq!(recover(&mem, silence()).await["reason"], "worker-refused");
    let stop_at = result(&mem, 1).unwrap().stop.unwrap().started_at_ms;
    mem.set_now_ms(stop_at + 10_000);
    clear_pauses();
    next_slice(&mem, silence(), json!({"slice": 1})).await;
    assert_eq!(pauses(), [65_000]);
    assert_eq!(end_of(&mem, 1)["decidedAtMs"], stop_at + 75_000);
    assert_eq!(end_of(&mem, 1)["recovery"]["used"], false);

    // An end the worker gave notice of: the 75 s count from the notice, which came before the service's own stop.
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &s, &sum));
    let known_ms = mem.now_ms();
    let at = dispatcher::items::rfc3339(known_ms / 1000);
    assert_eq!(recover(&mem, notice(&at)).await["recovery"], "copied");
    clear_pauses();
    next_slice(&mem, notice(&at), json!({"slice": 1})).await;
    assert_eq!(end_of(&mem, 1)["decidedAtMs"], known_ms + 75_000);
    assert_eq!(pauses(), [60_000], "the settle of 15 s is part of the 75 s");

    // The 75 s have passed when next-slice begins: it does not wait.
    let mem = lost_with_a_usable_copy(&s, &r).await;
    mem.set_now_ms(mem.now_ms() + 100_000);
    clear_pauses();
    next_slice(&mem, silence(), json!({"slice": 1})).await;
    assert!(pauses().is_empty(), "{:?}", pauses());

    // No wait where there is nothing to wait for: a run that switched the read off, and a session that ended with a result.
    let mem = seeded(json!({"maxRunS": 8_000, "recoverAfterStop": false}));
    call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
    mem.put_snapshot_t(1, &s);
    assert_eq!(recover(&mem, silence()).await["reason"], "off");
    clear_pauses();
    next_slice(&mem, silence(), json!({"slice": 1})).await;
    assert!(
        pauses().is_empty(),
        "off: the decision is made as it always was: {:?}",
        pauses()
    );
    assert_eq!(end_of(&mem, 1)["recovery"]["reason"], "off");
    assert_eq!(end_of(&mem, 1)["recovery"]["tried"], false);

    // A machine definition from before the read (no Recover state ran): a lost session of a run that reads back still gets the wait.
    let mem = started_with_copy(&s).await;
    clear_pauses();
    let t = mem.now_ms();
    next_slice(&mem, silence(), json!({"slice": 1})).await;
    assert_eq!(pauses(), [75_000]);
    assert_eq!(end_of(&mem, 1)["decidedAtMs"], t + 75_000);
    assert!(
        end_of(&mem, 1).get("recovery").is_none(),
        "no read was made, so no decision about one is recorded"
    );
}

#[tokio::test]
async fn b854_a_copy_that_lands_while_the_storage_is_read_wins() {
    let (s, r, later) = (sha('5'), sha('6'), sha('8'));
    // The read looked usable against the saved copy s. Then the stopped machine's last upload lands: a newer saved copy.
    let mem = lost_with_a_usable_copy(&s, &r).await;
    mem.put_snapshot_t(1, &later);
    assert_eq!(
        next_slice(&mem, silence(), json!({"slice": 1})).await["continue"],
        true
    );
    let end = end_of(&mem, 1);
    assert_eq!(end["recovery"]["used"], false);
    assert_eq!(end["recovery"]["reason"], "superseded");
    let plan = plan_of(&mem, 2);
    assert_eq!(
        plan["restore"]["head"], later,
        "the run goes on from the newer saved copy"
    );
    assert!(plan.get("recovery").is_none());
    // The copy read back is kept, and the decision names it.
    assert!(mem.blob(&s3::recovery_bundle_key(RUN, 1)).is_some());
    assert_eq!(end["recovery"]["key"], s3::recovery_bundle_key(RUN, 1));
    assert_eq!(lost_events(&mem)[0]["from"], "copy");
    assert!(
        serde_json::to_value(mem.run())
            .unwrap()
            .get("recoveredCopy")
            .is_none()
    );

    // The same when the read was not usable to begin with (the marker named another copy): the newer saved copy wins, and it is the
    // move of the baseline that is recorded.
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &sha('4'), &sum));
    recover(&mem, silence()).await;
    mem.put_snapshot_t(1, &later);
    next_slice(&mem, silence(), json!({"slice": 1})).await;
    assert_eq!(end_of(&mem, 1)["recovery"]["reason"], "superseded");
    assert_eq!(plan_of(&mem, 2)["restore"]["head"], later);
    // Trigger: without the late copy the first case is used.
    let mem = lost_with_a_usable_copy(&s, &r).await;
    next_slice(&mem, silence(), json!({"slice": 1})).await;
    assert_eq!(end_of(&mem, 1)["recovery"]["used"], true, "TRIGGER PRESENT");
}

#[tokio::test]
async fn b854_next_slice_closes_an_attempt_without_a_result_and_a_late_result_is_not_used() {
    let (s, r) = (sha('5'), sha('6'));
    // The op dies after its attempt record (the store fails at the fence): an attempt with no result.
    let mem = started_with_copy(&s).await;
    mem.fail(
        "fence_agent",
        RunSessionError::Unavailable("InternalServerError".into()),
    );
    call(
        &mem,
        json!({"op": "recover", "runId": RUN, "session": silence(), "loop": {"slice": 1}}),
    )
    .await
    .unwrap_err();
    assert!(attempt(&mem, 1).is_some() && result(&mem, 1).is_none());
    // Meanwhile a worker stored a copy all the same.
    let sum = mem.put_recovery_bundle(1, &r);

    assert_eq!(
        next_slice(&mem, silence(), json!({"slice": 1})).await["continue"],
        true
    );
    assert_eq!(
        result(&mem, 1).unwrap().state,
        "abandoned",
        "next-slice closed the attempt"
    );
    let end = end_of(&mem, 1);
    assert_eq!(end["recovery"]["used"], false);
    assert_eq!(end["recovery"]["reason"], "abandoned");
    assert_eq!(end["recovery"]["tried"], true);
    assert_eq!(
        end["recovery"]["key"],
        s3::recovery_bundle_key(RUN, 1),
        "the copy that was stored before the limit is named, so it can be downloaded"
    );
    assert_eq!(plan_of(&mem, 2)["restore"]["head"], s);
    assert!(plan_of(&mem, 2).get("recovery").is_none());

    // A result that arrives late cannot replace the abandoned one, and asking again changes nothing.
    let late = serde_json::to_vec(&json!({"state": "copied"})).unwrap();
    let written = {
        use run_session::ports::Blobs;
        mem.put_if_absent(&s3::recovery_result_key(RUN, 1), &late)
            .await
            .unwrap()
    };
    assert!(!written, "the result is written once");
    assert_eq!(
        call(
            &mem,
            json!({"op": "recover", "runId": RUN, "session": silence(), "loop": {"slice": 1}})
        )
        .await
        .unwrap(),
        json!({"recovery": "duplicate"})
    );
    let again = next_slice(&mem, silence(), json!({"slice": 1})).await;
    assert_eq!(again["continue"], true);
    assert_eq!(end_of(&mem, 1), end, "the decision is made once");
    assert!(mem.blob(&s3::recovery_bundle_key(RUN, 1)).is_some());
    let _ = sum;
}

/// The state `Recover` can end on its Catch while the function has not written its first record yet (an error of the invoke, a slow
/// start). `next-slice` then finds neither an attempt nor a result and waits for the stopped machine. What the read wrote during
/// that wait is looked at once more: an attempt without a result is closed, so a late result is never used, and a copy the read stored
/// is named with the decision.
#[tokio::test]
async fn b854_an_attempt_written_while_next_slice_waits_is_closed_and_its_copy_is_named() {
    let (s, r) = (sha('5'), sha('6'));
    // The attempt record as the op writes it: taken from a world whose read died right after it.
    let other = started_with_copy(&s).await;
    other.fail(
        "fence_agent",
        RunSessionError::Unavailable("InternalServerError".into()),
    );
    call(
        &other,
        json!({"op": "recover", "runId": RUN, "session": silence(), "loop": {"slice": 1}}),
    )
    .await
    .unwrap_err();
    let begun = other
        .blob(&s3::recovery_attempt_key(RUN, 1))
        .expect("the attempt record of a read");

    let mem = started_with_copy(&s).await;
    assert!(
        attempt(&mem, 1).is_none() && result(&mem, 1).is_none(),
        "TRIGGER PRESENT: no read is on record when next-slice begins"
    );
    // While next-slice waits, the function writes its attempt and stores the copy. It has written no result.
    let world = mem.clone();
    let copy = r.clone();
    during_the_next_pause(move || {
        world.put_blob(&s3::recovery_attempt_key(RUN, 1), begun);
        world.put_recovery_bundle(1, &copy);
    });
    clear_pauses();
    let out = next_slice(&mem, silence(), json!({"slice": 1})).await;
    assert!(
        !pauses().is_empty(),
        "TRIGGER PRESENT: next-slice waited for the stopped machine"
    );
    assert!(
        attempt(&mem, 1).is_some(),
        "TRIGGER PRESENT: the attempt was written during the wait"
    );
    assert_eq!(out["continue"], true, "{out}");
    assert_eq!(
        result(&mem, 1).map(|r| r.state),
        Some("abandoned".to_string()),
        "next-slice closed the attempt it found after the wait"
    );
    let end = end_of(&mem, 1);
    assert_eq!(end["recovery"]["tried"], true, "{end}");
    assert_eq!(end["recovery"]["used"], false, "{end}");
    assert_eq!(end["recovery"]["reason"], "abandoned", "{end}");
    assert_eq!(
        end["recovery"]["key"],
        s3::recovery_bundle_key(RUN, 1),
        "the copy the read stored is named, so it can be downloaded"
    );
    assert_eq!(plan_of(&mem, 2)["restore"]["head"], s);
    assert!(plan_of(&mem, 2).get("recovery").is_none());
    // The service's word says that files read back are kept: the client names the command that downloads them.
    assert_eq!(lost_events(&mem)[0]["recovery"]["kept"], true);

    // A result that arrives late cannot replace the abandoned one.
    let late = serde_json::to_vec(&json!({"state": "copied"})).unwrap();
    let written = {
        use run_session::ports::Blobs;
        mem.put_if_absent(&s3::recovery_result_key(RUN, 1), &late)
            .await
            .unwrap()
    };
    assert!(!written, "the result is written once");
}

#[tokio::test]
async fn b854_a_known_end_is_a_lost_session_with_the_cause_end_requested() {
    let s = sha('5');
    let mem = started_with_copy(&s).await;
    let at = dispatcher::items::rfc3339(mem.now_ms() / 1000);
    recover(&mem, notice(&at)).await;
    let out = next_slice(&mem, notice(&at), json!({"slice": 1})).await;
    // A notice is no result: the session counts as lost, and the run goes on.
    assert_eq!(
        (out["continue"].clone(), out["lost"].clone()),
        (json!(true), json!(1))
    );
    let end = end_of(&mem, 1);
    assert_eq!(end["kind"], "lost");
    assert_eq!(
        end["lostCause"], "end-requested",
        "the cause is the notice, which is no proof of a stop"
    );
    assert_eq!(end["endedAt"], at);
    let events = lost_events(&mem);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["cause"], "end-requested");
    assert_eq!(events[0]["endedAt"], at);
    assert_eq!(events[0]["then"], "restart");
    // Trigger: silence is still told as silence.
    let mem = started_with_copy(&s).await;
    recover(&mem, silence()).await;
    next_slice(&mem, silence(), json!({"slice": 1})).await;
    assert_eq!(end_of(&mem, 1)["lostCause"], "no-heartbeat");
    assert!(end_of(&mem, 1).get("endedAt").is_none());
}

#[tokio::test]
async fn b854_the_event_says_which_copy_the_next_session_started_from() {
    let (s, r) = (sha('5'), sha('6'));
    // Used.
    let mem = lost_with_a_usable_copy(&s, &r).await;
    next_slice(&mem, silence(), json!({"slice": 1})).await;
    let e = &lost_events(&mem)[0];
    assert_eq!(e["from"], "recovered");
    assert_eq!(
        e["recovery"],
        json!({"tried": true, "used": true, "kept": true})
    );
    assert_eq!(
        e["readBackSession"], 1,
        "the event names the session whose storage the files were read back from"
    );
    assert!(e.get("savedAt").is_none());
    // Asking again writes the same event and no second one.
    next_slice(&mem, silence(), json!({"slice": 1})).await;
    assert_eq!(lost_events(&mem).len(), 1);
    assert_eq!(&lost_events(&mem)[0], e);

    // Read and not used: the marker named another copy. The run goes on from the saved copy and says why.
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &sha('4'), &sum));
    recover(&mem, silence()).await;
    next_slice(&mem, silence(), json!({"slice": 1})).await;
    let e = &lost_events(&mem)[0];
    assert_eq!(e["from"], "copy");
    assert_eq!(
        e["recovery"],
        json!({"tried": true, "used": false, "reason": "storage-older", "kept": true}),
        "the files read back are kept, and the event says so: the client names the command that downloads them"
    );
    assert!(
        e.get("readBackSession").is_none(),
        "the run goes on from the saved copy"
    );
    assert!(mem.blob(&s3::recovery_bundle_key(RUN, 1)).is_some());
    assert_eq!(plan_of(&mem, 2)["restore"]["head"], s);

    // Read and not used: a file of the saved copy holds more than the storage did.
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    let mut answer = stored(&r, &s, &s, &sum);
    answer["comparison"] = json!({"state": "done", "sameAsBaseline": false, "regressed": 2, "regressedPaths": ["notes/log.txt", "b.txt"], "rewritten": 0});
    mem.recover_answers(&answer);
    recover(&mem, silence()).await;
    next_slice(&mem, silence(), json!({"slice": 1})).await;
    assert_eq!(
        lost_events(&mem)[0]["recovery"],
        json!({"tried": true, "used": false, "reason": "older-file", "regressed": 2, "regressedPath": "notes/log.txt", "kept": true})
    );

    // The read missed.
    let mem = started_with_copy(&s).await;
    recover(&mem, silence()).await;
    next_slice(&mem, silence(), json!({"slice": 1})).await;
    let e = &lost_events(&mem)[0];
    assert_eq!(e["from"], "copy");
    assert_eq!(
        e["recovery"],
        json!({"tried": true, "used": false, "reason": "worker-refused"}),
        "nothing was stored, so nothing is said to be kept"
    );

    // No read was made: the run switched it off.
    let mem = seeded(json!({"maxRunS": 8_000, "recoverAfterStop": false}));
    call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
    mem.put_snapshot_t(1, &s);
    recover(&mem, silence()).await;
    next_slice(&mem, silence(), json!({"slice": 1})).await;
    let e = &lost_events(&mem)[0];
    assert_eq!(e["from"], "copy");
    assert_eq!(
        e["recovery"],
        json!({"tried": false, "used": false, "reason": "off"})
    );
}

#[tokio::test]
async fn b854_the_run_item_names_the_recovery_copy_only_when_it_is_used() {
    let (s, r) = (sha('5'), sha('6'));
    let mem = lost_with_a_usable_copy(&s, &r).await;
    next_slice(&mem, silence(), json!({"slice": 1})).await;
    let run = serde_json::to_value(mem.run()).unwrap();
    assert_eq!(run["recoveredCopy"]["session"], 1);
    assert_eq!(run["recoveredCopy"]["key"], s3::recovery_bundle_key(RUN, 1));
    assert_eq!(run["recoveredCopy"]["headSha"], r);
    assert_eq!(run["recoveredCopy"]["bundleBytes"], 60);
    // A copy that was read back and not used is not named on the Run item: it is found by its session's key.
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &sha('4'), &sum));
    recover(&mem, silence()).await;
    next_slice(&mem, silence(), json!({"slice": 1})).await;
    assert!(
        serde_json::to_value(mem.run())
            .unwrap()
            .get("recoveredCopy")
            .is_none()
    );
    assert!(mem.blob(&s3::recovery_bundle_key(RUN, 1)).is_some());
}

/// Session 1 was lost and its read-back copy `r` is used: session 2 is planned to start from it. Returns the world with session 2 started
/// when `start` is set.
async fn second_session_from_the_files_read_back(s: &str, r: &str, start: bool) -> Mem {
    let mem = lost_with_a_usable_copy(s, r).await;
    let out = next_slice(&mem, silence(), json!({"slice": 1})).await;
    assert_eq!(
        (out["continue"].clone(), out["lost"].clone()),
        (json!(true), json!(1))
    );
    assert_eq!(
        plan_of(&mem, 2)["recovery"]["head"],
        r,
        "TRIGGER PRESENT: session 2 is planned from the files read back"
    );
    if start {
        call(&mem, json!({"op": "start-session", "runId": RUN, "taskToken": "t2", "staged": staged(), "slice": 2}))
            .await
            .unwrap();
    }
    mem.clear_events();
    mem
}

#[tokio::test]
async fn b854_a_session_that_cannot_restore_the_files_read_back_is_followed_by_one_from_the_saved_copy()
 {
    let (s, r) = (sha('5'), sha('6'));
    let loop2 = json!({"slice": 2, "lost": 1, "empty": 0, "continue": true});
    let from_the_saved_copy = |mem: &Mem| {
        let plan = plan_of(mem, 3);
        assert_eq!(plan["restore"]["kind"], "snapshot");
        assert_eq!(
            plan["restore"]["head"], s,
            "session 3 starts from the saved copy"
        );
        assert!(
            plan.get("recovery").is_none(),
            "the copy that could not be started from is not tried again"
        );
        assert!(
            mem.blob(&s3::recovery_bundle_key(RUN, 1)).is_some(),
            "and it stays in the bucket"
        );
    };

    // The worker of session 2 could not restore the copy: it answers with a failed setup.
    let mem = second_session_from_the_files_read_back(&s, &r, true).await;
    let mut failed = summary_n(2, DirectOutcome::StoppedSetupFailed, &sha('0'));
    failed.git.bundle_uri = None;
    failed.git.head_sha = None;
    mem.put_summary_t(2, &failed);
    let mut turn = turn_complete(2, "stopped_setup_failed", &sha('0'), false);
    turn.as_object_mut().unwrap().remove("bundleUri");
    turn.as_object_mut().unwrap().remove("lastCommitSha");
    let out = next_slice(&mem, json!({"turn": turn, "failure": {}}), loop2.clone()).await;
    assert_eq!(out["continue"], true);
    assert_eq!(
        out["lost"], 1,
        "the failed start is not counted as a lost session"
    );
    from_the_saved_copy(&mem);
    assert_eq!(end_of(&mem, 2)["lostCause"], "restore-failed");
    let e = &lost_events(&mem)[0];
    assert_eq!(
        (e["session"].clone(), e["cause"].clone(), e["from"].clone()),
        (json!(2), json!("restore-failed"), json!("copy"))
    );

    // The start of session 2 was refused before any machine was called: no Turn was invoked.
    let mem = second_session_from_the_files_read_back(&s, &r, false).await;
    let refused = json!({"turn": {}, "failure": {"Error": "RunSessionInvalid", "Cause": "invocation: repo must be an S3Bundle"}});
    let out = call(
        &mem,
        json!({"op": "recover", "runId": RUN, "session": refused, "loop": loop2}),
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        json!({"recovery": "not-needed"}),
        "a session that was never invoked has no storage to read"
    );
    let out = next_slice(&mem, refused.clone(), loop2.clone()).await;
    assert_eq!(
        (out["continue"].clone(), out["lost"].clone()),
        (json!(true), json!(1))
    );
    from_the_saved_copy(&mem);

    // Session 2 was invoked and lost before it began: nothing of its own was saved, and the read of its storage found no marker that
    // names session 2 and the restored commit. The storage was read, so this is proof: the restore check of session 2 never passed.
    let mem = second_session_from_the_files_read_back(&s, &r, true).await;
    mem.recover_answers(&read_without_a_start_marker(&mem));
    call(
        &mem,
        json!({"op": "recover", "runId": RUN, "session": silence(), "loop": loop2}),
    )
    .await
    .unwrap();
    assert_eq!(
        result(&mem, 2).unwrap().state,
        "copied",
        "TRIGGER PRESENT: the storage of session 2 was read"
    );
    let out = next_slice(&mem, silence(), loop2.clone()).await;
    assert_eq!(
        (out["continue"].clone(), out["lost"].clone()),
        (json!(true), json!(1))
    );
    from_the_saved_copy(&mem);
    assert_eq!(end_of(&mem, 2)["lostCause"], "restore-failed");

    // At lost session two of three, the failed start does not end the run.
    let mem = second_session_from_the_files_read_back(&s, &r, true).await;
    mem.recover_answers(&read_without_a_start_marker(&mem));
    call(&mem, json!({"op": "recover", "runId": RUN, "session": silence(), "loop": {"slice": 2, "lost": 2}})).await.unwrap();
    let out = next_slice(
        &mem,
        silence(),
        json!({"slice": 2, "lost": 2, "empty": 0, "continue": true}),
    )
    .await;
    assert_eq!(
        (out["continue"].clone(), out["lost"].clone()),
        (json!(true), json!(2)),
        "a third lost session would have ended the run"
    );
    from_the_saved_copy(&mem);

    // The converse: session 2 restored the copy and went to work (the marker on its storage names session 2 and the restored commit),
    // then was lost without a copy of its own. It counts as a lost session, and the work is still only in the copy it started from:
    // session 3 starts from that copy again.
    let mem = second_session_from_the_files_read_back(&s, &r, true).await;
    let again = sha('9');
    let sum = mem.put_recovery_bundle(2, &again);
    let mut answer = stored(&again, &r, &r, &sum);
    answer["session"] = json!(2);
    answer["copy"]["marker"] = json!({"schemaVersion": 1, "session": 2, "count": 5, "headSha": r});
    answer["comparison"] =
        json!({"state": "done", "sameAsBaseline": true, "regressed": 0, "rewritten": 0});
    mem.recover_answers(&answer);
    call(
        &mem,
        json!({"op": "recover", "runId": RUN, "session": silence(), "loop": loop2}),
    )
    .await
    .unwrap();
    assert_eq!(result(&mem, 2).unwrap().state, "copied");
    let out = next_slice(&mem, silence(), loop2.clone()).await;
    assert_eq!(
        (out["continue"].clone(), out["lost"].clone()),
        (json!(true), json!(2)),
        "a session that worked and was lost is counted"
    );
    let plan = plan_of(&mem, 3);
    assert_eq!(plan["restore"]["head"], s);
    assert_eq!(
        plan["recovery"],
        json!({"key": s3::recovery_bundle_key(RUN, 1), "head": r}),
        "the copy session 2 started from is carried forward"
    );
    assert_eq!(end_of(&mem, 2)["recovery"]["reason"], "nothing-newer");
    assert_eq!(end_of(&mem, 2)["lostCause"], "no-heartbeat");
}

/// What the worker of a read of session 2's storage answers when that storage holds no marker of session 2: it stored the tree it
/// found (commit `8…`, without a marker), and the comparison found nothing in doubt. Session 2's restore check never passed.
fn read_without_a_start_marker(mem: &Mem) -> Value {
    let found = sha('8');
    let sum = mem.put_recovery_bundle(2, &found);
    let mut answer = stored(&found, &sha('6'), &sha('6'), &sum);
    answer["session"] = json!(2);
    answer["copy"].as_object_mut().unwrap().remove("marker");
    answer
}

/// A session that started from a read-back copy and was lost before a copy of its own, while the read of its own storage came
/// to nothing (it missed, it was abandoned, or none was made). Whether the session worked is then not known. It is never worded as a
/// start that failed: it is a lost session like any other, and the run goes on from the copy that session started from, which is the
/// newest copy the service found whole. Before this rule the run fell back to the older saved copy and dropped the files read back.
#[tokio::test]
async fn b854_a_read_that_came_to_nothing_leaves_the_start_unknown_and_the_run_goes_on_from_the_copy_the_session_started_from()
 {
    let (s, r) = (sha('5'), sha('6'));
    let loop2 = json!({"slice": 2, "lost": 1, "empty": 0, "continue": true});
    let carried = json!({"key": s3::recovery_bundle_key(RUN, 1), "head": r});
    let goes_on_from_the_files_read_back = |mem: &Mem, out: &Value| {
        assert_eq!(
            (out["continue"].clone(), out["lost"].clone()),
            (json!(true), json!(2)),
            "a session whose start is not known is a lost session like any other"
        );
        let plan = plan_of(mem, 3);
        assert_eq!(
            plan["recovery"], carried,
            "session 3 starts from the copy session 2 started from"
        );
        assert_eq!(plan["restore"]["head"], s, "`restore` stays the saved copy");
        let end = end_of(mem, 2);
        assert_eq!(
            end["lostCause"], "no-heartbeat",
            "the cause is the silence, not a start that failed"
        );
        let e = &lost_events(mem)[0];
        assert_eq!(
            (e["session"].clone(), e["cause"].clone(), e["from"].clone()),
            (json!(2), json!("no-heartbeat"), json!("recovered"))
        );
        assert_eq!(
            e["readBackSession"], 1,
            "the event names the session whose storage the files came from: session 1, not the lost session 2"
        );
    };

    // The read of session 2's storage missed (here: the worker refused the call).
    let mem = second_session_from_the_files_read_back(&s, &r, true).await;
    call(
        &mem,
        json!({"op": "recover", "runId": RUN, "session": silence(), "loop": loop2}),
    )
    .await
    .unwrap();
    assert_eq!(
        result(&mem, 2).unwrap().state,
        "missed",
        "TRIGGER PRESENT: the read of session 2's storage missed"
    );
    let out = next_slice(&mem, silence(), loop2.clone()).await;
    goes_on_from_the_files_read_back(&mem, &out);

    // The read began and left no result: next-slice closes it as abandoned.
    let mem = second_session_from_the_files_read_back(&s, &r, true).await;
    mem.fail(
        "fence_agent",
        RunSessionError::Unavailable("InternalServerError".into()),
    );
    call(
        &mem,
        json!({"op": "recover", "runId": RUN, "session": silence(), "loop": loop2}),
    )
    .await
    .unwrap_err();
    assert!(attempt(&mem, 2).is_some() && result(&mem, 2).is_none());
    let out = next_slice(&mem, silence(), loop2.clone()).await;
    assert_eq!(result(&mem, 2).unwrap().state, "abandoned");
    goes_on_from_the_files_read_back(&mem, &out);

    // No read was made at all (the state of the read never ran).
    let mem = second_session_from_the_files_read_back(&s, &r, true).await;
    let out = next_slice(&mem, silence(), loop2.clone()).await;
    assert!(attempt(&mem, 2).is_none() && result(&mem, 2).is_none());
    goes_on_from_the_files_read_back(&mem, &out);

    // As the third lost session in a row the run ends, and it delivers the copy session 2 started from, not the older saved copy.
    let ending = json!({"slice": 2, "lost": 2, "empty": 0, "continue": true});
    let mem = second_session_from_the_files_read_back(&s, &r, true).await;
    call(
        &mem,
        json!({"op": "recover", "runId": RUN, "session": silence(), "loop": ending}),
    )
    .await
    .unwrap();
    let out = next_slice(&mem, silence(), ending.clone()).await;
    assert_eq!(
        (out["continue"].clone(), out["lost"].clone()),
        (json!(false), json!(3)),
        "{out}"
    );
    assert_eq!(end_of(&mem, 2)["lostCause"], "no-heartbeat");
    let p = plan_delivery(&mem, silence(), out).await;
    assert_eq!(p["deliver"], true, "{p}");
    assert_eq!(
        p["headSha"], r,
        "the files read back, not the older saved copy"
    );
    assert_eq!(evidence_of(&mem)["sourceKey"], carried["key"]);
    assert!(
        p["detail"].as_str().unwrap().ends_with(READ_BACK_SENTENCE),
        "{p}"
    );
    let e = &lost_events(&mem)[0];
    assert_eq!(
        (
            e["then"].clone(),
            e["delivers"].clone(),
            e["readBackSession"].clone()
        ),
        (json!("end"), json!("copy"), json!(1)),
        "the event of the end names the files the run delivers"
    );
}

/// With, the ruling on a read after a cancel: the storage of a stopped session IS read after the user's cancel, no
/// further engine session starts, and a discard wins over the read.
///
/// This test holds this lane's half. Nothing in the read looks at a cancel: a run whose user asked to stop and keep the work is still
/// an active run, so the read makes its one bounded call of the old session id. That call is a body of kind `recover`, which starts
/// no engine, so the read itself starts no session. Only a run that is no longer active gets no read, and that is what a discard
/// leaves: before the read nothing is called, and between the stop and the call the old session id is not called.
///
/// The other half is the cancel lane's: the user's cancel as a member of the Run item, `next-slice` that delivers for it and
/// `start-session` that refuses. This branch has no such member, so "no session follows a cancel" cannot be stated here. At the join
/// this test sets that member before the read and during it and asserts that no plan of the next session is written.
#[tokio::test]
async fn b854_a_cancelled_run_is_still_read_back_and_starts_no_session() {
    let (s, r) = (sha('5'), sha('6'));
    // The user's cancel that keeps the work leaves the run active. The session is gone, and the read is made.
    let mem = started_with_copy(&s).await;
    let old_session = mem.agent().unwrap().runtime_session_id.clone();
    let turns_before = mem.invocations().len();
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &s, &sum));
    assert_eq!(mem.run().status, GoalStatus::Active);
    assert_eq!(recover(&mem, silence()).await["recovery"], "copied");
    assert_eq!(
        mem.recover_bodies().len(),
        1,
        "TRIGGER PRESENT: one bounded call was made"
    );
    assert_eq!(mem.recover_bodies()[0]["kind"], "recover");
    assert_eq!(
        mem.recover_targets()[0].2,
        old_session,
        "the call goes to the stopped session's own id"
    );
    assert_eq!(
        mem.invocations().len(),
        turns_before,
        "the read starts no engine session: no turn was invoked"
    );
    assert!(mem.blob(&s3::recovery_bundle_key(RUN, 1)).is_some());

    // A discard wins over the read. Before the read: nothing is fenced, stopped or called.
    let mem = started_with_copy(&s).await;
    mem.0.runs.lock().unwrap().get_mut(RUN).unwrap().status = GoalStatus::Cleared;
    assert_eq!(
        recover(&mem, silence()).await,
        json!({"recovery": "skipped", "reason": "run-ended"})
    );
    assert!(mem.recovery_calls().is_empty());
    // During the read (after the stop, before the call): the old session id is not called, and no session follows.
    let mem = started_with_copy(&s).await;
    *mem.0.flip_run_after_reads.lock().unwrap() = Some((1, GoalStatus::Cleared));
    assert_eq!(
        recover(&mem, silence()).await,
        json!({"recovery": "skipped", "reason": "run-ended"})
    );
    assert!(mem.recover_bodies().is_empty());
    let out = next_slice(&mem, silence(), json!({"slice": 1})).await;
    assert_eq!(
        out["continue"],
        json!(false),
        "a run that was discarded starts no further session: {out}"
    );
    assert!(record(&mem, &s3::slice_record_key(RUN, 2)).is_none());
}

// ---- the delivery -----------------------------------------------------------------------------------------------------------------

const READ_BACK_SENTENCE: &str =
    "The work is what the service read back from the session's storage after the session ended.";

async fn plan_delivery(mem: &Mem, session: Value, loop_state: Value) -> Value {
    call(
        mem,
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged(), "session": session, "loop": loop_state}),
    )
    .await
    .unwrap()
}

/// The evidence file of the run, which the delivery steps and the card read.
fn evidence_of(mem: &Mem) -> Value {
    record(mem, &s3::evidence_key(RUN)).expect("evidence/direct.json")
}

/// Session 1 was lost with a usable read-back copy `r`, and it is the third lost session in a row: the run ends on it. Returns the
/// world and the loop state the machine goes on with.
async fn ended_on_the_lost_session(s: &str, r: &str) -> (Mem, Value) {
    let mem = lost_with_a_usable_copy(s, r).await;
    let out = next_slice(
        &mem,
        silence(),
        json!({"slice": 1, "lost": 2, "empty": 0, "continue": true}),
    )
    .await;
    assert_eq!(
        out["continue"],
        json!(false),
        "TRIGGER PRESENT: the third lost session in a row ends the run: {out}"
    );
    assert_eq!(end_of(&mem, 1)["recovery"]["used"], json!(true));
    (mem, out)
}

#[tokio::test]
async fn b854_a_run_that_ends_on_the_lost_session_delivers_the_files_read_back() {
    let (s, r) = (sha('5'), sha('6'));
    let read_back = s3::recovery_bundle_key(RUN, 1);

    // The third lost session in a row, with a usable copy read back: the run delivers that copy and says what it is.
    let (mem, loop_state) = ended_on_the_lost_session(&s, &r).await;
    let p = plan_delivery(&mem, silence(), loop_state).await;
    assert_eq!(p["deliver"], true, "{p}");
    assert_eq!(p["headSha"], r, "the files read back, not the saved copy");
    assert_eq!(p["headSource"], "snapshot");
    assert_eq!(p["outcome"], "stopped_session_lost");
    assert_eq!(
        p["detail"],
        format!("no heartbeat for 600 s. {READ_BACK_SENTENCE}")
    );
    assert!(
        !p["detail"]
            .as_str()
            .unwrap()
            .contains("last saved snapshot"),
        "a copy read back is not worded as a saved snapshot"
    );
    let evidence = evidence_of(&mem);
    assert_eq!(evidence["sourceKey"], read_back, "{evidence}");
    assert_eq!(evidence["headSha"], r);
    assert_eq!(
        p["validate"]["sourceBundleUri"],
        s3::uri(BUCKET, &read_back),
        "the validator judges the read-back object: {p}"
    );

    // The read-back object is gone at delivery: that is no refusal, and the run delivers the saved copy.
    let (mem, loop_state) = ended_on_the_lost_session(&s, &r).await;
    mem.remove_blob(&read_back);
    let p = plan_delivery(&mem, silence(), loop_state).await;
    assert_eq!(p["deliver"], true, "{p}");
    assert_eq!(p["headSha"], s, "the saved copy");
    assert!(
        p["detail"]
            .as_str()
            .unwrap()
            .contains("The work is the last saved snapshot"),
        "{p}"
    );
    assert!(evidence_of(&mem)["sourceKey"].is_null());

    // A copy that was read back and not used is not delivered: the run delivers the saved copy, and the object stays in the bucket.
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &sha('4'), &sum));
    recover(&mem, silence()).await;
    let out = next_slice(
        &mem,
        silence(),
        json!({"slice": 1, "lost": 2, "empty": 0, "continue": true}),
    )
    .await;
    assert_eq!(end_of(&mem, 1)["recovery"]["used"], json!(false));
    let p = plan_delivery(&mem, silence(), out).await;
    assert_eq!(p["headSha"], s);
    assert!(mem.blob(&read_back).is_some());
}

#[tokio::test]
async fn b854_a_later_session_that_restored_the_files_read_back_and_left_nothing_newer_delivers_them()
 {
    let (s, r) = (sha('5'), sha('6'));
    let ending = json!({"slice": 2, "lost": 2, "empty": 0, "continue": true});

    // Session 2 restored the copy read back from session 1 and went to work (the marker on its storage names it and the commit), then
    // was lost without a copy of its own, as the third lost session in a row. The work is still the copy it started from.
    let mem = second_session_from_the_files_read_back(&s, &r, true).await;
    let again = sha('9');
    let sum = mem.put_recovery_bundle(2, &again);
    let mut answer = stored(&again, &r, &r, &sum);
    answer["session"] = json!(2);
    answer["copy"]["marker"] = json!({"schemaVersion": 1, "session": 2, "count": 5, "headSha": r});
    answer["comparison"] =
        json!({"state": "done", "sameAsBaseline": true, "regressed": 0, "rewritten": 0});
    mem.recover_answers(&answer);
    call(
        &mem,
        json!({"op": "recover", "runId": RUN, "session": silence(), "loop": ending}),
    )
    .await
    .unwrap();
    let out = next_slice(&mem, silence(), ending.clone()).await;
    assert_eq!(
        out["continue"],
        json!(false),
        "TRIGGER PRESENT: the run ends on session 2: {out}"
    );
    assert_eq!(end_of(&mem, 2)["recovery"]["used"], json!(false));
    let p = plan_delivery(&mem, silence(), out).await;
    assert_eq!(p["deliver"], true, "{p}");
    assert_eq!(
        p["headSha"], r,
        "the copy session 2 started from, not the older saved copy"
    );
    assert_eq!(
        evidence_of(&mem)["sourceKey"],
        s3::recovery_bundle_key(RUN, 1)
    );
    assert!(
        p["detail"].as_str().unwrap().ends_with(READ_BACK_SENTENCE),
        "it is not described as an ordinary saved copy: {p}"
    );

    // Session 2 never worked on that copy (its storage was read and holds no marker that names the restored commit) and the run has no
    // time for another session: the delivery takes the saved copy, never a copy no session could start from.
    let mem = second_session_from_the_files_read_back(&s, &r, true).await;
    mem.recover_answers(&read_without_a_start_marker(&mem));
    call(
        &mem,
        json!({"op": "recover", "runId": RUN, "session": silence(), "loop": ending}),
    )
    .await
    .unwrap();
    mem.set_now_ms(mem.now_ms() + 8_000_000);
    let out = next_slice(&mem, silence(), ending.clone()).await;
    assert_eq!(
        out["continue"],
        json!(false),
        "TRIGGER PRESENT: no time is left for another session: {out}"
    );
    assert_eq!(end_of(&mem, 2)["lostCause"], "restore-failed");
    let p = plan_delivery(&mem, silence(), out).await;
    assert_eq!(p["headSha"], s, "the saved copy: {p}");
    assert!(
        !p["detail"].as_str().unwrap().contains(READ_BACK_SENTENCE),
        "{p}"
    );
}

#[tokio::test]
async fn b854_a_session_that_was_asked_to_end_is_named_in_the_delivery() {
    let (s, r) = (sha('5'), sha('6'));
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &s, &sum));
    let ended = notice(&dispatcher::items::rfc3339(mem.now_ms() / 1000));
    assert_eq!(recover(&mem, ended.clone()).await["recovery"], "copied");
    let out = next_slice(
        &mem,
        ended.clone(),
        json!({"slice": 1, "lost": 2, "empty": 0, "continue": true}),
    )
    .await;
    assert_eq!(out["continue"], json!(false), "{out}");
    let p = plan_delivery(&mem, ended, out).await;
    assert_eq!(p["outcome"], "stopped_session_lost");
    assert_eq!(
        p["detail"],
        format!("the session was asked to end from outside. {READ_BACK_SENTENCE}")
    );
    assert_eq!(p["headSha"], r);
}

/// The gate of the rule that a check may refuse to deliver work and never hides it. A run went on from the files the service
/// read back, ended on that session, and its delivery was refused or a later step failed. The blocked reason names that copy and the
/// command that downloads it. Before this change the reason named the time of the older saved copy, and the reason of a run without a
/// saved copy said that there was no snapshot to download, although the files read back were in the bucket.
#[tokio::test]
async fn b854_a_run_that_ends_blocked_names_the_command_that_downloads_the_files_read_back() {
    let (s, r) = (sha('5'), sha('6'));
    let refused = |violations: Value| json!({"runId": RUN, "validated": {"result": {"pass": false, "violations": violations}}});
    let secret = json!([{"rule": "secret", "path": "src/a.py", "detail": "a detected secret"}]);
    let names_it = format!(
        "The run's newest copy is what the service read back from the storage of session 1 after that session ended. Download it with \
         `ham pull {RUN} --read-back 1`."
    );
    let blocked = |mem: Mem, state: Value| async move {
        let out = call(
            &mem,
            json!({"op": "mark-blocked", "runId": RUN, "state": state}),
        )
        .await
        .unwrap();
        assert_eq!(mem.run().status, GoalStatus::Blocked);
        out["reason"].as_str().unwrap().to_string()
    };

    // The service chose the files read back, the run ended on that session, and the validator refused them.
    let (mem, _) = ended_on_the_lost_session(&s, &r).await;
    assert_eq!(
        serde_json::to_value(mem.run()).unwrap()["recoveredCopy"]["session"],
        1,
        "TRIGGER PRESENT: the Run item names the copy the run went on from"
    );
    let reason = blocked(mem, refused(secret.clone())).await;
    assert!(reason.ends_with(&names_it), "{reason}");
    assert!(
        !reason.contains("last snapshot was saved at"),
        "the older saved copy is not what the command downloads: {reason}"
    );

    // A step that failed after the delivery was planned: the same sentence.
    let (mem, _) = ended_on_the_lost_session(&s, &r).await;
    let reason = blocked(
        mem,
        json!({"runId": RUN, "failure": {"Error": "Packager.Failed", "Cause": "the zip could not be written"}}),
    )
    .await;
    assert!(reason.ends_with(&names_it), "{reason}");

    // A run that saved no copy at all before its session was lost: nothing says that there is no work to download.
    let mem = seeded(json!({"maxRunS": 8_000}));
    call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
    let base = staged()["baseSha"].as_str().unwrap().to_string();
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &base, &base, &sum));
    assert_eq!(recover(&mem, silence()).await["recovery"], "copied");
    let out = next_slice(
        &mem,
        silence(),
        json!({"slice": 1, "lost": 2, "empty": 0, "continue": true}),
    )
    .await;
    assert_eq!(out["continue"], json!(false), "{out}");
    assert_eq!(
        end_of(&mem, 1)["recovery"]["used"],
        json!(true),
        "TRIGGER PRESENT: the files read back are the run's only copy"
    );
    let reason = blocked(mem, refused(secret.clone())).await;
    assert!(reason.ends_with(&names_it), "{reason}");
    assert!(!reason.contains("no snapshot to download"), "{reason}");

    // Trigger: files that were read back and not used leave the sentences of the saved copy as they were.
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &sha('4'), &sum));
    recover(&mem, silence()).await;
    next_slice(
        &mem,
        silence(),
        json!({"slice": 1, "lost": 2, "empty": 0, "continue": true}),
    )
    .await;
    let reason = blocked(mem, refused(secret)).await;
    assert!(!reason.contains("--read-back"), "{reason}");
    assert!(reason.contains("--snapshot"), "{reason}");
}
