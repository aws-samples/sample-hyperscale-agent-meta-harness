//! Package index: installs resolve through the CodeArtifact `packages` repository. `run-session` mints the
//! authorization token outside the VM with its own role ([`mint_token`]) and hands it to agentd in the invocation
//! (`DirectInvocationRequest.packagesToken`), so the worker session's credentials, which the engine uses, carry no
//! CodeArtifact grant. agentd keeps the token in the loopback package proxy's memory (`direct::PkgProxyLauncher`); the engine
//! gets the proxy's address and no token. The token is a secret: it is never logged, and errors carry only the status and
//! CodeArtifact's message.

use crate::http::{SignedHttp, SignedRequest};
use harness_protocol::api::internal::ProcessCredentials;

/// CodeArtifact accepts token durations of 900 to 43,200 seconds.
const MIN_TOKEN_S: u32 = 900;
const MAX_TOKEN_S: u32 = 43_200;
/// The token outlives the turn by this margin.
const TOKEN_MARGIN_S: u32 = 300;
const MAX_MESSAGE_CHARS: usize = 300;

/// `AGENTD_PACKAGES_DOMAIN`, `AGENTD_PACKAGES_OWNER`, `AGENTD_PACKAGES_REPOSITORY` (default
/// `packages`) and `AGENTD_PACKAGES_ENDPOINT` (the CodeArtifact API endpoint; tests only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackagesIndex {
    pub domain: String,
    pub owner: String,
    pub repository: String,
    pub api_endpoint: Option<String>,
}

impl PackagesIndex {
    /// Off unless both the domain and its owner account are set.
    pub fn from_vars(get: &impl Fn(&str) -> Option<String>) -> Option<Self> {
        let set = |k: &str| get(k).filter(|v| !v.is_empty());
        Some(Self {
            domain: set("AGENTD_PACKAGES_DOMAIN")?,
            owner: set("AGENTD_PACKAGES_OWNER")?,
            repository: set("AGENTD_PACKAGES_REPOSITORY").unwrap_or_else(|| "packages".into()),
            api_endpoint: set("AGENTD_PACKAGES_ENDPOINT"),
        })
    }
}

/// `POST /v1/authorization-token?domain=&domain-owner=&duration=` on the regional API endpoint.
pub fn token_url(ix: &PackagesIndex, region: &str, duration_s: u32) -> String {
    let base = ix
        .api_endpoint
        .clone()
        .unwrap_or_else(|| format!("https://codeartifact.{region}.amazonaws.com"));
    format!(
        "{}/v1/authorization-token?domain={}&domain-owner={}&duration={duration_s}",
        base.trim_end_matches('/'),
        ix.domain,
        ix.owner
    )
}

/// The repository endpoint host, served by the `codeartifact.repositories` interface endpoint.
pub fn repository_host(ix: &PackagesIndex, region: &str) -> String {
    format!(
        "{}-{}.d.codeartifact.{region}.amazonaws.com",
        ix.domain, ix.owner
    )
}

/// Long enough for the whole turn plus a margin, within CodeArtifact's range.
pub fn token_duration_s(turn_timeout_s: u32) -> u32 {
    turn_timeout_s
        .saturating_add(TOKEN_MARGIN_S)
        .clamp(MIN_TOKEN_S, MAX_TOKEN_S)
}

fn token_shape_ok(t: &str) -> bool {
    harness_protocol::runtime::PackagesToken::new(t.to_string()).shape_ok()
}

fn error_message(body: &[u8]) -> String {
    let msg = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .or_else(|| v.get("Message"))
                .and_then(|m| m.as_str().map(str::to_owned))
        })
        .unwrap_or_default();
    msg.chars().take(MAX_MESSAGE_CHARS).collect()
}

/// A CodeArtifact authorization token lasting `duration_s`, asked for with `creds` (the caller's
/// own: `run-session`'s, never the worker session's).
pub async fn mint_token(
    http: &SignedHttp,
    creds: &ProcessCredentials,
    ix: &PackagesIndex,
    region: &str,
    duration_s: u32,
) -> Result<String, String> {
    let url = token_url(ix, region, duration_s);
    let resp = http
        .send(SignedRequest {
            method: "POST",
            url: &url,
            service: "codeartifact",
            region,
            creds,
            content_type: None,
            body: vec![],
        })
        .await
        .map_err(|e| format!("codeartifact token: {e}"))?;
    if resp.status != 200 {
        let msg = error_message(&resp.body);
        return Err(if msg.is_empty() {
            format!("codeartifact token returned {}", resp.status)
        } else {
            format!("codeartifact token returned {}: {msg}", resp.status)
        });
    }
    serde_json::from_slice::<serde_json::Value>(&resp.body)
        .ok()
        .and_then(|v| v.get("authorizationToken")?.as_str().map(str::to_owned))
        .filter(|t| token_shape_ok(t))
        .ok_or_else(|| "codeartifact token: unexpected response shape".to_string())
}
