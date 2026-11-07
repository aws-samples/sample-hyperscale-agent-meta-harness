//! Control API types and route tables.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub mod internal;
pub mod v1;

/// One HTTP route; [`crate::export`] attaches its request and response schemas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    pub method: &'static str,
    pub path: &'static str,
    pub operation_id: &'static str,
    pub summary: &'static str,
}

const fn route(
    method: &'static str,
    path: &'static str,
    operation_id: &'static str,
    summary: &'static str,
) -> Route {
    Route {
        method,
        path,
        operation_id,
        summary,
    }
}

/// Public API `/v1` (AWS_IAM; owner check on every route; Deny for `harness-worker-session`).
pub const V1_ROUTES: &[Route] = &[
    route(
        "POST",
        "/v1/direct/runs",
        "startDirectRun",
        "Start a direct run: the prompt, one model and the user's rules go to Cline unchanged (direct stage only)",
    ),
    route(
        "GET",
        "/v1/direct/models",
        "listDirectModels",
        "The models a direct run may name, the default marked (direct stage only)",
    ),
    route("GET", "/v1/runs", "listRuns", "List the caller's runs"),
    route("GET", "/v1/runs/{runId}", "getRun", "Read one run"),
    route(
        "POST",
        "/v1/runs/{runId}/cancel",
        "cancelRun",
        "Cancel a run",
    ),
    route(
        "POST",
        "/v1/runs/{runId}/input",
        "answerInput",
        "Answer the question a direct run's Cline is waiting on: a new prompt, a fresh session, another model, or finish (direct stage only)",
    ),
    route(
        "POST",
        "/v1/runs/{runId}/messages",
        "sendMessage",
        "Send a message to a running Cline: delivered at the end of its turn (next) or by an interrupt (now), with a status the server keeps; the id is the client's idempotency key (direct stage only)",
    ),
    route(
        "GET",
        "/v1/runs/{runId}/messages",
        "listMessages",
        "The run's messages with their statuses, the current session and whether the run still takes messages (direct stage only)",
    ),
    route(
        "GET",
        "/v1/runs/{runId}/messages/{messageId}",
        "getMessage",
        "One message and its status (direct stage only)",
    ),
    route(
        "POST",
        "/v1/runs/{runId}/messages/{messageId}/withdraw",
        "withdrawMessage",
        "Withdraw a message nobody has claimed yet (pending or held; direct stage only)",
    ),
    route(
        "GET",
        "/v1/runs/{runId}/events",
        "getEvents",
        "Cursor read of one event shard",
    ),
    route(
        "GET",
        "/v1/runs/{runId}/artifacts",
        "listArtifacts",
        "Artifact list",
    ),
    route(
        "POST",
        "/v1/runs/{runId}/artifacts/{artifactId}/download-url",
        "createDownloadUrl",
        "Presigned URL with its true expiry",
    ),
    route("GET", "/v1/models", "listModels", "Catalog with exclusions"),
    route("GET", "/v1/whoami", "whoAmI", "Caller identity"),
    route(
        "GET",
        "/v1/repos",
        "listRepos",
        "The repository allowlist (ham doctor)",
    ),
    route(
        "POST",
        "/v1/admin/repos",
        "adminRepos",
        "Allow or deny a repository (ham-admin repos; operator principal only)",
    ),
];

/// Internal API `/internal` (private API Gateway; only `harness-worker-session` may call it).
pub const INTERNAL_ROUTES: &[Route] = &[
    route(
        "POST",
        "/internal/creds",
        "refreshCredentials",
        "Re-mint session credentials after the goal, session and nonce checks",
    ),
    route(
        "POST",
        "/internal/heartbeat",
        "heartbeat",
        "SendTaskHeartbeat for the caller's open turn",
    ),
    route(
        "POST",
        "/internal/turn-complete",
        "completeTurn",
        "SendTaskSuccess for the caller's open turn; carries no verdict",
    ),
    route(
        "POST",
        "/internal/events",
        "postEvents",
        "Store agent events; returns pending steer, cancel and approval answers and the notices of pending messages",
    ),
    route(
        "POST",
        "/internal/message-session",
        "reportMessageSession",
        "agentd reports its incarnation and its session (the epoch); messages pending in an older session become held",
    ),
    route(
        "POST",
        "/internal/message-claim",
        "claimMessages",
        "agentd claims pending messages before it sends them: one conditional write each, fenced by generation and session",
    ),
    route(
        "POST",
        "/internal/message-seal",
        "sealMessages",
        "agentd asks whether anything is pending; if not, admission closes in the same write",
    ),
    route(
        "POST",
        "/internal/message-outcome",
        "postMessageOutcome",
        "agentd reports that a claimed message was submitted, failed, released or rejected",
    ),
    route(
        "POST",
        "/internal/research/search",
        "researchSearch",
        "Search the web through the stage's research route (Tavily); served by the research-api Lambda, not worker-api",
    ),
    route(
        "POST",
        "/internal/research/fetch",
        "researchFetch",
        "Fetch pages the run has seen as stored research items; served by research-api",
    ),
    route(
        "POST",
        "/internal/research/repo",
        "researchRepo",
        "Start the download of one public GitHub repository as a stored research item (a job); served by research-api",
    ),
    route(
        "POST",
        "/internal/research/job",
        "researchJob",
        "Ask after a research job; served by research-api",
    ),
    route(
        "POST",
        "/internal/research/limits",
        "researchLimits",
        "Read the run's research limits and use; research-api answers disabled when research is off for the run",
    ),
];

/// Error body for every non-2xx response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    pub code: String,
    /// The exact reason shown in `ham`.
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}
