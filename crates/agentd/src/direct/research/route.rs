//! The signed call from agentd to the research route.
//!
//! The VM already reaches the stage account over a private path: agentd signs a request with the run's session credentials (SigV4,
//! service `execute-api`) and posts it to the internal API. The research route is five more resources on that API
//! (`harness_protocol::research::route`); no firewall rule, endpoint or session policy changes. The caller's identity is the signature;
//! the body never says who is asking.
//!
//! [`Route`] is the seam (tests give a script). [`HttpRoute`] is the real one. An answer that is not a success is read in the route's own
//! words ([`ResearchError`]); an answer that is not, such as the gateway's own refusal, is read by its status. [`FailureKind`] says
//! whether asking again can help, so the caller's retry loop never has to know a status code.

use crate::BoxFuture;
use crate::creds::{CredError, CredStore};
use crate::http::{HttpError, SignedHttp, SignedRequest};
use harness_protocol::research::{ResearchError, ResearchErrorCode};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Whether asking again can help.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// The route was not reached or said "slow down" or failed: the same request may work in a moment.
    Transient,
    /// The gateway has no such route for this run (or the run may not call it): research is not available. Not asked again.
    Absent,
    /// The route said no, or the answer is unusable. Asking again gives the same answer.
    Final,
}

/// A call that did not give an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteFailure {
    pub error: ResearchError,
    pub kind: FailureKind,
}

impl RouteFailure {
    pub fn new(code: ResearchErrorCode, message: impl Into<String>, kind: FailureKind) -> Self {
        Self {
            error: ResearchError::new(code, message),
            kind,
        }
    }

    /// The route is not there for this run.
    pub fn absent() -> Self {
        Self::new(
            ResearchErrorCode::Disabled,
            "the research route is not available to this run",
            FailureKind::Absent,
        )
    }

    /// Worth trying again.
    pub fn transient(message: impl Into<String>) -> Self {
        Self::new(ResearchErrorCode::Internal, message, FailureKind::Transient)
    }

    /// Not worth trying again.
    pub fn fatal(message: impl Into<String>) -> Self {
        Self::new(ResearchErrorCode::Internal, message, FailureKind::Final)
    }
}

/// The route, as agentd calls it: post `body` to `path` and return the JSON of a 200 or 202.
pub trait Route: Send + Sync {
    fn call<'a>(
        &'a self,
        path: &'static str,
        body: &'a Value,
    ) -> BoxFuture<'a, Result<Value, RouteFailure>>;
}

/// The real route: SigV4-signed HTTP with the session credentials.
pub struct HttpRoute {
    http: SignedHttp,
    creds: Arc<CredStore>,
    base_url: String,
    region: String,
    timeout: Duration,
}

impl std::fmt::Debug for HttpRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpRoute")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl HttpRoute {
    /// `base_url` is the internal API of the run (`HookEnv.internal_api_url`); `region` the Region it is signed for.
    pub fn new(creds: Arc<CredStore>, base_url: String, region: String) -> Self {
        Self {
            http: SignedHttp::new(),
            creds,
            base_url,
            region,
            timeout: Duration::from_secs(120),
        }
    }

    /// How long one call may take.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Reads a non-success answer.
fn classify(status: u16, body: &[u8]) -> RouteFailure {
    if let Ok(e) = serde_json::from_slice::<ResearchError>(body) {
        let kind = match e.code {
            ResearchErrorCode::LimitRate
            | ResearchErrorCode::UpstreamRateLimited
            | ResearchErrorCode::Internal => FailureKind::Transient,
            _ => FailureKind::Final,
        };
        return RouteFailure { error: e, kind };
    }
    match status {
        // The gateway's own refusals: no such resource, or the run may not call it.
        403..=405 => RouteFailure::absent(),
        429 => RouteFailure::new(
            ResearchErrorCode::LimitRate,
            "the route asked for a pause",
            FailureKind::Transient,
        ),
        500..=599 => RouteFailure::transient(format!("the route answered {status}")),
        _ => RouteFailure::fatal(format!("the route answered {status}")),
    }
}

impl Route for HttpRoute {
    fn call<'a>(
        &'a self,
        path: &'static str,
        body: &'a Value,
    ) -> BoxFuture<'a, Result<Value, RouteFailure>> {
        Box::pin(async move {
            let creds = self.creds.current().await.map_err(|e| match e {
                CredError::RefreshFailed(_) => {
                    RouteFailure::transient("the session credentials could not be refreshed")
                }
                _ => RouteFailure::fatal("the session credentials are not available"),
            })?;
            let bytes = serde_json::to_vec(body)
                .map_err(|_| RouteFailure::fatal("the request cannot be written as JSON"))?;
            let url = format!("{}{path}", self.base_url.trim_end_matches('/'));
            let resp = self
                .http
                .send_timeout(
                    SignedRequest {
                        method: "POST",
                        url: &url,
                        service: "execute-api",
                        region: &self.region,
                        creds: &creds,
                        content_type: Some("application/json"),
                        body: bytes,
                    },
                    self.timeout,
                )
                .await
                .map_err(|e| match e {
                    HttpError::Transport(_) => {
                        RouteFailure::transient("the research route could not be reached")
                    }
                    HttpError::Sign(_) | HttpError::BadUrl(_) => {
                        RouteFailure::fatal("the request to the research route cannot be signed")
                    }
                })?;
            match resp.status {
                200 | 202 => serde_json::from_slice(&resp.body)
                    .map_err(|_| RouteFailure::fatal("the route's answer is not JSON")),
                s => Err(classify(s, &resp.body)),
            }
        })
    }
}
