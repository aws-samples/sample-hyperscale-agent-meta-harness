//! `plan-delivery`, `finalize`, `mark-blocked` and `run-ended`, run over the in-memory ports. The payloads are parsed
//! into the real request types of the delivery Lambdas, so a payload the unchanged gateway, validator or packager
//! would refuse fails here.
#![allow(clippy::unwrap_used)]

mod support;

use delivery::gateway::GatewayRequest;
use delivery::packager::PackageRequest;
use delivery::validator::{EngineConfigMode, ValidateRequest};
use harness_protocol::direct::{DirectOutcome, HeadSource, banned_claim_word, s3};
use harness_protocol::goal::{GoalPhase, GoalStatus};
use harness_protocol::target::RunTarget;
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use run_session::run_ended::{parse_run_end, reap};
use serde_json::{Value, json};
use support::fakes::*;

async fn call(mem: &Mem, v: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(v).unwrap();
    handle(&mem.deps(), &op).await
}

async fn plan(mem: &Mem, session: Value) -> Value {
    call(
        mem,
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged(), "session": session}),
    )
    .await
    .unwrap()
}

fn seeded(target: RunTarget) -> Mem {
    let mem = Mem::default();
    mem.seed(target, "Add percent_change(a, b) with tests", MODEL);
    mem
}

fn done_session() -> Value {
    json!({"turn": turn_output(true), "failure": {}})
}

fn lost_session() -> Value {
    json!({"turn": {}, "failure": {"Error": "States.HeartbeatTimeout", "Cause": "heartbeat"}})
}

// ---- plan-delivery: the head ------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_finished_session_delivers_its_final_bundle() {
    let mem = seeded(repo_target());
    mem.put_bundle();
    mem.put_summary(&summary(
        DirectOutcome::ReportedDone,
        Some("Added pct(a, b)."),
        true,
    ));
    let p = plan(&mem, done_session()).await;
    assert_eq!(p["deliver"], true);
    assert_eq!(p["headSha"], HEAD);
    assert_eq!(p["headSource"], "final");
    assert_eq!(p["outcome"], "reported_done");
    assert_eq!(p["label"], "Cline reported done");
    assert!(
        p.get("reason").is_none() && p.get("detail").is_none(),
        "{p}"
    );
    let v: ValidateRequest = serde_json::from_value(p["validate"].clone()).unwrap();
    assert_eq!(v.head_sha, HEAD);
    assert_eq!(v.base_sha.as_deref(), Some(BASE));
    assert_eq!(v.allowed_globs, ["**", "!.git/**"]);
    assert_eq!(v.engine_config, EngineConfigMode::ExecutableOnly);
    assert_eq!(
        v.source_bundle_uri.as_deref(),
        Some(format!("s3://{BUCKET}/runs/{RUN}/agents/direct/turns/1/bundle").as_str())
    );
    let pkg: PackageRequest = serde_json::from_value(p["package"].clone()).unwrap();
    assert_eq!(pkg.repo, "hamh-e2e-python-calc");
    assert_eq!(pkg.head_sha, HEAD);
    assert_eq!(pkg.objective, "Add percent_change(a, b) with tests");
    assert!(pkg.pr_url.is_none());
    let d = pkg.direct;
    assert_eq!(d.label, "Cline reported done");
    assert_eq!(d.head_source, HeadSource::Final);
    assert_eq!(d.model_id, MODEL);
    assert_eq!(d.rules[0].name, "00-my-rules.md");
    assert_eq!(d.cline_message.as_deref(), Some("Added pct(a, b)."));
    let GatewayRequest::PushPr(pr) = serde_json::from_value(p["pushPr"].clone()).unwrap() else {
        panic!("push-pr expected");
    };
    assert_eq!(pr.head_sha, HEAD);
    assert_eq!(pr.title, "ham: Add percent_change(a, b) with tests");
    assert!(
        pr.description.starts_with("Cline reported done\nRun "),
        "{}",
        pr.description
    );
    assert!(pr.description.contains("Wall time 1h 2m 11s."));
    assert!(pr.description.contains("> Added pct(a, b)."));
    assert!(
        pr.description
            .ends_with("The zip carries what Cline reported (EVIDENCE.md).")
    );
    assert!(pr.comments.is_empty());
    assert_eq!(mem.run().phase, GoalPhase::Delivering);
    assert_eq!(mem.event_types(), ["goal.status"]);
}

/// The producer's real output into the reader the head route and the chat client share.
#[tokio::test]
async fn the_evidence_file_holds_clines_message_where_the_head_route_reads_it() {
    let mem = seeded(repo_target());
    mem.put_bundle();
    let message = "Added pct(a, b).\nRan the suite: 3 passed.";
    mem.put_summary(&summary(DirectOutcome::ReportedDone, Some(message), true));
    let p = plan(&mem, done_session()).await;
    assert_eq!(p["deliver"], true);
    let written: Value =
        serde_json::from_slice(&mem.blob(&s3::evidence_key(RUN)).unwrap()).unwrap();
    assert_eq!(
        ham_core::head::cline_message_in_evidence(&written),
        Some(message),
        "the message must stand where the reader looks for it: {written}"
    );
    assert!(
        written.get("clineMessage").is_none(),
        "the envelope has no message of its own, so no reader may look for one there: {written}"
    );
}

#[tokio::test]
async fn the_summary_alone_names_the_head_when_the_turn_output_is_missing() {
    let mem = seeded(repo_target());
    mem.put_bundle();
    mem.put_summary(&summary(DirectOutcome::ReportedDone, Some("m"), true));
    let p = plan(
        &mem,
        json!({"turn": {}, "failure": {"Error": "States.HeartbeatTimeout", "Cause": "x"}}),
    )
    .await;
    assert_eq!(p["headSource"], "final");
    assert_eq!(p["headSha"], HEAD);
    // Cline's own words win over how the state ended
    assert_eq!(p["outcome"], "reported_done");
}

#[tokio::test]
async fn a_lost_session_delivers_its_last_snapshot() {
    let mem = seeded(repo_target());
    let snap = "3333333333333333333333333333333333333333";
    mem.put_snapshot(snap);
    let p = plan(&mem, lost_session()).await;
    assert_eq!(p["deliver"], true);
    assert_eq!(p["headSha"], snap);
    assert_eq!(p["headSource"], "snapshot");
    assert_eq!(p["outcome"], "stopped_session_lost");
    assert_eq!(p["label"], "Cline stopped: the session was lost");
    assert_eq!(
        p["detail"],
        "no heartbeat for 600 s. The work is the last saved snapshot, taken at 2026-10-13 18:50 UTC."
    );
    let v: ValidateRequest = serde_json::from_value(p["validate"].clone()).unwrap();
    assert_eq!(
        v.source_bundle_uri.as_deref(),
        Some(format!("s3://{BUCKET}/runs/{RUN}/agents/direct/turns/1/snapshot.bundle").as_str())
    );
    let pkg: PackageRequest = serde_json::from_value(p["package"].clone()).unwrap();
    let d = pkg.direct;
    assert_eq!(d.head_source, HeadSource::Snapshot);
    assert_eq!(
        d.rules[0].name, "00-my-rules.md",
        "the rules come from the request when no summary exists"
    );
    assert!(d.cline_message.is_none());
}

#[tokio::test]
async fn a_session_that_saved_nothing_delivers_nothing() {
    let mem = seeded(repo_target());
    let p = plan(&mem, lost_session()).await;
    assert_eq!(p["deliver"], false);
    assert_eq!(p["reason"], "the session ended before any work was saved");
    assert_eq!(p["outcome"], "stopped_session_lost");
    assert!(
        p.get("validate").is_none() && p.get("pushPr").is_none() && p.get("package").is_none(),
        "{p}"
    );
    assert_eq!(
        mem.run().phase,
        GoalPhase::Planning,
        "no delivery phase for a run that delivers nothing"
    );
    assert!(mem.events().is_empty());
}

#[tokio::test]
async fn the_state_timeout_and_other_failures_map_to_their_outcomes() {
    for (error, outcome) in [
        ("States.Timeout", "stopped_timeout"),
        ("States.HeartbeatTimeout", "stopped_session_lost"),
        ("RunSessionInvalid", "stopped_session_lost"),
        ("States.TaskFailed", "stopped_session_lost"),
    ] {
        let mem = seeded(repo_target());
        let p = plan(
            &mem,
            json!({"turn": {}, "failure": {"Error": error, "Cause": "c"}}),
        )
        .await;
        assert_eq!(p["outcome"], outcome, "{error}");
    }
    // no failure and no turn output at all
    let mem = seeded(repo_target());
    let p = plan(&mem, json!({"turn": {}, "failure": {}})).await;
    assert_eq!(p["outcome"], "stopped_session_lost");
}

#[tokio::test]
async fn a_bundle_that_is_not_the_pinned_key_or_does_not_exist_is_not_delivered() {
    // the turn names another agent's bundle
    let mem = seeded(repo_target());
    mem.put_bundle();
    let mut out = turn_output(true);
    out["bundleUri"] = json!(format!(
        "s3://{BUCKET}/runs/{RUN}/agents/other/turns/1/bundle"
    ));
    let p = plan(&mem, json!({"turn": out, "failure": {}})).await;
    assert_eq!(p["deliver"], false);
    // the turn names the pinned bundle but the upload never landed: the snapshot is delivered
    let mem = seeded(repo_target());
    mem.put_snapshot(BASE);
    let p = plan(&mem, done_session()).await;
    assert_eq!(p["headSource"], "snapshot");
    // a head that is not a git object id
    let mem = seeded(repo_target());
    mem.put_bundle();
    let mut out = turn_output(true);
    out["lastCommitSha"] = json!("HEAD");
    assert_eq!(
        plan(&mem, json!({"turn": out, "failure": {}})).await["deliver"],
        false
    );
}

#[tokio::test]
async fn a_summary_that_fails_its_own_checks_is_not_believed() {
    let mem = seeded(repo_target());
    mem.put_bundle();
    let mut s = summary(DirectOutcome::ReportedDone, Some("m"), true);
    s.label = "Cline reported done and everything is verified".into();
    mem.put_summary(&s);
    let p = plan(
        &mem,
        json!({"turn": {}, "failure": {"Error": "States.HeartbeatTimeout", "Cause": "x"}}),
    )
    .await;
    assert_eq!(p["outcome"], "stopped_session_lost", "{p}");
    assert_eq!(p["deliver"], false);
}

// ---- plan-delivery: payloads --------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_s3_only_run_and_an_unchanged_repository_open_no_pull_request() {
    let mem = seeded(RunTarget::S3Only);
    mem.put_bundle();
    mem.put_summary(&summary(DirectOutcome::ReportedDone, Some("m"), true));
    let p = plan(&mem, done_session()).await;
    assert!(
        p.get("pushPr").is_none() && p.get("noPrReason").is_none(),
        "{p}"
    );
    assert_eq!(p["package"]["repo"], "s3-only");
    // head == base
    let mem = seeded(repo_target());
    mem.put_bundle();
    let mut out = turn_output(true);
    out["lastCommitSha"] = json!(BASE);
    let mut s = summary(DirectOutcome::ReportedDone, Some("m"), true);
    s.git.head_sha = Some(BASE.into());
    mem.put_summary(&s);
    let p = plan(&mem, json!({"turn": out, "failure": {}})).await;
    assert!(p.get("pushPr").is_none(), "{p}");
    assert_eq!(p["noPrReason"], "Cline left the repository unchanged");
    assert_eq!(p["deliver"], true);
}

#[tokio::test]
async fn the_title_is_the_first_line_cut_and_the_description_cuts_a_long_message() {
    let long_line = format!("\n\n   {}   \nsecond line", "x".repeat(300));
    let mem = Mem::default();
    mem.seed(repo_target(), &long_line, MODEL);
    mem.put_bundle();
    let long_message = "word ".repeat(1_500);
    mem.put_summary(&summary(
        DirectOutcome::ReportedDone,
        Some(&long_message),
        true,
    ));
    let p = plan(&mem, done_session()).await;
    let title = p["pushPr"]["title"].as_str().unwrap();
    assert_eq!(title, format!("ham: {}…", "x".repeat(120)));
    assert!(title.chars().count() <= 150 && !title.starts_with("[goal:"));
    let description = p["pushPr"]["description"].as_str().unwrap();
    assert!(description.chars().count() <= 10_240);
    assert!(
        description.contains("the whole message is in the first comment"),
        "{description}"
    );
    let comments = p["pushPr"]["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 1);
    assert!(comments[0].as_str().unwrap().chars().count() <= 10_240);
    assert!(comments[0].as_str().unwrap().contains("word word word"));
    // the objective is cut for the package
    assert!(p["package"]["objective"].as_str().unwrap().chars().count() <= 8_000);
}

#[tokio::test]
async fn the_pull_request_never_claims_the_harness_checked_the_work() {
    for outcome in DirectOutcome::ALL {
        let mem = seeded(repo_target());
        mem.put_bundle();
        mem.put_summary(&summary(outcome, Some("I changed three files."), true));
        let p = plan(&mem, done_session()).await;
        assert_eq!(p["label"], outcome.label(), "{outcome:?}");
        for field in ["title", "description"] {
            let text = p["pushPr"][field].as_str().unwrap();
            assert_eq!(banned_claim_word(text), None, "{outcome:?} {field}: {text}");
        }
        assert_eq!(banned_claim_word(p["label"].as_str().unwrap()), None);
    }
}

#[tokio::test]
async fn the_decision_is_written_once_and_a_retry_keeps_its_time_and_event_ids() {
    let mem = seeded(repo_target());
    mem.put_bundle();
    mem.put_summary(&summary(DirectOutcome::ReportedDone, Some("m"), true));
    let first = plan(&mem, done_session()).await;
    let key = s3::evidence_key(RUN);
    let file: Value = serde_json::from_slice(&mem.blob(&key).unwrap()).unwrap();
    assert_eq!(file["schemaVersion"], 1);
    assert_eq!(file["outcome"], "reported_done");
    assert_eq!(file["headSource"], "final");
    assert_eq!(file["headSha"], HEAD);
    assert_eq!(file["baseSha"], BASE);
    assert_eq!(file["plannedAtMs"], first["plannedAtMs"]);
    assert_eq!(file["failure"], Value::Null);
    assert_eq!(file["summary"]["outcome"], "reported_done");
    let ids: Vec<String> = mem.envelopes().iter().map(|e| e.event_id.clone()).collect();
    // a retry minutes later
    support::fakes::set_clock(T0 + 600_000);
    mem.clear_events();
    let second = plan(&mem, done_session()).await;
    assert_eq!(second["plannedAtMs"], first["plannedAtMs"]);
    assert_eq!(second, first);
    assert_eq!(
        mem.envelopes()
            .iter()
            .map(|e| e.event_id.clone())
            .collect::<Vec<_>>(),
        ids
    );
}

// ---- finalize ---------------------------------------------------------------------------------------------------------

async fn planned_and_started(target: RunTarget, outcome: DirectOutcome) -> (Mem, Value) {
    let mem = seeded(target);
    mem.put_bundle();
    mem.put_summary(&summary(outcome, Some("Added pct."), true));
    // the session ran: its agent exists, so finalize has a session to stop
    call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
    mem.clear_events();
    let p = plan(&mem, done_session()).await;
    (mem, p)
}

fn packaged(pr: Option<&str>) -> Value {
    let mut v = json!({"artifactId": "zip-0123456789abcdef0123", "sha256": "ab".repeat(32), "sizeBytes": 4096});
    if let Some(u) = pr {
        v["prUrl"] = json!(u);
    }
    v
}

const PR: &str = "https://us-east-1.console.aws.amazon.com/codesuite/codecommit/repositories/hamh-e2e-python-calc/pull-requests/1/details";

async fn finalize(
    mem: &Mem,
    plan: &Value,
    pr_link: Value,
    pr_failure: Value,
) -> Result<Value, RunSessionError> {
    call(
        mem,
        json!({"op": "finalize", "runId": RUN, "plan": plan, "validated": {"pass": true},
            "packaged": packaged(pr_link.get("prUrl").and_then(Value::as_str)), "prLink": pr_link, "prFailure": pr_failure}),
    )
    .await
}

#[tokio::test]
async fn finalize_publishes_the_delivery_events_in_order_and_completes_the_run_with_the_label() {
    let (mem, p) = planned_and_started(repo_target(), DirectOutcome::ReportedDone).await;
    finalize(&mem, &p, json!({"prUrl": PR}), json!({}))
        .await
        .unwrap();
    assert_eq!(
        mem.event_types(),
        [
            "goal.status",
            "delivery.pr",
            "delivery.ready",
            "goal.status",
            "run.completed"
        ],
        "rows 9 to 13, in the order of the table: {:?}",
        mem.events()
    );
    let events = mem.events();
    assert_eq!(
        events[0].1,
        json!({"status": "active", "phase": "delivering"})
    );
    assert_eq!(events[1].1, json!({"url": PR}));
    assert_eq!(events[2].1["artifactId"], "zip-0123456789abcdef0123");
    assert_eq!(events[2].1["prUrl"], PR);
    assert_eq!(
        events[3].1,
        json!({"status": "complete", "phase": "delivering"})
    );
    assert_eq!(
        events[4].1,
        json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"})
    );
    // the ids sort in the order of the table
    let ids: Vec<String> = mem.envelopes().iter().map(|e| e.event_id.clone()).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted);
    assert_eq!(mem.run().status, GoalStatus::Complete);
    assert_eq!(mem.goal().status, GoalStatus::Complete);
    // the session was stopped at the pinned endpoint
    assert_eq!(
        mem.stops(),
        [("v7".to_string(), format!("{RUN}-direct-g1"))]
    );
}

#[tokio::test]
async fn a_run_cline_stopped_halfway_still_completes_and_the_label_says_how_it_stopped() {
    for outcome in DirectOutcome::ALL {
        let (mem, p) = planned_and_started(repo_target(), outcome).await;
        finalize(&mem, &p, json!({"prUrl": PR}), json!({}))
            .await
            .unwrap();
        let completed = mem
            .events()
            .into_iter()
            .find(|(t, _)| t == "run.completed")
            .unwrap()
            .1;
        // A run that stopped at the time limit is paused; a run whose session was lost or whose Cline went silent is stalled. Every
        // other end stays complete, and the label says how Cline stopped in all of them.
        let want = match outcome {
            DirectOutcome::StoppedTimeout => "paused",
            DirectOutcome::StoppedSessionLost | DirectOutcome::StoppedSilence => "stalled",
            _ => "complete",
        };
        assert_eq!(completed["status"], want, "{outcome:?}");
        assert_eq!(completed["outcome"], outcome.as_str());
        assert_eq!(completed["label"], outcome.label());
        assert_eq!(
            banned_claim_word(completed["label"].as_str().unwrap()),
            None
        );
    }
}

#[tokio::test]
async fn a_pull_request_that_failed_does_not_block_the_run() {
    let (mem, p) = planned_and_started(repo_target(), DirectOutcome::ReportedDone).await;
    let failure = json!({"Error": "Refused", "Cause": json!({"errorMessage": "refused: cc:x is outside the hamh-e2e- prefix"}).to_string()});
    finalize(&mem, &p, json!({}), failure).await.unwrap();
    assert_eq!(mem.run().status, GoalStatus::Complete);
    assert!(!mem.event_types().contains(&"delivery.pr".to_string()));
    let completed = mem
        .events()
        .into_iter()
        .find(|(t, _)| t == "run.completed")
        .unwrap()
        .1;
    assert_eq!(
        completed["detail"],
        "pull request not opened: refused: cc:x is outside the hamh-e2e- prefix"
    );
}

#[tokio::test]
async fn an_s3_only_run_completes_without_a_pull_request_event_or_detail() {
    let (mem, p) = planned_and_started(RunTarget::S3Only, DirectOutcome::ReportedDone).await;
    finalize(&mem, &p, json!({}), json!({})).await.unwrap();
    assert_eq!(
        mem.event_types(),
        [
            "goal.status",
            "delivery.ready",
            "goal.status",
            "run.completed"
        ]
    );
    let completed = mem.events().pop().unwrap().1;
    assert!(completed.get("detail").is_none(), "{completed}");
}

#[tokio::test]
async fn finalize_is_safe_to_repeat() {
    let (mem, p) = planned_and_started(repo_target(), DirectOutcome::ReportedDone).await;
    finalize(&mem, &p, json!({"prUrl": PR}), json!({}))
        .await
        .unwrap();
    let events = mem.events();
    let ids: Vec<String> = mem.envelopes().iter().map(|e| e.event_id.clone()).collect();
    finalize(&mem, &p, json!({"prUrl": PR}), json!({}))
        .await
        .unwrap();
    assert_eq!(mem.events(), events);
    assert_eq!(
        mem.envelopes()
            .iter()
            .map(|e| e.event_id.clone())
            .collect::<Vec<_>>(),
        ids
    );
    assert_eq!(mem.run().status, GoalStatus::Complete);
    // a crash after the status change and before run.completed: the retry writes the missing event
    let (mem, p) = planned_and_started(repo_target(), DirectOutcome::ReportedDone).await;
    // delivery.pr and delivery.ready are written, the status moves, and run.completed fails
    mem.pass("put_event");
    mem.pass("put_event");
    mem.fail(
        "put_event",
        RunSessionError::Unavailable("throttled".into()),
    );
    finalize(&mem, &p, json!({"prUrl": PR}), json!({}))
        .await
        .unwrap_err();
    assert_eq!(mem.run().status, GoalStatus::Complete);
    assert!(!mem.event_types().contains(&"run.completed".to_string()));
    finalize(&mem, &p, json!({"prUrl": PR}), json!({}))
        .await
        .unwrap();
    assert!(mem.event_types().contains(&"run.completed".to_string()));
    assert_eq!(mem.run().status, GoalStatus::Complete);
}

#[tokio::test]
async fn a_discarded_run_stays_cleared_and_publishes_nothing() {
    let (mem, p) = planned_and_started(repo_target(), DirectOutcome::ReportedDone).await;
    mem.set_run_status_raw(GoalStatus::Cleared);
    mem.clear_events();
    let out = finalize(&mem, &p, json!({"prUrl": PR}), json!({}))
        .await
        .unwrap();
    assert_eq!(out["status"], "cleared");
    assert!(mem.events().is_empty());
    assert_eq!(mem.run().status, GoalStatus::Cleared);
    assert_eq!(mem.stops().len(), 1);
}

#[tokio::test]
async fn a_lost_status_race_is_retryable_and_a_failing_stop_fails_the_op_without_undoing_the_run() {
    let (mem, p) = planned_and_started(repo_target(), DirectOutcome::ReportedDone).await;
    // another writer moved the run to paused between our read and our write
    mem.set_run_status_raw(GoalStatus::Paused);
    mem.0.runs.lock().unwrap().get_mut(RUN).unwrap().status = GoalStatus::Active;
    mem.fail("set_run_status", RunSessionError::Unavailable("x".into()));
    let e = finalize(&mem, &p, json!({}), json!({})).await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionUnavailable");
    // stop fails: the delivery stands, but the op fails so the machine retries the stop (review S2: a session that could
    // not be stopped is still running, with live credentials)
    let (mem, p) = planned_and_started(repo_target(), DirectOutcome::ReportedDone).await;
    mem.fail(
        "stop",
        RunSessionError::Unavailable("AgentCore down".into()),
    );
    let e = finalize(&mem, &p, json!({}), json!({})).await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionUnavailable");
    assert_eq!(mem.run().status, GoalStatus::Complete);
    finalize(&mem, &p, json!({}), json!({})).await.unwrap();
    assert_eq!(mem.stops().len(), 1);
}

// ---- mark-blocked -----------------------------------------------------------------------------------------------------

async fn blocked(mem: &Mem, state: Value) -> Value {
    call(
        mem,
        json!({"op": "mark-blocked", "runId": RUN, "state": state}),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn mark_blocked_names_the_first_reason_that_applies() {
    let failure = json!({"Error": "RunSessionInvalid", "Cause": json!({"errorMessage": "model x is not supported"}).to_string()});
    let violations = json!({"pass": false, "violations": [
        {"rule": "workflow-file", "path": ".github/workflows/ci.yml", "detail": "a workflow file"},
        {"rule": "bundle", "detail": "no source bundle"}]});
    let cases = [
        // the failed step's own error comes first
        (
            json!({"failure": failure, "validated": {"result": violations}}),
            "model x is not supported",
        ),
        // then the validation's violations, even after a lost session
        (
            json!({"validated": {"result": violations}, "session": {"failure": {"Error": "States.HeartbeatTimeout"}}}),
            "bundle validation failed: workflow-file .github/workflows/ci.yml: a workflow file; bundle: no source bundle",
        ),
        // then why nothing could be delivered, with how the session failed
        (
            json!({"plan": {"result": {"reason": "the session ended before any work was saved"}},
                "session": {"failure": {"Error": "States.HeartbeatTimeout", "Cause": "no heartbeat"}}}),
            "the session ended before any work was saved (no heartbeat)",
        ),
        (
            json!({"session": {"failure": {"Error": "States.Timeout"}}}),
            "States.Timeout",
        ),
        (
            json!({"session": {"turn": {}, "failure": {}}}),
            "the run could not be delivered",
        ),
    ];
    for (state, want) in cases {
        let mem = seeded(repo_target());
        let out = blocked(&mem, state.clone()).await;
        assert_eq!(out["status"], "blocked", "{state}");
        assert_eq!(out["reason"], want, "{state}");
    }
}

#[tokio::test]
async fn mark_blocked_writes_the_blocked_events_and_stops_the_session() {
    let (mem, p) = planned_and_started(repo_target(), DirectOutcome::StoppedSilence).await;
    let p = json!({"result": p});
    mem.clear_events();
    blocked(
        &mem,
        json!({"plan": p, "failure": {"Error": "Invalid", "Cause": "package refused"}}),
    )
    .await;
    assert_eq!(
        mem.event_types(),
        ["goal.blocked", "goal.status", "run.completed"]
    );
    let events = mem.events();
    assert_eq!(events[0].1["reason"], "package refused");
    assert_eq!(
        events[1].1,
        json!({"status": "blocked", "phase": "delivering"})
    );
    assert_eq!(events[2].1["status"], "blocked");
    assert_eq!(events[2].1["outcome"], "stopped_silence");
    assert_eq!(
        events[2].1["label"],
        "Cline stopped: no output for the silence limit"
    );
    assert_eq!(mem.run().status, GoalStatus::Blocked);
    assert_eq!(mem.goal().status, GoalStatus::Blocked);
    assert_eq!(mem.stops().len(), 1);
    // safe to repeat
    let again = mem.events();
    blocked(
        &mem,
        json!({"plan": p, "failure": {"Error": "Invalid", "Cause": "package refused"}}),
    )
    .await;
    assert_eq!(mem.events(), again);
}

#[tokio::test]
async fn a_late_failure_does_not_turn_a_finished_or_discarded_run_into_a_blocked_one() {
    let (mem, p) = planned_and_started(repo_target(), DirectOutcome::ReportedDone).await;
    finalize(&mem, &p, json!({}), json!({})).await.unwrap();
    let events = mem.events();
    let out = blocked(&mem, json!({"failure": {"Error": "X", "Cause": "late"}})).await;
    assert_eq!(out["status"], "complete");
    assert_eq!(mem.events(), events);
    assert_eq!(mem.run().status, GoalStatus::Complete);
    let mem = seeded(repo_target());
    mem.set_run_status_raw(GoalStatus::Cleared);
    assert_eq!(blocked(&mem, json!({})).await["status"], "cleared");
    assert_eq!(mem.run().status, GoalStatus::Cleared);
}

// ---- run-ended --------------------------------------------------------------------------------------------------------

fn status_change(status: &str, name: &str) -> Value {
    json!({"source": "aws.states", "detail-type": "Step Functions Execution Status Change",
        "detail": {"status": status, "name": name, "stateMachineArn": MACHINE,
            "executionArn": format!("{}:{name}", MACHINE.replace(":stateMachine:", ":execution:")),
            "stopDate": 1_790_000_100_000u64, "error": "States.Runtime", "cause": "An error\noccurred"}})
}

#[test]
fn only_an_unfinished_end_of_the_direct_run_machine_is_a_run_end() {
    for status in ["ABORTED", "FAILED", "TIMED_OUT"] {
        let end = parse_run_end(&status_change(status, RUN), MACHINE).unwrap();
        assert_eq!(end.run_id, RUN);
        assert_eq!(end.stopped_at_ms, Some(1_790_000_100_000));
    }
    for (v, want) in [
        (status_change("SUCCEEDED", RUN), "only ABORTED"),
        (status_change("RUNNING", RUN), "only ABORTED"),
        (status_change("ABORTED", "not-a-run"), "not a run id"),
        (
            json!({"source": "aws.events", "detail": {}}),
            "not a Step Functions",
        ),
        (json!({"op": "prepare"}), "not a Step Functions"),
    ] {
        let e = parse_run_end(&v, MACHINE).unwrap_err();
        assert_eq!(e.error_type(), "RunSessionInvalid");
        assert!(e.to_string().contains(want), "{e}");
    }
    let mut other = status_change("ABORTED", RUN);
    other["detail"]["stateMachineArn"] =
        json!("arn:aws:states:us-east-1:700000000011:stateMachine:hamh-live-run");
    assert!(
        parse_run_end(&other, MACHINE)
            .unwrap_err()
            .to_string()
            .contains("not one of the run machine")
    );
}

#[tokio::test]
async fn an_aborted_execution_clears_the_run_records_the_end_and_stops_the_session() {
    let (mem, _p) = planned_and_started(repo_target(), DirectOutcome::ReportedDone).await;
    mem.clear_events();
    let end = parse_run_end(&status_change("ABORTED", RUN), MACHINE).unwrap();
    let r = reap(&mem.deps(), &end).await.unwrap();
    assert!(r.found && r.cleared && r.session_stopped);
    let run = mem.run();
    assert_eq!(run.status, GoalStatus::Cleared);
    assert_eq!(mem.goal().status, GoalStatus::Cleared);
    let aborted = run.aborted.unwrap();
    assert_eq!(aborted.error.as_deref(), Some("States.Runtime"));
    assert_eq!(
        aborted.cause.as_deref(),
        Some("An error occurred"),
        "single-line"
    );
    assert_eq!(mem.event_types(), ["goal.status", "run.aborted"]);
    assert_eq!(
        mem.stops(),
        [("v7".to_string(), format!("{RUN}-direct-g1"))]
    );
    // a second delivery of the event changes nothing but stops the session again
    let events = mem.events();
    let again = reap(&mem.deps(), &end).await.unwrap();
    assert!(again.found && !again.cleared);
    assert_eq!(mem.events(), events);
}

#[tokio::test]
async fn a_finished_run_is_not_touched_and_an_unknown_run_is_not_found() {
    let (mem, p) = planned_and_started(repo_target(), DirectOutcome::ReportedDone).await;
    finalize(&mem, &p, json!({}), json!({})).await.unwrap();
    let end = parse_run_end(&status_change("FAILED", RUN), MACHINE).unwrap();
    let r = reap(&mem.deps(), &end).await.unwrap();
    assert!(r.found && !r.cleared);
    assert_eq!(mem.run().status, GoalStatus::Complete);
    let r = reap(&Mem::default().deps(), &end).await.unwrap();
    assert!(!r.found && !r.cleared);
}

#[tokio::test]
async fn a_lost_race_is_retried_and_a_stop_failure_fails_the_invocation_so_the_event_is_retried() {
    let (mem, _p) = planned_and_started(repo_target(), DirectOutcome::ReportedDone).await;
    *mem.0.abort_races.lock().unwrap() = 2;
    let end = parse_run_end(&status_change("TIMED_OUT", RUN), MACHINE).unwrap();
    assert!(reap(&mem.deps(), &end).await.unwrap().cleared);
    let (mem, _p) = planned_and_started(repo_target(), DirectOutcome::ReportedDone).await;
    *mem.0.abort_races.lock().unwrap() = 99;
    let e = reap(&mem.deps(), &end).await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionUnavailable");
    let (mem, _p) = planned_and_started(repo_target(), DirectOutcome::ReportedDone).await;
    mem.fail(
        "stop",
        RunSessionError::Unavailable("AgentCore down".into()),
    );
    assert!(reap(&mem.deps(), &end).await.is_err());
    assert_eq!(
        mem.run().status,
        GoalStatus::Cleared,
        "the run is cleared even when the stop fails; the retry stops"
    );
}
