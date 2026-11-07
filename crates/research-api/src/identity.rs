//! The caller of a route, from the API Gateway IAM identity and never from the body: `arn:aws:sts::<account>:assumed-role/<role>/<run>.<agent>`
//! where the role is `harness-worker-session`, the account is the API's own, the run is a ULID and the agent is `direct`.

use crate::ports::Fail;
use harness_protocol::common::is_ulid;
use harness_protocol::direct::DIRECT_AGENT_ID;
use harness_protocol::research::ResearchErrorCode;

/// The worker session behind a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    pub run: String,
    pub agent: String,
}

fn not_a_session() -> Fail {
    Fail::new(
        ResearchErrorCode::Disabled,
        "the caller is not a research-enabled worker session",
    )
}

/// `arn` is `requestContext.identity.userArn`, `api_account` is `requestContext.accountId`. Any mismatch is `disabled`: a caller that is
/// not this stage's worker session of a run has no policy row and nothing to ask.
pub fn parse_caller(
    arn: Option<&str>,
    api_account: Option<&str>,
    role_name: &str,
) -> Result<Caller, Fail> {
    let arn = arn.ok_or_else(not_a_session)?;
    let rest = arn
        .strip_prefix("arn:aws:sts::")
        .ok_or_else(not_a_session)?;
    let (account, rest) = rest
        .split_once(":assumed-role/")
        .ok_or_else(not_a_session)?;
    if Some(account) != api_account || account.is_empty() {
        return Err(not_a_session());
    }
    let (role, session) = rest.split_once('/').ok_or_else(not_a_session)?;
    if role != role_name {
        return Err(not_a_session());
    }
    let (run, agent) = session.split_once('.').ok_or_else(not_a_session)?;
    if !is_ulid(run) || agent != DIRECT_AGENT_ID {
        return Err(not_a_session());
    }
    Ok(Caller {
        run: run.to_string(),
        agent: agent.to_string(),
    })
}
