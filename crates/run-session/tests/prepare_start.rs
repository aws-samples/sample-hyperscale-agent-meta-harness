//! `prepare` and `start-session`, run over the in-memory ports.
#![allow(clippy::unwrap_used)]

mod support;

use delivery::gateway::GatewayRequest;
use harness_protocol::direct::{DirectInvocationRequest, validate_direct_invocation};
use harness_protocol::goal::{GoalPhase, GoalStatus};
use harness_protocol::target::RunTarget;
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use run_session::start_session::session_timeout_s;
use serde_json::{Value, json};
use support::fakes::*;

async fn call(mem: &Mem, v: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(v).unwrap();
    handle(&mem.deps(), &op).await
}

async fn prepare(mem: &Mem) -> Result<Value, RunSessionError> {
    call(mem, json!({"op": "prepare", "runId": RUN})).await
}

async fn start(mem: &Mem, token: &str) -> Result<Value, RunSessionError> {
    call(
        mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": token, "staged": staged()}),
    )
    .await
}

fn new_repo_target() -> RunTarget {
    serde_json::from_value(json!({"mode": "new-repo", "repo": "cc:hamh-e2e-new-20261013"})).unwrap()
}

// ---- prepare ----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn prepare_builds_the_stage_payload_the_gateway_parses() {
    let mem = Mem::default();
    mem.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
    let out = prepare(&mem).await.unwrap();
    assert!(out.get("newRepo").is_none(), "{out}");
    assert_eq!(out["repo"], "hamh-e2e-python-calc");
    assert_eq!(out["stage"]["operation"], "stage");
    assert_eq!(out["stage"]["runId"], RUN);
    assert_eq!(out["stage"]["target"], "cc:hamh-e2e-python-calc");
    assert!(out["stage"].get("baseRef").is_none());
    serde_json::from_value::<GatewayRequest>(out["stage"].clone()).unwrap();
    assert_eq!(
        out["target"],
        json!({"mode": "repo", "repo": "cc:hamh-e2e-python-calc"})
    );
    assert_eq!(out["model"]["id"], MODEL);
    assert_eq!(out["model"]["region"], "us-east-1");
    assert_eq!(out["model"]["protocol"], "converse");
    assert_eq!(out["model"]["contextTokens"], 1_000_000);
    assert_eq!(out["model"]["maxOutputTokens"], 128_000);
}

#[tokio::test]
async fn prepare_passes_the_base_ref_on() {
    let mem = Mem::default();
    mem.seed_with(repo_target(), "p", MODEL, Some("release"));
    let out = prepare(&mem).await.unwrap();
    assert_eq!(out["stage"]["baseRef"], "release");
    serde_json::from_value::<GatewayRequest>(out["stage"].clone()).unwrap();
}

#[tokio::test]
async fn prepare_falls_back_to_the_run_items_base_ref_when_the_request_has_none() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    mem.0.runs.lock().unwrap().get_mut(RUN).unwrap().base_ref = Some("release/1.2".into());
    let v = prepare(&mem).await.unwrap();
    assert_eq!(v["stage"]["baseRef"], "release/1.2");
    // the request's own base ref wins
    let mem = Mem::default();
    mem.seed_with(repo_target(), "p", MODEL, Some("main"));
    mem.0.runs.lock().unwrap().get_mut(RUN).unwrap().base_ref = Some("release/1.2".into());
    assert_eq!(prepare(&mem).await.unwrap()["stage"]["baseRef"], "main");
}

#[tokio::test]
async fn prepare_for_a_new_repository_adds_the_new_repo_payload() {
    let mem = Mem::default();
    mem.seed(new_repo_target(), "p", MODEL);
    let out = prepare(&mem).await.unwrap();
    assert_eq!(out["newRepo"]["operation"], "new-repo");
    assert_eq!(out["newRepo"]["target"], "cc:hamh-e2e-new-20261013");
    assert_eq!(out["repo"], "hamh-e2e-new-20261013");
    serde_json::from_value::<GatewayRequest>(out["newRepo"].clone()).unwrap();
    serde_json::from_value::<GatewayRequest>(out["stage"].clone()).unwrap();
}

#[tokio::test]
async fn prepare_for_s3_only_stages_the_empty_base() {
    let mem = Mem::default();
    mem.seed(RunTarget::S3Only, "p", MODEL);
    let out = prepare(&mem).await.unwrap();
    assert_eq!(out["stage"]["target"], json!({"mode": "s3-only"}));
    assert_eq!(out["repo"], "s3-only");
    serde_json::from_value::<GatewayRequest>(out["stage"].clone()).unwrap();
}

#[tokio::test]
async fn prepare_refuses_what_can_never_run_with_the_no_retry_error() {
    // a model that is not in models.json
    let mem = Mem::default();
    mem.seed(repo_target(), "p", "us.mistral.large-9");
    let e = prepare(&mem).await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionInvalid");
    assert!(e.to_string().contains("us.mistral.large-9"), "{e}");
    // a run that is not active
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    mem.set_run_status_raw(GoalStatus::Cleared);
    let e = prepare(&mem).await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionInvalid");
    assert!(e.to_string().contains("cleared"), "{e}");
    // a run with no Run item
    let e = prepare(&Mem::default()).await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionInvalid");
    // a request.json whose bytes differ from the run.created record
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    let mut body = mem
        .blob(&harness_protocol::direct::s3::request_key(RUN))
        .unwrap();
    body.push(b' ');
    mem.put_blob(&harness_protocol::direct::s3::request_key(RUN), body);
    let e = prepare(&mem).await.unwrap_err();
    assert!(e.to_string().contains("run.created"), "{e}");
    // a prompt the Run item does not hold
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    mem.0
        .runs
        .lock()
        .unwrap()
        .get_mut(RUN)
        .unwrap()
        .prompt_sha256 = "0".repeat(64);
    assert!(
        prepare(&mem)
            .await
            .unwrap_err()
            .to_string()
            .contains("prompt")
    );
    // a run id that is not a ULID
    let e = call(&mem, json!({"op": "prepare", "runId": "not-a-run"}))
        .await
        .unwrap_err();
    assert_eq!(e.error_type(), "RunSessionInvalid");
}

#[tokio::test]
async fn prepare_has_no_model_outside_the_five() {
    for id in ham_core::models::builtin()
        .models()
        .iter()
        .filter(|m| m.selectable())
        .map(|m| m.id.clone())
    {
        let mem = Mem::default();
        mem.seed(repo_target(), "p", &id);
        assert_eq!(prepare(&mem).await.unwrap()["model"]["id"], id.as_str());
    }
}

// ---- start-session ----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn start_session_invokes_the_microvm_with_a_valid_direct_request() {
    let mem = Mem::default();
    mem.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
    let ack = start(&mem, "token-A").await.unwrap();
    assert_eq!(ack["runtimeSessionId"], format!("{RUN}-direct-g1"));
    assert_eq!(ack["qualifier"], "v7");
    assert_eq!(ack["alreadyInvoked"], false);
    let sent = mem.invocations();
    assert_eq!(sent.len(), 1);
    let req: DirectInvocationRequest = serde_json::from_value(sent[0].clone()).unwrap();
    validate_direct_invocation(&req).unwrap();
    assert_eq!(req.run_id, RUN);
    assert_eq!(req.agent_id, "direct");
    assert_eq!(req.turn, 1);
    assert_eq!(req.runtime_session_id, format!("{RUN}-direct-g1"));
    assert_eq!(req.engine, "cline");
    assert_eq!(req.model.id, MODEL);
    assert_eq!(req.model.region, "us-east-1");
    assert_eq!(req.timeout_s, 27_900);
    assert_eq!(req.snapshot_every_s, Some(60));
    assert_eq!(req.provider_silence_s, None);
    assert_eq!(req.region, "us-east-1");
    assert_eq!(
        req.internal_api_url,
        "https://abc.execute-api.us-east-1.amazonaws.com/direct"
    );
    assert_eq!(
        req.request.uri,
        format!("s3://{BUCKET}/runs/{RUN}/input/request.json")
    );
    let stored = mem.blob(&format!("runs/{RUN}/input/request.json")).unwrap();
    assert_eq!(req.request.sha256, hex(&stored));
    assert_eq!(
        serde_json::to_value(&req.repo).unwrap(),
        json!({"kind": "s3Bundle", "uri": format!("s3://{BUCKET}/runs/{RUN}/input/repo.bundle")})
    );
    assert!(req.credentials.access_key_id.starts_with("ASIA"));
    assert_eq!(
        req.packages_token.as_ref().map(|t| t.expose()),
        Some("tok.en-1234567890")
    );
    assert!(req.packages_token_expires_at.unwrap() > T0 / 1000 + 27_900);
    // the body never carries the prompt or the rules
    let text = serde_json::to_string(&sent[0]).unwrap();
    assert!(
        !text.contains("percent_change") && !text.contains("Write the test first"),
        "{text}"
    );
    assert!(text.len() < 64 * 1024);
}

#[tokio::test]
async fn start_session_records_the_agent_the_turn_the_pin_and_the_start_events() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    start(&mem, "token-A").await.unwrap();
    let agent = mem.agent().unwrap();
    assert_eq!(agent.agent_id, "direct");
    assert_eq!(agent.task_id, "root");
    assert_eq!(agent.pool, "direct");
    assert_eq!(agent.attempt, 1);
    assert_eq!(agent.generation, 1);
    assert_eq!(agent.runtime_arn, RUNTIME);
    assert_eq!(agent.runtime_session_id, format!("{RUN}-direct-g1"));
    assert_eq!(agent.runtime_endpoint.as_deref(), Some("v7"));
    assert_eq!(agent.runtime_version.as_deref(), Some("7"));
    assert_eq!(agent.model_id, MODEL);
    assert!(!agent.engine_version.is_empty());
    let creds = agent.creds.as_ref().unwrap();
    assert_eq!(creds.tenant, "testuser");
    assert_eq!(creds.mode, "repo");
    // the Agent item holds the nonce's hash and the invocation the nonce itself
    let sent: DirectInvocationRequest =
        serde_json::from_value(mem.invocations()[0].clone()).unwrap();
    assert_eq!(
        creds.nonce_sha256,
        harness_protocol::store::CredsBinding::hash_nonce(&sent.nonce)
    );
    let turn = mem.turn().unwrap();
    assert_eq!(turn.n, 1);
    assert_eq!(turn.role.as_deref(), Some("direct"));
    assert_eq!(turn.base_sha.as_deref(), Some(BASE));
    assert!(turn.invoked_at.is_some());
    assert_eq!(turn.runtime_endpoint.as_deref(), Some("v7"));
    assert_eq!(turn.max_output_tokens, Some(128_000));
    assert_eq!(turn.context_tokens, Some(1_000_000));
    assert_eq!(turn.started_at_ms, Some(T0));
    assert_eq!(
        turn.task_token_ciphertext,
        format!("ct(token-A;{RUN};direct;1)")
    );
    assert_eq!(mem.run().phase, GoalPhase::Green);
    assert_eq!(mem.run().runtime_endpoint.as_deref(), Some("v7"));
    let events = mem.events();
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0].0, "goal.status");
    assert_eq!(events[0].1, json!({"status": "active", "phase": "green"}));
    assert_eq!(events[1].0, "session.started");
    assert_eq!(events[1].1["agentId"], "direct");
    assert_eq!(events[1].1["engine"], "cline");
    assert_eq!(events[1].1["modelId"], MODEL);
    assert_eq!(events[1].1["role"], "direct");
    assert_eq!(events[1].1["contextTokens"], 1_000_000);
    // harness authority, so only the harness can say the session started
    assert!(
        mem.envelopes()
            .iter()
            .all(|e| e.authority == harness_protocol::envelope::Authority::Harness)
    );
}

#[tokio::test]
async fn start_session_pins_live_when_the_version_endpoint_is_missing() {
    let mem = Mem::default();
    mem.0.endpoints.lock().unwrap().remove("v7");
    mem.seed(repo_target(), "p", MODEL);
    let ack = start(&mem, "t").await.unwrap();
    assert_eq!(ack["qualifier"], "live");
    assert_eq!(mem.run().runtime_endpoint.as_deref(), Some("live"));
}

#[tokio::test]
async fn start_session_without_a_live_endpoint_is_retryable_not_invalid() {
    let mem = Mem::default();
    mem.0.endpoints.lock().unwrap().clear();
    mem.seed(repo_target(), "p", MODEL);
    let e = start(&mem, "t").await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionUnavailable");
    assert!(mem.invocations().is_empty());
}

#[tokio::test]
async fn a_failed_package_token_mint_does_not_stop_the_run() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    *mem.0.package_token.lock().unwrap() = Some(Err("codeartifact token returned 500".into()));
    start(&mem, "t").await.unwrap();
    let req = &mem.invocations()[0];
    assert!(
        req.get("packagesToken").is_none() && req.get("packagesTokenExpiresAt").is_none(),
        "{req}"
    );
    // and a stage without a package index sends none either
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    *mem.0.package_token.lock().unwrap() = Some(Ok(None));
    start(&mem, "t").await.unwrap();
    assert!(mem.invocations()[0].get("packagesToken").is_none());
}

#[tokio::test]
async fn a_retry_after_a_failed_invoke_holds_the_newest_token_and_the_first_nonce() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    mem.fail(
        "invoke",
        RunSessionError::Unavailable("AgentCore 503".into()),
    );
    let e = start(&mem, "token-A").await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionUnavailable");
    assert_eq!(
        mem.turn().unwrap().task_token_ciphertext,
        format!("ct(token-A;{RUN};direct;1)")
    );
    assert!(mem.turn().unwrap().invoked_at.is_none());
    let first_nonce_hash = mem.agent().unwrap().creds.unwrap().nonce_sha256;
    start(&mem, "token-B").await.unwrap();
    assert_eq!(
        mem.agent().unwrap().creds.unwrap().nonce_sha256,
        first_nonce_hash,
        "the nonce is the run's, made once (review S1)"
    );
    // the Step Functions retry holds a new token and worker-api completes whichever the Turn item holds
    assert_eq!(
        mem.turn().unwrap().task_token_ciphertext,
        format!("ct(token-B;{RUN};direct;1)")
    );
    assert!(mem.turn().unwrap().invoked_at.is_some());
    assert_eq!(mem.invocations().len(), 1, "one accepted invocation");
    let sent: DirectInvocationRequest =
        serde_json::from_value(mem.invocations()[0].clone()).unwrap();
    assert_eq!(
        mem.agent().unwrap().creds.unwrap().nonce_sha256,
        harness_protocol::store::CredsBinding::hash_nonce(&sent.nonce),
        "the Agent item matches the nonce the running agentd holds"
    );
}

#[tokio::test]
async fn a_retry_after_the_invoke_replaces_the_token_and_invokes_and_rebinds_nothing() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    start(&mem, "token-A").await.unwrap();
    let agent_before = mem.agent().unwrap();
    let events_before = mem.envelopes();
    mem.clear_events();
    let ack = start(&mem, "token-B").await.unwrap();
    assert_eq!(ack["alreadyInvoked"], true);
    assert_eq!(mem.invocations().len(), 1, "no second invocation");
    assert_eq!(
        mem.agent().unwrap(),
        agent_before,
        "the running agentd keeps its nonce"
    );
    assert_eq!(mem.0.minted.lock().unwrap().len(), 1, "no second mint");
    assert_eq!(
        mem.turn().unwrap().task_token_ciphertext,
        format!("ct(token-B;{RUN};direct;1)")
    );
    // the start events are written again, with the same ids
    let after = mem.envelopes();
    assert_eq!(after.len(), 2);
    assert_eq!(
        after.iter().map(|e| e.event_id.clone()).collect::<Vec<_>>(),
        events_before
            .iter()
            .map(|e| e.event_id.clone())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn an_event_write_that_failed_after_the_invoke_is_written_by_the_retry() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    // twice: the op retries the announcement once in process
    for _ in 0..2 {
        mem.fail(
            "put_event",
            RunSessionError::Unavailable("throttled".into()),
        );
    }
    start(&mem, "token-A").await.unwrap_err();
    assert!(
        mem.turn().unwrap().invoked_at.is_some(),
        "invoked before the event write failed"
    );
    assert!(mem.events().is_empty());
    start(&mem, "token-B").await.unwrap();
    assert_eq!(mem.event_types(), ["goal.status", "session.started"]);
    assert_eq!(mem.invocations().len(), 1);
}

#[tokio::test]
async fn a_refusal_by_agentd_is_invalid_and_a_busy_session_is_retryable() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    mem.fail(
        "invoke",
        RunSessionError::Invalid("Received error (400) from runtime".into()),
    );
    assert_eq!(
        start(&mem, "t").await.unwrap_err().error_type(),
        "RunSessionInvalid"
    );
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    *mem.0.invoke_reply.lock().unwrap() = Some(
        serde_json::to_vec(
            &json!({"runId": RUN, "agentId": "direct", "turn": 1, "accepted": false}),
        )
        .unwrap(),
    );
    let e = start(&mem, "t").await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionUnavailable");
    assert!(mem.turn().unwrap().invoked_at.is_none());
    *mem.0.invoke_reply.lock().unwrap() = Some(b"not json".to_vec());
    assert_eq!(
        start(&mem, "t").await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
}

#[tokio::test]
async fn start_session_refuses_a_run_that_is_no_longer_active() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    mem.set_run_status_raw(GoalStatus::Cleared);
    assert_eq!(
        start(&mem, "t").await.unwrap_err().error_type(),
        "RunSessionInvalid"
    );
    assert!(mem.invocations().is_empty());
    assert!(
        mem.agent().is_none(),
        "no agent is bound for a discarded run"
    );
}

#[tokio::test]
async fn the_session_never_outlasts_the_microvm() {
    assert_eq!(session_timeout_s(28_800), 27_900);
    assert_eq!(session_timeout_s(100_000), 27_900);
    assert_eq!(session_timeout_s(3_600), 2_700);
    assert_eq!(session_timeout_s(900), 1);
    assert_eq!(session_timeout_s(0), 1);
}

#[tokio::test]
async fn the_credentials_come_from_the_agents_own_mint_plan() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    start(&mem, "t").await.unwrap();
    let minted = mem.0.minted.lock().unwrap();
    assert_eq!(minted.len(), 1);
    assert_eq!(minted[0].role_session_name, format!("{RUN}.direct"));
    assert!(minted[0].tags.iter().any(|(k, v)| k == "run" && v == RUN));
    assert!(
        minted[0]
            .tags
            .iter()
            .any(|(k, v)| k == "agent" && v == "direct")
    );
    assert!(minted[0].session_policy.contains("bedrock:InvokeModel"));
}

// ---- the worker runtime is read when the session starts (defect F1/2) ----------------------------------------------------------

#[tokio::test]
async fn the_worker_runtime_is_looked_up_each_time_a_session_starts_and_recorded_on_the_agent() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    mem.runtime_from_lookup(Ok(RUNTIME.into()));
    start(&mem, "token-A").await.unwrap();
    assert_eq!(mem.runtime_lookups(), 1);
    assert_eq!(mem.agent().unwrap().runtime_arn, RUNTIME);
    // A retry looks again: a warm function sees a parameter that was written after it started.
    start(&mem, "token-B").await.unwrap();
    assert_eq!(mem.runtime_lookups(), 2);
    assert_eq!(mem.invocations().len(), 1, "the retry did not invoke again");
}

#[tokio::test]
async fn a_stage_whose_runtime_parameter_is_missing_fails_the_start_with_its_reason_and_starts_nothing()
 {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    mem.runtime_from_lookup(Err(RunSessionError::Invalid(
        "the worker runtime is not deployed: SSM parameter /hamh/direct/runtime-arn/worker-python does not exist".into(),
    )));
    let e = start(&mem, "token-A").await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionInvalid");
    assert!(e.to_string().contains("not deployed"), "{e}");
    assert!(mem.agent().is_none() && mem.turn().is_none());
    assert!(mem.invocations().is_empty());
    assert!(
        mem.0.minted.lock().unwrap().is_empty(),
        "no credentials were minted for a session that cannot start"
    );
    // A lookup that failed for a reason that can pass is retried by the machine.
    mem.runtime_from_lookup(Err(RunSessionError::Unavailable("SSM throttled".into())));
    assert_eq!(
        start(&mem, "token-A").await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
    // The configured ARN of a test or a fixed deployment is used without a lookup.
    let fixed = Mem::default();
    fixed.seed(repo_target(), "p", MODEL);
    start(&fixed, "token-A").await.unwrap();
    assert_eq!(fixed.runtime_lookups(), 0);
}

#[tokio::test]
async fn no_configured_arn_and_no_lookup_means_no_runtime_is_deployed() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    *mem.0.runtime_cfg.lock().unwrap() = Some(String::new());
    let e = start(&mem, "token-A").await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionInvalid");
    assert!(
        e.to_string().contains("no worker runtime is deployed"),
        "{e}"
    );
}
