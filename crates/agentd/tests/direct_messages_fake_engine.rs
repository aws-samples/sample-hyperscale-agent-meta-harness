//! The fake engine that behaves like the pinned Cline where message control depends on it.
//!
//! `fake-acp-engine` with `FAKE_ACP_CLINE=1` is not a replay of recorded turns: it is a small ACP agent with the measured behaviour of Cline
//! 3.0.71 on the four points the module relies on, so that a harness bug shows up in a test the way it would show up on the live engine.
//!
//! - A second `session/prompt` while one is open is neither refused nor merged: the engine queues its text, answers the second request at once
//!   with `end_turn` and all usage 0, and runs the text later as a turn of its own with no open prompt, no stop reason and no usage.
//!   `session/cancel` after that does nothing.
//! - `session/cancel` of a single open prompt answers `cancelled` in about 0.2 s, kills the shell tool, and leaves a file edit complete or absent.
//! - A tool's processes outlive a kill of the engine's process group (the tool runs in a session of its own; a command that forks and
//!   exits leaves its child to init).
//! - An engine can hang (it answers no cancel) or die.
//!
//! A prompt carries `@fake{...}` directives that script its turn; see `crates/ham-e2e/src/fake_cline.rs`. Every test starts the real
//! binary (built once by `common::fake_engine_dir`, as the other direct tests do) and speaks newline-delimited JSON-RPC to it.
#![allow(clippy::unwrap_used)]
#![cfg(unix)]

mod common;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn scratch() -> PathBuf {
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros());
    let d = std::env::temp_dir().join(format!(
        "fc-{}-{n}-{:x}",
        std::process::id(),
        micros % 0xf_ffff
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Engine {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Value>,
    ws: PathBuf,
    next_id: i64,
    tag: String,
}

impl Engine {
    fn start(ws: &Path) -> Self {
        Self::start_with(ws, &[])
    }

    fn start_with(ws: &Path, env: &[(&str, &str)]) -> Self {
        std::fs::create_dir_all(ws).unwrap();
        let tag = format!(
            "fc-tag-{}",
            scratch().file_name().unwrap().to_string_lossy()
        );
        let mut cmd = Command::new(common::fake_engine_dir().join("fake-acp-engine"));
        cmd.args(["--transcripts", "/nonexistent", "--workspace"])
            .arg(ws)
            .env("FAKE_ACP_CLINE", "1")
            .env("FAKE_ACP_MODEL", "test-model")
            .env("FAKE_ACP_LOG", ws.join("fake.log"))
            .env("AGENTD_ENGINE_TAG", &tag)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        let stdin = child.stdin.take();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&l) {
                    let _ = tx.send(v);
                }
            }
        });
        Self {
            child,
            stdin,
            lines: rx,
            ws: ws.to_path_buf(),
            next_id: 1,
            tag,
        }
    }

    fn send(&mut self, v: Value) {
        let w = self.stdin.as_mut().unwrap();
        writeln!(w, "{v}").unwrap();
        w.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// The next message, or `None` after `within`.
    fn next(&self, within: Duration) -> Option<Value> {
        self.lines.recv_timeout(within).ok()
    }

    /// Reads until the response with `id`; returns it and the notifications that came before it.
    fn response(&self, id: i64, within: Duration) -> Option<(Value, Vec<Value>)> {
        let end = Instant::now() + within;
        let mut before = Vec::new();
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let m = self.next(left)?;
            if m.get("id") == Some(&json!(id)) && m.get("method").is_none() {
                return Some((m, before));
            }
            before.push(m);
        }
    }

    fn handshake(&mut self) -> String {
        let id = self.request("initialize", json!({"protocolVersion": 1}));
        let (r, _) = self.response(id, Duration::from_secs(5)).unwrap();
        assert_eq!(r["result"]["agentInfo"]["name"], "fake-cline");
        let id = self.request(
            "session/new",
            json!({"cwd": self.ws.display().to_string(), "mcpServers": []}),
        );
        let (r, _) = self.response(id, Duration::from_secs(5)).unwrap();
        r["result"]["sessionId"].as_str().unwrap().to_string()
    }

    fn prompt(&mut self, sid: &str, text: &str) -> i64 {
        self.request(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]}),
        )
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.ws.join("fake.log")).unwrap_or_default()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", self.child.id())])
            .status();
        let _ = self.child.wait();
        // the tools it left (the tag is in their environment)
        kill_tagged(&self.tag);
    }
}

/// Stops every process whose environment carries `AGENTD_ENGINE_TAG=<tag>`: a test must not leave a sleeping tool behind.
fn kill_tagged(tag: &str) {
    let want = format!("AGENTD_ENGINE_TAG={tag}");
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return;
    };
    for e in dir.filter_map(Result::ok) {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(env) = std::fs::read(e.path().join("environ")) else {
            continue;
        };
        if env.split(|b| *b == 0).any(|x| x == want.as_bytes()) {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
    }
}

fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.trim_start().chars().next())
            .is_some_and(|c| c != 'Z' && c != 'X'),
        Err(_) => false,
    }
}

fn pids_of(file: &Path) -> Vec<u32> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

fn wait_for(file: &Path, want: usize) -> Vec<u32> {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let v = pids_of(file);
        if v.len() >= want {
            return v;
        }
        assert!(
            Instant::now() < end,
            "waiting for {} to hold {want} pids: {v:?}",
            file.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stop_of(m: &Value) -> &str {
    m["result"]["stopReason"].as_str().unwrap_or("")
}

fn usage_total(m: &Value) -> u64 {
    let u = &m["result"]["_meta"]["usage"];
    ["inputTokens", "outputTokens"]
        .iter()
        .map(|k| u[k].as_u64().unwrap_or(0))
        .sum()
}

fn updates(v: &[Value]) -> usize {
    v.iter().filter(|m| m["method"] == "session/update").count()
}

// ---- the handshake and a plain turn ----------------------------------------------------------------------------------------------

#[test]
fn the_handshake_names_a_session_and_the_pinned_model() {
    let ws = scratch();
    let mut e = Engine::start(&ws);
    let id = e.request("initialize", json!({"protocolVersion": 1}));
    let (r, _) = e.response(id, Duration::from_secs(5)).unwrap();
    assert_eq!(r["result"]["protocolVersion"], 1);
    let id = e.request("session/new", json!({"cwd": "/w", "mcpServers": []}));
    let (r, _) = e.response(id, Duration::from_secs(5)).unwrap();
    assert!(
        r["result"]["sessionId"]
            .as_str()
            .unwrap()
            .starts_with("cline-")
    );
    assert_eq!(r["result"]["configOptions"][0]["id"], "model");
    assert_eq!(
        r["result"]["configOptions"][0]["currentValue"],
        "test-model"
    );
}

#[test]
fn a_plain_prompt_streams_a_message_and_ends_with_usage() {
    let ws = scratch();
    let mut e = Engine::start(&ws);
    let sid = e.handshake();
    let id = e.prompt(&sid, "hello there");
    let (r, before) = e.response(id, Duration::from_secs(5)).unwrap();
    assert_eq!(stop_of(&r), "end_turn");
    assert!(usage_total(&r) > 0, "{r}");
    assert!(updates(&before) >= 1);
    assert!(
        before
            .iter()
            .any(|m| m["params"]["update"]["sessionUpdate"] == "agent_message_chunk")
    );
}

#[test]
fn a_prompt_for_a_session_the_engine_does_not_know_is_refused() {
    let ws = scratch();
    let mut e = Engine::start(&ws);
    e.handshake();
    let id = e.prompt("nope", "hi");
    let (r, _) = e.response(id, Duration::from_secs(5)).unwrap();
    assert_eq!(r["error"]["code"], -32602);
}

#[test]
fn the_directive_write_edits_a_file_of_the_workspace_and_say_sets_the_final_message() {
    let ws = scratch();
    let mut e = Engine::start(&ws);
    let sid = e.handshake();
    let id = e.prompt(&sid, r#"edit @fake{"write": {"path": "note.txt", "text": "from the turn"}, "say": "all written"}"#);
    let (r, before) = e.response(id, Duration::from_secs(5)).unwrap();
    assert_eq!(stop_of(&r), "end_turn");
    assert_eq!(
        std::fs::read_to_string(ws.join("note.txt")).unwrap(),
        "from the turn"
    );
    assert!(before.iter().any(|m| {
        m["params"]["update"]["content"]["text"]
            .as_str()
            .is_some_and(|t| t.contains("all written"))
    }));
}

// ---- the internal queue of the pinned engine --------------------------------------------------------------------------------------

#[test]
fn a_second_prompt_while_one_is_open_is_queued_and_answered_at_once_with_no_usage() {
    let ws = scratch();
    let mut e = Engine::start(&ws);
    let sid = e.handshake();
    let first = e.prompt(&sid, r#"long @fake{"work_ms": 1500}"#);
    std::thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    let second = e.prompt(&sid, r#"second @fake{"say": "SECOND-RAN"}"#);
    let (r2, before2) = e.response(second, Duration::from_secs(5)).unwrap();
    // The measured behaviour: end_turn, usage 0, no model work, within milliseconds.
    assert!(
        t.elapsed() < Duration::from_millis(800),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(stop_of(&r2), "end_turn");
    assert_eq!(usage_total(&r2), 0, "{r2}");
    assert!(
        !before2
            .iter()
            .any(|m| m["params"]["update"]["content"]["text"]
                .as_str()
                .is_some_and(|t| t.contains("SECOND-RAN"))),
        "the queued text must not run before the first prompt ends"
    );
    // The first prompt still ends on its own.
    let (r1, _) = e.response(first, Duration::from_secs(10)).unwrap();
    assert_eq!(stop_of(&r1), "end_turn");
    assert!(usage_total(&r1) > 0);
    // The queued text then runs as a turn of its own: updates with no open prompt and no response.
    let end = Instant::now() + Duration::from_secs(5);
    let mut ran = false;
    while Instant::now() < end && !ran {
        if let Some(m) = e.next(Duration::from_millis(200)) {
            assert!(
                m.get("id").is_none() || m.get("method").is_some(),
                "the drained turn sends no response: {m}"
            );
            ran = m["params"]["update"]["content"]["text"]
                .as_str()
                .is_some_and(|t| t.contains("SECOND-RAN"));
        }
    }
    assert!(
        ran,
        "the queued text ran after the first prompt: {}",
        e.log()
    );
    assert!(
        e.log().contains("overlap"),
        "the engine notes the overlap in its log: {}",
        e.log()
    );
}

#[test]
fn a_cancel_after_a_queued_prompt_does_nothing() {
    let ws = scratch();
    let mut e = Engine::start(&ws);
    let sid = e.handshake();
    let first = e.prompt(&sid, r#"long @fake{"work_ms": 1500}"#);
    std::thread::sleep(Duration::from_millis(150));
    let second = e.prompt(&sid, "queued");
    e.response(second, Duration::from_secs(5)).unwrap();
    e.notify("session/cancel", json!({"sessionId": sid}));
    let t = Instant::now();
    let (r1, _) = e.response(first, Duration::from_secs(10)).unwrap();
    assert_eq!(stop_of(&r1), "end_turn", "the cancel was ignored: {r1}");
    assert!(
        t.elapsed() > Duration::from_millis(700),
        "the turn ran on after the cancel: {:?}",
        t.elapsed()
    );
    assert!(e.log().contains("acted=false"), "{}", e.log());
}

#[test]
fn a_prompt_marked_queued_reply_gets_the_queue_treatment_with_no_other_prompt_open() {
    // The tripwire's trigger for the module's test: an engine whose queue already held something.
    let ws = scratch();
    let mut e = Engine::start(&ws);
    let sid = e.handshake();
    let id = e.prompt(&sid, r#"x @fake{"queued_reply": true, "say": "DRAINED"}"#);
    let (r, before) = e.response(id, Duration::from_secs(5)).unwrap();
    assert_eq!(stop_of(&r), "end_turn");
    assert_eq!(usage_total(&r), 0);
    assert_eq!(updates(&before), 0);
    // and the text then runs as a drained turn
    let m = e.next(Duration::from_secs(5)).unwrap();
    assert_eq!(m["method"], "session/update");
}

// ---- cancel ---------------------------------------------------------------------------------------------------------------------

#[test]
fn a_cancel_of_a_single_open_prompt_answers_cancelled_quickly_and_kills_the_shell_tool() {
    let ws = scratch();
    let mut e = Engine::start(&ws);
    let sid = e.handshake();
    let pids = ws.join("tool.pids");
    let id = e.prompt(
        &sid,
        &format!(
            r#"run @fake{{"tool": {{"secs": 60, "pidfile": "{}"}}}}"#,
            pids.display()
        ),
    );
    let tool = wait_for(&pids, 1)[0];
    assert!(alive(tool), "TRIGGER PRESENT: the tool runs");
    let t = Instant::now();
    e.notify("session/cancel", json!({"sessionId": sid}));
    let (r, before) = e.response(id, Duration::from_secs(5)).unwrap();
    assert_eq!(stop_of(&r), "cancelled");
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert!(
        before
            .iter()
            .any(|m| m["params"]["update"]["rawOutput"]["error"] == "Command was aborted"),
        "the aborted tool is reported as the pinned engine reports it: {before:?}"
    );
    let end = Instant::now() + Duration::from_secs(3);
    while alive(tool) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(tool), "the shell tool is gone after the cancel");
    // a new prompt on the same session works
    let id = e.prompt(&sid, "after");
    let (r, _) = e.response(id, Duration::from_secs(5)).unwrap();
    assert_eq!(stop_of(&r), "end_turn");
}

#[test]
fn a_tool_that_leaves_an_orphan_leaves_it_after_a_cancel_and_after_a_kill_of_the_engine_group() {
    let ws = scratch();
    let mut e = Engine::start(&ws);
    let sid = e.handshake();
    let pids = ws.join("orphan.pids");
    let id = e.prompt(
        &sid,
        &format!(
            r#"run @fake{{"tool": {{"secs": 60, "orphan": true, "pidfile": "{}"}}}}"#,
            pids.display()
        ),
    );
    let found = wait_for(&pids, 2);
    e.notify("session/cancel", json!({"sessionId": sid}));
    e.response(id, Duration::from_secs(5)).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    // the shell tool is gone, the orphan it started is not (the case a process-group kill and a cancel both miss)
    assert!(
        found.iter().any(|p| alive(*p)),
        "TRIGGER PRESENT: an orphan survives: {found:?}"
    );
    Command::new("kill")
        .args(["-KILL", "--", &format!("-{}", e.child.id())])
        .status()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        found.iter().any(|p| alive(*p)),
        "and it survives a kill of the engine's process group"
    );
}

#[test]
fn survive_cancel_keeps_the_tool_running_after_the_cancel() {
    let ws = scratch();
    let mut e = Engine::start(&ws);
    let sid = e.handshake();
    let pids = ws.join("surv.pids");
    let id = e.prompt(
        &sid,
        &format!(
            r#"run @fake{{"tool": {{"secs": 60, "survive_cancel": true, "pidfile": "{}"}}}}"#,
            pids.display()
        ),
    );
    let tool = wait_for(&pids, 1)[0];
    e.notify("session/cancel", json!({"sessionId": sid}));
    e.response(id, Duration::from_secs(5)).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        alive(tool),
        "the tool the engine did not kill is still running"
    );
}

#[test]
fn ignore_cancel_makes_an_engine_that_answers_no_cancel() {
    let ws = scratch();
    let mut e = Engine::start(&ws);
    let sid = e.handshake();
    let id = e.prompt(
        &sid,
        r#"stuck @fake{"work_ms": 2500, "ignore_cancel": true}"#,
    );
    std::thread::sleep(Duration::from_millis(200));
    e.notify("session/cancel", json!({"sessionId": sid}));
    assert!(
        e.response(id, Duration::from_millis(1500)).is_none(),
        "no answer within 1.5 s"
    );
    let (r, _) = e.response(id, Duration::from_secs(5)).unwrap();
    assert_eq!(
        stop_of(&r),
        "end_turn",
        "it finished the work as if nothing was asked"
    );
}

// ---- death, outage, generations --------------------------------------------------------------------------------------------------

#[test]
fn die_ms_ends_the_process_in_the_middle_of_a_turn_with_no_response() {
    let ws = scratch();
    let mut e = Engine::start(&ws);
    let sid = e.handshake();
    let id = e.prompt(&sid, r#"x @fake{"work_ms": 3000, "die_ms": 200}"#);
    assert!(e.response(id, Duration::from_secs(3)).is_none());
    let end = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(s) = e.child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < end, "the engine is still running");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(3));
}

#[test]
fn fail_times_answers_the_first_prompts_with_the_providers_503() {
    let ws = scratch();
    let mut e = Engine::start(&ws);
    let sid = e.handshake();
    for _ in 0..2 {
        let id = e.prompt(&sid, r#"flaky @fake{"fail_times": 2}"#);
        let (r, _) = e.response(id, Duration::from_secs(5)).unwrap();
        assert_eq!(r["error"]["code"], -32603);
        assert_eq!(r["error"]["data"]["statusCode"], 503);
        assert_eq!(r["error"]["data"]["isRetryable"], true);
    }
    let id = e.prompt(&sid, r#"flaky @fake{"fail_times": 2}"#);
    let (r, _) = e.response(id, Duration::from_secs(5)).unwrap();
    assert_eq!(stop_of(&r), "end_turn");
}

#[test]
fn a_restarted_engine_names_its_sessions_differently() {
    let ws = scratch();
    let a = {
        let mut e = Engine::start(&ws);
        e.handshake()
    };
    let b = {
        let mut e = Engine::start(&ws);
        e.handshake()
    };
    assert_ne!(a, b);
}

#[test]
fn the_log_lists_every_prompt_with_its_open_and_end_and_marks_the_overlap() {
    let ws = scratch();
    let mut e = Engine::start(&ws);
    let sid = e.handshake();
    let id = e.prompt(&sid, "one");
    e.response(id, Duration::from_secs(5)).unwrap();
    let id = e.prompt(&sid, "two");
    e.response(id, Duration::from_secs(5)).unwrap();
    let log = e.log();
    let opens = log.lines().filter(|l| l.starts_with("prompt-open")).count();
    let ends = log.lines().filter(|l| l.starts_with("prompt-end")).count();
    assert_eq!((opens, ends), (2, 2), "{log}");
    assert!(!log.contains("overlap"), "{log}");
}
