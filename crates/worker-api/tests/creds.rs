//! Pure decision logic of /internal/creds: caller parsing, the allow/deny decision and nonces.

#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use harness_protocol::api::internal::CredsRequest;
use harness_protocol::common::{Engine, Transport};
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::{AgentItem, AgentStatus, CredsBinding};
use worker_api::caller::{Caller, parse_caller};
use worker_api::creds::{Denial, MintPlan, SESSION_DURATION_S, decide, new_nonce};

const ROLE: &str = "hamh-live-harness-worker-session";
const SESSION: &str = "R1-a1-session-000000000000000000000001";

fn agent(nonce: &str) -> AgentItem {
    AgentItem {
        run_id: "R1".into(),
        agent_id: "a1".into(),
        task_id: "t1".into(),
        attempt: 1,
        runtime_arn: "arn:aws:bedrock-agentcore:us-west-2:800000000022:runtime/harness_worker-x"
            .into(),
        runtime_session_id: SESSION.into(),
        generation: 1,
        engine: Engine::CLINE,
        engine_version: "3.0.65".into(),
        transport: Transport::Acp,
        model_id: "us.anthropic.claude-sonnet-5".into(),
        pool: "p".into(),
        status: AgentStatus::Running,
        heartbeat_at: None,
        creds: Some(CredsBinding {
            nonce_sha256: CredsBinding::hash_nonce(nonce),
            revoked: false,
            tenant: "testuser".into(),
            mode: "repo".into(),
            session_policy: "{\"Version\":\"2012-10-17\",\"Statement\":[]}".into(),
            completion_key_sha256: None,
            output_pins_required: None,
        }),
        version: 1,
        session_generation: None,
        generation_started_at: None,
        runtime_version: None,
        runtime_endpoint: None,
        engine_policy_version: None,
        harness_controls: vec![],
        features: vec![],
        capacity_provider_id: None,
        nonce_ciphertext: None,
        compute: None,
    }
}

fn req(nonce: &str) -> CredsRequest {
    CredsRequest {
        run_id: "R1".into(),
        agent_id: "a1".into(),
        runtime_session_id: SESSION.into(),
        nonce: nonce.into(),
    }
}

type Case<'a> = (
    Caller,
    CredsRequest,
    Option<&'a AgentItem>,
    Option<GoalStatus>,
    Denial,
);

fn caller() -> Caller {
    Caller {
        run_id: "R1".into(),
        agent_id: "a1".into(),
    }
}

#[test]
fn parses_the_worker_session_caller() {
    let c = parse_caller(
        &format!("arn:aws:sts::800000000022:assumed-role/{ROLE}/R1.a1"),
        ROLE,
    )
    .unwrap();
    assert_eq!(c, caller());
}

#[test]
fn rejects_other_roles_and_malformed_arns() {
    for arn in [
        "arn:aws:sts::800000000022:assumed-role/Admin/R1.a1",
        &format!("arn:aws:sts::800000000022:assumed-role/{ROLE}/R1a1"),
        &format!("arn:aws:sts::800000000022:assumed-role/{ROLE}/.a1"),
        &format!("arn:aws:sts::800000000022:assumed-role/{ROLE}/R1."),
        &format!("arn:aws:iam::800000000022:role/{ROLE}"),
        "",
    ] {
        assert!(parse_caller(arn, ROLE).is_err(), "{arn}");
    }
}

#[test]
fn a_bound_active_agent_with_the_current_nonce_gets_an_identical_plan() {
    let plan = decide(
        &caller(),
        &req("n0"),
        Some(&agent("n0")),
        Some(GoalStatus::Active),
    )
    .unwrap();
    assert_eq!(
        plan,
        MintPlan {
            role_session_name: "R1.a1".into(),
            source_identity: "a1".into(),
            tags: vec![
                ("run".into(), "R1".into()),
                ("agent".into(), "a1".into()),
                ("tenant".into(), "testuser".into()),
                ("mode".into(), "repo".into()),
                ("engine".into(), "cline".into())
            ],
            session_policy: "{\"Version\":\"2012-10-17\",\"Statement\":[]}".into(),
            duration_seconds: SESSION_DURATION_S,
        }
    );
    assert_eq!(SESSION_DURATION_S, 3600);
}

#[test]
fn every_failed_check_is_denied() {
    let other_caller = Caller {
        run_id: "R2".into(),
        agent_id: "a1".into(),
    };
    let mut revoked = agent("n0");
    revoked.creds.as_mut().unwrap().revoked = true;
    let mut unbound = agent("n0");
    unbound.creds = None;
    let mut stale = req("n0");
    stale.runtime_session_id = "R1-a1-session-000000000000000000000002".into();
    let mut stopped = agent("n0");
    stopped.status = AgentStatus::Stopped;
    let mut failed = agent("n0");
    failed.status = AgentStatus::Failed;
    let a = agent("n0");
    let cases: Vec<Case<'_>> = vec![
        (
            other_caller,
            req("n0"),
            Some(&a),
            Some(GoalStatus::Active),
            Denial::CallerMismatch,
        ),
        (
            caller(),
            req("n0"),
            None,
            Some(GoalStatus::Active),
            Denial::UnknownAgent,
        ),
        (
            caller(),
            req("n0"),
            Some(&unbound),
            Some(GoalStatus::Active),
            Denial::NotBound,
        ),
        (
            caller(),
            req("n0"),
            Some(&revoked),
            Some(GoalStatus::Active),
            Denial::Revoked,
        ),
        (
            caller(),
            stale,
            Some(&a),
            Some(GoalStatus::Active),
            Denial::StaleSession,
        ),
        (
            caller(),
            req("n1"),
            Some(&a),
            Some(GoalStatus::Active),
            Denial::BadNonce,
        ),
        (
            caller(),
            req(""),
            Some(&a),
            Some(GoalStatus::Active),
            Denial::BadNonce,
        ),
        (
            caller(),
            req("n0"),
            Some(&a),
            Some(GoalStatus::Paused),
            Denial::GoalInactive,
        ),
        (
            caller(),
            req("n0"),
            Some(&a),
            Some(GoalStatus::Complete),
            Denial::GoalInactive,
        ),
        (caller(), req("n0"), Some(&a), None, Denial::GoalInactive),
        (
            caller(),
            req("n0"),
            Some(&stopped),
            Some(GoalStatus::Active),
            Denial::AgentInactive,
        ),
        (
            caller(),
            req("n0"),
            Some(&failed),
            Some(GoalStatus::Active),
            Denial::AgentInactive,
        ),
    ];
    for (i, (c, r, a, g, want)) in cases.into_iter().enumerate() {
        assert_eq!(decide(&c, &r, a, g), Err(want), "case {i}");
    }
}

#[test]
fn denial_codes_are_stable_and_distinct() {
    let all = [
        Denial::CallerMismatch,
        Denial::UnknownAgent,
        Denial::NotBound,
        Denial::Revoked,
        Denial::StaleSession,
        Denial::BadNonce,
        Denial::GoalInactive,
        Denial::AgentInactive,
    ];
    let codes: std::collections::BTreeSet<&str> = all.iter().map(|d| d.code()).collect();
    assert_eq!(codes.len(), all.len());
    assert_eq!(Denial::BadNonce.code(), "bad_nonce");
}

#[test]
fn nonces_are_32_random_bytes_in_url_safe_base64() {
    let a = new_nonce();
    let b = new_nonce();
    assert_eq!(a.len(), 43);
    assert_ne!(a, b);
    assert!(
        a.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    );
}
