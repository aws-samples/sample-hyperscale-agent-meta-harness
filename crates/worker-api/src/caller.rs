//! The caller of an `/internal` route, from the API Gateway IAM identity (never from the body).

/// The worker session `<run>.<agent>` behind a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    pub run_id: String,
    pub agent_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CallerError {
    #[error("caller is not an assumed-role session of {0}")]
    NotWorkerSession(String),
    #[error("role session name is not <run>.<agent>")]
    BadSessionName,
}

/// Parses `arn:aws:sts::<account>:assumed-role/<role>/<run>.<agent>` for the given role name.
pub fn parse_caller(arn: &str, role_name: &str) -> Result<Caller, CallerError> {
    let not_worker = || CallerError::NotWorkerSession(role_name.to_string());
    let rest = arn.strip_prefix("arn:aws:sts::").ok_or_else(not_worker)?;
    let (_account, rest) = rest.split_once(":assumed-role/").ok_or_else(not_worker)?;
    let (role, session) = rest.split_once('/').ok_or_else(not_worker)?;
    if role != role_name {
        return Err(not_worker());
    }
    let (run, agent) = session.split_once('.').ok_or(CallerError::BadSessionName)?;
    if run.is_empty() || agent.is_empty() || agent.contains('.') {
        return Err(CallerError::BadSessionName);
    }
    Ok(Caller {
        run_id: run.to_string(),
        agent_id: agent.to_string(),
    })
}
