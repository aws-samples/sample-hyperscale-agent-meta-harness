//! What the engines agentd knows by name say when their provider path fails,
//! for `crate::acp::AcpError::fault_hint`. Measured shapes of a JSON-RPC error's `data`: the first
//! engine (the Cline fork, 3.0.65) sends `{message}`, the second (OpenCode 1.18.32) `{service,
//! errorName}`, the fork's patch 0011 `{message, statusCode, errorName, isRetryable, retryAfterS}`.

/// The text an engine gives when its own idle timeout fires: the first engine (3.0.65) and the
/// second (1.18.32, `options.timeout`) `The operation timed out.`, the second's response-header
/// wait `Provider response headers timed out after <n>ms`, after the `Internal error: ` the ACP
/// layer adds. Anchored and case-sensitive: any other text is not read as a timeout.
pub fn is_idle_timeout(message: &str) -> bool {
    let m = message.strip_prefix("Internal error: ").unwrap_or(message);
    matches!(m, "The operation timed out." | "The operation timed out")
        || m.strip_prefix("Provider response headers timed out after ")
            .and_then(|rest| rest.strip_suffix("ms"))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}
