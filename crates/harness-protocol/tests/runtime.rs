//! AgentCore Runtime contract types: GET /ping and POST /invocations
//! between the dispatcher and agentd.

use harness_protocol::common::Usage;
use harness_protocol::runtime::{
    InvocationAccepted, InvocationResponse, PingResponse, PingStatus, RUNTIME_PORT, TurnOutcome,
};
use serde_json::json;

#[test]
fn runtime_port_is_the_agentcore_contract_port() {
    assert_eq!(RUNTIME_PORT, 8080);
}

#[test]
fn ping_serializes_with_the_contract_field_names() {
    let v = serde_json::to_value(PingResponse {
        status: PingStatus::HealthyBusy,
        time_of_last_update: 1_790_000_000,
    })
    .unwrap();
    assert_eq!(
        v,
        json!({"status": "HealthyBusy", "time_of_last_update": 1_790_000_000u64})
    );
    let v = serde_json::to_value(PingResponse {
        status: PingStatus::Healthy,
        time_of_last_update: 1,
    })
    .unwrap();
    assert_eq!(v["status"], "Healthy");
}

#[test]
fn response_round_trips_and_names_outcomes_in_snake_case() {
    let resp = InvocationResponse {
        summary: None,
        run_id: "r".into(),
        agent_id: "a1".into(),
        turn: 1,
        outcome: TurnOutcome::TimedOut,
        stop_reason: Some("max_tokens".into()),
        last_commit_sha: Some("abc".into()),
        bundle_uri: None,
        usage: Some(Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
        }),
        error: None,
        quarantined: vec![],
        reverted: vec![],
        refused: vec![],
        system_prompt_sha256: None,
        rules_digest: None,
        rules_manifest: None,
        fault_hint: None,
        max_silent_gap_s: None,
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["outcome"], "timed_out");
    assert_eq!(v["lastCommitSha"], "abc");
    let back: InvocationResponse = serde_json::from_value(v).unwrap();
    assert_eq!(back, resp);
}

#[test]
fn the_invocation_is_acknowledged_in_camel_case() {
    // AgentCore cuts synchronous requests at 15 minutes, so /invocations answers at once.
    let ack = InvocationAccepted {
        run_id: "r".into(),
        agent_id: "a1".into(),
        turn: 2,
        accepted: true,
    };
    let v = serde_json::to_value(&ack).unwrap();
    assert_eq!(
        v,
        json!({"runId": "r", "agentId": "a1", "turn": 2, "accepted": true})
    );
    assert_eq!(
        serde_json::from_value::<InvocationAccepted>(v).unwrap(),
        ack
    );
}

#[test]
fn a_result_lists_quarantined_paths_only_when_there_are_some() {
    use harness_protocol::runtime::InvocationResponse;
    let v = serde_json::json!({
        "runId": "r", "agentId": "a1", "turn": 1, "outcome": "completed", "stopReason": "end_turn",
        "lastCommitSha": null, "bundleUri": null,
        "usage": {"inputTokens": 0, "outputTokens": 0, "cacheReadInputTokens": 0, "cacheWriteInputTokens": 0},
        "error": null
    });
    let r: InvocationResponse = serde_json::from_value(v).unwrap();
    assert!(r.quarantined.is_empty());
    assert!(
        serde_json::to_value(&r)
            .unwrap()
            .get("quarantined")
            .is_none()
    );
    let mut q = r.clone();
    q.quarantined = vec![".cline/hooks".into()];
    assert_eq!(
        serde_json::to_value(&q).unwrap()["quarantined"],
        serde_json::json!([".cline/hooks"])
    );
}

#[test]
fn usage_is_null_when_the_engine_reported_none() {
    use harness_protocol::runtime::InvocationResponse;
    let v = serde_json::json!({
        "runId": "r", "agentId": "a1", "turn": 1, "outcome": "completed", "stopReason": "end_turn",
        "lastCommitSha": null, "bundleUri": null, "usage": null, "error": null
    });
    let r: InvocationResponse = serde_json::from_value(v).unwrap();
    assert_eq!(r.usage, None);
    // Written as an explicit null, so a reader cannot mistake it for zero tokens.
    let back = serde_json::to_value(&r).unwrap();
    assert!(
        back.get("usage").is_some_and(serde_json::Value::is_null),
        "{back}"
    );
    let mut missing = back.clone();
    missing.as_object_mut().unwrap().remove("usage");
    let r: InvocationResponse = serde_json::from_value(missing).unwrap();
    assert_eq!(r.usage, None);
}

#[test]
fn a_result_names_the_sha256_of_the_prompt_agentd_supplied() {
    use harness_protocol::runtime::InvocationResponse;
    let mut v = serde_json::json!({
        "runId": "r", "agentId": "a1", "turn": 1, "outcome": "completed", "stopReason": "end_turn",
        "lastCommitSha": null, "bundleUri": null, "usage": null, "error": null
    });
    let r: InvocationResponse = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(r.system_prompt_sha256, None);
    assert!(
        serde_json::to_value(&r)
            .unwrap()
            .get("systemPromptSha256")
            .is_none()
    );
    v["systemPromptSha256"] = serde_json::json!("ab".repeat(32));
    let r: InvocationResponse = serde_json::from_value(v).unwrap();
    assert_eq!(r.system_prompt_sha256, Some("ab".repeat(32)));
}

#[test]
fn a_result_lists_the_paths_agentd_reverted_only_when_there_are_some() {
    use harness_protocol::runtime::InvocationResponse;
    let v = serde_json::json!({
        "runId": "r", "agentId": "a1", "turn": 1, "outcome": "completed", "stopReason": "end_turn",
        "lastCommitSha": null, "bundleUri": null, "usage": null, "error": null
    });
    let r: InvocationResponse = serde_json::from_value(v).unwrap();
    assert!(r.reverted.is_empty());
    assert!(serde_json::to_value(&r).unwrap().get("reverted").is_none());
    let mut q = r.clone();
    q.reverted = vec!["calc/__init__.py".into()];
    assert_eq!(
        serde_json::to_value(&q).unwrap()["reverted"],
        serde_json::json!(["calc/__init__.py"])
    );
}
