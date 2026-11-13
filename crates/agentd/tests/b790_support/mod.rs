//! Shared helpers of the OpenCode arm tests: an emit context, JSON readers and a scripted ACP engine that speaks like
//! OpenCode: it can send requests of its own (a permission ask), hang, and replay a recorded fixture.
#![allow(dead_code, clippy::unwrap_used)]

use agentd::emit::EmitContext;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub const MODEL: &str = "us.anthropic.claude-opus-5-5";
/// The value OpenCode shows for MODEL in its ACP `model` option.
pub const MODEL_VALUE: &str = "amazon-bedrock/us.anthropic.claude-opus-5-5";
pub const TOP: &str = "/mnt/workspace/run1/direct/repo";
pub const SESSION: &str = "ses_1";

static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// A fresh scratch folder under the test temp directory.
pub fn scratch(label: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let d = std::env::temp_dir().join(format!("b790-{}-{nanos}-{n}-{label}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The one place a test builds an `EmitContext`: a new field of the struct is added here and nowhere else.
pub fn ctx(model: &str) -> EmitContext {
    let d = scratch("ctx");
    EmitContext {
        engine: "opencode".into(),
        model: model.into(),
        region: "us-west-2".into(),
        data_dir: d.join("data"),
        config_dir: d.join("config"),
        agentd_exe: PathBuf::from("/opt/harness/bin/agentd"),
        socket: PathBuf::from("/run/agentd/creds.sock"),
        max_output_tokens: Some(64_000),
        context_tokens: Some(1_000_000),
        compaction: None,
    }
}

pub fn read_json(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

/// A permission ask as OpenCode 1.18.32 sends it: three options of the kinds `allow_once`, `allow_always` and `reject_once`.
pub fn opencode_ask(tool_call_id: &str) -> Value {
    json!({
        "sessionId": SESSION,
        "toolCall": {"toolCallId": tool_call_id, "title": "bash", "kind": "execute", "status": "pending", "rawInput": {}},
        "options": [
            {"optionId": "once", "kind": "allow_once", "name": "Allow once"},
            {"optionId": "always", "kind": "allow_always", "name": "Always allow"},
            {"optionId": "reject", "kind": "reject_once", "name": "Reject"}
        ]
    })
}

pub fn frame(v: &Value) -> String {
    format!("{v}\n")
}

/// What a scripted engine does after `session/prompt`.
#[derive(Clone, Debug)]
pub enum Step {
    Update(Value),
    Sleep(u64),
    Finish(&'static str),
    /// A request from the engine; the client's answer is kept in the `answers` list the engine function returns.
    Request(&'static str, Value),
    /// Nothing more, ever.
    Silent,
    /// Waits for `session/cancel`, then answers `cancelled`.
    AwaitCancel,
}

pub fn ask(params: Value) -> Step {
    Step::Request("session/request_permission", params)
}

pub fn chunk(text: &str) -> Step {
    Step::Update(
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
    )
}

pub fn tool(id: &str, title: &str, status: &str) -> Step {
    Step::Update(
        json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": "other", "status": status}),
    )
}

pub fn tool_done(id: &str) -> Step {
    Step::Update(
        json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed"}),
    )
}

pub struct Script {
    /// `currentValue` of the model option at `session/new`.
    pub model: String,
    pub prompts: Vec<Vec<Step>>,
    /// The `initialize` result, when a recording supplies one.
    pub init_result: Option<Value>,
    /// The `session/new` result, when a recording supplies one (its model option's `currentValue` is replaced by `model`).
    pub new_result: Option<Value>,
    /// Notifications sent right after the `session/new` answer (`available_commands_update`).
    pub after_new: Vec<Value>,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            model: MODEL_VALUE.into(),
            prompts: vec![vec![chunk("Done."), Step::Finish("end_turn")]],
            init_result: None,
            new_result: None,
            after_new: Vec::new(),
        }
    }
}

pub type Client = agentd::acp::AcpClient<tokio::io::DuplexStream, tokio::io::DuplexStream>;
pub type Received = Arc<Mutex<Vec<Value>>>;

/// The client's ends of an in-process engine, and what the engine saw.
pub struct Pipes {
    pub r: tokio::io::DuplexStream,
    pub w: tokio::io::DuplexStream,
    pub received: Received,
    pub answers: Received,
}

/// A client wired to an in-process engine: what the engine received, and the answers it got to its own requests.
pub fn engine(script: Script) -> (Client, Received, Received) {
    let p = spawn(script);
    (agentd::acp::AcpClient::new(p.r, p.w), p.received, p.answers)
}

/// An in-process engine that speaks like OpenCode, as pipes.
pub fn spawn(script: Script) -> Pipes {
    let (client_w, engine_r) = tokio::io::duplex(1 << 16);
    let (mut out, client_r) = tokio::io::duplex(1 << 16);
    let received: Received = Arc::default();
    let answers: Received = Arc::default();
    let (seen, got) = (received.clone(), answers.clone());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let mut lines = BufReader::new(engine_r).lines();
        while let Ok(Some(raw)) = lines.next_line().await {
            let msg: Value = serde_json::from_str(&raw).unwrap();
            seen.lock().unwrap().push(msg.clone());
            let _ = tx.send(msg);
        }
    });
    tokio::spawn(async move {
        let mut prompt_no = 0usize;
        let mut req_id = 9000u64;
        while let Some(msg) = rx.recv().await {
            let id = msg.get("id").cloned();
            let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
            match msg["method"].as_str().unwrap_or_default() {
                "initialize" => {
                    let result = script.init_result.clone().unwrap_or_else(|| json!({"protocolVersion": 1, "agentInfo": {"name": "OpenCode", "version": "1.18.32"}}));
                    out.write_all(frame(&reply(result)).as_bytes())
                        .await
                        .unwrap();
                }
                "session/new" => {
                    let mut result = script.new_result.clone().unwrap_or_else(|| json!({"sessionId": SESSION, "configOptions": [{"id": "model", "category": "model", "currentValue": "", "options": []}]}));
                    result["sessionId"] = json!(SESSION);
                    if let Some(opts) = result["configOptions"].as_array_mut() {
                        for o in opts.iter_mut().filter(|o| o["id"] == "model") {
                            o["currentValue"] = json!(script.model);
                        }
                    }
                    out.write_all(frame(&reply(result)).as_bytes())
                        .await
                        .unwrap();
                    for n in &script.after_new {
                        out.write_all(frame(n).as_bytes()).await.unwrap();
                    }
                }
                "session/prompt" => {
                    let steps = script.prompts.get(prompt_no).cloned().unwrap_or_default();
                    prompt_no += 1;
                    for step in steps {
                        match step {
                            Step::Update(u) => out
                                .write_all(frame(&json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": SESSION, "update": u}})).as_bytes())
                                .await
                                .unwrap(),
                            Step::Sleep(s) => tokio::time::sleep(Duration::from_secs(s)).await,
                            Step::Finish(reason) => out
                                .write_all(frame(&reply(json!({"stopReason": reason}))).as_bytes())
                                .await
                                .unwrap(),
                            Step::Request(method, params) => {
                                req_id += 1;
                                out.write_all(frame(&json!({"jsonrpc": "2.0", "id": req_id, "method": method, "params": params})).as_bytes())
                                    .await
                                    .unwrap();
                                out.flush().await.unwrap();
                                // The answer is the next message that carries that id and no method.
                                while let Some(m) = rx.recv().await {
                                    if m.get("id") == Some(&json!(req_id)) && m.get("method").is_none() {
                                        got.lock().unwrap().push(m);
                                        break;
                                    }
                                }
                            }
                            Step::AwaitCancel => {
                                while let Some(m) = rx.recv().await {
                                    if m["method"] == "session/cancel" {
                                        break;
                                    }
                                }
                                out.write_all(frame(&reply(json!({"stopReason": "cancelled"}))).as_bytes())
                                    .await
                                    .unwrap();
                            }
                            Step::Silent => std::future::pending::<()>().await,
                        }
                        out.flush().await.unwrap();
                    }
                }
                _ => {}
            }
            out.flush().await.unwrap();
        }
    });
    Pipes {
        r: client_r,
        w: client_w,
        received,
        answers,
    }
}

pub fn methods(received: &Received) -> Vec<String> {
    received
        .lock()
        .unwrap()
        .iter()
        .filter_map(|m| m["method"].as_str().map(str::to_string))
        .collect()
}

/// The agent-to-client lines of a recorded fixture, with the placeholders of the synthetic turns filled in.
pub fn fixture_lines(file: &str) -> Vec<Value> {
    let text = std::fs::read_to_string(repo_root().join("fixtures/acp").join(file)).unwrap();
    text.lines()
        .filter(|l| l.contains("\"dir\""))
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|e| e["dir"] == "agent->client")
        .map(|e| {
            let s = e["msg"].to_string().replace("<session>", SESSION);
            serde_json::from_str(&s).unwrap()
        })
        .collect()
}

/// The steps of a recorded turn: its updates, then its answer.
pub fn steps_of_turn(file: &str) -> Vec<Step> {
    let mut steps = Vec::new();
    for msg in fixture_lines(file) {
        if msg["method"] == "session/update" {
            steps.push(Step::Update(msg["params"]["update"].clone()));
        } else if let Some(reason) = msg["result"]["stopReason"].as_str() {
            steps.push(Step::Finish(match reason {
                "end_turn" => "end_turn",
                "cancelled" => "cancelled",
                "max_tokens" => "max_tokens",
                _ => "refusal",
            }));
        }
    }
    steps
}

/// A script that replays a recorded handshake and one recorded turn.
pub fn replay(handshake: &str, turn: &str) -> Script {
    let lines = fixture_lines(handshake);
    let mut script = Script::default();
    for msg in &lines {
        match msg["id"].as_u64() {
            Some(1) => script.init_result = Some(msg["result"].clone()),
            Some(2) => script.new_result = Some(msg["result"].clone()),
            _ => {
                if msg["method"] == "session/update" {
                    script.after_new.push(msg.clone());
                }
            }
        }
    }
    script.prompts = vec![steps_of_turn(turn)];
    script
}
