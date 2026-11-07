//! The small test request of provider failover: while every route has failed, agentd asks each route a
//! one-message Converse question with a few output tokens, instead of resuming the engine with the whole conversation. A resume re-sends
//! up to a million tokens, costs real money when it succeeds and adds to the throttling it is waiting out. The answer says whether the
//! route is reachable and allowed, nothing more: a route that answers the small request may still refuse the long one, which is why the
//! next step after a healthy probe is a real resume under the normal budget.
//!
//! The call is signed with the session credentials (the same ones the engine gets through `credential_process`) and goes to the same
//! `bedrock-runtime` endpoint the engine uses, so the network path and the identity are the engine's own.

use super::creds_fn::{CredsFn, encode_segment};
use crate::direct::failover::{ProbeAnswer, Route, RouteProbe};
use crate::http::{HttpError, SignedHttp, SignedRequest};
use serde_json::json;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// A probe waits this long for an answer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Output tokens of the probe: few, and above the smallest limit any model of the catalog accepts.
const PROBE_MAX_TOKENS: u32 = 16;

pub struct BedrockProbe {
    creds: CredsFn,
    http: SignedHttp,
    /// Base URL (tests); default `https://bedrock-runtime.<region>.amazonaws.com`.
    base: Option<String>,
}

impl std::fmt::Debug for BedrockProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BedrockProbe")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl BedrockProbe {
    pub fn new(creds: CredsFn) -> Self {
        Self {
            creds,
            http: SignedHttp::new(),
            base: None,
        }
    }

    /// Sends the request to `base` instead of the Region's endpoint (tests).
    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = Some(base.into());
        self
    }

    fn url(&self, route: &Route) -> String {
        let base = self
            .base
            .clone()
            .unwrap_or_else(|| format!("https://bedrock-runtime.{}.amazonaws.com", route.region));
        format!(
            "{}/model/{}/converse",
            base.trim_end_matches('/'),
            encode_segment(&route.id)
        )
    }
}

/// The class of an answer to the probe. 200 and any other answer that shows the service reached the model layer without refusing the
/// route (a 400 about the request's own shape) are healthy; a throttle, a server error and a model that is not ready are down; a
/// missing model and an access denial are the route being refused.
pub fn answer_for(status: u16, error_type: Option<&str>, body: &[u8]) -> ProbeAnswer {
    let kind = error_type
        .map(|t| t.to_lowercase())
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
        .trim_end_matches("exception")
        .to_string();
    let text = String::from_utf8_lossy(body).to_lowercase();
    let detail = || {
        let mut d = format!("HTTP {status}");
        if !kind.is_empty() {
            d.push(' ');
            d.push_str(&kind);
        }
        d
    };
    if status == 200 {
        return ProbeAnswer::Healthy;
    }
    if matches!(
        kind.as_str(),
        "throttling"
            | "toomanyrequests"
            | "modelnotready"
            | "serviceunavailable"
            | "internalserver"
            | "modeltimeout"
            | "modelstream"
            | "expiredtoken"
    ) || matches!(status, 408 | 424 | 425 | 429)
        || status >= 500
    {
        return ProbeAnswer::Down(detail());
    }
    if status == 404
        || matches!(kind.as_str(), "accessdenied" | "resourcenotfound")
        || text.contains("model identifier is invalid")
        || text.contains("on-demand throughput")
    {
        return ProbeAnswer::Refused(detail());
    }
    ProbeAnswer::Healthy
}

impl RouteProbe for BedrockProbe {
    fn probe<'a>(
        &'a self,
        route: &'a Route,
    ) -> Pin<Box<dyn Future<Output = ProbeAnswer> + Send + 'a>> {
        Box::pin(async move {
            let creds = match (self.creds)().await {
                Ok(c) => c,
                Err(e) => {
                    return ProbeAnswer::Down(format!(
                        "credentials: {}",
                        crate::direct::classify::scrub_detail(&e)
                    ));
                }
            };
            let url = self.url(route);
            let body = json!({
                "messages": [{"role": "user", "content": [{"text": "Reply with the word OK."}]}],
                "inferenceConfig": {"maxTokens": PROBE_MAX_TOKENS},
            })
            .to_string()
            .into_bytes();
            match self
                .http
                .send_timeout(
                    SignedRequest {
                        method: "POST",
                        url: &url,
                        service: "bedrock",
                        region: &route.region,
                        creds: &creds,
                        content_type: Some("application/json"),
                        body,
                    },
                    PROBE_TIMEOUT,
                )
                .await
            {
                Ok(r) => answer_for(r.status, r.error_type.as_deref(), &r.body),
                Err(HttpError::BadUrl(_) | HttpError::Sign(_)) => {
                    ProbeAnswer::Down("the request could not be built or signed".into())
                }
                Err(HttpError::Transport(e)) => {
                    ProbeAnswer::Down(crate::direct::classify::scrub_detail(&e))
                }
            }
        })
    }
}
