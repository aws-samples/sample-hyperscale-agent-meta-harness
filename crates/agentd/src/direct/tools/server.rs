//! The transport of the tool server: MCP over streamable HTTP on the loopback, answering
//! with JSON only and keeping no state of a client.
//!
//! | what | rule |
//! |---|---|
//! | `POST /mcp` | the one endpoint; the answer is `application/json` (or 202 with no body) |
//! | `GET`, `DELETE`, any other method | 405 with `Allow: POST` |
//! | any other path | 404 |
//! | `Host` | one header, a loopback name (`localhost`, `127.0.0.1`, `[::1]`, with a port or without); anything else 403 |
//! | `Origin` | when present, `http` or `https` and a loopback name; anything else 403 |
//! | peer | the connection's peer must be a loopback address; anything else 403 |
//! | body | at most 64 KiB (413 above, and 408 when it does not arrive in time) |
//! | message | one JSON-RPC object. A batch, text that is not JSON, a null or non-integer id, a missing `jsonrpc: "2.0"`: 400 with an error that has no id. A notification (no id, any method) or a response of the client: 202, empty |
//! | `initialize` | answers the highest of 2025-11-25, 2025-06-18, 2025-03-26 that the client asked for (the highest supported for any other value); no `Mcp-Session-Id` ever |
//! | `ping`, `tools/list`, `tools/call` | as the protocol says; another method of an identified request: `-32601` in a 200 |
//! | `notifications/cancelled` | stops the call with that request id of that client program (the `User-Agent`) |
//!
//! Why stateless: a server that issues `Mcp-Session-Id` answers 404 to the engine's old id after a restart on the same port, and the engine never
//! initializes again. So nothing here depends on an earlier request; a restart of the serving task is invisible to a client. The
//! cost is that a cancel names only a request id, and two clients that both count from 0 would cancel each other's call: the `User-Agent` is part
//! of the match, which separates two different client programs and not two copies of one.
//!
//! The protocol version header is counted and never a gate. The specification asks for a 400 to a value the server does not know; but the engine
//! connects once per session, and a 400 at `tools/list` would lose the tools for the whole session without a word, while the three versions
//! this server speaks do not differ in anything it implements.
//!
//! A call: the input is validated against the table (`error: usage: ...`), a slot is taken (eight at once, the ninth gets `error: busy`), the
//! backend runs in a task of its own, and the call ends with the backend's answer, a cancel, the backstop, or the end of the connection. On a
//! cancel the backend's token fires and the backend has a grace (500 ms) to end its children; then the task is aborted. A cancelled request gets
//! a JSON-RPC error `-32800` (the specification asks for no answer; a connection that is still open needs one, and a client that has abandoned
//! the request ignores it).
//!
//! The serving task is supervised: it runs on a clone of one bound socket that the supervisor keeps, so when the task ends the port stays bound
//! (nobody else can take it, and connections that arrive meanwhile wait in the backlog), calls in flight are cancelled, and a new task serves the
//! same socket. At most three restarts; a fourth end leaves the server down and logs it once.
//!
//! Logs and counters hold a tool name, a result word, a time and a size. Never a pattern, a query, a path, a header value or a result.

use crate::direct::tools::backend::{
    CancelToken, FailureWord, ToolBackend, ToolContext, ToolOutput, validate_input,
};
use crate::direct::tools::result::{render, serialized_len};
use crate::direct::tools::stats::{Counters, Stats};
use crate::logging::LogSink;
use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use harness_protocol::tools::{SERVER_NAME, ToolName, tools_list_result};
use http_body_util::BodyExt;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::{AbortHandle, JoinHandle};

/// A request body is at most this many bytes.
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// Calls that run at once; the next one is told `error: busy`.
pub const MAX_INFLIGHT: usize = 8;
/// The protocol versions the server speaks, highest first.
pub const SUPPORTED_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];
/// The serving task is started again at most this many times.
pub const MAX_RESTARTS: u32 = 3;
/// JSON-RPC: the error code of a request that was cancelled (the convention of the language server protocol).
const CODE_CANCELLED: i64 = -32800;
/// How much of a body that was refused is read and thrown away, so that the client reads the answer instead of a reset.
const DRAIN_MAX_BYTES: usize = 1024 * 1024;
const DRAIN_TIME: Duration = Duration::from_secs(1);
/// How long a stop waits for the serving task to drain before it aborts it.
const STOP_GRACE: Duration = Duration::from_secs(5);
/// The longest `User-Agent` that takes part in the match of a cancel.
const MAX_AGENT_CHARS: usize = 128;

/// The limits of the server that a test shortens.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// How long a request body may take to arrive (10 s). The answer is 408.
    pub body_timeout: Duration,
    /// How long a backend has to end its children after a cancel before its task is aborted (500 ms).
    pub cancel_grace: Duration,
    /// The longest a call may run (130 s: a little over the 120 s the engine waits). The answer is `error: timeout`. A hung backend would
    /// otherwise hold one of the eight slots for as long as the client stays connected.
    pub call_backstop: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            body_timeout: Duration::from_secs(10),
            cancel_grace: Duration::from_millis(500),
            call_backstop: Duration::from_secs(130),
        }
    }
}

// ---- the shared state ---------------------------------------------------------------------------------------------------------

/// Everything a handler needs.
pub struct Shared {
    tools: BTreeMap<ToolName, Arc<dyn ToolBackend>>,
    ctx: Arc<ToolContext>,
    log: Arc<dyn LogSink>,
    max_result_chars: usize,
    limits: Limits,
    permits: Arc<Semaphore>,
    calls: Mutex<HashMap<String, Vec<(u64, CancelToken)>>>,
    next_call: AtomicU64,
    header_noted: AtomicBool,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("tools", &self.listed())
            .finish_non_exhaustive()
    }
}

impl Shared {
    /// A server over these backends with the default limits. A second backend for a tool that already has one is left out (the first stays).
    /// `max_result_chars` is the profile's `maxResultChars`.
    pub fn new(
        backends: Vec<Box<dyn ToolBackend>>,
        ctx: ToolContext,
        log: Arc<dyn LogSink>,
        max_result_chars: usize,
    ) -> Arc<Self> {
        Self::with_limits(backends, ctx, log, max_result_chars, Limits::default())
    }

    pub fn with_limits(
        backends: Vec<Box<dyn ToolBackend>>,
        ctx: ToolContext,
        log: Arc<dyn LogSink>,
        max_result_chars: usize,
        limits: Limits,
    ) -> Arc<Self> {
        let mut tools: BTreeMap<ToolName, Arc<dyn ToolBackend>> = BTreeMap::new();
        for b in backends {
            tools.entry(b.name()).or_insert_with(|| Arc::from(b));
        }
        Arc::new(Self {
            tools,
            ctx: Arc::new(ctx),
            log,
            max_result_chars,
            limits,
            permits: Arc::new(Semaphore::new(MAX_INFLIGHT)),
            calls: Mutex::new(HashMap::new()),
            next_call: AtomicU64::new(0),
            header_noted: AtomicBool::new(false),
        })
    }

    /// The bare names of the tools the server lists, in the order of the table.
    pub fn listed(&self) -> Vec<&'static str> {
        harness_protocol::tools::TOOLS
            .iter()
            .filter(|t| self.tools.contains_key(&t.name))
            .map(|t| t.name.as_str())
            .collect()
    }

    pub fn stats(&self) -> Stats {
        self.ctx.counters.snapshot()
    }

    pub fn counters(&self) -> &Arc<Counters> {
        &self.ctx.counters
    }

    fn say(&self, line: &str) {
        self.log.line(&format!("direct tools {line}"));
    }

    // ---- the calls in flight, by (client program, request id) ----------------------------------------------------------------

    fn register(&self, key: &str, no: u64, token: CancelToken) {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.to_string())
            .or_default()
            .push((no, token));
    }

    fn unregister(&self, key: &str, no: u64) {
        let mut calls = self.calls.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(list) = calls.get_mut(key) {
            list.retain(|(n, _)| *n != no);
            if list.is_empty() {
                calls.remove(key);
            }
        }
    }

    /// Fires the token of every call with this key. Nothing else: a cancel for a call that is not in flight is forgotten, so it cannot cancel
    /// a later call that gets the same number (a new session of the engine counts from 0 again).
    fn cancel_matching(&self, key: &str) {
        let tokens: Vec<CancelToken> = self
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .map(|l| l.iter().map(|(_, t)| t.clone()).collect())
            .unwrap_or_default();
        for t in tokens {
            t.cancel();
        }
    }

    /// Fires the token of every call in flight: the serving task ended, or the server is stopping.
    pub fn cancel_all(&self) {
        let tokens: Vec<CancelToken> = self
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .flat_map(|l| l.iter().map(|(_, t)| t.clone()))
            .collect();
        for t in tokens {
            t.cancel();
        }
    }

    // ---- reading a message -----------------------------------------------------------------------------------------------------

    fn note_protocol_header(&self, headers: &HeaderMap) {
        let Some(v) = headers.get("mcp-protocol-version") else {
            return;
        };
        if v.to_str().is_ok_and(|s| SUPPORTED_VERSIONS.contains(&s)) {
            return;
        }
        self.ctx.counters.record_unknown_protocol_header();
        if !self.header_noted.swap(true, Ordering::SeqCst) {
            self.say("protocol version header not known; served as the highest version (counted, logged once)");
        }
    }

    fn protocol_error(&self, status: u16, code: i64, message: &str) -> Response {
        self.ctx.counters.record_protocol_error();
        rpc_error(status, Value::Null, code, message)
    }

    async fn handle(self: &Arc<Self>, agent: &str, bytes: &[u8]) -> Response {
        let message: Value = match serde_json::from_slice(bytes) {
            Ok(v) => v,
            Err(_) => return self.protocol_error(400, -32700, "parse error"),
        };
        match message {
            Value::Array(_) => self.protocol_error(400, -32600, "batches are not supported"),
            Value::Object(o) => self.envelope(agent, o).await,
            _ => self.protocol_error(400, -32600, "not a JSON-RPC message"),
        }
    }

    async fn envelope(self: &Arc<Self>, agent: &str, o: Map<String, Value>) -> Response {
        if o.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return self.protocol_error(400, -32600, "not a JSON-RPC 2.0 message");
        }
        match (o.get("method"), o.get("id")) {
            (Some(Value::String(method)), None) => {
                self.notification(agent, method, o.get("params"));
                accepted()
            }
            (Some(Value::String(method)), Some(id)) if valid_id(id) => {
                self.request(agent, id.clone(), method, o.get("params"))
                    .await
            }
            (Some(_), _) => self.protocol_error(400, -32600, "invalid request"),
            (None, _) if o.contains_key("result") || o.contains_key("error") => accepted(),
            (None, _) => self.protocol_error(400, -32600, "invalid request"),
        }
    }

    fn notification(&self, agent: &str, method: &str, params: Option<&Value>) {
        if method != "notifications/cancelled" {
            return;
        }
        if let Some(id) = params
            .and_then(|p| p.get("requestId"))
            .filter(|id| valid_id(id))
        {
            self.cancel_matching(&call_key(agent, id));
        }
    }

    async fn request(
        self: &Arc<Self>,
        agent: &str,
        id: Value,
        method: &str,
        params: Option<&Value>,
    ) -> Response {
        match method {
            "initialize" => self.initialize(id, params),
            "ping" => rpc_ok(id, json!({})),
            "tools/list" => {
                self.ctx.counters.record_list();
                rpc_ok(id, self.list_result())
            }
            "tools/call" => self.call(agent, id, params).await,
            _ => rpc_error(200, id, -32601, "method not found"),
        }
    }

    fn initialize(&self, id: Value, params: Option<&Value>) -> Response {
        let Some(asked) = params
            .and_then(|p| p.get("protocolVersion"))
            .and_then(Value::as_str)
        else {
            return rpc_error(200, id, -32602, "invalid params");
        };
        let version = SUPPORTED_VERSIONS
            .iter()
            .copied()
            .find(|v| *v == asked)
            .unwrap_or(SUPPORTED_VERSIONS[0]);
        self.ctx.counters.record_initialize();
        rpc_ok(
            id,
            json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
            }),
        )
    }

    /// The table's `tools/list` result with only the tools this server has a backend for.
    fn list_result(&self) -> Value {
        let all = tools_list_result();
        let tools: Vec<Value> = all
            .get("tools")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|t| {
                        t.get("name")
                            .and_then(Value::as_str)
                            .and_then(ToolName::from_word)
                            .is_some_and(|n| self.tools.contains_key(&n))
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        json!({ "tools": tools })
    }

    // ---- a call ----------------------------------------------------------------------------------------------------------------

    async fn call(self: &Arc<Self>, agent: &str, id: Value, params: Option<&Value>) -> Response {
        let Some(p) = params.and_then(Value::as_object) else {
            return rpc_error(200, id, -32602, "invalid params");
        };
        let Some(name) = p.get("name").and_then(Value::as_str) else {
            return rpc_error(200, id, -32602, "invalid params");
        };
        let Some((tool, backend)) =
            ToolName::from_word(name).and_then(|t| self.tools.get(&t).map(|b| (t, Arc::clone(b))))
        else {
            return rpc_error(200, id, -32602, "unknown tool");
        };
        let counters = &self.ctx.counters;
        counters.record_call(tool);
        let started = Instant::now();
        let arguments = match p.get("arguments") {
            None | Some(Value::Null) => json!({}),
            Some(v) => v.clone(),
        };
        if let Err(e) = validate_input(tool, &arguments) {
            let out = ToolOutput::failed(FailureWord::Usage, e.to_string());
            return self.finish(id, tool, &out, started, "usage", false);
        }
        let Ok(permit) = self.permits.clone().try_acquire_owned() else {
            let out =
                ToolOutput::failed(FailureWord::Busy, "too many calls at once; retry shortly");
            return self.finish(id, tool, &out, started, "busy", false);
        };

        let token = CancelToken::new();
        let key = call_key(agent, &id);
        let no = self.next_call.fetch_add(1, Ordering::SeqCst);
        self.register(&key, no, token.clone());
        let mut guard = CallGuard {
            shared: Arc::clone(self),
            key,
            no,
            token: token.clone(),
            abort: None,
            _permit: permit,
        };
        let (ctx, cancel) = (Arc::clone(&self.ctx), token.clone());
        let mut task = tokio::spawn(async move { backend.call(&ctx, arguments, cancel).await });
        guard.abort = Some(task.abort_handle());

        let first = tokio::select! {
            r = &mut task => Ended::Task(r),
            () = token.cancelled() => Ended::Cancelled,
            () = tokio::time::sleep(self.limits.call_backstop) => Ended::Backstop,
        };
        let ended = match first {
            Ended::Task(Ok(out)) => Ended::Output(out),
            Ended::Task(Err(e)) if e.is_panic() => Ended::Fault,
            Ended::Task(Err(_)) => Ended::Cancelled,
            other => {
                // The token fires (it already has for a cancel) and the backend has its grace to end its children; then the task is aborted.
                token.cancel();
                if tokio::time::timeout(self.limits.cancel_grace, &mut task)
                    .await
                    .is_err()
                {
                    task.abort();
                    let _ = (&mut task).await;
                }
                other
            }
        };
        drop(guard);
        match ended {
            Ended::Output(out) => {
                let word = out_word(&out);
                self.finish(id, tool, &out, started, word, true)
            }
            Ended::Fault => {
                let out = ToolOutput::failed(
                    FailureWord::Unavailable,
                    "internal fault; do not retry, the call may have had effects",
                );
                self.finish(id, tool, &out, started, "fault", true)
            }
            Ended::Backstop => {
                let out = ToolOutput::failed(
                    FailureWord::Timeout,
                    "the call ran past the limit of the server; narrow the request",
                );
                self.finish(id, tool, &out, started, "timeout", true)
            }
            Ended::Cancelled | Ended::Task(_) => {
                counters.record_cancelled();
                self.say(&format!("call tool={} result=cancelled", tool.as_str()));
                rpc_error(200, id, CODE_CANCELLED, "request cancelled")
            }
        }
    }

    /// Renders `out`, counts it and logs it: the one place where a call's answer is made.
    fn finish(
        &self,
        id: Value,
        tool: ToolName,
        out: &ToolOutput,
        started: Instant,
        word: &str,
        ran: bool,
    ) -> Response {
        let rendered = render(out, self.max_result_chars);
        let counters = &self.ctx.counters;
        match rendered.failure {
            Some(w) if w.is_refusal() => counters.record_refused(),
            Some(_) => counters.record_failed(),
            None => {}
        }
        if rendered.truncated {
            counters.record_truncated();
        }
        let elapsed = started.elapsed();
        if ran {
            counters.record_latency(elapsed);
        }
        self.say(&format!(
            "call tool={} result={word} ms={} bytes={}{}",
            tool.as_str(),
            elapsed.as_millis(),
            rendered.text_bytes,
            if rendered.truncated { " truncated" } else { "" },
        ));
        debug_assert!(
            serialized_len(&rendered.value)
                <= self
                    .max_result_chars
                    .max(crate::direct::tools::result::MIN_RESULT_CHARS)
        );
        rpc_ok(id, rendered.value)
    }
}

/// How a call ended.
enum Ended {
    Task(Result<ToolOutput, tokio::task::JoinError>),
    Output(ToolOutput),
    Fault,
    Cancelled,
    Backstop,
}

/// The word a backend's answer goes in the log under.
fn out_word(out: &ToolOutput) -> &'static str {
    match out {
        ToolOutput::Text { .. } => "ok",
        ToolOutput::Failed { word, .. } => word.as_str(),
    }
}

/// Ends the registration of a call, and the call itself when the handler is dropped before it finished (the client hung up): the token fires and
/// the task is aborted, which drops the backend's future and with it the children its guards hold.
struct CallGuard {
    shared: Arc<Shared>,
    key: String,
    no: u64,
    token: CancelToken,
    abort: Option<AbortHandle>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        self.shared.unregister(&self.key, self.no);
        self.token.cancel();
        if let Some(a) = self.abort.take() {
            a.abort();
        }
    }
}

// ---- JSON-RPC ----------------------------------------------------------------------------------------------------------------

/// A usable request id: a string, or an integer.
fn valid_id(id: &Value) -> bool {
    match id {
        Value::String(_) => true,
        Value::Number(n) => n.is_i64() || n.is_u64(),
        _ => false,
    }
}

/// The key a cancel is matched on: the client program and the id, as JSON (so `5` and `"5"` are different requests).
fn call_key(agent: &str, id: &Value) -> String {
    let agent: String = agent.chars().take(MAX_AGENT_CHARS).collect();
    format!("{agent}\u{1f}{id}")
}

fn json_response(status: u16, v: &Value) -> Response {
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        [(header::CONTENT_TYPE, "application/json")],
        v.to_string(),
    )
        .into_response()
}

fn rpc_ok(id: Value, result: Value) -> Response {
    json_response(200, &json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn rpc_error(status: u16, id: Value, code: i64, message: &str) -> Response {
    json_response(
        status,
        &json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    )
}

/// 202 with no body: a notification or a response of the client was taken.
fn accepted() -> Response {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = StatusCode::ACCEPTED;
    r
}

fn forbidden() -> Response {
    rpc_error(403, Value::Null, -32000, "forbidden")
}

// ---- loopback ----------------------------------------------------------------------------------------------------------------

/// Whether the connection's peer is on this machine.
pub fn is_loopback_peer(peer: &SocketAddr) -> bool {
    peer.ip().to_canonical().is_loopback()
}

/// Whether `value` is the `Host` of a request for this machine: `localhost`, an IPv4 address of 127.0.0.0/8, or a bracketed IPv6 loopback, with
/// an optional decimal port. The reading is strict: only letters, digits and `.:-[]` are allowed at all, so a name such as
/// `localhost.example.com`, a user part, a path, a query or a fragment never passes.
pub fn is_loopback_host(value: &str) -> bool {
    if value.is_empty()
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-' | '[' | ']'))
    {
        return false;
    }
    let (host, port) = if let Some(rest) = value.strip_prefix('[') {
        let Some((inner, after)) = rest.split_once(']') else {
            return false;
        };
        let port = match after {
            "" => None,
            _ => match after.strip_prefix(':') {
                Some(p) => Some(p),
                None => return false,
            },
        };
        return Ipv6Addr::from_str(inner).is_ok_and(|a| IpAddr::V6(a).to_canonical().is_loopback())
            && port.is_none_or(valid_port);
    } else {
        match value.split_once(':') {
            None => (value, None),
            Some((h, p)) => (h, Some(p)),
        }
    };
    if port.is_some_and(|p| !valid_port(p)) {
        return false;
    }
    host.eq_ignore_ascii_case("localhost")
        || Ipv4Addr::from_str(host).is_ok_and(|a| a.is_loopback())
}

fn valid_port(p: &str) -> bool {
    !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok()
}

/// Whether `value` is the `Origin` of a page served from this machine: `http://` or `https://` and a loopback host as [`is_loopback_host`] reads it.
pub fn is_loopback_origin(value: &str) -> bool {
    value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .is_some_and(is_loopback_host)
}

async fn gate(ConnectInfo(peer): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    if !is_loopback_peer(&peer) {
        return forbidden();
    }
    let hosts = req.headers().get_all(header::HOST);
    let host_ok = hosts.iter().count() == 1
        && hosts
            .iter()
            .next()
            .and_then(|v| v.to_str().ok())
            .is_some_and(is_loopback_host);
    if !host_ok {
        return forbidden();
    }
    if let Some(origin) = req.headers().get(header::ORIGIN)
        && !origin.to_str().ok().is_some_and(is_loopback_origin)
    {
        return forbidden();
    }
    next.run(req).await
}

// ---- the endpoint --------------------------------------------------------------------------------------------------------------

enum ReadError {
    TooLarge,
    Failed,
}

async fn read_limited(body: &mut Body, limit: usize) -> Result<Vec<u8>, ReadError> {
    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| ReadError::Failed)?;
        if let Ok(data) = frame.into_data() {
            if out.len() + data.len() > limit {
                return Err(ReadError::TooLarge);
            }
            out.extend_from_slice(&data);
        }
    }
    Ok(out)
}

/// Reads and throws away what is left of a body that was refused (bounded in bytes and in time).
async fn drain(body: &mut Body) {
    let _ = tokio::time::timeout(DRAIN_TIME, async {
        let mut read = 0usize;
        while let Some(Ok(frame)) = body.frame().await {
            if let Ok(data) = frame.into_data() {
                read += data.len();
                if read > DRAIN_MAX_BYTES {
                    break;
                }
            }
        }
    })
    .await;
}

fn closing(mut r: Response) -> Response {
    r.headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    r
}

async fn mcp(State(sh): State<Arc<Shared>>, req: Request) -> Response {
    let (parts, mut body) = req.into_parts();
    sh.note_protocol_header(&parts.headers);
    let agent = parts
        .headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let declared = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<usize>().ok());
    if declared.is_some_and(|n| n > MAX_BODY_BYTES) {
        drain(&mut body).await;
        return closing(sh.protocol_error(413, -32600, "request too large"));
    }
    let bytes = match tokio::time::timeout(
        sh.limits.body_timeout,
        read_limited(&mut body, MAX_BODY_BYTES),
    )
    .await
    {
        Err(_) => {
            return closing(rpc_error(408, Value::Null, -32600, "request timed out"));
        }
        Ok(Err(ReadError::TooLarge)) => {
            drain(&mut body).await;
            return closing(sh.protocol_error(413, -32600, "request too large"));
        }
        Ok(Err(ReadError::Failed)) => {
            return closing(sh.protocol_error(400, -32600, "request body could not be read"));
        }
        Ok(Ok(b)) => b,
    };
    sh.handle(&agent, &bytes).await
}

async fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, HeaderValue::from_static("POST"))],
    )
        .into_response()
}

async fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/mcp", post(mcp).fallback(method_not_allowed))
        .fallback(not_found)
        .layer(middleware::from_fn(gate))
        .with_state(shared)
}

// ---- running it ----------------------------------------------------------------------------------------------------------------

/// Whether the server serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerState {
    Up,
    /// Not serving for good, with one kebab word for why: `not-loopback` (the listener was not on a loopback address), `socket` (the bound
    /// socket could not be shared with a new task) or `restarts-used` (the serving task ended a fourth time).
    Down(&'static str),
}

struct Control {
    stop: watch::Sender<bool>,
    end: watch::Sender<u64>,
    abort: Mutex<Option<AbortHandle>>,
    state: Mutex<ServerState>,
    restarts: AtomicU32,
}

impl Control {
    fn new(state: ServerState) -> Arc<Self> {
        Arc::new(Self {
            stop: watch::channel(false).0,
            end: watch::channel(0).0,
            abort: Mutex::new(None),
            state: Mutex::new(state),
            restarts: AtomicU32::new(0),
        })
    }

    fn set_down(&self, why: &'static str) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = ServerState::Down(why);
    }
}

/// A running server.
#[derive(Debug)]
pub struct ServerHandle {
    addr: SocketAddr,
    ctl: Arc<Control>,
    shared: Arc<Shared>,
    task: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Control").finish_non_exhaustive()
    }
}

impl ServerHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn state(&self) -> ServerState {
        *self.ctl.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How many times the serving task has been started again.
    pub fn restarts(&self) -> u32 {
        self.ctl.restarts.load(Ordering::SeqCst)
    }

    /// Whether a ping to this address is answered the way this server answers it (see [`self_check_addr`]).
    pub async fn self_check(&self) -> bool {
        self_check_addr(self.addr).await
    }

    /// Ends the serving task as a panic would (test hook): the task is aborted, calls in flight are cancelled, a new task serves the same socket.
    #[doc(hidden)]
    pub fn crash_for_tests(&self) {
        if let Some(a) = self
            .ctl
            .abort
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            a.abort();
        }
    }

    /// Ends the serving task through its graceful shutdown (test hook): idle connections are closed, so a client has to connect again.
    #[doc(hidden)]
    pub fn restart_gracefully_for_tests(&self) {
        self.ctl.end.send_modify(|n| *n += 1);
    }

    /// Stops serving: calls in flight are cancelled, the port is released, and the task is aborted if it has not drained in five seconds.
    pub async fn stop(mut self) {
        self.ctl.stop.send_replace(true);
        self.shared.cancel_all();
        if let Some(mut t) = self.task.take()
            && tokio::time::timeout(STOP_GRACE + Duration::from_secs(2), &mut t)
                .await
                .is_err()
        {
            t.abort();
        }
    }
}

/// Serves `shared` on `listener`, which must be bound to a loopback address; a listener on any other address is closed at once and nothing is
/// served (the state is `Down("not-loopback")`). The supervisor keeps the bound socket and serves a clone of it; see the module text.
pub fn start(listener: tokio::net::TcpListener, shared: Arc<Shared>) -> ServerHandle {
    let addr = listener
        .local_addr()
        .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 0)));
    let down = |why: &'static str, shared: Arc<Shared>| ServerHandle {
        addr,
        ctl: Control::new(ServerState::Down(why)),
        shared,
        task: None,
    };
    if !addr.ip().to_canonical().is_loopback() {
        drop(listener);
        shared.say("not started: the listener is not on a loopback address");
        return down("not-loopback", shared);
    }
    let master = match listener.into_std() {
        Ok(m) => m,
        Err(_) => {
            shared.say("not started: the listener could not be taken over");
            return down("socket", shared);
        }
    };
    let ctl = Control::new(ServerState::Up);
    let task = tokio::spawn(supervise(master, shared.clone(), ctl.clone(), addr));
    ServerHandle {
        addr,
        ctl,
        shared,
        task: Some(task),
    }
}

/// Runs the serving task and starts it again when it ends without a stop (see the module text).
async fn supervise(
    master: std::net::TcpListener,
    shared: Arc<Shared>,
    ctl: Arc<Control>,
    addr: SocketAddr,
) {
    let mut used: u32 = 0;
    loop {
        let listener = match master
            .try_clone()
            .and_then(tokio::net::TcpListener::from_std)
        {
            Ok(l) => l,
            Err(_) => {
                ctl.set_down("socket");
                shared
                    .say("stopped for good: the bound socket could not be shared with a new task");
                return;
            }
        };
        let app = router(shared.clone()).into_make_service_with_connect_info::<SocketAddr>();
        let (mut stop, mut end) = (ctl.stop.subscribe(), ctl.end.subscribe());
        let mut stop_wait = ctl.stop.subscribe();
        let mut serve = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    tokio::select! {
                        _ = stop.wait_for(|s| *s) => {},
                        _ = end.changed() => {},
                    }
                })
                .await;
        });
        *ctl.abort.lock().unwrap_or_else(|e| e.into_inner()) = Some(serve.abort_handle());
        if used > 0 {
            ctl.restarts.store(used, Ordering::SeqCst);
            shared.ctx.counters.record_restart();
            shared.say(&format!("restarted on {addr} ({used} of {MAX_RESTARTS})"));
        }
        tokio::select! {
            _ = &mut serve => {},
            () = async {
                let _ = stop_wait.wait_for(|s| *s).await;
                tokio::time::sleep(STOP_GRACE).await;
            } => {
                serve.abort();
                let _ = (&mut serve).await;
            }
        }
        if *ctl.stop.borrow() {
            return;
        }
        // The serving task ended on its own. Whatever it was serving ends with it: a call is never replayed and never outlives its task.
        shared.cancel_all();
        if used >= MAX_RESTARTS {
            ctl.set_down("restarts-used");
            shared.say(&format!(
                "stopped for good: the serving task ended {} times",
                MAX_RESTARTS + 1
            ));
            return;
        }
        used += 1;
    }
}

/// Binds `127.0.0.1:port`, else any free port on `127.0.0.1` when the port is taken. Port 0 asks for any free port at once.
pub async fn bind(port: u16) -> Result<tokio::net::TcpListener, String> {
    match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => Ok(l),
        Err(e) if port != 0 && e.kind() == std::io::ErrorKind::AddrInUse => {
            tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .map_err(|e| format!("tools: {}", e.kind()))
        }
        Err(e) => Err(format!("tools: {}", e.kind())),
    }
}

/// Whether the server at `addr` is this server: a fresh connection, a `ping`, and the answer a ping gets here (200, the same id, an empty
/// result). A port that something else holds, or that answers with something else, fails. The hook asks it after `start` and before it writes
/// the engine's settings file, so that the file never names a port that another process serves.
pub async fn self_check_addr(addr: SocketAddr) -> bool {
    let probe = async {
        let body = r#"{"jsonrpc":"2.0","id":"self-check","method":"ping"}"#;
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut s = tokio::net::TcpStream::connect(addr).await.ok()?;
        s.write_all(request.as_bytes()).await.ok()?;
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.ok()?;
        let text = String::from_utf8_lossy(&out);
        let (head, body) = text.split_once("\r\n\r\n")?;
        if !head.starts_with("HTTP/1.1 200") {
            return None;
        }
        let v: Value = serde_json::from_str(body).ok()?;
        (v.get("id") == Some(&json!("self-check")) && v.get("result") == Some(&json!({})))
            .then_some(())
    };
    matches!(
        tokio::time::timeout(Duration::from_secs(2), probe).await,
        Ok(Some(()))
    )
}
