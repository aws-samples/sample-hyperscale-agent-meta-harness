//! The scripted mandated path of the agentd entry point, without AWS: the real `agentd serve` process answers `POST
//! /invocations` with a direct turn, drives the fake ACP engine, talks to a fake worker-api (the internal API) and a fake S3,
//! and leaves exactly what run-session reads: the events in order, `summary.json`, the bundle, the turn-complete and
//! `result.json`.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use common::git;
use direct_support::{BUCKET, Direct, MODEL, Opts, PROMPT, RUN, no_secrets, sha};
use harness_protocol::direct::{DirectOutcome, s3 as keys};
use harness_protocol::envelope::EventType;
use harness_protocol::runtime::TurnOutcome;

#[tokio::test(flavor = "multi_thread")]
async fn the_agentd_binary_runs_a_direct_turn_from_the_invocation_to_the_result() {
    let d = Direct::start_process(Opts::default()).await;
    let (ack, resp) = d.invoke().await;
    assert_eq!(
        (
            ack.run_id.as_str(),
            ack.agent_id.as_str(),
            ack.turn,
            ack.accepted
        ),
        (RUN, "direct", 1, true)
    );
    d.wait_healthy().await;

    // Events: turn.started (with the rules digest) first, the engine's calls, turn.ended last with the label.
    let events = d.aws.events();
    assert_eq!(events.first().unwrap().0, EventType::TurnStarted);
    assert!(events[0].1["rulesDigest"].as_str().is_some());
    assert_eq!(events.last().unwrap().0, EventType::TurnEnded);
    let kinds: Vec<EventType> = events.iter().map(|(t, _)| *t).collect();
    let pos = |t: EventType| kinds.iter().position(|k| *k == t).unwrap();
    assert!(pos(EventType::TurnStarted) < pos(EventType::ToolSummary));
    assert!(pos(EventType::ToolSummary) < pos(EventType::TurnEnded));
    let ended = d.events_of(EventType::TurnEnded);
    assert_eq!(ended[0]["directOutcome"], "reported_done");
    assert_eq!(ended[0]["label"], "Cline reported done");

    // summary.json is what run-session reads: the label, the heads, the rules, the request.
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    assert_eq!(s.label, "Cline reported done");
    assert_eq!(s.request.sha256, sha(&d.request_json));
    assert_eq!(s.model.pinned.as_deref(), Some(MODEL));
    assert!(s.rules_preflight.ran);
    assert!(s.git.committed_by_harness);
    let bundle = d.bundle_file().expect("the bundle");
    let wt = d.worktree();
    git(&wt, &["bundle", "verify", bundle.to_str().unwrap()]);
    assert!(
        git(&wt, &["bundle", "list-heads", bundle.to_str().unwrap()])
            .contains(&format!("refs/heads/harness/{RUN}/direct"))
    );

    // The turn-complete and the result agree with the summary.
    let complete = d.aws.complete();
    assert_eq!(complete.len(), 1);
    assert_eq!(
        complete[0].bundle_uri.as_deref(),
        Some(keys::uri(BUCKET, &keys::bundle_key(RUN)).as_str())
    );
    assert_eq!(complete[0].last_commit_sha, s.git.head_sha);
    assert_eq!(resp.outcome, TurnOutcome::Completed);
    assert_eq!(resp.last_commit_sha, s.git.head_sha);

    // The prompt reached the engine unchanged, once.
    assert_eq!(d.prompts_received(), vec![PROMPT.to_string()]);

    // The process's own log names the run and holds no secret.
    let log = std::fs::read_to_string(&d.process.as_ref().unwrap().log_file).unwrap();
    no_secrets(&log);
    assert!(log.contains("agentd: listening on"), "{log}");
    assert!(
        log.contains(&format!("agentd: direct turn started: run={RUN}")),
        "{log}"
    );
    assert!(
        log.contains(&format!(
            "agentd: direct turn ended: run={RUN} outcome=reported_done"
        )),
        "{log}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agentd_binary_ends_a_run_it_cannot_start_with_a_summary_and_a_turn_complete() {
    let d = Direct::start_process(Opts {
        tamper_request: true,
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    d.wait_healthy().await;
    assert_eq!(resp.outcome, TurnOutcome::Failed);
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed);
    assert_eq!(s.label, "Cline did not start");
    assert_eq!(s.detail.as_deref(), Some("request.json sha256 differs"));
    assert_eq!(d.aws.complete().len(), 1);
    assert_eq!(d.aws.complete()[0].bundle_uri, None);
    assert!(d.prompts_received().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agentd_binary_refuses_a_second_direct_turn_while_one_runs_and_serves_ping() {
    let d = Direct::start_process(Opts {
        scenario: "end_turn_done",
        engine_env: vec![("FAKE_ACP_HOLD_PROMPTS", "1".into())],
        timeout_s: 6,
        ..Opts::default()
    })
    .await;
    let (code, _) = d.post().await;
    assert_eq!(code, 200);
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(
        d.ping().await,
        harness_protocol::runtime::PingStatus::HealthyBusy
    );
    assert_eq!(d.post().await.0, 409);
    let resp = d.wait_result().await;
    assert_eq!(resp.outcome, TurnOutcome::TimedOut);
    assert_eq!(d.summary().outcome, DirectOutcome::StoppedTimeout);
}
