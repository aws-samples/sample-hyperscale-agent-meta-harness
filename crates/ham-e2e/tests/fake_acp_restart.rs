//! The behaviours the fake engine gained for the user correction path: a second `session/new` in one process
//! (`FAKE_ACP_NEW_SESSION_SCENARIO`), a restarted process that keeps its turn cursor (`FAKE_ACP_KEEP_CURSOR`) or replays another scenario
//! (`FAKE_ACP_RESTART_SCENARIO`), a death or a silence that happens only once per run, and the `sessions.log` that tells a test which Cline
//! session answered which prompt. The replayed scenarios are the eight directories added with them under
//! `fixtures/engines/fake-acp/transcripts/direct/`. Without the new variables the engine behaves as before (the tests marked "as before").

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A fresh workspace with the file the done turns edit; the name is unique, so nothing is ever deleted.
fn scratch(label: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let base = std::env::var_os("TMPDIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")), PathBuf::from);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let d = base.join(format!(
        "fake-acp-restart-{label}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(d.join("ws/src")).unwrap();
    std::fs::create_dir_all(d.join("ws/tests")).unwrap();
    std::fs::write(
        d.join("ws/src/calc.py"),
        "def add(a, b):\n    return a + b\n",
    )
    .unwrap();
    d.join("ws")
}

struct Eng {
    child: std::process::Child,
    out: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Eng {
    /// One engine process on `ws` replaying `direct/<scenario>/`, with the given extra environment.
    fn start(ws: &Path, scenario: &str, env: &[(&str, &str)]) -> Eng {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fake-acp-engine"))
            .arg("--transcripts")
            .arg(repo().join("fixtures/engines/fake-acp/transcripts"))
            .arg("--workspace")
            .arg(ws)
            .env("FAKE_ACP_SCENARIO", scenario)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Eng {
            child,
            out,
            next_id: 0,
        }
    }

    fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        )
        .unwrap();
        stdin.flush().unwrap();
        id
    }

    /// A notification (no id), such as `session/cancel`.
    fn notify(&mut self, method: &str, params: Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "method": method, "params": params})
        )
        .unwrap();
        stdin.flush().unwrap();
    }

    /// Reads until the response to `id`; the notifications before it come second. `None` when the process closed its output first.
    fn response(&mut self, id: u64) -> Option<(Value, Vec<Value>)> {
        let mut notes = vec![];
        loop {
            let mut line = String::new();
            if self.out.read_line(&mut line).unwrap() == 0 {
                return None;
            }
            let v: Value = serde_json::from_str(&line).unwrap();
            if v.get("id") == Some(&json!(id)) && v.get("method").is_none() {
                return Some((v, notes));
            }
            notes.push(v);
        }
    }

    fn call(&mut self, method: &str, params: Value) -> (Value, Vec<Value>) {
        let id = self.send(method, params);
        self.response(id).expect("the engine closed its output")
    }

    fn session_new(&mut self, ws: &Path) -> Value {
        self.call("session/new", json!({"cwd": ws, "mcpServers": []}))
            .0
    }

    /// `initialize` and `session/new`; returns the session id.
    fn handshake(&mut self, ws: &Path) -> String {
        self.call("initialize", json!({"protocolVersion": 1}));
        self.session_new(ws)["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn prompt(&mut self, sid: &str, text: &str) -> (Value, Vec<Value>) {
        self.call(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]}),
        )
    }

    /// Closes stdin and waits for the exit.
    fn finish(mut self) -> ExitStatus {
        drop(self.child.stdin.take());
        self.child.wait().unwrap()
    }
}

fn stop_reason(resp: &Value) -> String {
    resp["result"]["stopReason"]
        .as_str()
        .unwrap_or("<none>")
        .to_string()
}

fn sessions_log(ws: &Path) -> Vec<String> {
    std::fs::read_to_string(ws.join(".fake-acp/sessions.log"))
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

const A15: &str = "The format of the value at messages.7.content.0.toolUse.input is invalid. Provide a json object for the field and try again.";

#[test]
fn a_second_session_new_replays_the_new_session_scenario_from_its_first_turn() {
    let ws = scratch("newsession");
    let engine_log = ws.with_file_name("fake-acp.log");
    let mut e = Eng::start(
        &ws,
        "replay_rejected_then_fresh_done",
        &[
            ("FAKE_ACP_NEW_SESSION_SCENARIO", "fresh_session_done"),
            ("FAKE_ACP_LOG", engine_log.to_str().unwrap()),
        ],
    );
    let s1 = e.handshake(&ws);
    assert_eq!(s1, "fake-session-0001");
    for text in ["the task", "continue"] {
        let (r, _) = e.prompt(&s1, text);
        assert_eq!(r["error"]["code"], -32603, "{r}");
        assert!(r["error"]["message"].as_str().unwrap().contains(A15), "{r}");
        assert_eq!(r["error"]["data"]["statusCode"], 400);
        assert_eq!(r["error"]["data"]["isRetryable"], false);
    }
    let again = e.session_new(&ws);
    assert_eq!(
        again["result"]["sessionId"], "fake-session-0002",
        "the second session/new answers with the new scenario's session: {again}"
    );
    let (r, notes) = e.prompt("fake-session-0002", "fresh prompt with the notes");
    assert_eq!(stop_reason(&r), "end_turn", "{r}");
    assert!(
        notes
            .iter()
            .all(|n| n["params"]["sessionId"] == "fake-session-0002"),
        "no update of the old session is replayed: {notes:?}"
    );
    let calc = std::fs::read_to_string(ws.join("src/calc.py")).unwrap();
    assert!(calc.contains("def percent_change(a, b):"), "{calc}");
    assert_eq!(
        sessions_log(&ws),
        [
            "new-session fake-session-0001 generation=0",
            "prompt fake-session-0001 history=0",
            "prompt fake-session-0001 history=1",
            "new-session fake-session-0002 generation=0",
            "prompt fake-session-0002 history=0",
        ]
    );
    // the same lines are in the engine's log (the state directory is removed from a work tree when a run ends)
    let logged: Vec<String> = std::fs::read_to_string(&engine_log)
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_prefix("sessions: "))
        .map(String::from)
        .collect();
    assert_eq!(logged, sessions_log(&ws));
    e.finish();
}

#[test]
fn a_second_session_new_with_nothing_to_replay_is_refused_as_before() {
    let ws = scratch("refused");
    let mut e = Eng::start(&ws, "timeout_then_done", &[]);
    e.handshake(&ws);
    let again = e.session_new(&ws);
    assert_eq!(again["error"]["code"], -32600, "{again}");
    // the engine keeps serving
    let (r, _) = e.prompt("fake-session-0001", "go");
    assert_eq!(r["error"]["code"], -32603, "{r}");
    e.finish();
}

#[test]
fn with_the_cursor_kept_a_second_session_new_gets_its_own_id_and_goes_on_with_the_next_turn() {
    let ws = scratch("samecursor");
    let mut e = Eng::start(
        &ws,
        "stuck_engine_stop_then_done",
        &[("FAKE_ACP_KEEP_CURSOR", "1")],
    );
    let s1 = e.handshake(&ws);
    assert_eq!(stop_reason(&e.prompt(&s1, "the task").0), "cancelled");
    let again = e.session_new(&ws);
    let s2 = again["result"]["sessionId"].as_str().unwrap().to_string();
    assert_eq!(
        s2, "fake-session-0001-s2",
        "a new session has its own id: {again}"
    );
    // the cursor continues: the second turn of the scenario, under the new id
    let (r, notes) = e.prompt(&s2, "again");
    assert_eq!(stop_reason(&r), "cancelled");
    assert!(
        notes
            .iter()
            .all(|n| n["params"]["sessionId"] == s2.as_str()),
        "{notes:?}"
    );
    // the old session is gone: a prompt for it is refused and uses no turn
    let (stale, _) = e.prompt(&s1, "stale");
    assert_eq!(stale["error"]["code"], -32602, "{stale}");
    assert!(
        stale["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown session"),
        "{stale}"
    );
    assert_eq!(
        sessions_log(&ws),
        [
            "new-session fake-session-0001 generation=0",
            "prompt fake-session-0001 history=0",
            "new-session fake-session-0001-s2 generation=0",
            "prompt fake-session-0001-s2 history=0",
        ]
    );
    e.finish();
}

#[test]
fn a_restarted_process_keeps_the_turn_cursor_when_asked_to() {
    let ws = scratch("keepcursor");
    let env = [("FAKE_ACP_KEEP_CURSOR", "1")];
    // the loop guard belongs to the Cline session: two stops of five identical calls, then stops after one call each
    for (generation, calls) in [5usize, 5, 1, 1].into_iter().enumerate() {
        let mut e = Eng::start(&ws, "stuck_engine_stop_then_done", &env);
        let sid = e.handshake(&ws);
        let (r, notes) = e.prompt(&sid, "go on");
        assert_eq!(stop_reason(&r), "cancelled", "generation {generation}: {r}");
        let titles: Vec<&str> = notes
            .iter()
            .filter_map(|n| n.pointer("/params/update/title").and_then(Value::as_str))
            .collect();
        assert_eq!(titles.len(), calls, "generation {generation}: {titles:?}");
        assert!(titles.iter().all(|t| *t == titles[0]), "{titles:?}");
        e.finish();
    }
    let calc = std::fs::read_to_string(ws.join("src/calc.py")).unwrap();
    assert!(
        !calc.contains("percent_change"),
        "no turn of this scenario writes the work: {calc}"
    );
    let news: Vec<String> = sessions_log(&ws)
        .into_iter()
        .filter(|l| l.starts_with("new-session"))
        .collect();
    assert_eq!(
        news,
        [
            "new-session fake-session-0001 generation=0",
            "new-session fake-session-0001 generation=1",
            "new-session fake-session-0001 generation=2",
            "new-session fake-session-0001 generation=3",
        ]
    );
}

#[test]
fn a_restarted_process_starts_over_without_the_cursor_as_before() {
    let ws = scratch("nocursor");
    for _ in 0..2 {
        let mut e = Eng::start(&ws, "stuck_engine_stop_then_done", &[]);
        let sid = e.handshake(&ws);
        assert_eq!(stop_reason(&e.prompt(&sid, "go").0), "cancelled");
        e.finish();
    }
    assert!(
        !ws.join(".fake-acp/sessions.log").exists(),
        "no new variable, no sessions.log: the workspace state is what it was"
    );
}

#[test]
fn a_death_on_prompt_happens_once_per_run_and_the_restart_scenario_takes_over() {
    let ws = scratch("dies");
    let env = [
        ("FAKE_ACP_DIE_ON_PROMPT", "3"),
        ("FAKE_ACP_RESTART_SCENARIO", "fresh_session_done"),
    ];
    let mut first = Eng::start(&ws, "engine_dies_then_fresh", &env);
    let sid = first.handshake(&ws);
    assert_eq!(sid, "fake-session-0001");
    let id = first.send(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "the task"}]}),
    );
    assert!(
        first.response(id).is_none(),
        "the prompt's response never comes: the process died first"
    );
    let status = first.child.wait().unwrap();
    assert_eq!(status.code(), Some(3), "{status:?}");

    // the restarted process replays the restart scenario and does not die again
    let mut second = Eng::start(&ws, "engine_dies_then_fresh", &env);
    let sid = second.handshake(&ws);
    assert_eq!(sid, "fake-session-0002", "the restart scenario's session");
    let (r, _) = second.prompt(&sid, "the notes");
    assert_eq!(stop_reason(&r), "end_turn", "{r}");
    assert!(second.finish().success());
    let calc = std::fs::read_to_string(ws.join("src/calc.py")).unwrap();
    assert!(calc.contains("def percent_change(a, b):"), "{calc}");
    assert_eq!(
        sessions_log(&ws),
        [
            "new-session fake-session-0001 generation=0",
            "prompt fake-session-0001 history=0",
            "new-session fake-session-0002 generation=1",
            "prompt fake-session-0002 history=0",
        ]
    );
}

#[test]
fn a_death_on_prompt_without_the_cursor_kills_every_process_as_before() {
    let ws = scratch("diesalways");
    for _ in 0..2 {
        let mut e = Eng::start(
            &ws,
            "engine_dies_then_fresh",
            &[("FAKE_ACP_DIE_ON_PROMPT", "3")],
        );
        let sid = e.handshake(&ws);
        let id = e.send(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type": "text", "text": "go"}]}),
        );
        assert!(e.response(id).is_none());
        assert_eq!(e.child.wait().unwrap().code(), Some(3));
    }
}

#[test]
fn the_silence_covers_only_the_first_prompts_when_limited() {
    let ws = scratch("silence");
    let mut e = Eng::start(
        &ws,
        "silence_then_done",
        &[
            ("FAKE_ACP_SILENCE_S", "2"),
            ("FAKE_ACP_SILENCE_PROMPTS", "1"),
        ],
    );
    let sid = e.handshake(&ws);
    let t0 = Instant::now();
    let (r1, _) = e.prompt(&sid, "the task");
    assert_eq!(stop_reason(&r1), "end_turn");
    assert!(
        t0.elapsed() >= Duration::from_millis(1900),
        "the first prompt was silent: {:?}",
        t0.elapsed()
    );
    let t1 = Instant::now();
    let (r2, _) = e.prompt(&sid, "resume");
    assert_eq!(stop_reason(&r2), "end_turn");
    assert!(
        t1.elapsed() < Duration::from_millis(1500),
        "the second prompt was not silent: {:?}",
        t1.elapsed()
    );
    e.finish();
}

/// What each new scenario does turn by turn when driven by one session: the stop (or the error) and what changed.
#[test]
fn the_new_scenarios_replay_their_stops_in_order() {
    let a15_error = |r: &Value| {
        r["error"]["code"] == -32603 && r["error"]["message"].as_str().unwrap().contains(A15)
    };
    // (scenario, per turn: the stop reason or "error", whether calc.py has percent_change afterwards)
    let table: [(&str, &[(&str, bool)]); 5] = [
        (
            "stuck_engine_stop_then_done",
            &[
                ("cancelled", false),
                ("cancelled", false),
                ("cancelled", false),
                ("cancelled", false),
            ],
        ),
        (
            "gave_up_then_done",
            &[("end_turn", false), ("end_turn", true)],
        ),
        (
            "max_tokens_then_done",
            &[("max_tokens", false), ("end_turn", true)],
        ),
        (
            "try_model_then_done",
            &[("refusal", false), ("end_turn", true)],
        ),
        (
            "replay_rejected_then_fresh_done",
            &[("error", false), ("error", false)],
        ),
    ];
    for (scenario, turns) in table {
        let ws = scratch(scenario);
        let mut e = Eng::start(&ws, scenario, &[]);
        let sid = e.handshake(&ws);
        for (i, (want, edited)) in turns.iter().enumerate() {
            let (r, _) = e.prompt(&sid, "go");
            if *want == "error" {
                assert!(a15_error(&r), "{scenario} turn {}: {r}", i + 1);
            } else {
                assert_eq!(&stop_reason(&r), want, "{scenario} turn {}: {r}", i + 1);
            }
            let calc = std::fs::read_to_string(ws.join("src/calc.py")).unwrap();
            assert_eq!(
                calc.contains("def percent_change(a, b):"),
                *edited,
                "{scenario} turn {}",
                i + 1
            );
        }
        e.finish();
    }
}

#[test]
fn try_model_then_done_reports_the_switched_model_and_logs_it() {
    let ws = scratch("trymodel");
    let mut e = Eng::start(&ws, "try_model_then_done", &[]);
    let sid = e.handshake(&ws);
    assert_eq!(stop_reason(&e.prompt(&sid, "the task").0), "refusal");
    let (sw, _) = e.call(
        "session/set_config_option",
        json!({"sessionId": sid, "configId": "model", "value": "global.anthropic.claude-sonnet-5-5"}),
    );
    assert_eq!(
        sw["result"]["configOptions"][0]["currentValue"], "global.anthropic.claude-sonnet-5-5",
        "{sw}"
    );
    assert_eq!(stop_reason(&e.prompt(&sid, "continue").0), "end_turn");
    let log = std::fs::read_to_string(ws.join(".fake-acp/turns.log")).unwrap();
    assert_eq!(
        log.lines().collect::<Vec<_>>(),
        [
            "prompt",
            "model global.anthropic.claude-sonnet-5-5",
            "prompt"
        ]
    );
    e.finish();
}

#[test]
fn the_new_scenario_directories_exist_with_a_handshake_and_a_first_turn() {
    for name in [
        "stuck_engine_stop_then_done",
        "gave_up_then_done",
        "replay_rejected_then_fresh_done",
        "max_tokens_then_done",
        "silence_then_done",
        "engine_dies_then_fresh",
        "try_model_then_done",
        "fresh_session_done",
    ] {
        let dir = repo()
            .join("fixtures/engines/fake-acp/transcripts/direct")
            .join(name);
        assert!(dir.join("handshake.jsonl").is_file(), "{name}");
        assert!(dir.join("turn-1.jsonl").is_file(), "{name}");
    }
}

#[test]
fn an_outage_answers_every_prompt_with_a_503_until_it_is_over_and_uses_no_turn() {
    let ws = scratch("outage");
    let t0 = Instant::now();
    let mut e = Eng::start(&ws, "writes_files", &[("FAKE_ACP_OUTAGE_S", "2")]);
    let sid = e.handshake(&ws);
    for _ in 0..2 {
        let (r, notes) = e.prompt(&sid, "the task");
        assert_eq!(r["error"]["code"], -32603, "{r}");
        assert_eq!(r["error"]["data"]["statusCode"], 503, "{r}");
        assert_eq!(r["error"]["data"]["isRetryable"], true, "{r}");
        assert!(notes.is_empty(), "an outage sends no update: {notes:?}");
    }
    let calc = std::fs::read_to_string(ws.join("src/calc.py")).unwrap();
    assert!(!calc.contains("percent_change"), "{calc}");
    // when the outage is over the scenario answers from its first turn: the errors used none
    std::thread::sleep(Duration::from_millis(2100).saturating_sub(t0.elapsed()));
    let (r, _) = e.prompt(&sid, "the task");
    assert_eq!(stop_reason(&r), "end_turn", "{r}");
    let calc = std::fs::read_to_string(ws.join("src/calc.py")).unwrap();
    assert!(calc.contains("def percent_change(a, b):"), "{calc}");
    e.finish();
}

#[test]
fn the_engine_exits_after_the_nth_prompt_once_per_run_when_the_cursor_is_kept() {
    let ws = scratch("exitafter");
    let env = [
        ("FAKE_ACP_EXIT_AFTER_PROMPT", "1:7"),
        ("FAKE_ACP_KEEP_CURSOR", "1"),
    ];
    let mut first = Eng::start(&ws, "gave_up_then_done", &env);
    let sid = first.handshake(&ws);
    let (r, _) = first.prompt(&sid, "the task");
    assert_eq!(
        stop_reason(&r),
        "end_turn",
        "the response comes before the exit: {r}"
    );
    assert_eq!(first.child.wait().unwrap().code(), Some(7));
    // the restarted process answers from the next turn and stays up: the exit happens once per run
    let mut second = Eng::start(&ws, "gave_up_then_done", &env);
    let sid = second.handshake(&ws);
    let (r, _) = second.prompt(&sid, "again");
    assert_eq!(stop_reason(&r), "end_turn", "{r}");
    let calc = std::fs::read_to_string(ws.join("src/calc.py")).unwrap();
    assert!(calc.contains("def percent_change(a, b):"), "{calc}");
    assert!(second.finish().success());
}

#[test]
fn a_new_session_starts_on_the_launch_model_not_on_the_one_the_old_session_was_switched_to() {
    let ws = scratch("launchmodel");
    let mut e = Eng::start(
        &ws,
        "gave_up_then_done",
        &[
            ("FAKE_ACP_MODEL", "us.anthropic.claude-opus-5-5"),
            ("FAKE_ACP_NEW_SESSION_SCENARIO", "fresh_session_done"),
        ],
    );
    e.call("initialize", json!({"protocolVersion": 1}));
    let first = e.session_new(&ws);
    assert_eq!(
        first["result"]["configOptions"][0]["currentValue"], "us.anthropic.claude-opus-5-5",
        "{first}"
    );
    let (sw, _) = e.call(
        "session/set_config_option",
        json!({"sessionId": "fake-session-0001", "configId": "model", "value": "global.anthropic.claude-sonnet-5-5"}),
    );
    assert_eq!(
        sw["result"]["configOptions"][0]["currentValue"], "global.anthropic.claude-sonnet-5-5",
        "{sw}"
    );
    let second = e.session_new(&ws);
    assert_eq!(
        second["result"]["sessionId"], "fake-session-0002",
        "{second}"
    );
    assert_eq!(
        second["result"]["configOptions"][0]["currentValue"], "us.anthropic.claude-opus-5-5",
        "a new session starts on the launch model: {second}"
    );
    e.finish();
}

#[test]
fn a_cancel_during_a_silence_is_answered_cancelled_after_the_delay() {
    let ws = scratch("cancelsilence");
    let mut e = Eng::start(
        &ws,
        "silence_then_done",
        &[
            ("FAKE_ACP_SILENCE_S", "20"),
            ("FAKE_ACP_SILENCE_PROMPTS", "1"),
            ("FAKE_ACP_CANCEL_DELAY_MS", "400"),
        ],
    );
    let sid = e.handshake(&ws);
    let t0 = Instant::now();
    let id = e.send(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "the task"}]}),
    );
    std::thread::sleep(Duration::from_millis(300));
    e.notify("session/cancel", json!({"sessionId": sid}));
    let (r, _) = e.response(id).expect("the cancelled answer");
    assert_eq!(stop_reason(&r), "cancelled", "{r}");
    assert!(
        t0.elapsed() >= Duration::from_millis(650),
        "the answer is delayed: {:?}",
        t0.elapsed()
    );
    assert!(
        t0.elapsed() < Duration::from_secs(10),
        "the silence was cut short: {:?}",
        t0.elapsed()
    );
    // the engine goes on with the next turn
    let (r2, _) = e.prompt(&sid, "resume");
    assert_eq!(stop_reason(&r2), "end_turn", "{r2}");
    e.finish();
}

#[test]
fn late_notifications_and_a_late_response_of_the_old_session_arrive_with_the_new_sessions_first_prompt()
 {
    let ws = scratch("late");
    let mut e = Eng::start(
        &ws,
        "gave_up_then_done",
        &[
            ("FAKE_ACP_NEW_SESSION_SCENARIO", "fresh_session_done"),
            ("FAKE_ACP_LATE_OLD_SESSION", "1"),
        ],
    );
    let s1 = e.handshake(&ws);
    let old_prompt_id = e.send(
        "session/prompt",
        json!({"sessionId": s1, "prompt": [{"type": "text", "text": "the task"}]}),
    );
    assert!(e.response(old_prompt_id).is_some());
    let s2 = e.session_new(&ws)["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(s2, "fake-session-0002");
    let (r, notes) = e.prompt(&s2, "the fresh prompt");
    assert_eq!(stop_reason(&r), "end_turn", "{r}");
    let late: Vec<&Value> = notes
        .iter()
        .filter(|n| n["params"]["sessionId"] == s1.as_str())
        .collect();
    assert!(
        late.iter()
            .any(|n| n.to_string().contains("LATE-NOTE-FROM-THE-OLD-SESSION")),
        "a late message of the old session: {notes:?}"
    );
    assert!(
        late.iter()
            .any(|n| n.pointer("/params/update/usage/inputTokens") == Some(&json!(999_999))),
        "a late usage update of the old session: {notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n["id"] == json!(old_prompt_id) && n["result"]["stopReason"] == "end_turn"),
        "a late second response to the old prompt: {notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n["params"]["sessionId"] == s2.as_str()),
        "the new session's own updates follow: {notes:?}"
    );
    e.finish();
}
