//! Credential binding stored on the Agent item: the /internal/creds handler re-mints only
//! for the bound runtime session, with the current nonce, for an unrevoked agent. Only the nonce's
//! SHA-256 is stored.

use harness_protocol::store::{AgentItem, CredsBinding};
use serde_json::json;

fn agent_json() -> serde_json::Value {
    json!({
        "runId": "R1", "agentId": "a1", "taskId": "t1", "attempt": 1,
        "runtimeArn": "arn:aws:bedrock-agentcore:us-west-2:800000000022:runtime/harness_worker-x",
        "runtimeSessionId": "R1-a1-session-000000000000000000001", "generation": 1,
        "engine": "cline", "engineVersion": "3.0.65", "transport": "acp",
        "modelId": "us.anthropic.claude-sonnet-5", "pool": "us-west-2#sonnet-5", "status": "running",
        "version": 3
    })
}

#[test]
fn an_agent_without_a_binding_still_parses() {
    let a: AgentItem = serde_json::from_value(agent_json()).unwrap();
    assert!(a.creds.is_none());
    let back = serde_json::to_value(&a).unwrap();
    assert!(
        back.get("creds").is_none(),
        "absent binding is not serialized"
    );
}

#[test]
fn a_binding_round_trips_in_camel_case() {
    let mut v = agent_json();
    v["creds"] = json!({
        "nonceSha256": CredsBinding::hash_nonce("n0"),
        "revoked": false, "tenant": "testuser", "mode": "repo",
        "sessionPolicy": "{\"Version\":\"2012-10-17\",\"Statement\":[]}"
    });
    let a: AgentItem = serde_json::from_value(v.clone()).unwrap();
    let b = a.creds.as_ref().unwrap();
    assert_eq!(b.tenant, "testuser");
    assert_eq!(serde_json::to_value(&a).unwrap(), v);
}

#[test]
fn nonce_hash_is_lowercase_sha256_hex_and_matching_is_exact() {
    let h = CredsBinding::hash_nonce("abc");
    assert_eq!(
        h,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let b = CredsBinding {
        nonce_sha256: h,
        revoked: false,
        tenant: "t".into(),
        mode: "repo".into(),
        session_policy: "{}".into(),
        completion_key_sha256: None,
    };
    assert!(b.nonce_matches("abc"));
    assert!(!b.nonce_matches("abd"));
    assert!(!b.nonce_matches(""));
}

#[test]
fn debug_shows_the_hash_never_a_nonce() {
    let b = CredsBinding {
        nonce_sha256: CredsBinding::hash_nonce("secret-nonce"),
        revoked: true,
        tenant: "t".into(),
        mode: "repo".into(),
        session_policy: "{}".into(),
        completion_key_sha256: None,
    };
    let text = format!("{b:?}");
    assert!(!text.contains("secret-nonce"));
    assert!(text.contains("revoked: true"));
}
