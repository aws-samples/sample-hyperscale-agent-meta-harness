//! Support for the tool-server tests: a raw HTTP client that starts no process, a JSON-RPC helper, the rig that serves the echo
//! backend on a loopback port, and the check that a child process is gone. Later packages of the tool layer (the hook, `code_search`) reuse it.
#![allow(dead_code, clippy::unwrap_used)]

#[path = "../../examples/tools_server_support/echo.rs"]
pub mod echo;

use agentd::direct::tools::backend::{ToolBackend, ToolContext};
use agentd::direct::tools::server::{self, Limits, ServerHandle, Shared};
use agentd::direct::tools::stats::Counters;
use agentd::logging::MemoryLog;
use echo::{Echo, EchoState};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---- a client that starts no process --------------------------------------------------------------------------------------------

/// One HTTP answer.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    /// Names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {:?}", self.body))
    }
}

/// Reads one HTTP/1.1 answer from `s` until the server closes the connection.
async fn read_reply(s: &mut tokio::net::TcpStream) -> Reply {
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap_or_default();
    parse_reply(&out)
}

fn parse_reply(out: &[u8]) -> Reply {
    let text = String::from_utf8_lossy(out).into_owned();
    assert!(text.len() > 12, "no answer: {text:?}");
    let status = text[9..12].parse().unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let headers: Vec<(String, String)> = head
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    // No answer of the server ever names a session, whatever the request was.
    assert!(
        headers.iter().all(|(k, _)| k != "mcp-session-id"),
        "the server issued a session id: {headers:?}"
    );
    Reply {
        status,
        headers,
        body: body.to_string(),
    }
}

/// One request on a fresh connection that closes after the answer. `headers` replaces the defaults it names (Host, Content-Type, Accept).
pub async fn raw(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Reply {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut head = format!("{method} {path} HTTP/1.1\r\n");
    let given = |n: &str| headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(n));
    if !given("host") {
        head.push_str(&format!("Host: {addr}\r\n"));
    }
    if !given("content-type") {
        head.push_str("Content-Type: application/json\r\n");
    }
    if !given("accept") {
        head.push_str("Accept: application/json, text/event-stream\r\n");
    }
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    s.write_all(head.as_bytes()).await.unwrap();
    s.write_all(body).await.unwrap();
    read_reply(&mut s).await
}

/// A JSON-RPC message posted to `/mcp`.
pub async fn rpc(addr: SocketAddr, message: &Value) -> Reply {
    rpc_with(addr, &[], message).await
}

pub async fn rpc_with(addr: SocketAddr, headers: &[(&str, &str)], message: &Value) -> Reply {
    raw(
        addr,
        "POST",
        "/mcp",
        headers,
        message.to_string().as_bytes(),
    )
    .await
}

pub fn request(id: Value, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

/// `tools/call` of `code_search` with this `pattern`.
pub fn search(id: Value, pattern: &str) -> Value {
    request(
        id,
        "tools/call",
        json!({"name": "code_search", "arguments": {"pattern": pattern}}),
    )
}

/// The text of the first content block of a `tools/call` answer.
pub fn text_of(answer: &Value) -> String {
    answer["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no content text in {answer}"))
        .to_string()
}

// ---- processes -------------------------------------------------------------------------------------------------------------------

/// Whether `pid` is gone: no `/proc` entry, or a zombie that only waits to be reaped.
pub fn gone(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.trim_start().chars().next())
            .is_some_and(|state| state == 'Z' || state == 'X'),
    }
}

/// Polls `check` every 20 ms until it holds or `within` has passed; returns how long it took, or `None`.
pub async fn until(within: Duration, mut check: impl FnMut() -> bool) -> Option<Duration> {
    let t = Instant::now();
    loop {
        if check() {
            return Some(t.elapsed());
        }
        if t.elapsed() > within {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Whether nothing answers at `addr` (a refused connection). It tries again for up to three seconds: a process that another test forks at that
/// moment holds a copy of every open socket until its `exec` closes the copy, and a listener that was closed a moment ago can still accept
/// then, so one attempt right after a close proves nothing about the server.
pub async fn refuses(addr: SocketAddr) -> bool {
    let t = Instant::now();
    loop {
        if tokio::net::TcpStream::connect(addr).await.is_err() {
            return true;
        }
        if t.elapsed() > Duration::from_secs(3) {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---- the rig ---------------------------------------------------------------------------------------------------------------------

pub struct Rig {
    pub shared: Arc<Shared>,
    pub handle: Option<ServerHandle>,
    pub addr: SocketAddr,
    pub log: Arc<MemoryLog>,
    pub echo: Arc<EchoState>,
    pub work: PathBuf,
}

impl Rig {
    /// The echo backend, the profile's cap of 7,500 characters and the default limits.
    pub async fn start() -> Self {
        Self::with(7_500, Limits::default(), true).await
    }

    /// No backend at all.
    pub async fn empty() -> Self {
        Self::with(7_500, Limits::default(), false).await
    }

    pub async fn with(cap: usize, limits: Limits, with_echo: bool) -> Self {
        let work = super::common::scratch("tools-work");
        let log = MemoryLog::new();
        let (backends, echo): (Vec<Box<dyn ToolBackend>>, Arc<EchoState>) = if with_echo {
            let (b, s) = Echo::new();
            (vec![b], s)
        } else {
            (Vec::new(), Arc::new(EchoState::default()))
        };
        let ctx = ToolContext::for_directory(work.clone(), Counters::new());
        let shared = Shared::with_limits(backends, ctx, log.clone(), cap, limits);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let handle = server::start(listener, shared.clone());
        let addr = handle.addr();
        Self {
            shared,
            handle: Some(handle),
            addr,
            log,
            echo,
            work,
        }
    }

    pub fn handle(&self) -> &ServerHandle {
        self.handle.as_ref().unwrap()
    }

    pub async fn stop(mut self) {
        if let Some(h) = self.handle.take() {
            h.stop().await;
        }
    }

    /// Every log line, joined.
    pub fn log_text(&self) -> String {
        self.log.lines().join("\n")
    }
}
