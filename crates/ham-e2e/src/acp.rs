//! Fake ACP engine and the `agentd` side of the ACP seam, for the tests of the fake engine.
//!
//! [`ReplayEngine`] answers JSON-RPC requests from transcripts under `fixtures/acp/`: each client
//! request must match the next recorded request's method, and the recorded agent messages up to
//! the next request are returned with the request's `id` and the placeholders `<session>`,
//! `<prompt>` and `<cwd>` filled in. [`Agentd`] drives it the way `agentd` drives a real engine:
//! `initialize`, `session/new`, then one `session/prompt` per turn, against any [`AcpEngine`]: the
//! in-process replay, or [`ProcessEngine`], a real process launched from an engine profile
//! (`fake-acp-engine` behind `engine/profiles/fake-acp.toml`) speaking newline-delimited JSON-RPC.
//!
//! A transcript may hold an agent-to-client REQUEST (a message with a `method` and an `id`, in
//! practice `session/request_permission`). The replay returns the messages up to and including it and
//! then waits: the next client message must be the response with that id (the transcript records one
//! as a `client` entry with no `method`), and only then do the following agent messages, up to the
//! prompt's own response, come out. Both drivers answer such a request with a rejection
//! ([`answer_agent_request`]) and keep it in [`TurnOutput::permissions`].
//!
//! Prompt routing: a transcript directory is a handshake script and one script per turn, and
//! agentd starts a fresh engine process for every turn, so a turn must be reachable as the FIRST
//! prompt of a process. A `session/prompt` between turns is answered from the unused turn script whose
//! recorded client prompt text equals the request's prompt text; when none does, from the first unused
//! turn script in order (what sequential replay always did).

use harness_core::engine_profile::ResolvedLaunch;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Header {
    pub engine: String,
    pub version: String,
    #[serde(default)]
    pub recorded: bool,
    #[serde(default)]
    pub synthetic: bool,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum Dir {
    #[serde(rename = "client->agent")]
    ClientToAgent,
    #[serde(rename = "agent->client")]
    AgentToClient,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Entry {
    pub dir: Dir,
    pub msg: Value,
}

#[derive(Clone, Debug)]
pub struct Script {
    pub header: Header,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ReplayError {
    #[error("read {0}")]
    Io(String),
    #[error("{file}:{line}: {msg}")]
    Parse {
        file: String,
        line: usize,
        msg: String,
    },
    #[error("unexpected request: transcript expects {expected}, got {got}")]
    Unexpected { expected: String, got: String },
    #[error("transcript exhausted")]
    Exhausted,
    #[error("ACP protocol: {0}")]
    Protocol(String),
}

#[derive(Deserialize)]
struct HeaderLine {
    header: Header,
}

/// A line in either transcript format: `{"dir": "client->agent", "msg": ...}` (fixtures/acp) or
/// `{"direction": "client", "message": ...}` (fixtures/engines).
fn parse_entry(v: &Value) -> Option<Entry> {
    let dir = match (
        v.get("dir").and_then(Value::as_str),
        v.get("direction").and_then(Value::as_str),
    ) {
        (Some("client->agent"), _) | (_, Some("client")) => Dir::ClientToAgent,
        (Some("agent->client"), _) | (_, Some("agent")) => Dir::AgentToClient,
        _ => return None,
    };
    let msg = v.get("msg").or_else(|| v.get("message"))?.clone();
    Some(Entry { dir, msg })
}

impl Script {
    /// Loads one transcript. A `{"header": ...}` first line is optional.
    pub fn load(path: &Path) -> Result<Script, ReplayError> {
        let file = path.display().to_string();
        let text =
            std::fs::read_to_string(path).map_err(|e| ReplayError::Io(format!("{file}: {e}")))?;
        let mut header = Header::default();
        let mut entries = Vec::new();
        for (i, l) in text
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
        {
            let parse_err = |msg: String| ReplayError::Parse {
                file: file.clone(),
                line: i + 1,
                msg,
            };
            let v: Value = serde_json::from_str(l).map_err(|e| parse_err(e.to_string()))?;
            if entries.is_empty() && v.get("header").is_some() {
                header = serde_json::from_value::<HeaderLine>(v)
                    .map_err(|e| parse_err(format!("header: {e}")))?
                    .header;
                continue;
            }
            entries.push(
                parse_entry(&v)
                    .ok_or_else(|| parse_err("expected dir/msg or direction/message".into()))?,
            );
        }
        Ok(Script { header, entries })
    }

    /// Loads an engine transcript directory: `handshake.jsonl`, then `turn-*.jsonl` in name order.
    pub fn load_dir(dir: &Path) -> Result<Vec<Script>, ReplayError> {
        let mut turns: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| ReplayError::Io(format!("{}: {e}", dir.display())))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("turn-") && n.ends_with(".jsonl"))
            })
            .collect();
        turns.sort();
        let mut out = vec![Script::load(&dir.join("handshake.jsonl"))?];
        for t in turns {
            out.push(Script::load(&t)?);
        }
        Ok(out)
    }
}

/// An ACP agent the fake `agentd` can drive: one request in, the agent's messages out.
pub trait AcpEngine {
    /// Sends one JSON-RPC message; for a request, returns every agent message up to and including
    /// its response.
    fn handle(&mut self, req: &Value) -> Result<Vec<Value>, ReplayError>;
    /// Ends the engine (EOF on stdin for a process).
    fn close(&mut self) -> Result<(), ReplayError> {
        Ok(())
    }
}

fn method(v: &Value) -> Option<&str> {
    v.get("method").and_then(Value::as_str)
}

/// A JSON-RPC response: an `id` with a `result` or an `error` and no `method`.
pub fn is_response(v: &Value) -> bool {
    v.get("method").is_none()
        && v.get("id").is_some()
        && (v.get("result").is_some() || v.get("error").is_some())
}

/// A request the agent sends to the client: a `method` and an `id`.
pub fn is_agent_request(v: &Value) -> bool {
    v.get("method").is_some() && v.get("id").is_some()
}

/// What a recorded entry stands for in an `Unexpected` error.
fn describe(e: &Entry) -> String {
    match (method(&e.msg), e.msg.get("id")) {
        (Some(m), _) => m.to_string(),
        (None, Some(id)) if e.dir == Dir::ClientToAgent => {
            format!("<response to agent request {id}>")
        }
        _ => "<agent message>".to_string(),
    }
}

/// The driver's answer to a request the agent sent: `session/request_permission` is rejected with the
/// first `reject_once` or `reject_always` option (`cancelled` when the request offers none), the way
/// agentd answers a refusal; any other method gets JSON-RPC error -32601.
pub fn answer_agent_request(req: &Value) -> Value {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    if method(req) != Some("session/request_permission") {
        return json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method {} is not offered by the driver", method(req).unwrap_or_default())}});
    }
    let options = req
        .pointer("/params/options")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let pick = ["reject_once", "reject_always"].iter().find_map(|kind| {
        options
            .iter()
            .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
            .and_then(|o| o.get("optionId").cloned())
    });
    let outcome = match pick {
        Some(option_id) => json!({"outcome": "selected", "optionId": option_id}),
        None => json!({"outcome": "cancelled"}),
    };
    json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": outcome}})
}

/// Replays one or more scripts in order as a single engine process.
#[derive(Debug)]
pub struct ReplayEngine {
    entries: Vec<Entry>,
    pos: usize,
    session_id: String,
    prompt: String,
    cwd: String,
    workspace: String,
    /// The id of the agent request the engine waits for a response to.
    awaiting: Option<Value>,
    /// The client request (id, method) whose answer the engine resumes once `awaiting` is answered.
    pending: Option<(Value, String)>,
    /// One per script given to [`ReplayEngine::new`], for prompt routing.
    segments: Vec<Segment>,
}

/// The entries of one script. `prompt` is the recorded text of a turn script's `session/prompt` (its
/// first entry); a handshake script has none.
#[derive(Debug)]
struct Segment {
    start: usize,
    prompt: Option<String>,
    used: bool,
}

/// The text of a `session/prompt`'s `params`: its text blocks joined by newlines.
fn prompt_text(params: Option<&Value>) -> String {
    params
        .and_then(|p| p.get("prompt"))
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

impl ReplayEngine {
    pub fn new(scripts: Vec<Script>) -> Self {
        let mut entries = Vec::new();
        let mut segments = Vec::new();
        for script in scripts {
            let start = entries.len();
            entries.extend(script.entries);
            let prompt = entries
                .get(start)
                .filter(|e| e.dir == Dir::ClientToAgent && method(&e.msg) == Some("session/prompt"))
                .map(|e| prompt_text(e.msg.get("params")));
            segments.push(Segment {
                start,
                prompt,
                used: false,
            });
        }
        Self {
            entries,
            pos: 0,
            session_id: String::new(),
            prompt: String::new(),
            cwd: String::new(),
            workspace: String::new(),
            awaiting: None,
            pending: None,
            segments,
        }
    }

    /// Between turns, points the replay at the turn a `session/prompt` is for: the unused turn script
    /// recording the request's text, else the first unused one; with every turn used, past the end.
    /// Does nothing before the handshake ends, mid-turn, or when no script is a turn script.
    fn route_prompt(&mut self, req: &Value) {
        if self.awaiting.is_some() || self.segments.iter().all(|s| s.prompt.is_none()) {
            return;
        }
        let between_turns = self.pos >= self.entries.len()
            || self
                .segments
                .iter()
                .any(|s| s.start == self.pos && s.prompt.is_some());
        if !between_turns {
            return;
        }
        let text = prompt_text(req.get("params"));
        let pick = self
            .segments
            .iter()
            .position(|s| !s.used && s.prompt.as_deref() == Some(text.as_str()))
            .or_else(|| {
                self.segments
                    .iter()
                    .position(|s| !s.used && s.prompt.is_some())
            });
        match pick {
            Some(i) => {
                self.pos = self.segments[i].start;
                self.segments[i].used = true;
            }
            None => self.pos = self.entries.len(),
        }
    }

    /// The id of the agent request the engine sent and has no response to yet.
    pub fn awaiting(&self) -> Option<&Value> {
        self.awaiting.as_ref()
    }

    /// The positions (in the list given to [`ReplayEngine::new`]) of the turn scripts this engine already answered a prompt from.
    /// A restarted fake engine saves them so its next process goes on with the next turn.
    pub fn used_turns(&self) -> Vec<usize> {
        self.segments
            .iter()
            .enumerate()
            .filter(|(_, s)| s.used)
            .map(|(i, _)| i)
            .collect()
    }

    /// Marks turn scripts as used (positions from [`ReplayEngine::used_turns`] of an earlier engine on the same scripts). A position
    /// that is not a turn script is ignored.
    pub fn mark_turns_used(&mut self, used: &[usize]) {
        for i in used {
            if let Some(s) = self.segments.get_mut(*i).filter(|s| s.prompt.is_some()) {
                s.used = true;
            }
        }
    }

    /// The launch value `{workspace}` stands for (engine transcripts under fixtures/engines).
    pub fn with_workspace(mut self, workspace: &str) -> Self {
        self.workspace = workspace.trim_end_matches('/').to_string();
        self
    }

    fn fill(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(
                s.replace("<session>", &self.session_id)
                    .replace("<prompt>", &self.prompt)
                    .replace("<cwd>", &self.cwd)
                    .replace(
                        "{workspace}",
                        if self.workspace.is_empty() {
                            &self.cwd
                        } else {
                            &self.workspace
                        },
                    ),
            ),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.fill(x)).collect()),
            Value::Object(o) => {
                Value::Object(o.iter().map(|(k, x)| (k.clone(), self.fill(x))).collect())
            }
            other => other.clone(),
        }
    }

    /// Handles one client message and returns the agent's responses and notifications. A client
    /// response answers the agent request the engine waits for: a response to another id changes
    /// nothing (an empty list), and one with nothing outstanding is `Unexpected`.
    pub fn handle(&mut self, req: &Value) -> Result<Vec<Value>, ReplayError> {
        if is_response(req) {
            return self.handle_response(req);
        }
        let got = method(req).unwrap_or("<no method>").to_string();
        if got == "session/prompt" {
            self.route_prompt(req);
        }
        let expected = self.entries.get(self.pos).ok_or(ReplayError::Exhausted)?;
        if expected.dir != Dir::ClientToAgent || method(&expected.msg) != Some(got.as_str()) {
            return Err(ReplayError::Unexpected {
                expected: describe(expected),
                got,
            });
        }
        let params = req.get("params").cloned().unwrap_or(Value::Null);
        match got.as_str() {
            "session/new" => {
                self.cwd = params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            }
            "session/prompt" => self.prompt = prompt_text(Some(&params)),
            _ => {}
        }
        self.pos += 1;
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        Ok(self.emit(&id, &got))
    }

    /// The agent messages up to the next client entry, or up to and including an agent request (the
    /// engine then waits for its response). A response takes the id of the client request `id`; an
    /// agent request keeps its recorded id.
    fn emit(&mut self, id: &Value, got: &str) -> Vec<Value> {
        let mut out = Vec::new();
        while let Some(e) = self.entries.get(self.pos) {
            if e.dir == Dir::ClientToAgent {
                break;
            }
            let mut msg = self.fill(&e.msg);
            let request = is_agent_request(&msg);
            if msg.get("id").is_some() && !request {
                msg["id"] = id.clone();
                if got == "session/new"
                    && let Some(sid) = msg.pointer("/result/sessionId").and_then(Value::as_str)
                {
                    self.session_id = sid.to_string();
                }
            }
            if request {
                self.awaiting = msg.get("id").cloned();
                self.pending = Some((id.clone(), got.to_string()));
            }
            out.push(msg);
            self.pos += 1;
            if request {
                break;
            }
        }
        out
    }

    fn handle_response(&mut self, resp: &Value) -> Result<Vec<Value>, ReplayError> {
        let Some(awaiting) = self.awaiting.clone() else {
            return Err(ReplayError::Unexpected {
                expected: self
                    .entries
                    .get(self.pos)
                    .map_or("<end of transcript>".to_string(), describe),
                got: "<response>".to_string(),
            });
        };
        if resp.get("id") != Some(&awaiting) {
            return Ok(Vec::new());
        }
        let recorded = self.entries.get(self.pos).ok_or(ReplayError::Exhausted)?;
        if recorded.dir != Dir::ClientToAgent || !is_response(&recorded.msg) {
            return Err(ReplayError::Protocol(format!(
                "the transcript records no client response after the agent request {awaiting}"
            )));
        }
        self.pos += 1;
        self.awaiting = None;
        let (id, got) = self.pending.take().unwrap_or((Value::Null, String::new()));
        Ok(self.emit(&id, &got))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handshake {
    pub agent_name: String,
    pub agent_version: String,
    pub session_id: String,
}

/// A file edit reported as ACP `diff` tool-call content, relative to the session cwd.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub path: String,
    pub new_text: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TurnOutput {
    /// `session/update` notifications in order.
    pub updates: Vec<Value>,
    /// Agent message text chunks (untrusted).
    pub messages: Vec<String>,
    /// Tool-call titles (untrusted).
    pub tool_titles: Vec<String>,
    pub edits: Vec<Edit>,
    pub stop_reason: String,
    /// The requests the engine sent the client during the prompt and the driver's answers.
    pub permissions: Vec<PermissionExchange>,
}

/// One `session/request_permission` the engine sent and the driver's answer to it.
#[derive(Clone, Debug, PartialEq)]
pub struct PermissionExchange {
    /// The request's `params` (`toolCall`, `options`).
    pub request: Value,
    /// The answer's `result` (`outcome`).
    pub answer: Value,
}

/// The `agentd` side of the ACP seam, reduced to what the local E2E needs.
#[derive(Debug, Default)]
pub struct Agentd {
    next_id: u64,
    session_id: String,
    cwd: String,
    permissions: Vec<PermissionExchange>,
}

impl Agentd {
    fn request(
        &mut self,
        engine: &mut dyn AcpEngine,
        method: &str,
        params: Value,
    ) -> Result<(Value, Vec<Value>), ReplayError> {
        self.next_id += 1;
        let id = self.next_id;
        let mut out = engine
            .handle(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        // An engine that asks the client something waits for the answer; the driver rejects.
        while let Some(ask) = out.last().filter(|m| is_agent_request(m)).cloned() {
            let answer = answer_agent_request(&ask);
            if ask.get("method").and_then(Value::as_str) == Some("session/request_permission") {
                self.permissions.push(PermissionExchange {
                    request: ask.get("params").cloned().unwrap_or(Value::Null),
                    answer: answer.get("result").cloned().unwrap_or(Value::Null),
                });
            }
            out.extend(engine.handle(&answer)?);
        }
        let mut response = None;
        let mut notes = Vec::new();
        for m in out {
            if m.get("id") == Some(&json!(id)) && m.get("method").is_none() {
                response = Some(m);
            } else {
                notes.push(m);
            }
        }
        let response =
            response.ok_or_else(|| ReplayError::Protocol(format!("no response to {method}")))?;
        if let Some(err) = response.get("error") {
            return Err(ReplayError::Protocol(format!("{method}: {err}")));
        }
        Ok((response, notes))
    }

    pub fn handshake(
        &mut self,
        engine: &mut dyn AcpEngine,
        cwd: &str,
    ) -> Result<Handshake, ReplayError> {
        let (init, _) = self.request(engine, "initialize", json!({
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
            "clientInfo": {"name": "hamh-e2e-agentd", "version": env!("CARGO_PKG_VERSION")}
        }))?;
        let info = init
            .pointer("/result/agentInfo")
            .ok_or_else(|| ReplayError::Protocol("initialize: no agentInfo".into()))?;
        let text = |k: &str| {
            info.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let (agent_name, agent_version) = (text("name"), text("version"));
        let (new, _) =
            self.request(engine, "session/new", json!({"cwd": cwd, "mcpServers": []}))?;
        let session_id = new
            .pointer("/result/sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| ReplayError::Protocol("session/new: no sessionId".into()))?
            .to_string();
        self.session_id.clone_from(&session_id);
        self.cwd = cwd.trim_end_matches('/').to_string();
        Ok(Handshake {
            agent_name,
            agent_version,
            session_id,
        })
    }

    pub fn prompt(
        &mut self,
        engine: &mut dyn AcpEngine,
        text: &str,
    ) -> Result<TurnOutput, ReplayError> {
        let params =
            json!({"sessionId": self.session_id, "prompt": [{"type": "text", "text": text}]});
        self.permissions.clear();
        let (resp, notes) = self.request(engine, "session/prompt", params)?;
        let mut out = TurnOutput {
            stop_reason: resp
                .pointer("/result/stopReason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            permissions: std::mem::take(&mut self.permissions),
            ..TurnOutput::default()
        };
        for n in notes {
            if method(&n) != Some("session/update") {
                continue;
            }
            if let Some(u) = n.pointer("/params/update") {
                match u.get("sessionUpdate").and_then(Value::as_str) {
                    Some("agent_message_chunk") => {
                        if let Some(t) = u.pointer("/content/text").and_then(Value::as_str) {
                            out.messages.push(t.to_string());
                        }
                    }
                    Some("tool_call") => {
                        if let Some(t) = u.get("title").and_then(Value::as_str) {
                            out.tool_titles.push(t.to_string());
                        }
                    }
                    Some("tool_call_update") => {
                        for c in u
                            .get("content")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                        {
                            if c.get("type").and_then(Value::as_str) != Some("diff") {
                                continue;
                            }
                            let path = c.get("path").and_then(Value::as_str).unwrap_or_default();
                            let rel = path
                                .strip_prefix(&self.cwd)
                                .unwrap_or(path)
                                .trim_start_matches('/')
                                .to_string();
                            let new_text = c
                                .get("newText")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            out.edits.push(Edit {
                                path: rel,
                                new_text,
                            });
                        }
                    }
                    _ => {}
                }
            }
            out.updates.push(n);
        }
        Ok(out)
    }
}

impl AcpEngine for ReplayEngine {
    fn handle(&mut self, req: &Value) -> Result<Vec<Value>, ReplayError> {
        ReplayEngine::handle(self, req)
    }
}

/// How long a process engine may take to answer one request.
pub const PROCESS_TIMEOUT: Duration = Duration::from_secs(30);

/// An engine process launched from an engine profile, speaking newline-delimited JSON-RPC.
#[derive(Debug)]
pub struct ProcessEngine {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    /// The client request whose response is still outstanding while the engine waits for the client.
    pending: Option<Value>,
}

impl ProcessEngine {
    /// Spawns `launch.argv` with the profile's environment rules. `argv[0]` is looked up in
    /// `bin_dir` first, then on `PATH`. `inherited` adds variables to the inherited environment
    /// before `envRemove` is applied (tests use it to prove the removal).
    pub fn spawn(
        launch: &ResolvedLaunch,
        bin_dir: Option<&Path>,
        inherited: &[(&str, &str)],
    ) -> Result<Self, ReplayError> {
        let Some(program) = launch.argv.first() else {
            return Err(ReplayError::Protocol("launch argv is empty".into()));
        };
        let program = bin_dir
            .map(|d| d.join(program))
            .filter(|p| p.is_file())
            .unwrap_or_else(|| PathBuf::from(program));
        let mut cmd = Command::new(&program);
        cmd.args(&launch.argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in inherited {
            cmd.env(k, v);
        }
        for (k, v) in &launch.env {
            cmd.env(k, v);
        }
        for k in &launch.env_remove {
            cmd.env_remove(k);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| ReplayError::Io(format!("spawn {}: {e}", program.display())))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ReplayError::Protocol("no stdout pipe".into()))?;
        let stdin = child.stdin.take();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            lines,
            pending: None,
        })
    }

    fn write(&mut self, v: &Value) -> Result<(), ReplayError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| ReplayError::Protocol("engine stdin is closed".into()))?;
        writeln!(stdin, "{v}")
            .and_then(|()| stdin.flush())
            .map_err(|e| ReplayError::Io(format!("engine stdin: {e}")))
    }

    /// Closes stdin and waits for exit (killing the process after 5 s).
    pub fn shutdown(mut self) -> Result<(), ReplayError> {
        self.close()
    }
}

impl AcpEngine for ProcessEngine {
    fn handle(&mut self, req: &Value) -> Result<Vec<Value>, ReplayError> {
        self.write(req)?;
        // A client response goes on waiting for the request it interrupted; a request becomes it.
        let awaited = if is_response(req) {
            self.pending.clone()
        } else {
            req.get("id").cloned()
        };
        let Some(id) = awaited else {
            return Ok(Vec::new());
        };
        if !is_response(req) {
            self.pending = Some(id.clone());
        }
        let mut out = Vec::new();
        loop {
            let line = match self.lines.recv_timeout(PROCESS_TIMEOUT) {
                Ok(l) => l,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(ReplayError::Protocol(format!(
                        "no response to id {id} within {PROCESS_TIMEOUT:?}"
                    )));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(ReplayError::Protocol("engine closed stdout".into()));
                }
            };
            let v: Value = serde_json::from_str(&line)
                .map_err(|e| ReplayError::Protocol(format!("engine wrote non-JSON: {e}")))?;
            let done = v.get("id") == Some(&id) && v.get("method").is_none();
            // The engine stops at a request to the client until it is answered.
            let asks = is_agent_request(&v);
            out.push(v);
            if done {
                self.pending = None;
                return Ok(out);
            }
            if asks {
                return Ok(out);
            }
        }
    }

    fn close(&mut self) -> Result<(), ReplayError> {
        drop(self.stdin.take());
        for _ in 0..50 {
            match self.child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => {
                    return Err(ReplayError::Protocol(format!(
                        "engine exited with {status}"
                    )));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => return Err(ReplayError::Io(e.to_string())),
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        Err(ReplayError::Protocol(
            "engine did not exit within 5 s of EOF; killed".into(),
        ))
    }
}

impl Drop for ProcessEngine {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
