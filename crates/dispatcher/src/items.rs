//! The Agent item one dispatch writes, with its credential binding, and the mint plan the binding
//! implies.

use harness_protocol::api::internal::CredsRequest;
use harness_protocol::common::{Engine, Transport};
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::{AgentItem, AgentStatus, CredsBinding};
use worker_api::caller::Caller;
use worker_api::creds::{Denial, MintPlan, decide};

/// Everything one dispatch binds: the agent, its runtime session and its session tags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dispatch {
    pub run_id: String,
    pub agent_id: String,
    pub task_id: String,
    pub engine: Engine,
    pub engine_version: String,
    pub model_id: String,
    pub runtime_arn: String,
    pub runtime_session_id: String,
    pub tenant: String,
    pub mode: String,
}

/// Lowercase hex SHA-256.
pub fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `YYYY-MM-DDTHH:MM:SSZ` for Unix seconds (civil-from-days, Howard Hinnant).
pub fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// The agent bound to `nonce` (stored only as its SHA-256) and `session_policy`.
pub fn agent_item(d: &Dispatch, nonce: &str, session_policy: &str) -> AgentItem {
    AgentItem {
        run_id: d.run_id.clone(),
        agent_id: d.agent_id.clone(),
        task_id: d.task_id.clone(),
        attempt: 1,
        runtime_arn: d.runtime_arn.clone(),
        runtime_session_id: d.runtime_session_id.clone(),
        generation: 1,
        engine: d.engine,
        engine_version: d.engine_version.clone(),
        transport: Transport::Acp,
        model_id: d.model_id.clone(),
        pool: "hamh-dispatch".into(),
        status: AgentStatus::Running,
        heartbeat_at: None,
        creds: Some(CredsBinding {
            nonce_sha256: CredsBinding::hash_nonce(nonce),
            revoked: false,
            tenant: d.tenant.clone(),
            mode: d.mode.clone(),
            session_policy: session_policy.to_string(),
            // The hash of the key derived from the run's first nonce; the key itself is never stored.
            completion_key_sha256: Some(CredsBinding::hash_completion_key(
                &CredsBinding::completion_key(nonce),
            )),
        }),
        version: 1,
        session_generation: None,
        generation_started_at: None,
        runtime_version: None,
        runtime_endpoint: None,
        engine_policy_version: None,
        // The harness's own controls this agent takes; control-api refuses a message to an Agent that lists none.
        harness_controls: vec![
            harness_protocol::messages::MESSAGE_NEXT_CONTROL.to_string(),
            harness_protocol::messages::MESSAGE_NOW_CONTROL.to_string(),
        ],
        // The features of this worker image; the transcript route answers `unsupported` for an Agent that lists none.
        features: vec![harness_protocol::transcript::TRANSCRIPT_FEATURE.to_string()],
        capacity_provider_id: None,
        nonce_ciphertext: None,
        compute: None,
    }
}

/// The first mint, derived through worker-api's own refresh decision, so a refresh reproduces
/// the session name, source identity, tags, session policy and duration exactly.
pub fn mint_plan(agent: &AgentItem, nonce: &str) -> Result<MintPlan, Denial> {
    let caller = Caller {
        run_id: agent.run_id.clone(),
        agent_id: agent.agent_id.clone(),
    };
    let req = CredsRequest {
        run_id: agent.run_id.clone(),
        agent_id: agent.agent_id.clone(),
        runtime_session_id: agent.runtime_session_id.clone(),
        nonce: nonce.to_string(),
    };
    decide(&caller, &req, Some(agent), Some(GoalStatus::Active))
}
