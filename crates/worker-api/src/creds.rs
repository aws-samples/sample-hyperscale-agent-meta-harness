//! `/internal/creds` decision: re-mint only for the bound, unrevoked, active agent of an
//! active goal, on the live runtime session, with the current nonce; the plan reproduces the
//! dispatcher's session name, source identity, tags, session policy and duration.

use crate::caller::Caller;
use base64::Engine as _;
use harness_protocol::api::internal::CredsRequest;
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::{AgentItem, AgentStatus};

/// Role chaining from a Lambda role caps sessions at one hour.
pub const SESSION_DURATION_S: i32 = 3600;

/// What `sts:AssumeRole` on `harness-worker-session` is called with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MintPlan {
    pub role_session_name: String,
    pub source_identity: String,
    pub tags: Vec<(String, String)>,
    pub session_policy: String,
    pub duration_seconds: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denial {
    CallerMismatch,
    UnknownAgent,
    NotBound,
    Revoked,
    StaleSession,
    BadNonce,
    GoalInactive,
    AgentInactive,
    /// A turn-complete names a bundle or snapshot that is not the caller's own turn bundle (sweep SW-67).
    ForeignBundle,
    /// An events or turn-complete request that does not carry the run's completion key.
    CompletionKey,
}

impl Denial {
    pub fn code(&self) -> &'static str {
        match self {
            Self::CallerMismatch => "caller_mismatch",
            Self::UnknownAgent => "unknown_agent",
            Self::NotBound => "not_bound",
            Self::Revoked => "revoked",
            Self::StaleSession => "stale_session",
            Self::BadNonce => "bad_nonce",
            Self::GoalInactive => "goal_inactive",
            Self::AgentInactive => "agent_inactive",
            Self::ForeignBundle => "foreign_bundle",
            Self::CompletionKey => "completion_key",
        }
    }

    /// Shown to the caller; names the failed check and nothing it was compared with.
    pub fn message(&self) -> &'static str {
        match self {
            Self::CallerMismatch => {
                "the request names a different run or agent than the caller's session"
            }
            Self::UnknownAgent => "no agent item for the caller",
            Self::NotBound => "the agent has no credential binding",
            Self::Revoked => "the agent's credentials are revoked",
            Self::StaleSession => "the runtime session is not the agent's live session",
            Self::BadNonce => "the refresh nonce is not current",
            Self::GoalInactive => "the run's goal is not active",
            Self::AgentInactive => "the agent is stopped or failed",
            Self::ForeignBundle => {
                "the turn names a bundle or snapshot that is not the caller's own turn bundle"
            }
            Self::CompletionKey => "the request does not carry the run's completion key",
        }
    }
}

pub fn decide(
    caller: &Caller,
    req: &CredsRequest,
    agent: Option<&AgentItem>,
    goal: Option<GoalStatus>,
) -> Result<MintPlan, Denial> {
    if req.run_id != caller.run_id || req.agent_id != caller.agent_id {
        return Err(Denial::CallerMismatch);
    }
    let agent = agent.ok_or(Denial::UnknownAgent)?;
    let binding = agent.creds.as_ref().ok_or(Denial::NotBound)?;
    if binding.revoked {
        return Err(Denial::Revoked);
    }
    if agent.runtime_session_id != req.runtime_session_id {
        return Err(Denial::StaleSession);
    }
    if !binding.nonce_matches(&req.nonce) {
        return Err(Denial::BadNonce);
    }
    if goal != Some(GoalStatus::Active) {
        return Err(Denial::GoalInactive);
    }
    if matches!(agent.status, AgentStatus::Stopped | AgentStatus::Failed) {
        return Err(Denial::AgentInactive);
    }
    Ok(MintPlan {
        role_session_name: format!("{}.{}", caller.run_id, caller.agent_id),
        source_identity: caller.agent_id.clone(),
        tags: vec![
            ("run".into(), caller.run_id.clone()),
            ("agent".into(), caller.agent_id.clone()),
            ("tenant".into(), binding.tenant.clone()),
            ("mode".into(), binding.mode.clone()),
            ("engine".into(), agent.engine.as_str().into()),
        ],
        session_policy: binding.session_policy.clone(),
        duration_seconds: SESSION_DURATION_S,
    })
}

/// A fresh refresh nonce: 32 random bytes, URL-safe base64 without padding.
pub fn new_nonce() -> String {
    let mut bytes = [0u8; 32];
    // getrandom fails only when the OS has no entropy source, which a Lambda always has.
    getrandom::fill(&mut bytes).unwrap_or_else(|e| panic!("no OS entropy: {e}"));
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
