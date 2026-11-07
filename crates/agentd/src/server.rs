//! The AgentCore Runtime contract on port 8080: `GET /ping`, `POST /invocations`.
//!
//! `/invocations` takes a `kind: "direct"` body, validates it, claims the session's one turn slot (409 while a turn runs),
//! sets `/ping` to `HealthyBusy`, answers `InvocationAccepted` at once and runs the direct driver in the background:
//! AgentCore cuts synchronous requests at 15 minutes.

use crate::config::Config;
use crate::creds::{CredStore, HttpRefresher};
use crate::health::Health;
use crate::logging::LogSink;
use crate::time::SystemClock;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use harness_protocol::runtime::{InvocationAccepted, turn_result_path};
use serde_json::json;
use std::sync::Arc;

pub struct App {
    pub cfg: Config,
    pub health: Arc<Health>,
    pub log: Arc<dyn LogSink>,
    pub creds: Arc<CredStore>,
    /// One turn at a time: an AgentCore session belongs to one agent.
    pub(crate) turn_lock: Arc<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("cfg", &self.cfg)
            .field("health", &self.health)
            .finish_non_exhaustive()
    }
}

impl App {
    pub fn new(cfg: Config, log: Arc<dyn LogSink>) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            health: Health::new(),
            log,
            creds: CredStore::new(Arc::new(HttpRefresher::new()), Arc::new(SystemClock)),
            turn_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }
}

async fn ping(State(app): State<Arc<App>>) -> Response {
    axum::Json(app.health.ping()).into_response()
}

fn bad_request(message: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({ "error": message })),
    )
        .into_response()
}

/// `POST /invocations` of `kind: "direct"`: parse, validate, take the turn slot, answer at once
/// and run the direct driver in the background. 400 for a body that does not parse or validate (a message with a rule or a
/// position, never the input), 409 while a turn runs.
async fn invocations(State(app): State<Arc<App>>, body: Bytes) -> Response {
    use harness_protocol::direct::{
        DIRECT_AGENT_ID, DirectInvocationRequest, MAX_DIRECT_BODY_BYTES, validate_direct_invocation,
    };
    if body.len() > MAX_DIRECT_BODY_BYTES {
        return bad_request(format!("body exceeds {MAX_DIRECT_BODY_BYTES} bytes"));
    }
    let req: DirectInvocationRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return bad_request(format!(
                "body is not a DirectInvocationRequest (line {}, column {})",
                e.line(),
                e.column()
            ));
        }
    };
    if let Err(e) = validate_direct_invocation(&req) {
        app.log.line(&format!("direct invocation refused: {e}"));
        return bad_request(e.to_string());
    }
    let Ok(slot) = app.turn_lock.clone().try_lock_owned() else {
        app.log
            .line("direct turn refused: a turn is already running");
        return (
            StatusCode::CONFLICT,
            axum::Json(json!({ "error": "a turn is already running in this session" })),
        )
            .into_response();
    };
    let busy = Health::busy(&app.health);
    // A rerun must never be answered by the earlier result.
    let stale = turn_result_path(&app.cfg.data_root, &req.run_id, DIRECT_AGENT_ID, req.turn);
    let _ = std::fs::remove_file(stale);
    let ack = InvocationAccepted {
        run_id: req.run_id.clone(),
        agent_id: DIRECT_AGENT_ID.to_string(),
        turn: req.turn,
        accepted: true,
    };
    let task = app.clone();
    tokio::spawn(async move {
        crate::direct::run(&task, req).await;
        // The result file is on disk before /ping reads Healthy and the slot frees.
        drop(busy);
        drop(slot);
    });
    axum::Json(ack).into_response()
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/ping", get(ping))
        .route("/invocations", post(invocations))
        .with_state(app)
}

pub async fn serve(app: Arc<App>, listener: tokio::net::TcpListener) -> std::io::Result<()> {
    axum::serve(listener, router(app)).await
}
