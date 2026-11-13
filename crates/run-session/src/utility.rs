//! The utility model's Converse client: the one place `run-session` calls Bedrock. A helper function of the solution
//! (the run name now, a summary or a label later) is an op of this Lambda that builds its request with `ham_core::utility` and sends it
//! through [`UtilityClient`]; none of them holds a host name or a model id of its own.
//!
//! The call is SigV4-signed for the `bedrock` service with the function's own role credentials, over the same signed HTTP client that
//! agentd uses for CodeArtifact and for its route probe (`agentd::http::SignedHttp`), so this crate gains no dependency and the worker
//! image key does not move. It never uses a worker session's credentials: those are denied outside the worker network.

use agentd::http::{HttpError, SignedHttp, SignedRequest};
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use ham_core::utility::{UtilityFault, UtilityReply, parse_reply};
use harness_protocol::api::internal::ProcessCredentials;
use serde_json::Value;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// One Converse call: the inference profile id the URL names, the Region of the endpoint, the request body and the longest the attempt
/// may take (the caller also stops waiting at that time).
#[derive(Clone, Copy, Debug)]
pub struct UtilityCall<'a> {
    pub route: &'a str,
    pub region: &'a str,
    pub body: &'a Value,
    pub timeout: Duration,
}

pub type UtilityFuture<'a> =
    Pin<Box<dyn Future<Output = Result<UtilityReply, UtilityFault>> + Send + 'a>>;

/// What an op needs from the utility model: one call, answered with the reply or a fault in the classes the retry rule reads.
pub trait UtilityClient: Send + Sync + Debug {
    fn converse<'a>(&'a self, call: &'a UtilityCall<'a>) -> UtilityFuture<'a>;
}

/// Converse over REST: `POST https://bedrock-runtime.<region>.amazonaws.com/model/<id>/converse`.
#[derive(Debug)]
pub struct BedrockConverse {
    http: SignedHttp,
    credentials: SharedCredentialsProvider,
    /// Base URL of a test stub; `None` is the Region's endpoint.
    base: Option<String>,
}

impl BedrockConverse {
    pub fn new(credentials: SharedCredentialsProvider) -> Self {
        Self {
            http: SignedHttp::new(),
            credentials,
            base: None,
        }
    }

    /// Sends the request to `base` instead of the Region's endpoint (tests).
    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = Some(base.into());
        self
    }
}

/// The URL of a Converse call. The route is one path segment: `:` and `/` in it are percent-encoded.
pub fn converse_url(base: Option<&str>, region: &str, route: &str) -> String {
    let root = match base {
        Some(b) => b.trim_end_matches('/').to_string(),
        None => format!("https://bedrock-runtime.{region}.amazonaws.com"),
    };
    format!("{root}/model/{}/converse", encode_segment(route))
}

/// Percent-encodes one path segment the way `encodeURIComponent` does for model ids (the same rule as agentd's route probe).
fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl UtilityClient for BedrockConverse {
    fn converse<'a>(&'a self, call: &'a UtilityCall<'a>) -> UtilityFuture<'a> {
        Box::pin(async move {
            // A function with no usable credentials cannot be helped by a retry.
            let c = self
                .credentials
                .provide_credentials()
                .await
                .map_err(|_| UtilityFault::Config)?;
            let creds = ProcessCredentials {
                version: 1,
                access_key_id: c.access_key_id().to_string(),
                secret_access_key: c.secret_access_key().to_string(),
                session_token: c.session_token().unwrap_or_default().to_string(),
                expiration: String::new(),
            };
            let url = converse_url(self.base.as_deref(), call.region, call.route);
            let body = serde_json::to_vec(call.body).map_err(|_| UtilityFault::Config)?;
            let resp = self
                .http
                .send_timeout(
                    SignedRequest {
                        method: "POST",
                        url: &url,
                        service: "bedrock",
                        region: call.region,
                        creds: &creds,
                        content_type: Some("application/json"),
                        body,
                    },
                    call.timeout,
                )
                .await
                .map_err(|e| match e {
                    HttpError::BadUrl(_) | HttpError::Sign(_) => UtilityFault::Config,
                    // `SignedHttp` ends its own wait with "<url> timed out"; any other transport fault (a refused or reset
                    // connection) is the network's weather.
                    HttpError::Transport(t) if t.contains("timed out") => UtilityFault::Timeout,
                    HttpError::Transport(_) => UtilityFault::Service,
                })?;
            if resp.status == 200 {
                parse_reply(&resp.body)
            } else {
                Err(UtilityFault::from_http(
                    resp.status,
                    resp.error_type.as_deref(),
                ))
            }
        })
    }
}
