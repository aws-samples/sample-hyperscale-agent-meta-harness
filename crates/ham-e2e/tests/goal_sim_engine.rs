//! The goal-loop emulation inside the `fake-acp-engine` binary, over stdio like agentd uses it: a scenario with a
//! `goal.json` claims the goal at the first prompt, goes on by itself with its scripted rounds, answers a prompt that arrives during a round at
//! once, ignores `session/cancel` and ends when the client closes. The in-process branches are in `goal_sim.rs`.
#![allow(clippy::unwrap_used)]

use harness_core::engine_profile::fixtures_dir;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

const SID: &str = "fake-session-0001";
const LIMIT: Duration = Duration::from_secs(30);

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn scratch(label: &str) -> PathBuf {
    let base = std::env::var_os("TMPDIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")), PathBuf::from);
    let d = base.join(format!("goal-engine-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("ws/src")).unwrap();
    std::fs::write(
        d.join("ws/src/calc.py"),
        "def add(a, b):\n    return a + b\n",
    )
    .unwrap();
    d
}

struct Engine {
    child: Child,
    rx: Receiver<Value>,
    seen: Vec<Value>,
    dir: PathBuf,
}

impl Engine {
    /// Starts the engine on the scenario `name` with the goal plugin's folder placed and the goal pending, as agentd leaves them.
    fn start(label: &str, name: &str) -> Engine {
        let d = scratch(label);
        let (data, config) = (d.join("data"), d.join("config"));
        for p in ["goal", "harness-rounds"] {
            std::fs::create_dir_all(config.join("plugins").join(p)).unwrap();
            std::fs::write(
                config.join("plugins").join(p).join("index.js"),
                "export default {};\n",
            )
            .unwrap();
        }
        std::fs::create_dir_all(data.join("plugins/goal")).unwrap();
        std::fs::write(
            data.join("plugins/goal/goals.json"),
            json!({"version": 1, "pendingGoal": {"goal": "make the checks pass", "createdAt": "2026-10-16T00:00:00Z"}, "activeGoals": {}}).to_string(),
        )
        .unwrap();
        Engine::spawn(
            d,
            name,
            &fixtures_dir(&repo(), "fake-acp").join("transcripts"),
            &data,
            &config,
        )
    }

    fn spawn(dir: PathBuf, name: &str, transcripts: &Path, data: &Path, config: &Path) -> Engine {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fake-acp-engine"))
            .arg("--transcripts")
            .arg(transcripts)
            .arg("--workspace")
            .arg(dir.join("ws"))
            .env("FAKE_ACP_SCENARIO", name)
            .env("FAKE_ACP_LOG", data.join("fake.log"))
            .env("FAKE_ACP_DATA_DIR", data)
            .env("FAKE_ACP_CONFIG_DIR", config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { return };
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    return;
                }
            }
        });
        Engine {
            child,
            rx,
            seen: vec![],
            dir,
        }
    }

    fn send(&mut self, v: &Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    }

    /// Reads until a message satisfies `pred` (it is returned), or fails after the limit; everything read is kept in `seen`.
    fn until(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let end = Instant::now() + LIMIT;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(v) => {
                    self.seen.push(v.clone());
                    if pred(&v) {
                        return v;
                    }
                }
                Err(RecvTimeoutError::Timeout) => panic!(
                    "no {what} within {LIMIT:?}; saw {} messages",
                    self.seen.len()
                ),
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("the engine closed its output before {what}")
                }
            }
        }
    }

    fn handshake_and_first_prompt(&mut self) {
        self.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}}));
        self.until("the initialize response", |v| v["id"] == 1);
        self.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": self.dir.join("ws"), "mcpServers": []}}));
        self.until("the session/new response", |v| v["id"] == 2);
        self.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": SID, "prompt": [{"type": "text", "text": "Make the checks pass"}]}}));
        let done = self.until("the first answer", |v| v["id"] == 3);
        assert_eq!(done["result"]["stopReason"], "end_turn");
    }

    /// The state file; the engine rewrites it in place like the plugin does, so a read may meet a half-written file and is tried again.
    fn state(&self) -> Value {
        let end = Instant::now() + LIMIT;
        loop {
            let read = std::fs::read_to_string(self.dir.join("data/plugins/goal/goals.json"));
            if let Some(v) = read.ok().and_then(|t| serde_json::from_str(&t).ok()) {
                return v;
            }
            assert!(Instant::now() < end, "the state file never parsed");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn rounds(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.join("data/plugins/harness-rounds/rounds.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok()) // the engine may be midway through a line
            .collect()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("data/fake.log")).unwrap_or_default()
    }

    /// Waits until the observer has written `n` end lines.
    fn wait_rounds(&self, n: usize) {
        let end = Instant::now() + LIMIT;
        while self.rounds().iter().filter(|l| l["ev"] == "end").count() < n {
            assert!(
                Instant::now() < end,
                "the observer never wrote {n} end lines"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits until the engine's log holds `needle`.
    fn wait_log(&self, needle: &str) {
        let end = Instant::now() + LIMIT;
        while !self.log().contains(needle) {
            assert!(
                Instant::now() < end,
                "the engine's log never said {needle:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Closes the client's side and waits for the engine to exit by itself.
    fn close(mut self) -> std::process::ExitStatus {
        drop(self.child.stdin.take());
        let end = Instant::now() + LIMIT;
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s;
            }
            assert!(
                Instant::now() < end,
                "the engine did not exit after its input closed"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn completed(v: &Value) -> bool {
    v.pointer("/params/update/rawOutput/completed") == Some(&json!(true))
}

fn text_of(v: &Value) -> Option<&str> {
    (v.pointer("/params/update/sessionUpdate") == Some(&json!("agent_message_chunk")))
        .then(|| {
            v.pointer("/params/update/content/text")
                .and_then(Value::as_str)
        })
        .flatten()
}

#[test]
fn the_engine_claims_the_goal_and_goes_on_by_itself_until_the_completion_call_is_accepted() {
    let mut e = Engine::start("loop", "goal_loop_passes");
    e.handshake_and_first_prompt();
    assert_eq!(
        e.state()["activeGoals"][SID]["goal"],
        "make the checks pass",
        "claimed at the first prompt"
    );
    // no further request: the next rounds arrive as notifications
    e.until("the completion call", completed);
    // the model's words come after its tool calls
    e.until("the final message", |v| {
        text_of(v) == Some("Goal marked complete.")
    });
    let texts: Vec<_> = e.seen.iter().filter_map(text_of).collect();
    assert!(
        texts.contains(&"Not sure yet; running the checks."),
        "{texts:?}"
    );
    assert!(e.state()["activeGoals"].as_object().unwrap().is_empty());
    e.wait_rounds(3);
    let rounds = e.rounds();
    let (starts, ends) = (
        rounds.iter().filter(|l| l["ev"] == "start").count(),
        rounds.iter().filter(|l| l["ev"] == "end").count(),
    );
    assert_eq!(
        (starts, ends),
        (3, 3),
        "the first run and two follow-up rounds: {rounds:?}"
    );
    assert!(e.close().success());
}

#[test]
fn a_prompt_during_a_round_is_answered_at_once_and_a_cancel_changes_nothing() {
    let mut e = Engine::start("message", "goal_user_message");
    e.handshake_and_first_prompt();
    // round two holds its answer open for 600 ms after its tool call
    e.until("round two's tool call", |v| {
        v.pointer("/params/update/title").and_then(Value::as_str) == Some("run_commands: pytest")
    });
    e.send(&json!({"jsonrpc": "2.0", "id": 4, "method": "session/prompt", "params": {"sessionId": SID, "prompt": [{"type": "text", "text": "Add a README line"}]}}));
    e.send(&json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": SID}}));
    let answer = e.until("the answer to the prompt", |v| v["id"] == 4);
    assert_eq!(answer["result"]["stopReason"], "end_turn");
    e.until("the queued message's round", |v| {
        text_of(v) == Some("Added the README line.")
    });
    e.until("the completion call", completed);
    assert!(
        std::fs::read_to_string(e.dir.join("ws/README.md"))
            .unwrap()
            .contains("percent_change")
    );
    let log = e.log();
    assert!(
        log.contains("prompt queued during a round")
            && log.contains("session/cancel ignored by the goal loop"),
        "{log}"
    );
    assert!(e.close().success());
}

#[test]
fn a_prompt_after_the_loop_died_runs_as_a_normal_prompt() {
    let mut e = Engine::start("dead", "goal_dead_loop_then_done");
    e.handshake_and_first_prompt();
    // round two aborts: the loop is dead and queues nothing more
    e.wait_log("the loop stopped");
    e.send(&json!({"jsonrpc": "2.0", "id": 5, "method": "session/prompt", "params": {"sessionId": SID, "prompt": [{"type": "text", "text": "Continue"}]}}));
    let answer = e.until("the answer to the prompt", |v| v["id"] == 5);
    assert_eq!(answer["result"]["stopReason"], "end_turn");
    assert!(e.log().contains("prompt runs on a dead loop"));
    assert!(e.close().success());
}

#[test]
fn a_goal_script_that_cannot_be_read_stops_the_engine_with_status_one() {
    let d = scratch("badscript");
    let transcripts = d.join("transcripts");
    std::fs::create_dir_all(transcripts.join("direct/broken")).unwrap();
    std::fs::write(transcripts.join("direct/broken/goal.json"), "{").unwrap();
    let (data, config) = (d.join("data"), d.join("config"));
    std::fs::create_dir_all(&data).unwrap();
    let mut e = Engine::spawn(d, "broken", &transcripts, &data, &config);
    let status = loop {
        if let Some(s) = e.child.try_wait().unwrap() {
            break s;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(1));
}

#[test]
fn a_first_run_the_script_names_is_the_prompt_and_session_cancel_ends_it_with_cancelled() {
    let mut e = Engine::start("firstpoll", "goal_first_polling");
    e.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}}));
    e.until("the initialize response", |v| v["id"] == 1);
    e.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": e.dir.join("ws"), "mcpServers": []}}));
    e.until("the session/new response", |v| v["id"] == 2);
    e.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": SID, "prompt": [{"type": "text", "text": "Make the checks pass"}]}}));
    // 40 varied tool calls arrive and the run stays open
    let mut titles = std::collections::BTreeSet::new();
    while titles.len() < 40 {
        let v = e.until("a tool call", |v| {
            v.pointer("/params/update/sessionUpdate") == Some(&json!("tool_call"))
        });
        titles.insert(v["params"]["update"]["title"].as_str().unwrap().to_string());
    }
    assert_eq!(titles.len(), 40);
    e.send(&json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": SID}}));
    let done = e.until("the answer to the prompt", |v| v["id"] == 3);
    assert_eq!(done["result"]["stopReason"], "cancelled");
    assert!(
        e.log().contains("session/cancel ends the round"),
        "{}",
        e.log()
    );
    assert!(e.close().success());
}

/// Starts the first prompt of `name` and returns its first tool call and the update that ends it, then cancels the rest of the run.
fn first_call_and_update(label: &str, name: &str) -> (Value, Value) {
    let mut e = Engine::start(label, name);
    e.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}}));
    e.until("the initialize response", |v| v["id"] == 1);
    e.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": e.dir.join("ws"), "mcpServers": []}}));
    e.until("the session/new response", |v| v["id"] == 2);
    e.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": SID, "prompt": [{"type": "text", "text": "Make the checks pass"}]}}));
    let call = e.until("a tool call", |v| {
        v.pointer("/params/update/sessionUpdate") == Some(&json!("tool_call"))
    });
    let id = call["params"]["update"]["toolCallId"].clone();
    let update = e.until("the update that ends it", |v| {
        v.pointer("/params/update/sessionUpdate") == Some(&json!("tool_call_update"))
            && v.pointer("/params/update/toolCallId") == Some(&id)
    });
    e.send(&json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": SID}}));
    e.until("the answer to the prompt", |v| v["id"] == 3);
    assert!(e.close().success());
    (
        call["params"]["update"].clone(),
        update["params"]["update"].clone(),
    )
}

#[test]
fn a_scripted_tool_call_carries_its_input_and_its_output_like_the_real_engines() {
    // goal_explore_distinct: a read with an input and an answer
    let (call, update) = first_call_and_update("io-read", "goal_explore_distinct");
    assert_eq!(call["kind"], "read");
    assert_eq!(
        call["rawInput"],
        json!({"files": [{"path": "src/module_01.rs"}]})
    );
    assert_eq!(update["status"], "completed");
    assert_eq!(update["rawOutput"][0]["success"], true);
    assert!(
        update["rawOutput"][0]["result"]
            .as_str()
            .unwrap()
            .starts_with("// module 01\n"),
        "{update}"
    );
    // goal_explore_failing: the call ends with the status failed and its answer says so
    let (call, update) = first_call_and_update("io-failed", "goal_explore_failing");
    assert_eq!(call["rawInput"]["commands"][0], "cat missing/file_01.rs");
    assert_eq!(update["status"], "failed");
    assert_eq!(update["rawOutput"][0]["success"], false);
    // a bare scripted call (no input, no output) is what it always was
    let (call, update) = first_call_and_update("io-bare", "goal_first_polling");
    assert!(call.get("rawInput").is_none(), "{call}");
    assert_eq!(update["status"], "completed");
    assert!(update.get("rawOutput").is_none(), "{update}");
}
