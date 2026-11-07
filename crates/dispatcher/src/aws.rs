//! The AgentCore Runtime calls of a run's session: `InvokeAgentRuntime` and `StopRuntimeSession` at a named endpoint, with the retry of
//! the service's `RetryableConflictException`, and `DeleteCapacityProviderSession`, the call that ends an Instances session. Each call
//! is one request and its error mapping.

use aws_sdk_bedrockagentcore::error::DisplayErrorContext;
use aws_sdk_bedrockagentcore::primitives::Blob;

#[derive(Debug, thiserror::Error)]
pub enum AwsError {
    #[error("{op}: {message}")]
    Call { op: &'static str, message: String },
    /// AgentCore's qualifier is an endpoint name; a bare version never leaves.
    #[error("{op}: qualifier {qualifier:?} is not an endpoint name ([a-zA-Z][a-zA-Z0-9_]{{0,47}})")]
    Qualifier { op: &'static str, qualifier: String },
}

fn call<E: std::error::Error>(op: &'static str) -> impl Fn(E) -> AwsError {
    move |e| AwsError::Call {
        op,
        message: DisplayErrorContext(&e).to_string(),
    }
}

/// Attempts of one AgentCore call while the service provisions or tears down the session: it
/// answers `RetryableConflictException` (HTTP 409, "Session operation in progress") in that
/// window, which a planned handoff (stop, then probe) opens on purpose. Waits double from
/// `CONFLICT_BASE_MS`.
pub const CONFLICT_ATTEMPTS: u32 = 7;
pub const CONFLICT_BASE_MS: u64 = 1_000;

/// How long to back off after the `attempt`-th conflict (1-based); `base_ms` 0 in tests.
pub fn conflict_backoff_ms(attempt: u32, base_ms: u64) -> u64 {
    base_ms.saturating_mul(1u64 << attempt.saturating_sub(1).min(16))
}

async fn backoff(attempt: u32, base_ms: u64) {
    let ms = conflict_backoff_ms(attempt, base_ms);
    if ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }
}

/// Refuses a qualifier that is not an endpoint name before `op` sends anything: AgentCore answers
/// a bare version with `ResourceNotFoundException` (the 4f77949 outage).
fn endpoint_qualifier(op: &'static str, qualifier: &str) -> Result<(), AwsError> {
    if crate::endpoints::is_endpoint_name(qualifier) {
        Ok(())
    } else {
        Err(AwsError::Qualifier {
            op,
            qualifier: qualifier.to_string(),
        })
    }
}

/// `InvokeAgentRuntime` on the session at endpoint `qualifier`.
pub async fn invoke_at(
    agentcore: &aws_sdk_bedrockagentcore::Client,
    runtime_arn: &str,
    qualifier: &str,
    session: &str,
    payload: Vec<u8>,
    base_ms: u64,
) -> Result<Vec<u8>, AwsError> {
    endpoint_qualifier("InvokeAgentRuntime", qualifier)?;
    let mut attempt = 0;
    let out = loop {
        attempt += 1;
        let res = agentcore
            .invoke_agent_runtime()
            .agent_runtime_arn(runtime_arn)
            .qualifier(qualifier)
            .runtime_session_id(session)
            .content_type("application/json")
            .accept("application/json")
            .payload(Blob::new(payload.clone()))
            .send()
            .await;
        match res {
            Ok(o) => break o,
            Err(e)
                if attempt < CONFLICT_ATTEMPTS
                    && e.as_service_error()
                        .is_some_and(|s| s.is_retryable_conflict_exception()) =>
            {
                backoff(attempt, base_ms).await;
            }
            Err(e) => return Err(call("InvokeAgentRuntime")(e)),
        }
    };
    let body = out.response.collect().await.map_err(|e| AwsError::Call {
        op: "InvokeAgentRuntime",
        message: e.to_string(),
    })?;
    Ok(body.into_bytes().to_vec())
}

/// `StopRuntimeSession` at endpoint `qualifier` (see [`stop_session`]).
pub async fn stop_session_at(
    agentcore: &aws_sdk_bedrockagentcore::Client,
    runtime_arn: &str,
    qualifier: &str,
    session: &str,
    base_ms: u64,
) -> Result<bool, AwsError> {
    endpoint_qualifier("StopRuntimeSession", qualifier)?;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let res = agentcore
            .stop_runtime_session()
            .agent_runtime_arn(runtime_arn)
            .qualifier(qualifier)
            .runtime_session_id(session)
            .send()
            .await;
        match res {
            Ok(_) => return Ok(true),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|s| s.is_resource_not_found_exception()) =>
            {
                return Ok(false);
            }
            Err(e)
                if attempt < CONFLICT_ATTEMPTS
                    && e.as_service_error()
                        .is_some_and(|s| s.is_retryable_conflict_exception()) =>
            {
                backoff(attempt, base_ms).await;
            }
            Err(e) => return Err(call("StopRuntimeSession")(e)),
        }
    }
}

/// `DeleteCapacityProviderSession`: ends an Instances session and its billing. `StopRuntimeSession` stops only the agent inside the
/// session and leaves the machine up until its idle timeout; this call removes the machine, its network interface and its disks (the root volume
/// within a second, the workspace volume within a minute). `Ok(true)` when the service took the delete (the status is
/// `Deprovisioning`, `Deleting` or `Deleted`); `Ok(false)` when the session was already gone (`ResourceNotFoundException`). A provider id that is
/// not one, or a session id with a character outside `[a-zA-Z0-9._:-]`, is refused before anything is sent. The SDK retries throttling itself.
pub async fn delete_capacity_provider_session(
    agentcore: &aws_sdk_bedrockagentcore::Client,
    capacity_provider_id: &str,
    session_id: &str,
) -> Result<bool, AwsError> {
    const OP: &str = "DeleteCapacityProviderSession";
    if !crate::ids::is_capacity_provider_id(capacity_provider_id) {
        return Err(AwsError::Call {
            op: OP,
            message: "the capacity provider id is not a capacity provider id".into(),
        });
    }
    let session_ok = !session_id.is_empty()
        && session_id.len() <= 256
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'));
    if !session_ok {
        return Err(AwsError::Call {
            op: OP,
            message: "the session id is not a runtime session id".into(),
        });
    }
    match agentcore
        .delete_capacity_provider_session()
        .capacity_provider_id(capacity_provider_id)
        .session_id(session_id)
        .send()
        .await
    {
        Ok(_) => Ok(true),
        Err(e)
            if e.as_service_error()
                .is_some_and(|s| s.is_resource_not_found_exception()) =>
        {
            Ok(false)
        }
        Err(e) => Err(call(OP)(e)),
    }
}
