//! The AgentCore command shell's constants, URL segment encoder and endpoint rule.
//!
//! The module has no session code. The endpoint is `https://bedrock-agentcore.<region>.amazonaws.com`,
//! or `HAMH_SHELL_ENDPOINT` (https, or http to a loopback host: the tests' local endpoint).

use crate::https::check_url;

pub const SERVICE: &str = "bedrock-agentcore";
pub const SUBPROTOCOL: &str = "v1.command.agentcore.aws.dev";

/// Every byte outside the unreserved set as `%XX` (the ARN is one path segment).
pub fn encode_segment(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// The shell endpoint: `HAMH_SHELL_ENDPOINT` or the Region's `bedrock-agentcore` endpoint.
pub fn endpoint(region: &str, from_env: Option<String>) -> Result<String, String> {
    let e = from_env
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| format!("https://bedrock-agentcore.{region}.amazonaws.com"));
    check_url(&e).map_err(|x| format!("HAMH_SHELL_ENDPOINT: {x}"))?;
    Ok(e.trim_end_matches('/').to_string())
}
