//! Session credentials in memory: `run-session`'s `harness-worker-session` credentials
//! arrive with the invocation; inside [`REFRESH_WINDOW_S`] of expiry they are re-minted through
//! `/internal/creds` with the rotating nonce. A failed refresh fails closed.
//!
//! Every secret the store comes to hold (the secret key, the session token and the nonce of the
//! install and of every refresh) is added at that moment to the run's [`SecretSet`] when one is
//! watched ([`CredStore::watch_secrets`]), so the redaction of the engine's words knows a value
//! before the engine can print it.

use crate::BoxFuture;
use crate::http::{SignedHttp, SignedRequest};
use crate::secrets::SecretSet;
use crate::time::{Clock, parse_rfc3339};
use harness_protocol::api::internal::{CredsRequest, CredsResponse, ProcessCredentials};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Refresh when fewer than 10 minutes remain.
pub const REFRESH_WINDOW_S: i64 = 600;

/// What `/internal/creds` needs besides the nonce; set from the invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshContext {
    pub run_id: String,
    pub agent_id: String,
    pub runtime_session_id: String,
    pub internal_api_url: String,
    pub region: String,
}

pub trait Refresher: Send + Sync {
    fn refresh<'a>(
        &'a self,
        ctx: &'a RefreshContext,
        current: &'a ProcessCredentials,
        req: CredsRequest,
    ) -> BoxFuture<'a, Result<CredsResponse, String>>;
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CredError {
    #[error("no session credentials installed")]
    NotInstalled,
    #[error("session credentials carry an unparsable expiration")]
    BadExpiration,
    #[error("credential refresh failed: {0}")]
    RefreshFailed(String),
}

struct Held {
    ctx: RefreshContext,
    creds: ProcessCredentials,
    nonce: String,
}

pub struct CredStore {
    held: Mutex<Option<Held>>,
    refresher: Arc<dyn Refresher>,
    clock: Arc<dyn Clock>,
    /// The run's secret set, when one is watched: every secret the store installs or a refresh mints goes into it.
    watched: Mutex<Option<SecretSet>>,
}

impl std::fmt::Debug for CredStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredStore")
            .field("clock", &self.clock)
            .finish_non_exhaustive()
    }
}

impl CredStore {
    pub fn new(refresher: Arc<dyn Refresher>, clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self {
            held: Mutex::new(None),
            refresher,
            clock,
            watched: Mutex::new(None),
        })
    }

    pub async fn install(&self, ctx: RefreshContext, creds: ProcessCredentials, nonce: String) {
        // The held credentials and the set move under one lock order (held, then watched), the same as `current`.
        let mut held = self.held.lock().await;
        self.note(&creds, &nonce).await;
        *held = Some(Held { ctx, creds, nonce });
    }

    /// The set that receives every secret this store holds from now on: what is installed now goes in at once, and so does what
    /// every later install or refresh brings. The set is the run's: the collector, the recorder and the end sequence redact with it.
    pub async fn watch_secrets(&self, set: SecretSet) {
        let held = self.held.lock().await;
        if let Some(h) = held.as_ref() {
            set.add(&[
                h.creds.secret_access_key.clone(),
                h.creds.session_token.clone(),
                h.nonce.clone(),
            ]);
        }
        *self.watched.lock().await = Some(set);
    }

    /// Adds the secrets of `creds` and `nonce` to the watched set, when there is one. Called with `held` locked.
    async fn note(&self, creds: &ProcessCredentials, nonce: &str) {
        if let Some(set) = self.watched.lock().await.as_ref() {
            set.add(&[
                creds.secret_access_key.clone(),
                creds.session_token.clone(),
                nonce.to_string(),
            ]);
        }
    }

    pub async fn clear(&self) {
        *self.held.lock().await = None;
    }

    pub async fn context(&self) -> Option<RefreshContext> {
        self.held.lock().await.as_ref().map(|h| h.ctx.clone())
    }

    /// Credentials with at least [`REFRESH_WINDOW_S`] left, refreshing first when needed.
    pub async fn current(&self) -> Result<ProcessCredentials, CredError> {
        let mut guard = self.held.lock().await;
        let held = guard.as_mut().ok_or(CredError::NotInstalled)?;
        let expires = parse_rfc3339(&held.creds.expiration).ok_or(CredError::BadExpiration)?;
        if expires - self.clock.now_unix() >= REFRESH_WINDOW_S {
            return Ok(held.creds.clone());
        }
        let req = CredsRequest {
            run_id: held.ctx.run_id.clone(),
            agent_id: held.ctx.agent_id.clone(),
            runtime_session_id: held.ctx.runtime_session_id.clone(),
            nonce: held.nonce.clone(),
        };
        let resp = self
            .refresher
            .refresh(&held.ctx, &held.creds, req)
            .await
            .map_err(|e| CredError::RefreshFailed(e.chars().take(200).collect()))?;
        if parse_rfc3339(&resp.credentials.expiration).is_none() {
            return Err(CredError::BadExpiration);
        }
        // The new secrets are known to the redaction before anything (the engine's SDK included) can read them from the store.
        self.note(&resp.credentials, &resp.next_nonce).await;
        held.creds = resp.credentials;
        held.nonce = resp.next_nonce;
        Ok(held.creds.clone())
    }
}

/// `POST <internalApiUrl>/internal/creds`, SigV4-signed with the current session credentials.
#[derive(Debug, Default)]
pub struct HttpRefresher {
    http: SignedHttp,
}

impl HttpRefresher {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Refresher for HttpRefresher {
    fn refresh<'a>(
        &'a self,
        ctx: &'a RefreshContext,
        current: &'a ProcessCredentials,
        req: CredsRequest,
    ) -> BoxFuture<'a, Result<CredsResponse, String>> {
        Box::pin(async move {
            let url = format!(
                "{}/internal/creds",
                ctx.internal_api_url.trim_end_matches('/')
            );
            let body = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
            let resp = self
                .http
                .send(SignedRequest {
                    method: "POST",
                    url: &url,
                    service: "execute-api",
                    region: &ctx.region,
                    creds: current,
                    content_type: Some("application/json"),
                    body,
                })
                .await
                .map_err(|e| e.to_string())?;
            if resp.status != 200 {
                return Err(format!("creds refresh returned {}", resp.status));
            }
            serde_json::from_slice::<CredsResponse>(&resp.body).map_err(|e| {
                format!(
                    "creds refresh response is not a CredsResponse (line {}, column {})",
                    e.line(),
                    e.column()
                )
            })
        })
    }
}
