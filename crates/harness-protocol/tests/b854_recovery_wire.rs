//! The wire of reading a stopped session's storage back before the next session starts.
//!
//! When a session of a run is stopped from outside or stops answering, the service calls the old session id again with a body of kind
//! `recover`. The worker of the new machine copies the work tree the storage still holds into one bundle under a prefix of its own
//! (`runs/<run>/agents/recovery/turns/<n>/recovery.bundle`) and answers with the facts. The service decides whether the next session starts
//! from that copy. What this file pins:
//!
//! * the direct body and the recovery body cannot be mistaken for each other, and both stay strict;
//! * a later session may restore the read-back bundle of an earlier session of its own run, and nothing else under `agents/recovery/`;
//! * a heartbeat without `ending` is the text it always was, and a reader of that shape ignores the member;
//! * the Agent item carries the fence and the switch, the Run item names the copy the service chose, the start request takes the switch;
//! * the worker's answer can say that the comparison with the saved copy is unknown, and that says nothing about the copy being stored.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::api::internal::HeartbeatRequest;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{
    COMPARISON_DONE, COMPARISON_UNKNOWN, DirectInvocationError, DirectInvocationRequest,
    MAX_COMPARE_PATHS_SHOWN, MAX_PREFIX_CHECK_BYTES, OLD_WORKER_TAIL_S, RECOVERY_AGENT_ID,
    RECOVERY_CREDENTIALS_S, RECOVERY_LIMIT_S, RECOVERY_SETTLE_S, RECOVERY_STATE_TIMEOUT_S,
    RECOVERY_STOP_CAP_S, RECOVERY_WORKER_BUDGET_S, RecoveryAnswer, RecoveryInvocationError,
    RecoveryInvocationRequest, SESSION_END_REQUESTED, SessionEnded, s3, validate_direct_invocation,
    validate_recovery_invocation,
};
use harness_protocol::store::{AgentItem, RecoveredCopyRef, RunItem};
use serde_json::{Value, json};

const RUN: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const OTHER_RUN: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT2P";
const BUCKET: &str = "hamh-direct-harness-111122223333";
const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SHA_C: &str = "cccccccccccccccccccccccccccccccccccccccc";
const ATTEMPT: &str = "01M6B3S8K4D0YK5E9F7GR1XV2Q";

fn credentials() -> Value {
    json!({"Version": 1, "AccessKeyId": "ASIAEXAMPLEEXAMPLE", "SecretAccessKey": "the-secret-key", "SessionToken": "the-session-token",
           "Expiration": "2026-11-16T18:10:00Z"})
}

fn direct(turn: u32, repo_key: &str) -> Value {
    let mut v = json!({
        "kind": "direct", "runId": RUN, "agentId": "direct", "turn": turn,
        "runtimeSessionId": format!("{RUN}-direct-g{turn}"),
        "credentials": credentials(),
        "nonce": "n", "internalApiUrl": "https://abc.execute-api.us-east-1.amazonaws.com/direct", "region": "us-east-1",
        "engine": "cline",
        "model": {"id": "us.anthropic.claude-sonnet-5-5", "region": "us-east-1", "contextTokens": 200000, "maxOutputTokens": 64000,
                  "protocol": "converse"},
        "request": {"uri": format!("s3://{BUCKET}/runs/{RUN}/input/request.json"), "sha256": "ab".repeat(32)},
        "repo": {"kind": "s3Bundle", "uri": format!("s3://{BUCKET}/{repo_key}")},
        "timeoutS": 600, "snapshotEveryS": 600
    });
    if turn > 1 {
        v["slice"] = json!({"last": false, "baseSha": SHA_A, "restoreHead": SHA_B, "sessionsBefore": turn - 1});
    }
    v
}

fn recover(session: u32) -> Value {
    json!({
        "kind": "recover", "attemptId": ATTEMPT, "runId": RUN, "session": session,
        "runtimeSessionId": format!("{RUN}-direct-g{session}"),
        "region": "us-east-1",
        "credentials": credentials(),
        "object": format!("s3://{BUCKET}/runs/{RUN}/agents/recovery/turns/{session}/recovery.bundle"),
        "baseSha": SHA_A, "startHead": SHA_A,
        "baseline": {"headSha": SHA_B, "count": 4,
                     "object": format!("s3://{BUCKET}/runs/{RUN}/agents/direct/turns/{session}/snapshot.bundle")},
        "budgetS": 61
    })
}

fn parse_recover(v: Value) -> RecoveryInvocationRequest {
    serde_json::from_value(v).unwrap()
}

#[test]
fn b854_the_protocol_is_1_65_0() {
    assert_eq!(PROTOCOL_VERSION, "1.65.0");
    let docs: std::collections::BTreeMap<&str, Value> = harness_protocol::export::all_schemas()
        .into_iter()
        .collect();
    assert_eq!(docs["openapi-v1.json"]["info"]["version"], "1.65.0");
    assert_eq!(docs["openapi-internal.json"]["info"]["version"], "1.65.0");
}

#[test]
fn b854_a_direct_request_refuses_kind_recover_and_a_recovery_request_refuses_an_unknown_member() {
    // A valid recovery body parses, and it is no direct body.
    let req = parse_recover(recover(1));
    assert_eq!(req.session, 1);
    assert_eq!(req.attempt_id, ATTEMPT);
    assert_eq!(req.budget_s, 61);
    assert_eq!(validate_recovery_invocation(&req), Ok(()));
    assert!(serde_json::from_value::<DirectInvocationRequest>(recover(1)).is_err());
    // A valid direct body parses, and it is no recovery body.
    let d: DirectInvocationRequest =
        serde_json::from_value(direct(1, &s3::repo_bundle_key(RUN))).unwrap();
    assert_eq!(validate_direct_invocation(&d), Ok(()));
    assert!(
        serde_json::from_value::<RecoveryInvocationRequest>(direct(1, &s3::repo_bundle_key(RUN)))
            .is_err()
    );
    // A direct body whose kind is the other word is refused, whatever else it holds.
    let mut wrong = direct(1, &s3::repo_bundle_key(RUN));
    wrong["kind"] = json!("recover");
    assert!(serde_json::from_value::<DirectInvocationRequest>(wrong).is_err());
    // Both stay strict: one unknown member refuses the body.
    let mut extra = recover(1);
    extra["engine"] = json!("cline");
    assert!(serde_json::from_value::<RecoveryInvocationRequest>(extra).is_err());
    let mut extra = recover(1);
    extra["baseline"]["getUrl"] = json!("https://example.invalid/presigned");
    assert!(
        serde_json::from_value::<RecoveryInvocationRequest>(extra).is_err(),
        "the baseline is named as an object the worker fetches with its own credentials, never as a presigned URL"
    );
}

#[test]
fn b854_a_recovery_request_names_one_session_one_object_and_one_baseline_of_its_own_run() {
    use RecoveryInvocationError as E;
    let ok = parse_recover(recover(2));
    assert_eq!(validate_recovery_invocation(&ok), Ok(()));
    let bad = |edit: &dyn Fn(&mut Value)| {
        let mut v = recover(2);
        edit(&mut v);
        validate_recovery_invocation(&parse_recover(v))
    };
    assert_eq!(bad(&|v| v["runId"] = json!("no/slash")), Err(E::BadRunId));
    assert_eq!(bad(&|v| v["session"] = json!(0)), Err(E::BadSession));
    assert_eq!(bad(&|v| v["session"] = json!(257)), Err(E::BadSession));
    assert_eq!(
        bad(&|v| v["attemptId"] = json!("not a ulid")),
        Err(E::BadAttemptId)
    );
    // The session id is the one of that session of that run, exactly.
    assert_eq!(
        bad(&|v| v["runtimeSessionId"] = json!(format!("{RUN}-direct-g3"))),
        Err(E::BadRuntimeSessionId)
    );
    assert_eq!(
        bad(&|v| v["runtimeSessionId"] = json!(format!("{OTHER_RUN}-direct-g2"))),
        Err(E::BadRuntimeSessionId)
    );
    assert_eq!(bad(&|v| v["region"] = json!("nowhere")), Err(E::BadRegion));
    assert_eq!(
        bad(&|v| v["credentials"]["AccessKeyId"] = json!("AKIAEXAMPLEEXAMPLE")),
        Err(E::NotSessionCredentials)
    );
    // The object is the read-back key of that session and no other key.
    for key in [
        format!("runs/{RUN}/agents/recovery/turns/1/recovery.bundle"),
        format!("runs/{OTHER_RUN}/agents/recovery/turns/2/recovery.bundle"),
        format!("runs/{RUN}/agents/recovery/turns/2/other.bundle"),
        format!("runs/{RUN}/agents/direct/turns/2/snapshot.bundle"),
    ] {
        assert_eq!(
            bad(&|v| v["object"] = json!(format!("s3://{BUCKET}/{key}"))),
            Err(E::BadObject),
            "{key}"
        );
    }
    assert_eq!(bad(&|v| v["baseSha"] = json!("abc")), Err(E::BadCommit));
    assert_eq!(bad(&|v| v["startHead"] = json!("ABC")), Err(E::BadCommit));
    assert_eq!(
        bad(&|v| v["baseline"]["headSha"] = json!("")),
        Err(E::BadCommit)
    );
    assert_eq!(bad(&|v| v["budgetS"] = json!(0)), Err(E::BadBudget));
    assert_eq!(
        bad(&|v| v["budgetS"] = json!(RECOVERY_WORKER_BUDGET_S + 1)),
        Err(E::BadBudget)
    );
    // The baseline is a saved copy or a session bundle of this run, of this session or an earlier one, in the same bucket; or it is absent.
    assert_eq!(
        bad(&|v| {
            v["baseline"].as_object_mut().unwrap().remove("object");
        }),
        Ok(())
    );
    assert_eq!(
        bad(&|v| v["baseline"]["object"] = json!(format!(
            "s3://{BUCKET}/runs/{RUN}/agents/direct/turns/1/snapshot.bundle"
        ))),
        Ok(())
    );
    assert_eq!(
        bad(&|v| v["baseline"]["object"] = json!(format!(
            "s3://{BUCKET}/runs/{RUN}/agents/recovery/turns/1/recovery.bundle"
        ))),
        Ok(()),
        "a session that started from an earlier read-back copy and saved nothing has that copy as its baseline"
    );
    for key in [
        format!("runs/{RUN}/agents/direct/turns/1/bundle"),
        format!("runs/{RUN}/agents/direct/turns/3/snapshot.bundle"),
        format!("runs/{OTHER_RUN}/agents/direct/turns/1/snapshot.bundle"),
        format!("runs/{RUN}/agents/recovery/turns/2/recovery.bundle"),
        format!("runs/{RUN}/input/request.json"),
        format!("runs/{RUN}/slices/2.json"),
    ] {
        assert_eq!(
            bad(&|v| v["baseline"]["object"] = json!(format!("s3://{BUCKET}/{key}"))),
            Err(E::BadBaseline),
            "{key}"
        );
    }
    assert_eq!(
        bad(&|v| v["baseline"]["object"] = json!(format!(
            "s3://another-bucket/runs/{RUN}/agents/direct/turns/1/snapshot.bundle"
        ))),
        Err(E::BadBaseline)
    );
    // Every refusal says what it wants in words.
    assert!(E::BadObject.to_string().contains("recovery.bundle"));
    assert!(
        E::BadBudget
            .to_string()
            .contains(&RECOVERY_WORKER_BUDGET_S.to_string())
    );
}

#[test]
fn b854_a_heartbeat_without_ending_is_the_text_it_always_was() {
    let plain = HeartbeatRequest {
        run_id: RUN.into(),
        agent_id: "direct".into(),
        turn: 3,
        ending: None,
    };
    assert_eq!(
        serde_json::to_string(&plain).unwrap(),
        format!(r#"{{"runId":"{RUN}","agentId":"direct","turn":3}}"#)
    );
    let ending = HeartbeatRequest {
        ending: Some(true),
        ..plain.clone()
    };
    assert_eq!(serde_json::to_value(&ending).unwrap()["ending"], true);
    let back: HeartbeatRequest =
        serde_json::from_value(serde_json::to_value(&ending).unwrap()).unwrap();
    assert_eq!(back, ending);
    // A reader of the shape before this member (a worker-api of an earlier release) accepts the body and does not see the member.
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Before {
        run_id: String,
        agent_id: String,
        turn: u32,
    }
    let old: Before = serde_json::from_value(serde_json::to_value(&ending).unwrap()).unwrap();
    assert_eq!(
        (old.run_id.as_str(), old.agent_id.as_str(), old.turn),
        (RUN, "direct", 3)
    );
}

#[test]
fn b854_the_recovery_key_is_outside_the_direct_agents_prefix() {
    assert_eq!(RECOVERY_AGENT_ID, "recovery");
    let key = s3::recovery_bundle_key(RUN, 7);
    assert_eq!(
        key,
        format!("runs/{RUN}/agents/recovery/turns/7/recovery.bundle")
    );
    assert!(!key.starts_with(&format!("runs/{RUN}/agents/direct/")));
    assert!(!key.starts_with(&s3::turn_prefix_t(RUN, 7)));
    // The service's own records sit beside the slice records, where no session can write.
    assert_eq!(
        s3::recovery_attempt_key(RUN, 7),
        format!("runs/{RUN}/slices/7-recovery.json")
    );
    assert_eq!(
        s3::recovery_result_key(RUN, 7),
        format!("runs/{RUN}/slices/7-recovery-result.json")
    );
    assert_eq!(
        s3::slice_record_key(RUN, 7),
        format!("runs/{RUN}/slices/7.json")
    );
}

#[test]
fn b854_a_later_session_may_restore_the_read_back_bundle_of_an_earlier_session_and_nothing_else_under_that_prefix()
 {
    let check = |turn: u32, key: &str| {
        let req: DirectInvocationRequest = serde_json::from_value(direct(turn, key)).unwrap();
        validate_direct_invocation(&req)
    };
    // Accepted: the read-back bundle of an earlier session of the same run.
    assert_eq!(check(2, &s3::recovery_bundle_key(RUN, 1)), Ok(()));
    assert_eq!(check(5, &s3::recovery_bundle_key(RUN, 4)), Ok(()));
    assert_eq!(check(5, &s3::recovery_bundle_key(RUN, 1)), Ok(()));
    // Refused: the same session, a later session, the first session, another run, any other key under the prefix.
    assert_eq!(
        check(2, &s3::recovery_bundle_key(RUN, 2)),
        Err(DirectInvocationError::BadRepo)
    );
    assert_eq!(
        check(2, &s3::recovery_bundle_key(RUN, 3)),
        Err(DirectInvocationError::BadRepo)
    );
    assert_eq!(
        check(1, &s3::recovery_bundle_key(RUN, 1)),
        Err(DirectInvocationError::BadRepo)
    );
    assert_eq!(
        check(3, &s3::recovery_bundle_key(OTHER_RUN, 1)),
        Err(DirectInvocationError::BadRepo)
    );
    for key in [
        format!("runs/{RUN}/agents/recovery/turns/1/recovery.bundle.old"),
        format!("runs/{RUN}/agents/recovery/turns/1/other"),
        format!("runs/{RUN}/agents/recovery/turns/01/recovery.bundle"),
        format!("runs/{RUN}/agents/recovery/recovery.bundle"),
    ] {
        assert_eq!(check(3, &key), Err(DirectInvocationError::BadRepo), "{key}");
    }
    // Unchanged: the base bundle and the earlier sessions' own bundles and saved copies.
    assert_eq!(check(1, &s3::repo_bundle_key(RUN)), Ok(()));
    assert_eq!(check(3, &s3::repo_bundle_key(RUN)), Ok(()));
    assert_eq!(check(3, &s3::bundle_key_t(RUN, 2)), Ok(()));
    assert_eq!(check(3, &s3::snapshot_bundle_key_t(RUN, 1)), Ok(()));
    assert_eq!(
        check(3, &s3::snapshot_bundle_key_t(RUN, 3)),
        Err(DirectInvocationError::BadRepo)
    );
    // The refusal names the third kind a later session may restore.
    let text = DirectInvocationError::BadRepo.to_string();
    assert!(text.contains("input/repo.bundle"), "{text}");
    assert!(text.contains("recovery.bundle"), "{text}");
}

#[test]
fn b854_the_limits_are_the_ones_the_design_names() {
    assert_eq!(RECOVERY_LIMIT_S, 120);
    assert_eq!(RECOVERY_WORKER_BUDGET_S, 75);
    assert_eq!(RECOVERY_STATE_TIMEOUT_S, 150);
    assert_eq!(RECOVERY_SETTLE_S, 15);
    assert_eq!(RECOVERY_STOP_CAP_S, 30);
    assert_eq!(OLD_WORKER_TAIL_S, 75);
    assert_eq!(RECOVERY_CREDENTIALS_S, 900);
    assert_eq!(MAX_PREFIX_CHECK_BYTES, 67_108_864);
    assert_eq!(MAX_COMPARE_PATHS_SHOWN, 5);
}

fn repo_text(rel: &str) -> String {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

#[test]
fn b854_the_limits_fit_inside_each_other() {
    // The worker's budget and the stop fit in the op; the op ends before its state; the state ends before the function.
    assert!(u64::from(RECOVERY_WORKER_BUDGET_S) + RECOVERY_STOP_CAP_S <= RECOVERY_LIMIT_S);
    let asl: Value = serde_json::from_str(&repo_text("statemachines/direct.asl.json")).unwrap();
    let state_timeout = asl["States"]["Recover"]["TimeoutSeconds"].as_u64().unwrap();
    assert_eq!(state_timeout, RECOVERY_STATE_TIMEOUT_S);
    assert!(RECOVERY_LIMIT_S < state_timeout);
    // The function's timeout: `timeout: withInstances ? 900 : 300` in the stack of the run machine. The shorter one is the bound.
    let ts = repo_text("infra/lib/stacks/orchestration-direct.ts");
    let after = ts
        .split("timeout: withInstances ?")
        .nth(1)
        .expect("the run-session function's timeout in orchestration-direct.ts");
    let numbers: Vec<u64> = after
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .take(2)
        .map(|s| s.parse().unwrap())
        .collect();
    assert_eq!(numbers, [900, 300]);
    let function_timeout = *numbers.iter().min().unwrap();
    assert!(state_timeout < function_timeout && function_timeout <= 300);
    // The old machine called the service until 65.5 s after its stop began: no decision before that.
    const { assert!(OLD_WORKER_TAIL_S >= 66) };
    // The worst case on an end the worker gave notice of: 14 s of the settle left, 30 s for the stop, 5 s for the new machine. The
    // worker then gets the smaller of 75 s and what is left less 10 s, and the whole still ends inside the op's limit.
    let left = RECOVERY_LIMIT_S - 14 - RECOVERY_STOP_CAP_S - 5;
    assert_eq!(left, 71);
    let worker = u64::from(RECOVERY_WORKER_BUDGET_S).min(left - 10);
    assert_eq!(worker, 61);
    assert!(14 + RECOVERY_STOP_CAP_S + 5 + worker <= RECOVERY_LIMIT_S);
    const { assert!(RECOVERY_SETTLE_S > 14) };
    // The machine keeps its 600 s for a session that is silent: the read comes after it, never instead of it.
    assert_eq!(asl["States"]["StartSession"]["HeartbeatSeconds"], 600);
    assert_eq!(harness_protocol::direct::SESSION_LOST_AFTER_S, 600);
}

#[test]
fn b854_the_debug_text_of_a_recovery_request_holds_no_secret() {
    let req = parse_recover(recover(1));
    let text = format!("{req:?}");
    assert!(!text.contains("the-secret-key"), "{text}");
    assert!(!text.contains("the-session-token"), "{text}");
    // What the text does hold: which session, which object and which baseline.
    assert!(text.contains(ATTEMPT));
    assert!(text.contains("recovery.bundle"));
    assert!(text.contains("snapshot.bundle"));
    assert_eq!(
        req.baseline.object.as_deref(),
        Some(format!("s3://{BUCKET}/runs/{RUN}/agents/direct/turns/1/snapshot.bundle").as_str())
    );
}

#[test]
fn b854_an_answer_says_stored_and_compared_apart_and_reads_a_later_workers_members() {
    // Stored, compared, nothing in doubt.
    let v = json!({
        "attemptId": ATTEMPT, "runId": RUN, "session": 1, "recovered": true,
        "copy": {"headSha": SHA_C, "treeSha": SHA_A, "parentSha": SHA_B,
                 "marker": {"schemaVersion": 1, "session": 1, "count": 4, "headSha": SHA_B},
                 "stable": true, "files": 12, "bytes": 3456, "bundleBytes": 789,
                 "bundleSha256": "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="},
        "comparison": {"state": "done", "sameAsBaseline": false, "regressed": 0, "rewritten": 0},
        "uptimeS": 41, "tookMs": 2300, "agentdVersion": "0.1.0",
        "aMemberOfALaterWorker": {"x": 1}
    });
    let a: RecoveryAnswer = serde_json::from_value(v).unwrap();
    assert!(a.recovered);
    let copy = a.copy.as_ref().unwrap();
    assert_eq!(copy.parent_sha.as_deref(), Some(SHA_B));
    assert_eq!(copy.marker.as_ref().unwrap().head_sha, SHA_B);
    let c = a.comparison.as_ref().unwrap();
    assert_eq!(c.state, COMPARISON_DONE);
    assert!(c.done());
    assert!(!c.same_as_baseline);
    assert_eq!((c.regressed, c.rewritten), (0, 0));

    // Stored, and the comparison could not be made: the copy is still reported as stored, and nothing reads as "no file is older".
    let v = json!({
        "attemptId": ATTEMPT, "runId": RUN, "session": 1, "recovered": true,
        "copy": {"headSha": SHA_C, "treeSha": SHA_A, "stable": true, "files": 12, "bytes": 3456, "bundleBytes": 789,
                 "bundleSha256": "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="},
        "comparison": {"state": "unknown", "reason": "baseline-unreadable"},
        "tookMs": 900, "agentdVersion": "0.1.0"
    });
    let a: RecoveryAnswer = serde_json::from_value(v).unwrap();
    assert!(
        a.recovered,
        "a comparison that failed does not take the stored copy back"
    );
    assert_eq!(
        a.copy.as_ref().unwrap().parent_sha,
        None,
        "a copy without readable history has no parent"
    );
    let c = a.comparison.as_ref().unwrap();
    assert_eq!(c.state, COMPARISON_UNKNOWN);
    assert!(!c.done());
    assert_eq!(c.reason.as_deref(), Some("baseline-unreadable"));
    // An answer without the member (no comparison was tried) is not a comparison that found nothing.
    let v = json!({"attemptId": ATTEMPT, "runId": RUN, "session": 1, "recovered": false, "reason": "no-tree",
                   "tookMs": 12, "agentdVersion": "0.1.0"});
    let a: RecoveryAnswer = serde_json::from_value(v).unwrap();
    assert!(!a.recovered);
    assert_eq!(a.reason.as_deref(), Some("no-tree"));
    assert!(a.copy.is_none() && a.comparison.is_none());
    // A state word of a later worker is not "done".
    let v = json!({"state": "partly", "sameAsBaseline": false, "regressed": 0, "rewritten": 0});
    let c: harness_protocol::direct::RecoveryComparison = serde_json::from_value(v).unwrap();
    assert!(!c.done());
}

#[test]
fn b854_a_known_end_is_an_output_of_its_own_and_no_turn_complete() {
    let ended = SessionEnded {
        session: 2,
        cause: SESSION_END_REQUESTED.into(),
        at: "2026-11-16T14:03:07Z".into(),
    };
    assert_eq!(SESSION_END_REQUESTED, "end-requested");
    let out = json!({"sessionEnded": ended});
    assert_eq!(
        out,
        json!({"sessionEnded": {"session": 2, "cause": "end-requested", "at": "2026-11-16T14:03:07Z"}})
    );
    // The machine's gate reads `$.session.turn.runId`: a known end has no such member, a turn-complete has one.
    assert!(out.get("runId").is_none());
    assert!(
        serde_json::from_value::<harness_protocol::api::internal::TurnCompleteRequest>(out)
            .is_err(),
        "a known end never reads as a result of the session"
    );
}

fn agent_item(extra: Value) -> Value {
    let mut v = json!({
        "runId": RUN, "agentId": "direct", "taskId": "root", "attempt": 1,
        "runtimeArn": "arn:aws:bedrock-agentcore:us-east-1:111122223333:runtime/hamh_direct_worker-abc",
        "runtimeSessionId": format!("{RUN}-direct-g2"), "generation": 2, "engine": "cline", "engineVersion": "3.0.71",
        "transport": "acp", "modelId": "us.anthropic.claude-sonnet-5-5", "pool": "direct", "status": "running",
        "sessionGeneration": 2, "version": 9
    });
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

#[test]
fn b854_the_agent_item_carries_the_fence_and_the_switch_and_an_item_without_them_reads_as_before() {
    let before = agent_item(json!({}));
    let item: AgentItem = serde_json::from_value(before.clone()).unwrap();
    assert_eq!(item.fenced_generation, None);
    assert_eq!(item.recover_after_stop, None);
    assert_eq!(
        serde_json::to_value(&item).unwrap(),
        before,
        "an item without the members is written as it was"
    );
    let item: AgentItem = serde_json::from_value(agent_item(
        json!({"fencedGeneration": 2, "recoverAfterStop": false}),
    ))
    .unwrap();
    assert_eq!(item.fenced_generation, Some(2));
    assert_eq!(item.recover_after_stop, Some(false));
    let out = serde_json::to_value(&item).unwrap();
    assert_eq!(out["fencedGeneration"], 2);
    assert_eq!(out["recoverAfterStop"], false);
    // A reader of the item's shape before the members ignores them (the store types refuse no unknown member).
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Before {
        session_generation: Option<u32>,
    }
    let old: Before = serde_json::from_value(out).unwrap();
    assert_eq!(old.session_generation, Some(2));
}

#[test]
fn b854_the_run_item_names_the_read_back_copy_the_service_chose() {
    let copy = RecoveredCopyRef {
        session: 1,
        key: s3::recovery_bundle_key(RUN, 1),
        version_id: Some("3HL4kqtJlcpXroDTDmJ.rUAA.6jVzXkW".into()),
        head_sha: SHA_C.into(),
        at: "2026-11-16T14:05:00Z".into(),
        newest_write_at: None,
        bundle_bytes: 789,
    };
    let v = serde_json::to_value(&copy).unwrap();
    assert_eq!(
        v,
        json!({"session": 1, "key": format!("runs/{RUN}/agents/recovery/turns/1/recovery.bundle"),
               "versionId": "3HL4kqtJlcpXroDTDmJ.rUAA.6jVzXkW", "headSha": SHA_C, "at": "2026-11-16T14:05:00Z", "bundleBytes": 789})
    );
    let run = json!({
        "runId": RUN, "ownerPrincipalArn": "arn:aws:sts::111122223333:assumed-role/Builder/example", "promptSha256": "ab".repeat(32),
        "target": {"mode": "s3-only"}, "status": "active", "phase": "planning", "enginePolicyVersion": 1,
        "createdAt": "2026-11-16T14:00:00Z", "version": 3
    });
    let item: RunItem = serde_json::from_value(run.clone()).unwrap();
    assert_eq!(item.recovered_copy, None);
    assert_eq!(
        serde_json::to_value(&item).unwrap(),
        run,
        "a run that used no read-back copy has the item it always had"
    );
    let mut with = run;
    with["recoveredCopy"] = v;
    let item: RunItem = serde_json::from_value(with).unwrap();
    assert_eq!(item.recovered_copy, Some(Box::new(copy)));
}

#[test]
fn b854_a_start_request_takes_the_switch_and_a_default_request_has_no_member() {
    let start = |extra: Value| -> Result<StartDirectRunRequest, serde_json::Error> {
        let mut v = json!({"prompt": "Add a health check.", "target": {"mode": "s3-only"}, "model": "us.anthropic.claude-sonnet-5-5"});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        serde_json::from_value(v)
    };
    let off = start(json!({"recoverAfterStop": false})).unwrap();
    assert_eq!(off.recover_after_stop, Some(false));
    assert_eq!(
        serde_json::to_value(&off).unwrap()["recoverAfterStop"],
        false
    );
    let default = start(json!({})).unwrap();
    assert_eq!(default.recover_after_stop, None);
    assert!(
        serde_json::to_value(&default)
            .unwrap()
            .get("recoverAfterStop")
            .is_none(),
        "a request that keeps the default is byte for byte the request of protocol 1.62.0"
    );
    assert!(start(json!({"recoverAfterStop": "no"})).is_err());
}

#[test]
fn b854_the_contract_files_describe_the_recovery_wire() {
    let docs: std::collections::BTreeMap<&str, Value> = harness_protocol::export::all_schemas()
        .into_iter()
        .collect();
    let runtime = serde_json::to_string(&docs["runtime.schema.json"]).unwrap();
    for needle in [
        "RecoveryInvocationRequest",
        "RecoveryBaseline",
        "RecoveryAnswer",
        "RecoveredCopy",
        "RecoveryComparison",
        "CopyMarker",
        "SessionEnded",
        "attemptId",
        "budgetS",
    ] {
        assert!(
            runtime.contains(needle),
            "{needle:?} in runtime.schema.json"
        );
    }
    let internal = serde_json::to_string(&docs["openapi-internal.json"]).unwrap();
    assert!(
        internal.contains("\"ending\""),
        "the heartbeat's member in openapi-internal.json"
    );
    let v1 = serde_json::to_string(&docs["openapi-v1.json"]).unwrap();
    assert!(
        v1.contains("recoverAfterStop"),
        "the start request's member in openapi-v1.json"
    );
    let items = serde_json::to_string(&docs["items.schema.json"]).unwrap();
    for needle in ["fencedGeneration", "recoveredCopy", "RecoveredCopyRef"] {
        assert!(items.contains(needle), "{needle:?} in items.schema.json");
    }
}

/// The service's `session.started` event says that it reads a stopped session's storage back, and only then: `true` is the one value
/// that is written. An event without the member is the event of a run without the read and of a service that predates it.
#[test]
fn b854_session_started_says_that_storage_is_read_back_and_is_silent_otherwise() {
    use harness_protocol::orchestration::SessionStartedPayload;
    let plain =
        json!({"agentId": "direct", "engine": "cline", "modelId": "us.anthropic.claude-opus-5-5"});
    let before: SessionStartedPayload = serde_json::from_value(plain.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(&before).unwrap(),
        plain,
        "an event without the member is written as it always was"
    );
    let mut with = plain.clone();
    with["storageReadBack"] = json!(true);
    let said: SessionStartedPayload =
        serde_json::from_value(with.clone()).expect("the member is read");
    assert_eq!(
        serde_json::to_value(&said).unwrap(),
        with,
        "the member is kept when the event is written again"
    );
    let schema = repo_text("schemas/orchestration.schema.json");
    assert!(
        schema.contains("storageReadBack"),
        "the contract file names the member"
    );
}
