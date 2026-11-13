//! `start-session` under retries (review of the run machine, S1). The Step Functions state is retried after any
//! failure of the op, including one that happens after the microVM accepted the invocation. A retry must neither give the
//! running agentd a second nonce nor overwrite what worker-api recorded on the Agent item since.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::direct::DirectInvocationRequest;
use harness_protocol::store::{AgentStatus, CredsBinding};
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

async fn start(mem: &Mem, token: &str) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(
        json!({"op": "start-session", "runId": RUN, "taskToken": token, "staged": staged()}),
    )
    .unwrap();
    handle(&mem.deps(), &op).await
}

fn seeded() -> Mem {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    mem
}

fn sent(mem: &Mem, i: usize) -> DirectInvocationRequest {
    serde_json::from_value(mem.invocations()[i].clone()).unwrap()
}

/// The hash the Agent item holds for the nonce the running agentd was given.
fn bound(mem: &Mem) -> String {
    mem.agent().unwrap().creds.unwrap().nonce_sha256
}

const NONCE_KEY: &str = "runs/01M4X3Q7R9S0V1W2X3Y4Z5A6B7/agents/direct/nonce.enc";

fn runtime_409() -> RunSessionError {
    RunSessionError::Conflict(
        "InvokeAgentRuntime: Received error (409) from runtime: a turn is already running in this session".into(),
    )
}

#[tokio::test]
async fn the_nonce_is_made_once_per_run_and_kept_encrypted_under_its_own_context() {
    let mem = seeded();
    start(&mem, "token-A").await.unwrap();
    let ct =
        String::from_utf8(mem.blob(NONCE_KEY).expect("the nonce ciphertext is stored")).unwrap();
    let nonce = sent(&mem, 0).nonce;
    assert_eq!(
        ct,
        format!("ct({nonce};{RUN};direct-nonce;1)"),
        "encrypted under an agent context of its own, so a task-token ciphertext cannot be swapped for it"
    );
    assert_eq!(bound(&mem), CredsBinding::hash_nonce(&nonce));
    // only the hash is on the Agent item
    let item = serde_json::to_string(&mem.agent().unwrap()).unwrap();
    assert!(!item.contains(&nonce), "{item}");
}

#[tokio::test]
async fn a_retry_after_a_failed_invoke_sends_the_nonce_the_first_attempt_bound() {
    let mem = seeded();
    mem.fail(
        "invoke",
        RunSessionError::Unavailable("AgentCore 503".into()),
    );
    start(&mem, "token-A").await.unwrap_err();
    let first = bound(&mem);
    start(&mem, "token-B").await.unwrap();
    assert_eq!(mem.invocations().len(), 1);
    assert_eq!(
        CredsBinding::hash_nonce(&sent(&mem, 0).nonce),
        first,
        "the retry invokes with the nonce the Agent item already holds"
    );
    assert_eq!(bound(&mem), first);
}

#[tokio::test]
async fn a_retry_after_a_lost_ack_leaves_the_running_agentd_its_nonce() {
    let mem = seeded();
    // the microVM accepted the invocation, then recording it failed (twice: the op retries once in process)
    mem.fail(
        "mark_turn_invoked",
        RunSessionError::Unavailable("x".into()),
    );
    mem.fail(
        "mark_turn_invoked",
        RunSessionError::Unavailable("x".into()),
    );
    start(&mem, "token-A").await.unwrap_err();
    assert_eq!(mem.invocations().len(), 1);
    assert!(mem.turn().unwrap().invoked_at.is_none());
    let agent = mem.agent().unwrap();
    // the state is retried; agentd refuses the second invocation of its running turn
    mem.fail("invoke", runtime_409());
    mem.clear_events();
    let ack = start(&mem, "token-B").await.unwrap();
    assert_eq!(ack["alreadyInvoked"], true);
    assert_eq!(mem.agent().unwrap(), agent, "no new nonce, no rebinding");
    assert_eq!(bound(&mem), CredsBinding::hash_nonce(&sent(&mem, 0).nonce));
    assert!(mem.turn().unwrap().invoked_at.is_some());
    assert_eq!(
        mem.turn().unwrap().task_token_ciphertext,
        format!("ct(token-B;{RUN};direct;1)")
    );
    assert_eq!(mem.event_types(), ["goal.status", "session.started"]);
    assert_eq!(mem.invocations().len(), 1);
}

#[tokio::test]
async fn only_a_conflict_from_the_runtime_counts_as_accepted() {
    for (err, ok) in [
        (runtime_409(), true),
        (RunSessionError::Unavailable("AgentCore 503".into()), false),
        (
            RunSessionError::Invalid("Received error (400) from runtime".into()),
            false,
        ),
    ] {
        let mem = seeded();
        mem.fail("invoke", err.clone());
        let r = start(&mem, "t").await;
        assert_eq!(r.is_ok(), ok, "{err:?}");
        assert_eq!(mem.turn().unwrap().invoked_at.is_some(), ok, "{err:?}");
    }
}

#[tokio::test]
async fn the_agent_item_is_bound_once_and_a_retry_keeps_what_worker_api_recorded() {
    let mem = seeded();
    mem.fail(
        "invoke",
        RunSessionError::Unavailable("AgentCore 503".into()),
    );
    start(&mem, "token-A").await.unwrap_err();
    {
        let mut agents = mem.0.agents.lock().unwrap();
        let a = agents.get_mut("direct").unwrap();
        a.status = AgentStatus::Running;
        a.heartbeat_at = Some("2026-10-13T18:05:00Z".into());
    }
    start(&mem, "token-B").await.unwrap();
    let a = mem.agent().unwrap();
    assert_eq!(a.status, AgentStatus::Running);
    assert_eq!(a.heartbeat_at.as_deref(), Some("2026-10-13T18:05:00Z"));
}

#[tokio::test]
async fn the_acknowledgement_and_the_announcement_are_retried_once_in_process() {
    let mem = seeded();
    mem.fail(
        "mark_turn_invoked",
        RunSessionError::Unavailable("x".into()),
    );
    start(&mem, "t").await.unwrap();
    assert!(mem.turn().unwrap().invoked_at.is_some());
    assert_eq!(mem.invocations().len(), 1);
    let mem = seeded();
    mem.fail(
        "put_event",
        RunSessionError::Unavailable("throttled".into()),
    );
    start(&mem, "t").await.unwrap();
    assert_eq!(mem.event_types(), ["goal.status", "session.started"]);
    assert_eq!(mem.invocations().len(), 1);
}

#[tokio::test]
async fn a_nonce_another_attempt_stored_first_is_the_one_used() {
    let mem = seeded();
    // a concurrent attempt wrote its nonce between our read and our write
    mem.put_blob(
        NONCE_KEY,
        format!("ct(other-nonce;{RUN};direct-nonce;1)").into_bytes(),
    );
    start(&mem, "t").await.unwrap();
    assert_eq!(sent(&mem, 0).nonce, "other-nonce");
    assert_eq!(bound(&mem), CredsBinding::hash_nonce("other-nonce"));
}

#[tokio::test]
async fn an_attempt_that_loses_the_create_only_write_uses_the_winners_nonce() {
    let mem = seeded();
    *mem.0.blob_race.lock().unwrap() = Some((
        NONCE_KEY.to_string(),
        format!("ct(winner-nonce;{RUN};direct-nonce;1)").into_bytes(),
    ));
    start(&mem, "t").await.unwrap();
    assert_eq!(sent(&mem, 0).nonce, "winner-nonce");
    assert_eq!(bound(&mem), CredsBinding::hash_nonce("winner-nonce"));
    assert_eq!(
        mem.blob(NONCE_KEY).unwrap(),
        format!("ct(winner-nonce;{RUN};direct-nonce;1)").into_bytes(),
        "the stored nonce is the winner's"
    );
}

#[tokio::test]
async fn a_stored_nonce_that_will_not_decrypt_stops_the_start_and_is_never_replaced() {
    let mem = seeded();
    mem.put_blob(NONCE_KEY, b"ct(x;another-run;direct-nonce;1)".to_vec());
    let e = start(&mem, "t").await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionInvalid");
    assert!(mem.invocations().is_empty());
    assert_eq!(
        mem.blob(NONCE_KEY).unwrap(),
        b"ct(x;another-run;direct-nonce;1)".to_vec(),
        "a stored nonce is never overwritten"
    );
}

#[tokio::test]
async fn a_pinned_run_does_not_ask_agentcore_for_its_endpoints() {
    // the Run item holds the pin: a retry reads it from there (an unavailable control plane cannot fail it)
    let mem = seeded();
    mem.fail(
        "invoke",
        RunSessionError::Unavailable("AgentCore 503".into()),
    );
    start(&mem, "token-A").await.unwrap_err();
    assert_eq!(mem.run().runtime_endpoint.as_deref(), Some("v7"));
    mem.0.endpoints.lock().unwrap().clear();
    let ack = start(&mem, "token-B").await.unwrap();
    assert_eq!(ack["qualifier"], "v7");
}
