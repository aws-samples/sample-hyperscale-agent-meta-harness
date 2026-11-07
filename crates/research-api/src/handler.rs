//! The route's entry point, independent of the Lambda event types: one [`Request`] in, one [`Response`] out. Everything the VM may ask goes
//! through [`handle`]: identity, the run's policy row, the rate checks, the request's own validation, the operation, and one audit object.

use crate::audit;
use crate::counters::epoch_minute;
use crate::identity::parse_caller;
use crate::ops::{self, Ctx};
use crate::policy;
use crate::ports::{Blobs, Clock, Fail, Invoker, Provider, Stage, StageKind, Table};
use harness_protocol::research::{
    ResearchError, ResearchErrorCode, ResearchOp, ResearchProvider, route,
};
use serde::Serialize;
use std::fmt;

/// The operator's key-state question: `POST` with an empty object, answered with the provider a search would use now and the version id of
/// the key held for it. It is no route of the API: API Gateway has no method for it, so only a direct invoke of the function (which needs `lambda:InvokeFunction`)
/// reaches it, with an API Gateway proxy event that carries no caller (`fixtures/research/key-state-event.json`). It is not in `harness_protocol::research::route`.
pub const KEY_STATE_PATH: &str = "/internal/research/key-state";

/// Settings of one deployment (the function's environment, see `crate::lambda`).
#[derive(Clone, Debug)]
pub struct Config {
    /// `harness-worker-session`: the only role whose sessions may call.
    pub worker_role_name: String,
    /// Calls to the search provider per minute for the whole stage (80 percent of the key's limit).
    pub stage_rate_per_min: u32,
    pub stage_kind: StageKind,
}

/// What the route runs on.
pub struct Deps<T, B, P, I, C> {
    pub table: T,
    pub blobs: B,
    pub provider: P,
    pub invoker: I,
    pub clock: C,
    pub cfg: Config,
}

impl<T, B, P, I, C> fmt::Debug for Deps<T, B, P, I, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deps")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

/// One API Gateway request, reduced to what the route reads.
#[derive(Clone, Debug, Default)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// `requestContext.identity.userArn`: the caller, never read from the body.
    pub caller_arn: Option<String>,
    /// `requestContext.accountId`: the account the API lives in; the caller must be in it.
    pub account_id: Option<String>,
    /// `requestContext.identity.accessKey`: the caller's session key id, which no outbound text may contain.
    pub access_key: Option<String>,
    /// `requestContext.requestId`.
    pub request_id: String,
    pub body: String,
}

/// One log line per request: no body, no header, no key, no fetched text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogLine {
    pub op: String,
    pub run: Option<String>,
    pub status: u16,
    pub code: Option<String>,
    pub ms: u64,
    pub request_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: String,
    pub log: LogLine,
}

pub async fn handle<T, B, P, I, C>(deps: &Deps<T, B, P, I, C>, req: &Request) -> Response
where
    T: Table,
    B: Blobs,
    P: Stage,
    I: Invoker,
    C: Clock,
{
    let started = deps.clock.now_ms();
    let mut log = LogLine {
        request_id: req.request_id.clone(),
        ..LogLine::default()
    };
    let finish = |log: LogLine, status: u16, body: String, deps_now: u64| {
        let mut log = log;
        log.status = status;
        log.ms = deps_now.saturating_sub(started);
        Response { status, body, log }
    };
    let error_body = |f: &Fail| {
        let mut e: ResearchError = f.clone().into();
        e.request_id = Some(req.request_id.clone());
        serde_json::to_string(&e).unwrap_or_default()
    };

    if req.path == KEY_STATE_PATH {
        return key_state(deps, req, started).await;
    }
    let op = match req.path.as_str() {
        route::SEARCH => ResearchOp::Search,
        route::FETCH => ResearchOp::Fetch,
        route::REPO => ResearchOp::Repo,
        route::JOB => ResearchOp::Job,
        route::LIMITS => ResearchOp::Limits,
        _ => {
            let f = Fail::new(ResearchErrorCode::NotFound, "no such research route");
            log.code = Some(f.code.as_str().into());
            return finish(log, 404, error_body(&f), deps.clock.now_ms());
        }
    };
    log.op = serde_json::to_value(op)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    if req.method != "POST" {
        let f = Fail::new(ResearchErrorCode::Usage, "research routes take POST");
        log.code = Some(f.code.as_str().into());
        return finish(log, 405, error_body(&f), deps.clock.now_ms());
    }
    let refuse = |mut log: LogLine, f: Fail| {
        log.code = Some(f.code.as_str().into());
        let status = f.code.http_status();
        finish(log, status, error_body(&f), deps.clock.now_ms())
    };

    // 1. Who: the worker session of a run, from the IAM identity.
    let caller = match parse_caller(
        req.caller_arn.as_deref(),
        req.account_id.as_deref(),
        &deps.cfg.worker_role_name,
    ) {
        Ok(c) => c,
        Err(f) => return refuse(log, f),
    };
    log.run = Some(caller.run.clone());
    // 2. Whether research is on for the run, read on every call.
    let policy = match policy::active(&deps.table, &caller.run, started / 1000).await {
        Ok(p) => p,
        Err(f) => return refuse(log, f),
    };
    // 3. The run's calls per minute, for every operation: it bounds what a misbehaving caller can spend or write.
    if let Err(f) = deps
        .table
        .rate_tick(
            &caller.run,
            epoch_minute(started),
            policy.limits.calls_per_minute,
        )
        .await
    {
        return refuse(log, f);
    }
    // 4. The operation.
    let cx = Ctx {
        d: deps,
        caller: &caller,
        policy: &policy,
        req,
        now_ms: started,
    };
    // Search, fetch and limits are served by one provider, chosen here once for the whole call (a stage of one provider answers at once; the `auto` stage
    // reads the key cache). If the choice cannot be made nothing is spent and nothing is sent. Repository calls and job status use no search provider, so
    // they never ask.
    let (outcome, audit_provider) = match op {
        ResearchOp::Search => match deps.provider.serve().await {
            Ok(served) => (ops::search(&cx, served).await, served.id()),
            Err(f) => return refuse(log, f),
        },
        ResearchOp::Fetch => match deps.provider.serve().await {
            Ok(served) => (ops::fetch(&cx, served).await, served.id()),
            Err(f) => return refuse(log, f),
        },
        ResearchOp::Repo => (ops::repo(&cx).await, deps.provider.audit_word()),
        ResearchOp::Job => (ops::job(&cx).await, deps.provider.audit_word()),
        ResearchOp::Limits => {
            // The limits answer is information: when the choice cannot be made it leaves the provider out and still answers.
            let (provider, cap) = match deps.provider.serve().await {
                Ok(served) => (
                    Some(served.id()),
                    served
                        .max_query_chars()
                        .filter(|_| deps.provider.reports_provider_caps()),
                ),
                Err(_) => (None, None),
            };
            (
                ops::limits(&cx, provider, cap).await,
                deps.provider.audit_word(),
            )
        }
    };
    let code = outcome.result.as_ref().err().map(|f| f.code);
    // 5. One audit object per call, written last, so what it says happened has happened.
    let mut result = outcome.result;
    if let Some(draft) = outcome.draft {
        let now = deps.clock.now_ms();
        let obj = audit::build(
            &caller.run,
            &caller.agent,
            audit_provider,
            &req.request_id,
            now,
            now.saturating_sub(started),
            code,
            draft,
        );
        if let Err(f) = audit::write(&deps.blobs, &obj).await {
            result = Err(f);
        }
    }
    match result {
        Ok((status, body)) => finish(log, status, body, deps.clock.now_ms()),
        Err(f) => refuse(log, f),
    }
}

/// The answer of the key-state question: the provider word and, when a key is held, the Secrets Manager version id. Nothing else.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KeyStateAnswer {
    provider: ResearchProvider,
    #[serde(skip_serializing_if = "Option::is_none")]
    version_id: Option<String>,
}

/// Serves [`KEY_STATE_PATH`]. It reads no run, no policy row and no counter and writes no audit object: it has no run. A request that carries a worker
/// session is refused as if the path did not exist, so the question stays closed to the callers of the five routes even if an API method for it were ever added.
async fn key_state<T, B, P, I, C>(
    deps: &Deps<T, B, P, I, C>,
    req: &Request,
    started: u64,
) -> Response
where
    T: Table,
    B: Blobs,
    P: Stage,
    I: Invoker,
    C: Clock,
{
    let log = LogLine {
        op: "key_state".into(),
        request_id: req.request_id.clone(),
        ..LogLine::default()
    };
    let end = |mut log: LogLine, status: u16, body: String| {
        log.status = status;
        log.ms = deps.clock.now_ms().saturating_sub(started);
        Response { status, body, log }
    };
    let refuse = |mut log: LogLine, status: u16, f: Fail| {
        log.code = Some(f.code.as_str().into());
        let mut e: ResearchError = f.into();
        e.request_id = Some(req.request_id.clone());
        let body = serde_json::to_string(&e).unwrap_or_default();
        end(log, status, body)
    };
    // The worker check comes first: a worker gets "not found" whatever the method, so a 405 never tells it that the path is handled.
    if parse_caller(
        req.caller_arn.as_deref(),
        req.account_id.as_deref(),
        &deps.cfg.worker_role_name,
    )
    .is_ok()
    {
        let f = Fail::new(ResearchErrorCode::NotFound, "no such research route");
        return refuse(log, 404, f);
    }
    if req.method != "POST" {
        let f = Fail::new(ResearchErrorCode::Usage, "research routes take POST");
        return refuse(log, 405, f);
    }
    let empty_object = req.body.trim().is_empty()
        || matches!(serde_json::from_str::<serde_json::Value>(&req.body), Ok(serde_json::Value::Object(m)) if m.is_empty());
    if !empty_object {
        let f = Fail::new(
            ResearchErrorCode::Usage,
            "the key-state question takes an empty object",
        );
        return refuse(log, 400, f);
    }
    match deps.provider.key_state().await {
        Ok(state) => {
            let answer = KeyStateAnswer {
                provider: state.provider,
                version_id: state.version_id,
            };
            match serde_json::to_string(&answer) {
                Ok(body) => end(log, 200, body),
                Err(_) => {
                    let f = Fail::internal("answer encoding");
                    let status = f.code.http_status();
                    refuse(log, status, f)
                }
            }
        }
        Err(f) => {
            let status = f.code.http_status();
            refuse(log, status, f)
        }
    }
}
