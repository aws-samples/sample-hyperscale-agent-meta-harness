//! The fake engine's MCP client: the part of the pinned engine that reads its tool servers from a settings
//! file and calls them, played from a transcript so that agentd's tool hook can be run end to end with no real engine.
//!
//! The pinned engine reads the file named by `HAMH_FAKE_MCP_SETTINGS` when its first `session/prompt` of a session arrives,
//! connects to every server in it (`initialize`, `notifications/initialized`, `tools/list`), and offers the tools to the
//! model as `<server>__<tool>`. A transcript drives the same steps with tool calls whose titles carry the directive:
//!
//! | title of a transcript tool call | what the fake engine does |
//! |---|---|
//! | `mcp-connect: none \| handshake-only \| full` | how far the connect goes at the first prompt of the session: nothing at all (a file the engine skipped), `initialize` only, or `initialize` and `tools/list` (the default when a prompt holds any `mcp` line). The pair of messages (the call and its update) is removed from the replay; it is a directive, not a tool call |
//! | `mcp-settings: edit \| delete` | what the engine does to its file after it connected: rewrite it with other content or delete it. The pair is removed too |
//! | `mcp: ham/<tool> <json arguments>` | a `tools/call` of that tool with those arguments, through the connection the session made. The update of the call completes with the server's text, or fails with the JSON-RPC error's message; a call made when the connect did not reach `tools/list` fails with `tool not available` and sends nothing |
//!
//! A prompt that holds none of the three does not touch the file or the network. The log lines it adds (the engine's own log, `FAKE_ACP_LOG`) say what the
//! engine did and with which counts, never the text of a result:
//! `mcp connect <level> url=<url>` / `mcp connect <level> unreadable-settings`, `mcp initialize <status>`, `mcp tools/list <status> tools=<n>`,
//! `mcp call <tool> completed|failed|not-connected chars=<n>`, `mcp settings edited|deleted`.
//!
//! Tests: `crates/ham-e2e/tests/fake_acp_mcp.rs` (this client against a small HTTP server of the test) and `crates/agentd/tests/direct_tools_mcp_run.rs`
//! (against agentd's real tool server through a whole run).

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

/// How far a session's connect went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    None,
    HandshakeOnly,
    Full,
}

impl Level {
    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "none" => Some(Self::None),
            "handshake-only" => Some(Self::HandshakeOnly),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::HandshakeOnly => "handshake-only",
            Self::Full => "full",
        }
    }
}

/// The engine's connections: one per session, made at that session's first prompt that holds an `mcp` line.
#[derive(Debug, Default)]
pub struct McpSim {
    settings: Option<PathBuf>,
    /// session id -> how far its connect went and where the server is.
    sessions: HashMap<String, (Level, Option<String>)>,
    next_id: u64,
}

fn title_of(m: &Value) -> Option<(&str, &str)> {
    let u = m.pointer("/params/update")?;
    (u.get("sessionUpdate").and_then(Value::as_str) == Some("tool_call")).then_some(())?;
    Some((
        u.get("toolCallId").and_then(Value::as_str)?,
        u.get("title").and_then(Value::as_str)?,
    ))
}

impl McpSim {
    /// The client of a process: the settings file is `HAMH_FAKE_MCP_SETTINGS`.
    pub fn from_env() -> Self {
        Self::new(
            std::env::var("HAMH_FAKE_MCP_SETTINGS")
                .ok()
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
        )
    }

    pub fn new(settings: Option<PathBuf>) -> Self {
        Self {
            settings,
            ..Self::default()
        }
    }

    /// Plays the `mcp` lines of one prompt's replay `out` for the session `sid`. `say` receives the log lines.
    pub fn resolve(&mut self, out: &mut Vec<Value>, sid: &str, say: &mut dyn FnMut(&str)) {
        let mut level: Option<Level> = None;
        let mut settings_act: Option<String> = None;
        let mut directive_ids: Vec<String> = Vec::new();
        let mut calls: Vec<(String, String, Value)> = Vec::new();
        for m in out.iter() {
            let Some((id, title)) = title_of(m) else {
                continue;
            };
            if let Some(word) = title.strip_prefix("mcp-connect: ") {
                level = Level::parse(word);
                directive_ids.push(id.to_string());
            } else if let Some(act) = title.strip_prefix("mcp-settings: ") {
                settings_act = Some(act.trim().to_string());
                directive_ids.push(id.to_string());
            } else if let Some(rest) = title.strip_prefix("mcp: ") {
                let (target, args) = rest.split_once(' ').unwrap_or((rest, "{}"));
                let tool = target.strip_prefix("ham/").unwrap_or(target);
                calls.push((
                    id.to_string(),
                    tool.to_string(),
                    serde_json::from_str(args).unwrap_or_else(|_| json!({})),
                ));
            }
        }
        if directive_ids.is_empty() && calls.is_empty() {
            return;
        }
        // A directive is not a tool call: neither message of its pair reaches the client.
        out.retain(|m| {
            let id = m
                .pointer("/params/update/toolCallId")
                .and_then(Value::as_str);
            !id.is_some_and(|id| directive_ids.iter().any(|d| d == id))
        });
        if !self.sessions.contains_key(sid) {
            let want = level.unwrap_or(Level::Full);
            let connected = self.connect(want, say);
            self.sessions.insert(sid.to_string(), connected);
        }
        if let Some(act) = settings_act {
            self.act_on_settings(&act, say);
        }
        let (reached, url) = self
            .sessions
            .get(sid)
            .cloned()
            .unwrap_or((Level::None, None));
        for m in out.iter_mut() {
            let Some(u) = m.pointer_mut("/params/update") else {
                continue;
            };
            if u.get("sessionUpdate").and_then(Value::as_str) != Some("tool_call_update") {
                continue;
            }
            let Some((_, tool, args)) = u
                .get("toolCallId")
                .and_then(Value::as_str)
                .and_then(|id| calls.iter().find(|(c, _, _)| c == id))
            else {
                continue;
            };
            let text =
                |t: &str| json!([{"type": "content", "content": {"type": "text", "text": t}}]);
            let (status, body) = match (reached, &url) {
                (Level::Full, Some(url)) => self.call(url, tool, args, say),
                _ => {
                    say(&format!("mcp call {tool} not-connected chars=0"));
                    ("failed", format!("tool not available: ham__{tool}"))
                }
            };
            u["status"] = json!(status);
            u["content"] = text(&body);
        }
    }

    fn connect(&mut self, want: Level, say: &mut dyn FnMut(&str)) -> (Level, Option<String>) {
        if want == Level::None {
            say("mcp connect none");
            return (Level::None, None);
        }
        let Some(url) = self.url() else {
            say("mcp connect none unreadable-settings");
            return (Level::None, None);
        };
        say(&format!("mcp connect {} url={url}", want.word()));
        let init = json!({"jsonrpc": "2.0", "id": self.id(), "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "fake-acp-engine", "version": "0.1.0"}}});
        let status = post(&url, &init).map_or(0, |(s, _)| s);
        say(&format!("mcp initialize {status}"));
        if status != 200 {
            return (Level::None, Some(url));
        }
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let _ = post(&url, &note);
        if want == Level::HandshakeOnly {
            return (Level::HandshakeOnly, Some(url));
        }
        let list = json!({"jsonrpc": "2.0", "id": self.id(), "method": "tools/list", "params": {}});
        let (status, body) = post(&url, &list).unwrap_or((0, String::new()));
        let tools = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| {
                v.pointer("/result/tools")
                    .and_then(Value::as_array)
                    .map(Vec::len)
            })
            .unwrap_or(0);
        say(&format!("mcp tools/list {status} tools={tools}"));
        if status == 200 {
            (Level::Full, Some(url))
        } else {
            (Level::HandshakeOnly, Some(url))
        }
    }

    fn call(
        &mut self,
        url: &str,
        tool: &str,
        args: &Value,
        say: &mut dyn FnMut(&str),
    ) -> (&'static str, String) {
        let req = json!({"jsonrpc": "2.0", "id": self.id(), "method": "tools/call", "params": {"name": tool, "arguments": args}});
        let answer = post(url, &req)
            .ok()
            .and_then(|(_, b)| serde_json::from_str::<Value>(&b).ok());
        let Some(answer) = answer else {
            say(&format!("mcp call {tool} failed chars=0"));
            return ("failed", "the tool server did not answer".to_string());
        };
        if let Some(msg) = answer.pointer("/error/message").and_then(Value::as_str) {
            say(&format!(
                "mcp call {tool} failed chars={}",
                msg.chars().count()
            ));
            return ("failed", msg.to_string());
        }
        // An MCP result with `isError` reaches the model and ACP as a completed call.
        let text = answer
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        say(&format!(
            "mcp call {tool} completed chars={}",
            text.chars().count()
        ));
        ("completed", text)
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// The address of the first server of the settings file: `mcpServers.<name>.transport.url`.
    fn url(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.settings.as_ref()?).ok()?;
        let doc: Value = serde_json::from_str(&text).ok()?;
        doc.get("mcpServers")?
            .as_object()?
            .values()
            .find_map(|s| s.pointer("/transport/url").and_then(Value::as_str))
            .map(str::to_string)
    }

    fn act_on_settings(&self, act: &str, say: &mut dyn FnMut(&str)) {
        let Some(path) = &self.settings else {
            return;
        };
        match act {
            "edit" => {
                let _ = std::fs::write(path, "{\"mcpServers\":{}}\n");
                say("mcp settings edited");
            }
            "delete" => {
                let _ = std::fs::remove_file(path);
                say("mcp settings deleted");
            }
            _ => {}
        }
    }
}

/// One JSON-RPC message posted to `url` (`http://127.0.0.1:<port>/mcp`) the way an engine's client does it; the status and the body of the answer.
fn post(url: &str, body: &Value) -> std::io::Result<(u16, String)> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| std::io::Error::other("not an http address"))?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut s = TcpStream::connect(host)?;
    s.set_read_timeout(Some(Duration::from_secs(15)))?;
    s.set_write_timeout(Some(Duration::from_secs(15)))?;
    let payload = body.to_string();
    let request = format!(
        "POST /{path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nUser-Agent: fake-acp-engine-mcp\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    s.write_all(request.as_bytes())?;
    let mut out = Vec::new();
    s.read_to_end(&mut out)?;
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text
        .get(9..12)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| std::io::Error::other("no status line"))?;
    let body = text
        .split_once("\r\n\r\n")
        .map_or("", |(_, b)| b)
        .to_string();
    Ok((status, body))
}
