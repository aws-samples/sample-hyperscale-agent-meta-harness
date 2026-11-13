//! The standalone `fake-acp-engine` behind `engine/profiles/fake-acp.toml`:
//! newline-delimited ACP JSON-RPC over stdio, replaying `fixtures/engines/fake-acp/transcripts/`.

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_e2e::acp::{Agentd, ProcessEngine};
use harness_core::engine_profile::{fixtures_dir, load_profile, resolve_launch};
use harness_protocol::common::Transport;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn scratch(label: &str) -> PathBuf {
    let base = std::env::var_os("TMPDIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")), PathBuf::from);
    let d = base.join(format!("fake-acp-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("ws/src")).unwrap();
    std::fs::create_dir_all(d.join("ws/tests")).unwrap();
    std::fs::write(
        d.join("ws/src/calc.py"),
        "def add(a, b):\n    return a + b\n",
    )
    .unwrap();
    d
}

fn bin_dir() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_fake-acp-engine"))
        .parent()
        .unwrap()
        .to_path_buf()
}

struct Proc {
    child: std::process::Child,
    out: BufReader<std::process::ChildStdout>,
}

impl Proc {
    fn start(ws: &Path, log: &Path) -> Proc {
        Proc::start_with(ws, log, &[])
    }

    fn start_with(ws: &Path, log: &Path, env: &[(&str, &str)]) -> Proc {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fake-acp-engine"))
            .args(["--transcripts"])
            .arg(fixtures_dir(&repo(), "fake-acp").join("transcripts"))
            .arg("--workspace")
            .arg(ws)
            .env("FAKE_ACP_LOG", log)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Proc { child, out }
    }

    fn send(&mut self, v: &Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    }

    /// Reads messages until the agent's request `method`; returns (request, notifications before it).
    fn until_request(&mut self, method: &str) -> (Value, Vec<Value>) {
        let mut notes = vec![];
        loop {
            let mut line = String::new();
            assert!(
                self.out.read_line(&mut line).unwrap() > 0,
                "engine closed stdout"
            );
            let v: Value = serde_json::from_str(&line).unwrap();
            if v.get("method").and_then(Value::as_str) == Some(method) && v.get("id").is_some() {
                return (v, notes);
            }
            assert!(
                v.get("method").is_some(),
                "a response arrived before the agent's {method} request: {v}"
            );
            notes.push(v);
        }
    }

    /// Reads messages until the response with `id`; returns (response, notifications before it).
    fn until_response(&mut self, id: u64) -> (Value, Vec<Value>) {
        let mut notes = vec![];
        loop {
            let mut line = String::new();
            assert!(
                self.out.read_line(&mut line).unwrap() > 0,
                "engine closed stdout"
            );
            let v: Value = serde_json::from_str(&line).unwrap();
            if v.get("id") == Some(&json!(id)) {
                return (v, notes);
            }
            notes.push(v);
        }
    }
}

#[test]
fn handshake_and_one_turn_over_stdio_write_the_workspace() {
    let d = scratch("stdio");
    let log = d.join("fake-acp.log");
    let mut p = Proc::start(&d.join("ws"), &log);
    p.send(&json!({"jsonrpc": "2.0", "id": 7, "method": "initialize", "params": {"protocolVersion": 1}}));
    let (init, _) = p.until_response(7);
    assert_eq!(
        init["result"]["agentInfo"],
        json!({"name": "fake-acp", "version": "0.1.0"})
    );
    p.send(&json!({"jsonrpc": "2.0", "id": 8, "method": "session/new", "params": {"cwd": d.join("ws"), "mcpServers": [], "_meta": {"harness": {"systemPrompt": "role prompt"}}}}));
    let (new, _) = p.until_response(8);
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();
    p.send(&json!({"jsonrpc": "2.0", "id": 9, "method": "session/prompt", "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "Add a percent_change(a, b) function with tests"}]}}));
    let (done, notes) = p.until_response(9);
    assert_eq!(done["result"]["stopReason"], "end_turn");
    assert!(
        notes
            .iter()
            .all(|n| n["method"] == "session/update" && n["params"]["sessionId"] == sid.as_str())
    );
    let calc = std::fs::read_to_string(d.join("ws/src/calc.py")).unwrap();
    assert!(calc.contains("def percent_change(a, b):"), "{calc}");
    let tests = std::fs::read_to_string(d.join("ws/tests/test_calc.py")).unwrap();
    assert!(tests.contains("percent_change(0, 5)"));
    assert!(
        d.join("ws/.fake-acp").is_dir(),
        "engine state lives under its hidden path"
    );
    drop(p.child.stdin.take());
    assert!(
        p.child.wait().unwrap().success(),
        "EOF on stdin ends the engine with exit 0"
    );
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(logged.contains("session/prompt"), "{logged}");
}

#[test]
fn an_unexpected_request_gets_a_json_rpc_error_and_the_engine_keeps_serving() {
    let d = scratch("unexpected");
    let mut p = Proc::start(&d.join("ws"), &d.join("log"));
    p.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "session/prompt", "params": {}}));
    let (err, _) = p.until_response(1);
    assert_eq!(err["error"]["code"], -32600);
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("initialize")
    );
    p.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {"protocolVersion": 1}}));
    let (init, _) = p.until_response(2);
    assert_eq!(init["result"]["agentInfo"]["name"], "fake-acp");
    drop(p.child.stdin.take());
    assert!(p.child.wait().unwrap().success());
}

#[test]
fn agentd_drives_the_engine_launched_from_its_profile() {
    let d = scratch("profile");
    let profile = load_profile(&repo().join("engine/profiles/fake-acp.toml")).unwrap();
    let vars: BTreeMap<&str, String> = BTreeMap::from([
        ("workspace", d.join("ws").display().to_string()),
        ("dataDir", d.display().to_string()),
        (
            "fixtures",
            fixtures_dir(&repo(), "fake-acp").display().to_string(),
        ),
    ]);
    let launch = resolve_launch(&profile, Transport::Acp, &vars).unwrap();
    // The bearer token is added to the inherited environment; the profile envRemove must strip it.
    let mut engine = ProcessEngine::spawn(
        &launch,
        Some(&bin_dir()),
        &[("AWS_BEARER_TOKEN_BEDROCK", "bogus-bearer")],
    )
    .unwrap();
    let mut agentd = Agentd::default();
    let hs = agentd
        .handshake(&mut engine, &d.join("ws").display().to_string())
        .unwrap();
    assert_eq!(
        (hs.agent_name.as_str(), hs.agent_version.as_str()),
        ("fake-acp", profile.version.as_str())
    );
    let t = agentd
        .prompt(
            &mut engine,
            "Add a percent_change(a, b) function with tests",
        )
        .unwrap();
    assert_eq!(t.stop_reason, "end_turn");
    assert_eq!(
        t.edits.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
        ["src/calc.py", "tests/test_calc.py"]
    );
    engine.shutdown().unwrap();
    let logged = std::fs::read_to_string(d.join("fake-acp.log")).unwrap();
    assert!(
        logged.contains("env AWS_BEARER_TOKEN_BEDROCK=absent"),
        "envRemove must strip the bearer token: {logged}"
    );
}

#[test]
fn a_held_prompt_answers_cancelled_on_session_cancel_and_the_next_prompt_succeeds() {
    let d = scratch("cancel");
    let log = d.join("fake-acp.log");
    let mut p = Proc::start_with(&d.join("ws"), &log, &[("FAKE_ACP_HOLD_PROMPTS", "1")]);
    p.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}}));
    p.until_response(1);
    p.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": d.join("ws"), "mcpServers": []}}));
    let (new, _) = p.until_response(2);
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();
    p.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "long task"}]}}));
    let t0 = std::time::Instant::now();
    p.send(&json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}}));
    let (held, _) = p.until_response(3);
    assert_eq!(held["result"]["stopReason"], "cancelled", "{held}");
    assert!(t0.elapsed() < std::time::Duration::from_secs(5));
    assert!(
        !d.join("ws/src/calc.py").exists()
            || !std::fs::read_to_string(d.join("ws/src/calc.py"))
                .unwrap()
                .contains("percent_change"),
        "a cancelled prompt makes no edits"
    );
    p.send(&json!({"jsonrpc": "2.0", "id": 4, "method": "session/prompt", "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "Add a percent_change(a, b) function with tests"}]}}));
    let (next, _) = p.until_response(4);
    assert_eq!(next["result"]["stopReason"], "end_turn", "{next}");
    assert!(
        std::fs::read_to_string(d.join("ws/src/calc.py"))
            .unwrap()
            .contains("def percent_change(a, b):")
    );
    drop(p.child.stdin.take());
    assert!(p.child.wait().unwrap().success());
}

/// initialize, session/new and the first prompt (the recorded percent_change turn); returns the session id.
fn handshake_and_turn_one(p: &mut Proc, ws: &Path) -> String {
    p.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}}));
    p.until_response(1);
    p.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": ws, "mcpServers": []}}));
    let (new, _) = p.until_response(2);
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();
    p.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "Add a percent_change(a, b) function with tests"}]}}));
    let (done, _) = p.until_response(3);
    assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
    sid
}

fn reject(id: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"outcome": "selected", "optionId": "reject_once"}, "_meta": {"harness": {"reason": "no installs"}}}})
}

#[test]
fn an_agent_permission_request_waits_for_the_clients_response_then_continues_the_turn() {
    let d = scratch("permission");
    let log = d.join("fake-acp.log");
    let mut p = Proc::start(&d.join("ws"), &log);
    let sid = handshake_and_turn_one(&mut p, &d.join("ws"));
    p.send(&json!({"jsonrpc": "2.0", "id": 4, "method": "session/prompt", "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "Install the requests package"}]}}));
    let (req, before) = p.until_request("session/request_permission");
    assert_eq!(req["params"]["sessionId"], sid.as_str());
    let call = &req["params"]["toolCall"];
    assert_eq!(call["kind"], "execute", "{req}");
    assert_eq!(call["toolCallId"], "fake-call-3");
    assert_eq!(call["rawInput"]["commands"][0], "pip install requests");
    let options: Vec<(&str, &str)> = req["params"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| (o["kind"].as_str().unwrap(), o["optionId"].as_str().unwrap()))
        .collect();
    assert_eq!(
        options,
        [("allow_once", "allow_once"), ("reject_once", "reject_once")]
    );
    assert!(
        before
            .iter()
            .any(|n| n["params"]["update"]["toolCallId"] == "fake-call-3"),
        "the pending tool call precedes its permission request: {before:?}"
    );
    let rid = req["id"].clone();
    assert_ne!(rid, json!(4), "an agent request has an id of its own");
    // A response to another id is ignored and logged; the engine keeps waiting.
    p.send(&reject(&json!(999)));
    p.send(&reject(&rid));
    let (done, after) = p.until_response(4);
    assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
    let further: Vec<&Value> = after
        .iter()
        .filter(|n| n["params"]["update"]["sessionUpdate"] == "tool_call")
        .collect();
    assert_eq!(further.len(), 1, "{after:?}");
    assert_eq!(further[0]["params"]["update"]["kind"], "read");
    assert_eq!(further[0]["params"]["update"]["toolCallId"], "fake-call-4");
    drop(p.child.stdin.take());
    assert!(p.child.wait().unwrap().success());
    let logged = std::fs::read_to_string(&log).unwrap();
    let answered = format!("permission response id={rid} outcome=reject_once");
    assert_eq!(logged.matches(&answered).count(), 1, "{logged}");
    assert!(logged.contains("response ignored id=999"), "{logged}");
    assert!(
        !logged.contains("permission response id=999"),
        "a response to another id must not count: {logged}"
    );
    let at = |needle: &str| {
        logged
            .find(needle)
            .unwrap_or_else(|| panic!("{needle}: {logged}"))
    };
    assert!(
        at(&answered) < at("out {\"id\":4,"),
        "the prompt ends only after the client answered: {logged}"
    );
}

/// Starts turn two and returns the engine with the agent's permission request outstanding.
fn ask_permission(label: &str) -> (Proc, PathBuf, Value) {
    let d = scratch(label);
    let log = d.join("fake-acp.log");
    let mut p = Proc::start(&d.join("ws"), &log);
    let sid = handshake_and_turn_one(&mut p, &d.join("ws"));
    p.send(&json!({"jsonrpc": "2.0", "id": 4, "method": "session/prompt", "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "Install the requests package"}]}}));
    let (req, _) = p.until_request("session/request_permission");
    (p, log, req)
}

fn finish(mut p: Proc) {
    let (done, _) = p.until_response(4);
    assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
    drop(p.child.stdin.take());
    assert!(p.child.wait().unwrap().success());
}

#[test]
fn a_cancelled_answer_to_a_permission_request_is_logged_and_the_turn_continues() {
    let (mut p, log, req) = ask_permission("perm-cancelled");
    p.send(&json!({"jsonrpc": "2.0", "id": req["id"], "result": {"outcome": {"outcome": "cancelled"}}}));
    finish(p);
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(
        logged.contains(&format!(
            "permission response id={} outcome=cancelled",
            req["id"]
        )),
        "{logged}"
    );
}

#[test]
fn an_error_answer_to_a_permission_request_is_logged_and_the_turn_continues() {
    let (mut p, log, req) = ask_permission("perm-error");
    p.send(&json!({"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32000, "message": "declined"}}));
    finish(p);
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(
        logged.contains(&format!(
            "permission response id={} outcome=error:-32000",
            req["id"]
        )),
        "{logged}"
    );
}

#[test]
fn a_response_with_nothing_outstanding_is_ignored_and_logged() {
    let d = scratch("stray-response");
    let log = d.join("fake-acp.log");
    let mut p = Proc::start(&d.join("ws"), &log);
    p.send(&reject(&json!(55)));
    p.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}}));
    let (init, _) = p.until_response(1);
    assert_eq!(init["result"]["agentInfo"]["name"], "fake-acp", "{init}");
    drop(p.child.stdin.take());
    assert!(p.child.wait().unwrap().success());
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(logged.contains("response ignored id=55"), "{logged}");
}

const PERMISSION_PROMPT: &str = "Install the requests package with pip, then read src/calc.py";

/// initialize and session/new only; returns the session id.
fn handshake_only(p: &mut Proc, ws: &Path) -> String {
    p.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}}));
    p.until_response(1);
    p.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": ws, "mcpServers": []}}));
    let (new, _) = p.until_response(2);
    new["result"]["sessionId"].as_str().unwrap().to_string()
}

#[test]
fn a_fresh_process_answers_its_first_prompt_from_the_turn_recorded_for_its_text() {
    let d = scratch("route-permission");
    let log = d.join("fake-acp.log");
    let mut p = Proc::start(&d.join("ws"), &log);
    let sid = handshake_only(&mut p, &d.join("ws"));
    p.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": sid, "prompt": [{"type": "text", "text": PERMISSION_PROMPT}]}}));
    let (req, _) = p.until_request("session/request_permission");
    assert_eq!(
        req["params"]["toolCall"]["rawInput"]["commands"][0],
        "pip install requests"
    );
    p.send(&reject(&req["id"]));
    let (done, after) = p.until_response(3);
    assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
    assert!(
        after
            .iter()
            .any(|n| n["params"]["update"]["toolCallId"] == "fake-call-4"),
        "{after:?}"
    );
    assert!(
        !d.join("ws/src/calc.py").exists()
            || !std::fs::read_to_string(d.join("ws/src/calc.py"))
                .unwrap()
                .contains("percent_change"),
        "the percent_change turn did not run"
    );
    drop(p.child.stdin.take());
    assert!(p.child.wait().unwrap().success());
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(
        logged.contains(&format!(
            "permission response id={} outcome=reject_once",
            req["id"]
        )),
        "{logged}"
    );
}

#[test]
fn a_fresh_process_with_another_first_prompt_gets_turn_one() {
    let d = scratch("route-other");
    let mut p = Proc::start(&d.join("ws"), &d.join("fake-acp.log"));
    let sid = handshake_only(&mut p, &d.join("ws"));
    p.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "Add a percent_change(a, b) function with tests"}]}}));
    let (done, notes) = p.until_response(3);
    assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
    assert!(
        notes.iter().all(|n| n["method"] == "session/update"),
        "no agent request in turn one: {notes:?}"
    );
    assert!(
        std::fs::read_to_string(d.join("ws/src/calc.py"))
            .unwrap()
            .contains("def percent_change(a, b):")
    );
    drop(p.child.stdin.take());
    assert!(p.child.wait().unwrap().success());
}
