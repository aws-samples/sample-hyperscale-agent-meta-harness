//! The fake Cline engine served in process, over two pipes. The agentd tests start the same engine
//! as a child process, which the coverage run does not measure; these tests drive every directive and every request kind of it through
//! `serve`, so its lines are measured and its promises are asserted where they are made: a second prompt while one is open is queued with
//! usage 0, a cancel of a single open prompt kills the shell tool, an orphan survives, a cancel after the queue was used does nothing.
#![allow(clippy::unwrap_used)]

use ham_e2e::fake_cline::{Config, serve};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, PipeWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

fn scratch(label: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let base = std::env::var_os("TMPDIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")), PathBuf::from);
    let d = base.join(format!(
        "fake-cline-{label}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
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

fn wait_until(secs: u64, what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn stop(pids: &[u32]) {
    for p in pids {
        let _ = Command::new("kill")
            .args(["-KILL", &p.to_string()])
            .status();
    }
}

struct Engine {
    input: Option<PipeWriter>,
    lines: mpsc::Receiver<String>,
    died: Arc<AtomicBool>,
    handle: Option<JoinHandle<std::process::ExitCode>>,
    ws: PathBuf,
    next_id: u64,
}

impl Engine {
    fn start(label: &str, model: Option<&str>) -> Self {
        let ws = scratch(label);
        let (req_r, req_w) = std::io::pipe().unwrap();
        let (res_r, res_w) = std::io::pipe().unwrap();
        let died = Arc::new(AtomicBool::new(false));
        let flag = died.clone();
        let cfg = Config {
            workspace: ws.clone(),
            log: Some(ws.join("logs/fake.log")),
            model: model.map(String::from),
        };
        let handle = std::thread::spawn(move || {
            serve(
                cfg,
                BufReader::new(req_r),
                Box::new(res_w),
                Box::new(move || flag.store(true, Ordering::SeqCst)),
            )
        });
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(res_r).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            input: Some(req_w),
            lines: rx,
            died,
            handle: Some(handle),
            ws,
            next_id: 0,
        }
    }

    fn raw(&mut self, line: &str) {
        let w = self.input.as_mut().unwrap();
        writeln!(w, "{line}").unwrap();
        w.flush().unwrap();
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.raw(&json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string());
    }

    fn call(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.raw(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        );
        id
    }

    /// The response with `id` and the notifications that came before it.
    fn response(&mut self, id: u64, secs: u64) -> Option<(Value, Vec<Value>)> {
        let end = Instant::now() + Duration::from_secs(secs);
        let mut before = Vec::new();
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            let Ok(line) = self.lines.recv_timeout(left) else {
                break;
            };
            let v: Value = serde_json::from_str(&line).unwrap();
            if v.get("id") == Some(&json!(id)) {
                return Some((v, before));
            }
            before.push(v);
        }
        None
    }

    fn session(&mut self) -> String {
        let id = self.call("initialize", json!({"protocolVersion": 1}));
        let (r, _) = self.response(id, 5).unwrap();
        assert_eq!(r["result"]["agentInfo"]["name"], "fake-cline");
        let id = self.call("session/new", json!({"cwd": "/w", "mcpServers": []}));
        let (r, _) = self.response(id, 5).unwrap();
        r["result"]["sessionId"].as_str().unwrap().to_string()
    }

    fn prompt(&mut self, sid: &str, text: &str) -> u64 {
        self.call(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]}),
        )
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.ws.join("logs/fake.log")).unwrap_or_default()
    }

    fn finish(mut self) {
        drop(self.input.take());
        let _ = self.handle.take().unwrap().join();
    }
}

fn stop_of(r: &Value) -> &str {
    r["result"]["stopReason"].as_str().unwrap_or("")
}

fn text_chunks(notes: &[Value]) -> Vec<String> {
    notes
        .iter()
        .filter_map(|n| n.pointer("/params/update/content/text"))
        .filter_map(|t| t.as_str().map(String::from))
        .collect()
}

#[test]
fn a_plain_prompt_writes_its_file_streams_its_message_and_ends_with_usage() {
    let mut e = Engine::start("plain", Some("test-model"));
    let id = e.call("initialize", json!({}));
    e.response(id, 5).unwrap();
    let id = e.call("session/new", json!({"cwd": "/w"}));
    let (r, _) = e.response(id, 5).unwrap();
    let sid = r["result"]["sessionId"].as_str().unwrap().to_string();
    assert_eq!(
        r["result"]["configOptions"][0]["currentValue"],
        "test-model"
    );
    let id = e.call(
        "session/set_config_option",
        json!({"sessionId": sid, "configId": "model", "value": "other-model"}),
    );
    let (r, _) = e.response(id, 5).unwrap();
    assert_eq!(
        r["result"]["configOptions"][0]["currentValue"],
        "other-model"
    );

    let id = e.prompt(
        &sid,
        r#"do it @fake{"write": {"path": "out/a.txt", "text": "hi"}, "say": "all DONE", "work_ms": 20}"#,
    );
    let (r, notes) = e.response(id, 5).unwrap();
    assert_eq!(stop_of(&r), "end_turn");
    assert!(
        r["result"]["_meta"]["usage"]["totalTokens"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(text_chunks(&notes), ["working", "all DONE"]);
    assert_eq!(
        std::fs::read_to_string(e.ws.join("out/a.txt")).unwrap(),
        "hi"
    );
    assert!(e.log().contains("write out/a.txt"), "{}", e.log());

    // a prompt for a session that does not exist and a method that does not exist are errors; junk is ignored
    let id = e.prompt("nobody", "x");
    let (r, _) = e.response(id, 5).unwrap();
    assert_eq!(r["error"]["code"], -32602);
    let id = e.call("session/fork", json!({}));
    let (r, _) = e.response(id, 5).unwrap();
    assert_eq!(r["error"]["code"], -32601);
    e.raw("");
    e.raw("this is not json");
    e.notify("session/something", json!({}));
    e.notify("session/cancel", json!({"sessionId": "nobody"}));
    let id = e.prompt(&sid, "after the junk");
    assert_eq!(stop_of(&e.response(id, 5).unwrap().0), "end_turn");
    assert!(e.log().contains("cancel nobody acted=false"), "{}", e.log());
    e.finish();
}

#[test]
fn a_cancel_of_a_single_open_prompt_kills_its_shell_tool_and_answers_cancelled() {
    let mut e = Engine::start("cancel", None);
    let sid = e.session();
    let pidfile = e.ws.join("tool.pids");
    let id = e.prompt(
        &sid,
        &format!(
            r#"run @fake{{"work_ms": 10, "cancel_reply_ms": 20, "tool": {{"secs": 30, "pidfile": "{}"}}}}"#,
            pidfile.display()
        ),
    );
    wait_until(10, "the tool's pid", || !pids_of(&pidfile).is_empty());
    let tool = pids_of(&pidfile)[0];
    assert!(alive(tool), "TRIGGER PRESENT: the tool runs");
    e.notify("session/cancel", json!({"sessionId": sid}));
    let (r, notes) = e.response(id, 5).unwrap();
    assert_eq!(stop_of(&r), "cancelled");
    assert!(
        notes
            .iter()
            .any(|n| n.pointer("/params/update/rawOutput/error")
                == Some(&json!("Command was aborted"))),
        "{notes:?}"
    );
    wait_until(5, "the tool to end", || !alive(tool));
    // the session takes a new prompt
    let id = e.prompt(&sid, "after");
    assert_eq!(stop_of(&e.response(id, 5).unwrap().0), "end_turn");
    e.finish();
}

#[test]
fn a_tool_that_survives_the_cancel_and_an_orphan_outlive_it() {
    let mut e = Engine::start("orphan", None);
    let sid = e.session();
    let pidfile = e.ws.join("orphan.pids");
    let id = e.prompt(
        &sid,
        &format!(
            r#"run @fake{{"work_ms": 10, "cancel_reply_ms": 20, "tool": {{"secs": 30, "orphan": true, "survive_cancel": true, "pidfile": "{}"}}}}"#,
            pidfile.display()
        ),
    );
    wait_until(10, "the orphan's and the tool's pids", || {
        pids_of(&pidfile).len() >= 2
    });
    let pids = pids_of(&pidfile);
    assert!(pids.iter().all(|p| alive(*p)), "TRIGGER PRESENT: {pids:?}");
    e.notify("session/cancel", json!({"sessionId": sid}));
    assert_eq!(stop_of(&e.response(id, 5).unwrap().0), "cancelled");
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        pids.iter().all(|p| alive(*p)),
        "both outlived the cancel: {pids:?}"
    );
    stop(&pids);
    e.finish();
}

#[test]
fn a_tool_that_ends_by_itself_completes_the_turn() {
    let mut e = Engine::start("tool-ends", None);
    let sid = e.session();
    let pidfile = e.ws.join("short.pids");
    let id = e.prompt(
        &sid,
        &format!(
            r#"run @fake{{"work_ms": 10, "tool": {{"secs": 1, "pidfile": "{}"}}}}"#,
            pidfile.display()
        ),
    );
    let (r, notes) = e.response(id, 10).unwrap();
    assert_eq!(stop_of(&r), "end_turn");
    assert!(
        notes
            .iter()
            .any(|n| n.pointer("/params/update/status") == Some(&json!("completed"))),
        "{notes:?}"
    );
    e.finish();
}

#[test]
fn a_second_prompt_while_one_is_open_is_queued_with_usage_zero_and_a_later_cancel_does_nothing() {
    let mut e = Engine::start("queue", None);
    let sid = e.session();
    let first = e.prompt(&sid, r#"first @fake{"work_ms": 500}"#);
    wait_until(5, "the first prompt to open", || {
        e.log().contains("prompt-open")
    });
    let second = e.prompt(&sid, r#"second @fake{"say": "from the queue"}"#);
    let (r, _) = e.response(second, 5).unwrap();
    assert_eq!(stop_of(&r), "end_turn");
    assert_eq!(r["result"]["_meta"]["usage"]["totalTokens"], 0, "{r}");
    assert!(e.log().contains("overlap"), "TRIGGER PRESENT: {}", e.log());
    e.notify("session/cancel", json!({"sessionId": sid}));
    wait_until(5, "the cancel to be logged", || {
        e.log().contains("acted=false")
    });
    let (r, _) = e.response(first, 5).unwrap();
    assert_eq!(stop_of(&r), "end_turn", "the cancel did nothing: {r}");
    wait_until(5, "the queue to drain", || e.log().contains("drained-end"));
    e.finish();
}

#[test]
fn a_queued_reply_on_an_idle_session_answers_at_once_and_the_text_runs_unattached() {
    let mut e = Engine::start("queued-reply", None);
    let sid = e.session();
    let id = e.prompt(&sid, r#"swallowed @fake{"queued_reply": true}"#);
    let (r, _) = e.response(id, 5).unwrap();
    assert_eq!(stop_of(&r), "end_turn");
    assert_eq!(r["result"]["_meta"]["usage"]["totalTokens"], 0);
    wait_until(5, "the drained turn", || e.log().contains("drained-end"));
    // and a prompt after it is a plain prompt again
    let id = e.prompt(&sid, "plain");
    assert_eq!(stop_of(&e.response(id, 5).unwrap().0), "end_turn");
    e.finish();
}

#[test]
fn a_provider_failure_is_answered_the_scripted_number_of_times() {
    let mut e = Engine::start("fail", None);
    let sid = e.session();
    let text = r#"flaky @fake{"fail_times": 2}"#;
    for _ in 0..2 {
        let id = e.prompt(&sid, text);
        let (r, _) = e.response(id, 5).unwrap();
        assert_eq!(r["error"]["code"], -32603);
        assert_eq!(r["error"]["data"]["statusCode"], 503);
    }
    let id = e.prompt(&sid, text);
    assert_eq!(stop_of(&e.response(id, 5).unwrap().0), "end_turn");
    e.finish();
}

#[test]
fn an_engine_that_ignores_the_cancel_answers_when_its_work_ends_and_one_that_dies_sends_nothing() {
    let mut e = Engine::start("deaf", None);
    let sid = e.session();
    let id = e.prompt(
        &sid,
        r#"stuck @fake{"work_ms": 300, "ignore_cancel": true}"#,
    );
    wait_until(5, "the prompt to open", || e.log().contains("prompt-open"));
    e.notify("session/cancel", json!({"sessionId": sid}));
    let (r, _) = e.response(id, 5).unwrap();
    assert_eq!(stop_of(&r), "end_turn", "the cancel was not acted on: {r}");
    assert!(e.log().contains("acted=false"));

    let id = e.prompt(&sid, r#"fatal @fake{"work_ms": 5000, "die_ms": 30}"#);
    wait_until(5, "the engine to die", || e.died.load(Ordering::SeqCst));
    assert!(
        e.response(id, 1).is_none(),
        "a dead engine sends no response"
    );
    assert!(e.log().contains("exiting 3"));
    e.finish();
}

#[test]
fn a_directive_for_another_generation_is_ignored_and_a_tool_directive_with_a_cancel_during_work_ends_cancelled()
 {
    let mut e = Engine::start("gen", None);
    let sid = e.session();
    let id = e.prompt(&sid, r#"x @fake{"gen": 9, "say": "never said"}"#);
    let (r, notes) = e.response(id, 5).unwrap();
    assert_eq!(stop_of(&r), "end_turn");
    assert_eq!(text_chunks(&notes), ["working", "OK"]);
    // a cancel while the model is still working (no tool yet)
    let id = e.prompt(&sid, r#"y @fake{"work_ms": 2000, "cancel_reply_ms": 10}"#);
    wait_until(5, "the prompt to open", || {
        e.log().matches("prompt-open").count() >= 2
    });
    e.notify("session/cancel", json!({"sessionId": sid}));
    assert_eq!(stop_of(&e.response(id, 5).unwrap().0), "cancelled");
    // a directive that is not valid JSON is a plain prompt
    let id = e.prompt(&sid, "z @fake{not json}");
    assert_eq!(stop_of(&e.response(id, 5).unwrap().0), "end_turn");
    e.finish();
}

#[test]
fn a_streamed_answer_runs_to_its_end_or_stops_at_the_cancel_and_sends_nothing_after_its_answer() {
    let mut e = Engine::start("stream", None);
    let sid = e.session();
    // to its end
    let id = e.prompt(
        &sid,
        r#"talk @fake{"work_ms": 5, "stream": {"chunks": 4, "step_ms": 10}}"#,
    );
    let (r, notes) = e.response(id, 5).unwrap();
    assert_eq!(stop_of(&r), "end_turn");
    let streamed: Vec<String> = text_chunks(&notes)
        .into_iter()
        .filter(|t| t.starts_with("stream-"))
        .collect();
    assert_eq!(streamed, ["stream-1", "stream-2", "stream-3", "stream-4"]);
    // cancelled in the middle
    let id = e.prompt(
        &sid,
        r#"talk more @fake{"work_ms": 5, "cancel_reply_ms": 20, "stream": {"chunks": 200, "step_ms": 20}}"#,
    );
    wait_until(5, "three chunks", || {
        e.log().matches("stream-3").count() >= 2
    });
    e.notify("session/cancel", json!({"sessionId": sid}));
    let (r, notes) = e.response(id, 5).unwrap();
    assert_eq!(stop_of(&r), "cancelled");
    let n = text_chunks(&notes)
        .iter()
        .filter(|t| t.starts_with("stream-"))
        .count();
    assert!((3..200).contains(&n), "{n} chunks before the cancel");
    // nothing of the cancelled turn arrives after its answer
    std::thread::sleep(Duration::from_millis(200));
    let late: Vec<String> = e.lines.try_iter().collect();
    assert!(late.iter().all(|l| !l.contains("stream-")), "{late:?}");
    e.finish();
}

#[test]
fn a_file_edit_in_parts_completes_or_stops_at_the_cancel_leaving_the_parts_written() {
    let mut e = Engine::start("edit", None);
    let sid = e.session();
    let file = e.ws.join("src/edit.txt");
    // to its end
    let id = e.prompt(
        &sid,
        r#"edit @fake{"work_ms": 5, "edit": {"path": "src/edit.txt", "parts": ["a\n", "b\n", "c\n"], "step_ms": 10}}"#,
    );
    let (r, notes) = e.response(id, 5).unwrap();
    assert_eq!(stop_of(&r), "end_turn");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "a\nb\nc\n");
    assert!(
        notes
            .iter()
            .any(|n| n.pointer("/params/update/kind") == Some(&json!("edit"))),
        "{notes:?}"
    );
    // cancelled after the first parts: the parts written stay, nothing is written after the answer
    let id = e.prompt(
        &sid,
        r#"edit again @fake{"work_ms": 5, "cancel_reply_ms": 20, "edit": {"path": "src/other.txt", "parts": ["1\n", "2\n", "3\n", "4\n", "5\n", "6\n"], "step_ms": 150}}"#,
    );
    let other = e.ws.join("src/other.txt");
    wait_until(5, "the first part", || {
        std::fs::read_to_string(&other).is_ok_and(|t| t.starts_with("1\n"))
    });
    e.notify("session/cancel", json!({"sessionId": sid}));
    let (r, notes) = e.response(id, 5).unwrap();
    assert_eq!(stop_of(&r), "cancelled");
    assert!(
        notes.iter().any(|n| n
            .pointer("/params/update/rawOutput/error")
            .is_some_and(|v| v.as_str().is_some_and(|s| s.contains("aborted")))),
        "the aborted edit is reported: {notes:?}"
    );
    let at_answer = std::fs::read_to_string(&other).unwrap();
    assert!(
        at_answer.starts_with("1\n") && at_answer != "1\n2\n3\n4\n5\n6\n",
        "partly done: {at_answer:?}"
    );
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        std::fs::read_to_string(&other).unwrap(),
        at_answer,
        "no write after the answer"
    );
    e.finish();
}
