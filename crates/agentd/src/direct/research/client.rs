//! The typed calls to the research route, with the retry rule.
//!
//! A request keeps one idempotency key for all its tries, so the route does the work once however often agentd asks: a key the caller
//! sent is kept, a search or a fetch without one gets a fresh key, and a repository request must have one (the API makes it from the
//! arguments). A [`FailureKind::Transient`] failure is asked again after a pause that doubles (and honours the route's own hint, up to
//! ten seconds), up to the number of retries in the settings; any other failure is returned at once.

use crate::direct::research::route::{FailureKind, Route, RouteFailure};
use crate::direct::research::settings::Settings;
use crate::time::new_ulid;
use harness_protocol::research::route as paths;
use harness_protocol::research::{
    FetchRequest, FetchResponse, JobRequest, JobResponse, LimitsRequest, LimitsResponse,
    RepoRequest, ResearchError, ResearchErrorCode, ResearchLimits, SearchRequest, SearchResponse,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The longest pause the route's own `retryAfterS` can make agentd take between two tries.
const MAX_HINT: Duration = Duration::from_secs(10);
/// The longest pause between tries whatever the doubling gives.
const MAX_PAUSE: Duration = Duration::from_secs(8);

/// The calls to the route.
pub struct Client {
    route: Arc<dyn Route>,
    retries: u32,
    pause: Duration,
    limits: Mutex<ResearchLimits>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("retries", &self.retries)
            .finish_non_exhaustive()
    }
}

fn usage(e: ResearchError) -> RouteFailure {
    RouteFailure {
        error: e,
        kind: FailureKind::Final,
    }
}

impl Client {
    pub fn new(route: Arc<dyn Route>, settings: &Settings) -> Self {
        Self {
            route,
            retries: settings.retries,
            pause: settings.retry_pause,
            limits: Mutex::new(ResearchLimits::default()),
        }
    }

    /// The limits the request rules are checked with before a call: the default ones until the route has told the run its own.
    pub fn set_limits(&self, limits: ResearchLimits) {
        *self.limits.lock().unwrap_or_else(|e| e.into_inner()) = limits;
    }

    fn limits(&self) -> ResearchLimits {
        self.limits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    async fn send<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        path: &'static str,
        req: &Req,
    ) -> Result<Resp, RouteFailure> {
        let body = serde_json::to_value(req)
            .map_err(|_| RouteFailure::fatal("the request cannot be written as JSON"))?;
        let mut pause = self.pause;
        let mut tried = 0;
        loop {
            match self.route.call(path, &body).await {
                Ok(v) => {
                    return serde_json::from_value(v).map_err(|_| {
                        RouteFailure::fatal("the route's answer is not the shape the contract has")
                    });
                }
                Err(f) if f.kind == FailureKind::Transient && tried < self.retries => {
                    let hint = f.error.retry_after_s.map_or(Duration::ZERO, |s| {
                        Duration::from_secs(u64::from(s)).min(MAX_HINT)
                    });
                    tokio::time::sleep(pause.max(hint)).await;
                    pause = (pause * 2).min(MAX_PAUSE);
                    tried += 1;
                }
                Err(f) => return Err(f),
            }
        }
    }

    pub async fn search(&self, mut req: SearchRequest) -> Result<SearchResponse, RouteFailure> {
        req.validate(&self.limits()).map_err(usage)?;
        req.idempotency_key.get_or_insert_with(new_ulid);
        self.send(paths::SEARCH, &req).await
    }

    pub async fn fetch(&self, mut req: FetchRequest) -> Result<FetchResponse, RouteFailure> {
        req.validate(&self.limits()).map_err(usage)?;
        req.idempotency_key.get_or_insert_with(new_ulid);
        self.send(paths::FETCH, &req).await
    }

    pub async fn repo(&self, req: RepoRequest) -> Result<JobResponse, RouteFailure> {
        req.validate(&self.limits()).map_err(usage)?;
        self.send(paths::REPO, &req).await
    }

    pub async fn job(&self, job_id: &str) -> Result<JobResponse, RouteFailure> {
        let req = JobRequest {
            job_id: job_id.to_string(),
        };
        req.validate().map_err(usage)?;
        self.send(paths::JOB, &req).await
    }

    pub async fn limits_now(&self) -> Result<LimitsResponse, RouteFailure> {
        self.send(paths::LIMITS, &LimitsRequest {}).await
    }
}

/// The `usage` error with a message, for the API's own checks.
pub fn usage_error(message: impl Into<String>) -> ResearchError {
    ResearchError::new(ResearchErrorCode::Usage, message)
}
