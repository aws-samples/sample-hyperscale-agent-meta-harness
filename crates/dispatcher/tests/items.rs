//! The Agent item one dispatch writes, and the mint plan it implies: the same plan `/internal/creds` reproduces on every
//! refresh.

#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use dispatcher::ids::runtime_session_id;
use dispatcher::items::{Dispatch, agent_item, mint_plan, rfc3339, sha256_hex};
use dispatcher::policy::session_policy;
use harness_protocol::api::internal::CredsRequest;
use harness_protocol::common::{Engine, Transport};
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::{AgentStatus, Keyed};
use worker_api::caller::Caller;
use worker_api::creds::{Denial, SESSION_DURATION_S, decide};

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const MODEL: &str = "us.anthropic.claude-sonnet-4-5-20250929-v1:0";

fn dispatch() -> Dispatch {
    Dispatch {
        run_id: RUN.into(),
        agent_id: "a1".into(),
        task_id: "root".into(),
        engine: Engine::CLINE,
        engine_version: "3.0.65".into(),
        model_id: MODEL.into(),
        runtime_arn: "arn:aws:bedrock-agentcore:us-west-2:800000000022:runtime/w-abc".into(),
        runtime_session_id: runtime_session_id(RUN, "a1", 1),
        tenant: "testuser".into(),
        mode: "repo".into(),
    }
}

#[test]
fn hashes_and_timestamps_have_the_store_formats() {
    assert_eq!(
        sha256_hex("add percent_change"),
        "e84d609829b3f702c357e166ff37cd3cba20f9a6bb25714c03c666f1b26660ea"
    );
    assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(rfc3339(1_790_298_589), "2026-09-25T01:09:49Z");
    assert_eq!(rfc3339(4_102_444_799), "2099-12-31T23:59:59Z");
}

#[test]
fn the_agent_item_binds_the_nonce_hash_the_tags_and_the_session_policy() {
    let d = dispatch();
    let nonce = "n".repeat(43);
    let policy = session_policy(MODEL).unwrap();
    let agent = agent_item(&d, &nonce, &policy);
    assert_eq!(agent.key().sk, "AGENT#a1");
    assert_eq!(agent.engine, Engine::CLINE);
    assert_eq!(agent.engine_version, "3.0.65");
    assert_eq!(agent.transport, Transport::Acp);
    assert_eq!(agent.model_id, MODEL);
    assert_eq!(agent.status, AgentStatus::Running);
    assert_eq!(agent.runtime_session_id, d.runtime_session_id);
    assert_eq!(agent.runtime_arn, d.runtime_arn);
    assert_eq!(agent.task_id, "root");
    assert_eq!((agent.attempt, agent.generation, agent.version), (1, 1, 1));
    let c = agent.creds.as_ref().unwrap();
    assert!(c.nonce_matches(&nonce));
    assert!(!c.revoked);
    assert_eq!((c.tenant.as_str(), c.mode.as_str()), ("testuser", "repo"));
    assert_eq!(c.session_policy, policy);

    assert_eq!(agent.key().pk, format!("RUN#{RUN}"));
    let stored = serde_json::to_value(&agent).unwrap();
    assert_eq!(stored["creds"]["nonceSha256"], sha256_hex(&nonce));
    assert_eq!(stored["creds"]["revoked"], false);
    // The nonce itself is never stored.
    assert!(!stored.to_string().contains(&nonce));
}

#[test]
fn the_mint_plan_is_exactly_what_a_refresh_reproduces() {
    let d = dispatch();
    let nonce = "n".repeat(43);
    let policy = session_policy(MODEL).unwrap();
    let agent = agent_item(&d, &nonce, &policy);
    let plan = mint_plan(&agent, &nonce).unwrap();
    assert_eq!(plan.role_session_name, format!("{RUN}.a1"));
    assert_eq!(plan.source_identity, "a1");
    let tags: Vec<(&str, &str)> = plan
        .tags
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        tags,
        [
            ("run", RUN),
            ("agent", "a1"),
            ("tenant", "testuser"),
            ("mode", "repo"),
            ("engine", "cline"),
        ]
    );
    assert_eq!(plan.session_policy, policy);
    assert_eq!(plan.duration_seconds, SESSION_DURATION_S);

    let caller = Caller {
        run_id: RUN.into(),
        agent_id: "a1".into(),
    };
    let refresh = CredsRequest {
        run_id: RUN.into(),
        agent_id: "a1".into(),
        runtime_session_id: d.runtime_session_id.clone(),
        nonce: nonce.clone(),
    };
    assert_eq!(
        decide(&caller, &refresh, Some(&agent), Some(GoalStatus::Active)).unwrap(),
        plan
    );
}

#[test]
fn a_mint_plan_for_a_stale_nonce_or_a_revoked_binding_is_refused() {
    let nonce = "n".repeat(43);
    let mut agent = agent_item(&dispatch(), &nonce, "{}");
    assert_eq!(mint_plan(&agent, "other"), Err(Denial::BadNonce));
    agent.creds.as_mut().unwrap().revoked = true;
    assert_eq!(mint_plan(&agent, &nonce), Err(Denial::Revoked));
}

#[test]
fn the_agent_item_registers_the_features_of_this_build() {
    let policy = "{}";
    let nonce = "nonce".to_string();
    let agent = agent_item(&dispatch(), &nonce, policy);
    assert_eq!(agent.features, vec!["transcript.v1".to_string()]);
    assert_eq!(
        agent.harness_controls,
        vec!["message.next".to_string(), "message.now".to_string()],
        "the controls of the message path are a separate list and stay"
    );
}
