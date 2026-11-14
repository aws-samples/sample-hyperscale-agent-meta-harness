//! The fake Cline engine served in process, over two pipes. The agentd tests start the same engine
//! as a child process, which the coverage run does not measure; these tests drive every directive and every request kind of it through
//! `serve`, so its lines are measured and its promises are asserted where they are made: a second prompt while one is open is queued with
//! usage 0, a cancel of a single open prompt kills the shell tool, an orphan survives, a cancel after the queue was used does nothing.
//!
//! The `b836_` tests hold the order at the two ends of a turn: a session is free for the next prompt, and the turn's end is in
//! the log, before the answer leaves; a prompt is open for its cancel before the engine reads the next line. A client that sends its next
//! prompt the moment it has read an answer is then never queued behind a turn that already answered, however busy the machine is.
//! The end of a turn is one step for the engine's reader: two more tests send a prompt while the answer before it is still inside the
//! engine, and that prompt is not opened until the answer has left.
#![allow(clippy::unwrap_used)]

use ham_e2e::fake_cline::{Config, serve};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, PipeWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
        Self::start_with(label, model, |pipe, _| Box::new(pipe))
    }

    /// As [`Engine::start`], with the engine's output going through what `out` makes of the pipe; `out` also gets the path of the engine's log.
    fn start_with(
        label: &str,
        model: Option<&str>,
        out: impl FnOnce(PipeWriter, &Path) -> Box<dyn Write + Send>,
    ) -> Self {
        let ws = scratch(label);
        let (req_r, req_w) = std::io::pipe().unwrap();
        let (res_r, res_w) = std::io::pipe().unwrap();
        let died = Arc::new(AtomicBool::new(false));
        let flag = died.clone();
        let log = ws.join("logs/fake.log");
        let out = out(res_w, &log);
        let cfg = Config {
            workspace: ws.clone(),
            log: Some(log),
            model: model.map(String::from),
        };
        let handle = std::thread::spawn(move || {
            serve(
                cfg,
                BufReader::new(req_r),
                out,
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

    /// Several lines in one write, so the engine finds the second as soon as it has dealt with the first.
    fn raw_together(&mut self, lines: &[String]) {
        let mut bytes = Vec::new();
        for l in lines {
            bytes.extend_from_slice(l.as_bytes());
            bytes.push(b'\n');
        }
        let w = self.input.as_mut().unwrap();
        w.write_all(&bytes).unwrap();
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

// ---- the order at the two ends of a turn ---------------------------------------------------------------------------------

/// What the order tests count: how many answers are still to be held, and how many holds ended with the next prompt read by the engine.
#[derive(Default)]
struct Hold {
    left: AtomicUsize,
    met: AtomicUsize,
}

/// The engine's output for the order tests. Every line goes on to the pipe at once. After an answer to a prompt, the write does not
/// return until the engine's log shows that the engine has read the client's next prompt (one more `in` line of a `session/prompt`).
/// The thread that writes an answer is the turn's own, so the client's next prompt is inside the engine while the turn that answered has
/// not left its end: the gap that a loaded machine opens by chance is open here every time. The end of a turn is one step for the
/// engine's reader, so the engine judges that prompt when the write has returned, and it has to judge it a prompt of its own.
struct HeldAnswers {
    pipe: PipeWriter,
    line: Vec<u8>,
    log: PathBuf,
    hold: Arc<Hold>,
}

/// The prompts the engine has read, admitted or not: the `in` lines of its log that hold a `session/prompt`.
fn prompts_read(log: &Path) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("in ") && l.contains(r#""method":"session/prompt""#))
        .count()
}

fn lines_starting(log: &str, prefix: &str) -> usize {
    log.lines().filter(|l| l.starts_with(prefix)).count()
}

/// As [`lines_starting`], for the log file as it is now.
fn lines_in(log: &Path, prefix: &str) -> usize {
    lines_starting(&std::fs::read_to_string(log).unwrap_or_default(), prefix)
}

impl Write for HeldAnswers {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.line.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let line = std::mem::take(&mut self.line);
        let answer = serde_json::from_slice::<Value>(&line)
            .is_ok_and(|v| v.get("id").is_some() && v.get("method").is_none());
        let hold = answer
            && self
                .hold
                .left
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok();
        // read before the answer leaves: the client cannot have sent its next prompt yet
        let want = prompts_read(&self.log) + 1;
        self.pipe.write_all(&line)?;
        self.pipe.flush()?;
        if hold {
            let end = Instant::now() + Duration::from_secs(10);
            while Instant::now() < end {
                if prompts_read(&self.log) >= want {
                    self.hold.met.fetch_add(1, Ordering::SeqCst);
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    }
}

/// An engine whose answers are held as [`HeldAnswers`] holds them, with a session; `prompts` is how many prompts the test will send.
fn held_engine(label: &str, prompts: usize) -> (Engine, String, Arc<Hold>) {
    let hold = Arc::new(Hold::default());
    let h = hold.clone();
    let mut e = Engine::start_with(label, None, move |pipe, log| {
        Box::new(HeldAnswers {
            pipe,
            line: Vec::new(),
            log: log.to_path_buf(),
            hold: h,
        })
    });
    let sid = e.session();
    // nothing follows the last answer
    hold.left.store(prompts - 1, Ordering::SeqCst);
    (e, sid, hold)
}

const B836_FAILS: &str = r#"flaky @fake{"fail_times": 1000000}"#;
const B836_PLAIN: &str = r#"plain @fake{"work_ms": 0}"#;

fn usage_total(r: &Value) -> u64 {
    r["result"]["_meta"]["usage"]["totalTokens"]
        .as_u64()
        .unwrap_or(0)
}

#[test]
fn b836_a_prompt_sent_the_moment_a_provider_failure_is_read_is_a_prompt_of_its_own() {
    const PROMPTS: usize = 20;
    let (mut e, sid, hold) = held_engine("b836-fail", PROMPTS);
    for k in 1..=PROMPTS {
        let id = e.prompt(&sid, B836_FAILS);
        let (r, _) = e.response(id, 20).unwrap();
        assert_eq!(
            r["error"]["code"],
            -32603,
            "prompt {k} went behind a turn that had already answered: {r}\n{}",
            e.log()
        );
    }
    // TRIGGER PRESENT: every answer but the last was still being written when the engine read the next prompt
    assert_eq!(hold.met.load(Ordering::SeqCst), PROMPTS - 1, "{}", e.log());
    assert_eq!(lines_starting(&e.log(), "overlap "), 0, "{}", e.log());
    e.finish();
}

#[test]
fn b836_a_prompt_sent_the_moment_an_answer_is_read_is_a_prompt_of_its_own() {
    const PROMPTS: usize = 20;
    let (mut e, sid, hold) = held_engine("b836-plain", PROMPTS);
    for k in 1..=PROMPTS {
        let id = e.prompt(&sid, B836_PLAIN);
        let (r, notes) = e.response(id, 20).unwrap();
        assert_eq!(stop_of(&r), "end_turn", "prompt {k}: {r}");
        assert!(
            usage_total(&r) > 0,
            "prompt {k} went behind a turn that had already answered: {r}\n{}",
            e.log()
        );
        assert_eq!(text_chunks(&notes), ["working", "OK"], "prompt {k}");
    }
    // TRIGGER PRESENT: every answer but the last was still being written when the engine read the next prompt
    assert_eq!(hold.met.load(Ordering::SeqCst), PROMPTS - 1, "{}", e.log());
    assert_eq!(lines_starting(&e.log(), "overlap "), 0, "{}", e.log());
    e.finish();
}

#[test]
fn b836_the_end_of_a_turn_is_in_the_log_before_its_answer_leaves() {
    const PROMPTS: usize = 10;
    let (mut e, sid, hold) = held_engine("b836-log", PROMPTS);
    for k in 1..=PROMPTS {
        // odd prompts end with the provider's failure, even ones with a plain answer
        let id = e.prompt(&sid, if k % 2 == 1 { B836_FAILS } else { B836_PLAIN });
        e.response(id, 20).unwrap();
        // the turn's own thread is still inside the write of this answer (the hold), so the line is there only if it was written first
        let log = e.log();
        assert_eq!(
            lines_starting(&log, "prompt-end "),
            k,
            "the answer of prompt {k} left before its prompt-end line:\n{log}"
        );
    }
    assert_eq!(hold.met.load(Ordering::SeqCst), PROMPTS - 1, "{}", e.log());
    // every prompt opened after the one before it had ended
    let mut open = false;
    for l in e.log().lines() {
        if l.starts_with("prompt-open ") {
            assert!(
                !open,
                "a prompt opened while another was open:\n{}",
                e.log()
            );
            open = true;
        } else if l.starts_with("prompt-end ") {
            open = false;
        }
    }
    e.finish();
}

#[test]
fn b836_a_cancel_that_follows_its_prompt_at_once_reaches_that_prompt() {
    const PROMPTS: usize = 20;
    let mut e = Engine::start("b836-cancel", None);
    let sid = e.session();
    for k in 1..=PROMPTS {
        e.next_id += 1;
        let id = e.next_id;
        let prompt = json!({"jsonrpc": "2.0", "id": id, "method": "session/prompt", "params": {"sessionId": sid,
            "prompt": [{"type": "text", "text": r#"long @fake{"work_ms": 4000, "cancel_reply_ms": 1}"#}]}});
        let cancel =
            json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}});
        e.raw_together(&[prompt.to_string(), cancel.to_string()]);
        let (r, _) = e.response(id, 20).unwrap();
        assert_eq!(
            stop_of(&r),
            "cancelled",
            "the cancel of prompt {k} found no open prompt: {r}\n{}",
            e.log()
        );
    }
    assert_eq!(
        lines_starting(&e.log(), &format!("cancel {sid} acted=true")),
        PROMPTS,
        "{}",
        e.log()
    );
    e.finish();
}

/// How long an answer stays kept back after the engine has read the prompt that came too early, when the engine leaves that prompt alone.
const KEPT_BACK: Duration = Duration::from_millis(300);

/// What the tests of a prompt that comes too early count.
#[derive(Default)]
struct Kept {
    /// How many answers are still to be kept back.
    left: AtomicUsize,
    /// An answer is being kept back now.
    now: AtomicBool,
    /// The engine read the client's next prompt while an answer was kept back.
    read: AtomicUsize,
    /// The engine opened that prompt, as a prompt of its own, while the answer was still kept back.
    opened: AtomicUsize,
    /// The engine queued that prompt while the answer was still kept back.
    queued: AtomicUsize,
}

/// The engine's output for the tests of a prompt that comes too early. An answer to a prompt is kept back: nothing of it is in the pipe,
/// so the client cannot have read it. It goes on to the pipe when the engine has dealt with the client's next prompt (one more
/// `prompt-open` or `overlap` line in its log), or [`KEPT_BACK`] after the engine has read that prompt and left it alone.
struct AnswersKeptBack {
    pipe: PipeWriter,
    line: Vec<u8>,
    log: PathBuf,
    kept: Arc<Kept>,
}

impl Write for AnswersKeptBack {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.line.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let line = std::mem::take(&mut self.line);
        let answer = serde_json::from_slice::<Value>(&line)
            .is_ok_and(|v| v.get("id").is_some() && v.get("method").is_none());
        let keep = answer
            && self
                .kept
                .left
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok();
        if keep {
            let read = prompts_read(&self.log);
            let opened = lines_in(&self.log, "prompt-open ");
            let queued = lines_in(&self.log, "overlap ");
            self.kept.now.store(true, Ordering::SeqCst);
            let end = Instant::now() + Duration::from_secs(10);
            let mut read_at = None;
            while Instant::now() < end {
                if read_at.is_none() && prompts_read(&self.log) > read {
                    self.kept.read.fetch_add(1, Ordering::SeqCst);
                    read_at = Some(Instant::now());
                }
                if lines_in(&self.log, "prompt-open ") > opened {
                    self.kept.opened.fetch_add(1, Ordering::SeqCst);
                    break;
                }
                if lines_in(&self.log, "overlap ") > queued {
                    self.kept.queued.fetch_add(1, Ordering::SeqCst);
                    break;
                }
                if read_at.is_some_and(|t| t.elapsed() >= KEPT_BACK) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            self.kept.now.store(false, Ordering::SeqCst);
        }
        self.pipe.write_all(&line)?;
        self.pipe.flush()
    }
}

/// What a prompt that came too early met.
struct Early {
    kept: Arc<Kept>,
    first: Value,
    second: Value,
    log: String,
    /// The two prompts in the engine's log, in its order: `open` and `end` of a prompt, `answer` for the line that sends one of the two answers.
    marks: Vec<&'static str>,
}

/// A client that sends its next prompt while the answer of the prompt before it is still inside the engine: `text`, twice.
fn a_prompt_sent_before_the_answer_has_left(label: &str, text: &str) -> Early {
    let kept = Arc::new(Kept::default());
    let k = kept.clone();
    let mut e = Engine::start_with(label, None, move |pipe, log| {
        Box::new(AnswersKeptBack {
            pipe,
            line: Vec::new(),
            log: log.to_path_buf(),
            kept: k,
        })
    });
    let sid = e.session();
    kept.left.store(1, Ordering::SeqCst);
    let first = e.prompt(&sid, text);
    wait_until(10, "the first answer to be on its way out", || {
        kept.now.load(Ordering::SeqCst)
    });
    // TRIGGER PRESENT: the client does not have the first answer when it sends its second prompt
    let early: Vec<Value> = e
        .lines
        .try_iter()
        .map(|l| serde_json::from_str(&l).unwrap())
        .collect();
    assert!(
        early.iter().all(|v| v.get("id") != Some(&json!(first))),
        "the first answer reached the client while it was kept back: {early:?}"
    );
    let second = e.prompt(&sid, text);
    let (r1, _) = e.response(first, 20).unwrap();
    let (r2, _) = e.response(second, 20).unwrap();
    let log = e.log();
    e.finish();
    let marks = log
        .lines()
        .filter_map(|l| {
            if l.starts_with("prompt-open ") {
                Some("open")
            } else if l.starts_with("prompt-end ") {
                Some("end")
            } else if l.starts_with("overlap ") {
                Some("overlap")
            } else {
                let sent: Value = serde_json::from_str(l.strip_prefix("out ")?).ok()?;
                let id = sent.get("id")?;
                (id == &json!(first) || id == &json!(second)).then_some("answer")
            }
        })
        .collect();
    Early {
        kept,
        first: r1,
        second: r2,
        log,
        marks,
    }
}

/// What both tests of a prompt that came too early ask of the engine. The first answer was still inside the engine when the engine read
/// the second prompt, and the engine did not open that prompt before the answer had left.
///
/// The engine does not queue the prompt either. The answer was on its way out when the prompt came in, and the engine cannot tell this
/// prompt from one that a client sent after it had read the answer; to queue it would queue that client's prompt too.
/// So the prompt waits for the answer and then runs as a prompt of its own. The header of `fake_cline.rs` states this limit.
fn assert_not_opened_before_the_answer_left(early: &Early) {
    let log = &early.log;
    // TRIGGER PRESENT: the engine read the second prompt while the first answer had not left
    assert_eq!(early.kept.read.load(Ordering::SeqCst), 1, "{log}");
    assert_eq!(
        early.kept.opened.load(Ordering::SeqCst),
        0,
        "the engine opened the second prompt, as a prompt of its own, while the answer of the first had not left:\n{log}"
    );
    assert_eq!(early.kept.queued.load(Ordering::SeqCst), 0, "{log}");
    assert_eq!(
        early.marks,
        ["open", "end", "answer", "open", "end", "answer"],
        "{log}"
    );
}

#[test]
fn b836_a_prompt_read_while_an_answer_is_on_its_way_out_is_not_opened_before_the_answer_has_left() {
    let early = a_prompt_sent_before_the_answer_has_left("b836-early-plain", B836_PLAIN);
    assert_not_opened_before_the_answer_left(&early);
    for (k, r) in [&early.first, &early.second].into_iter().enumerate() {
        assert_eq!(stop_of(r), "end_turn", "answer {}: {r}", k + 1);
        assert!(usage_total(r) > 0, "answer {}: {r}\n{}", k + 1, early.log);
    }
}

#[test]
fn b836_a_prompt_read_while_a_provider_failure_is_on_its_way_out_is_not_opened_before_it_has_left()
{
    let early = a_prompt_sent_before_the_answer_has_left("b836-early-fail", B836_FAILS);
    assert_not_opened_before_the_answer_left(&early);
    for (k, r) in [&early.first, &early.second].into_iter().enumerate() {
        assert_eq!(
            r["error"]["code"],
            -32603,
            "answer {}: {r}\n{}",
            k + 1,
            early.log
        );
    }
}

/// Threads that do nothing but use a processor each, one per processor of the machine, until the value is dropped: with them every
/// thread of the engine and of the test competes for its turn on a processor, as on a loaded build host.
struct BusyThreads {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl BusyThreads {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let n = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
        let threads = (0..n)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }
                })
            })
            .collect();
        Self { stop, threads }
    }
}

impl Drop for BusyThreads {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[test]
fn b836_a_thousand_exchanges_beside_busy_threads_are_never_answered_from_the_queue() {
    // Eight clients, each with an engine of its own, so the thousand exchanges of each kind take seconds beside the busy threads: a
    // thread that is woken there waits for its turn on a processor at every step of an exchange.
    const CLIENTS: usize = 8;
    const EACH: usize = 125;
    const _: () = assert!(CLIENTS * EACH == 1000);
    let busy = BusyThreads::start();
    let started = Instant::now();
    let clients: Vec<JoinHandle<(Vec<String>, usize)>> = (0..CLIENTS)
        .map(|c| {
            std::thread::spawn(move || {
                let mut e = Engine::start(&format!("b836-stress-{c}"), None);
                let sid = e.session();
                let mut from_the_queue = Vec::new();
                // a provider failure, and the next prompt the moment the failure is read
                for k in 1..=EACH {
                    let id = e.prompt(&sid, B836_FAILS);
                    let (r, _) = e.response(id, 60).unwrap();
                    if r["error"]["code"] != -32603 {
                        from_the_queue.push(format!("client {c} failure {k}"));
                    }
                }
                // a plain answer, and the next prompt the moment the answer is read
                for k in 1..=EACH {
                    let id = e.prompt(&sid, B836_PLAIN);
                    let (r, _) = e.response(id, 60).unwrap();
                    if usage_total(&r) == 0 {
                        from_the_queue.push(format!("client {c} plain {k}"));
                    }
                }
                let overlaps = lines_starting(&e.log(), "overlap ");
                e.finish();
                (from_the_queue, overlaps)
            })
        })
        .collect();
    let mut from_the_queue = Vec::new();
    let mut overlaps = 0;
    for c in clients {
        let (q, o) = c.join().unwrap();
        from_the_queue.extend(q);
        overlaps += o;
    }
    let took = started.elapsed();
    drop(busy);
    assert!(
        from_the_queue.is_empty() && overlaps == 0,
        "{} of {} prompts were answered from the engine's queue ({overlaps} overlaps in the engines' logs, {took:?}); the first: {:?}",
        from_the_queue.len(),
        2 * CLIENTS * EACH,
        &from_the_queue[..from_the_queue.len().min(8)]
    );
}
