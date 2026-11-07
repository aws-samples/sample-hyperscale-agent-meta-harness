//! Emulation of the engine's goal plugin and of the harness's observer plugin for the fake ACP engine.
//!
//! A scenario directory with a `goal.json` makes the fake engine behave like the real one with the plugins placed: when the goal plugin's
//! folder exists in the engine's configuration directory the first prompt claims the goal that agentd wrote to the state file, and when
//! a run ends with status `completed` and the goal is still active the engine queues its own follow-up run after `gap_ms`, with no
//! `session/prompt` request. Each follow-up run is the next entry of `rounds`; the observer writes a start and an end line per run into the
//! rounds log. The completion tool is accepted only after a verification round, as the plugin does (`completed:false` before). `session/cancel`
//! is ignored. A client prompt that arrives while a round runs or is queued is answered at once and queued behind it; one that arrives when
//! the loop is dead runs as a normal prompt.
//!
//! `goal.json` (all members optional except `rounds`):
//!
//! ```json
//! {"transcripts": "writes_files", "gap_ms": 20, "claim": true, "observer": true, "repeat_last": false,
//!  "queued": {"text": "Done with your message."},
//!  "rounds": [{"text": "...", "tools": [{"title": "run_commands: pytest", "kind": "execute"}], "edit": {"path": "x.py", "text": "..."},
//!              "status": "completed", "iterations": 3, "delay_ms": 0, "hold_ms": 0, "complete": "summary", "refuse": false, "error": "..."}]}
//! ```
//!
//! `delay_ms` is silence between the start line and the first update; `hold_ms` keeps the round open after its updates.
//!
//! A `tools` entry may also carry what the real engine's calls carry on the wire (the brake's credit for new information): `input`
//! becomes the call's `rawInput`, `output` the update's `rawOutput`, and `"failed": true` ends the call with the status `failed` instead of
//! `completed`. An entry with none of the three is the bare call it always was.

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What the fake engine's loop reads from its input while a round runs.
#[derive(Debug)]
pub enum Recv {
    Line(String),
    Timeout,
    Closed,
}

/// The fake engine's side of a round: where messages go, what is logged, and the input that is serviced while the round waits.
pub trait SimIo {
    /// Writes one JSON-RPC message to the client (edits in its `diff` content are applied to the work tree by the implementation).
    fn emit(&mut self, msg: Value);
    fn log(&mut self, line: &str);
    fn recv(&mut self, wait: Duration) -> Recv;
}

/// What a scripted tool call carries beyond its title and kind.
#[derive(Clone, Debug, Default)]
pub struct ToolIo {
    /// The call's `rawInput`.
    pub input: Option<Value>,
    /// The update's `rawOutput`.
    pub output: Option<Value>,
    /// The call ends with the status `failed`.
    pub failed: bool,
}

/// One follow-up run of the engine.
#[derive(Clone, Debug, Default)]
pub struct RoundScript {
    pub text: String,
    pub tools: Vec<(String, String)>,
    /// What each of `tools` carries; as long as `tools` when the script is read.
    pub tool_io: Vec<ToolIo>,
    pub edit: Option<(String, String)>,
    /// `completed`, `aborted` or `failed`.
    pub status: String,
    /// Model calls in the run; 0 derives it from the tools.
    pub iterations: u32,
    pub delay_ms: u64,
    pub hold_ms: u64,
    pub complete: Option<String>,
    pub refuse: bool,
    pub error: Option<String>,
    /// The edit comes before the tool calls (the order of a model that writes a file and then checks it).
    pub edit_first: bool,
    /// The edit comes after this many tool calls (a model that checks, writes, checks again); the default is after all of them.
    pub edit_after_calls: Option<u32>,
    /// Silence between two tool calls, serviced like `delay_ms` (a polling loop calls over minutes, not in one burst).
    pub tool_gap_ms: u64,
}

#[derive(Clone, Debug)]
pub struct GoalScript {
    /// The scenario whose transcripts serve the first prompt.
    pub transcripts: Option<String>,
    pub gap_ms: u64,
    pub claim: bool,
    pub observer: bool,
    pub repeat_last: bool,
    pub queued: RoundScript,
    pub rounds: Vec<RoundScript>,
    /// The run of the first prompt, instead of the replayed transcript: it ends when its hold is over or when `session/cancel` arrives
    /// (the engine honours a cancel of a prompt; only its follow-up rounds ignore it).
    pub first: Option<RoundScript>,
}

fn round_of(v: &Value) -> RoundScript {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
    let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    RoundScript {
        text: s("text").unwrap_or_default(),
        tools: v
            .get("tools")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|t| {
                        (
                            t["title"].as_str().unwrap_or("tool").to_string(),
                            t["kind"].as_str().unwrap_or("execute").to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
        tool_io: v
            .get("tools")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|t| ToolIo {
                        input: t.get("input").cloned(),
                        output: t.get("output").cloned(),
                        failed: t.get("failed").and_then(Value::as_bool).unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        edit: v.get("edit").map(|e| {
            (
                e["path"].as_str().unwrap_or_default().to_string(),
                e["text"].as_str().unwrap_or_default().to_string(),
            )
        }),
        status: s("status").unwrap_or_else(|| "completed".into()),
        iterations: u32::try_from(n("iterations")).unwrap_or(0),
        delay_ms: n("delay_ms"),
        hold_ms: n("hold_ms"),
        complete: s("complete"),
        refuse: v.get("refuse").and_then(Value::as_bool).unwrap_or(false),
        error: s("error"),
        edit_first: v
            .get("edit_first")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        edit_after_calls: v
            .get("edit_after_calls")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        tool_gap_ms: n("tool_gap_ms"),
    }
}

impl GoalScript {
    pub fn parse(text: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("goal.json: {e}"))?;
        let rounds = v
            .get("rounds")
            .and_then(Value::as_array)
            .ok_or("goal.json: no rounds")?
            .iter()
            .map(round_of)
            .collect();
        Ok(Self {
            transcripts: v
                .get("transcripts")
                .and_then(Value::as_str)
                .map(String::from),
            gap_ms: v.get("gap_ms").and_then(Value::as_u64).unwrap_or(20),
            claim: v.get("claim").and_then(Value::as_bool).unwrap_or(true),
            observer: v.get("observer").and_then(Value::as_bool).unwrap_or(true),
            repeat_last: v
                .get("repeat_last")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            queued: v.get("queued").map_or_else(
                || RoundScript {
                    text: "Done with your message.".into(),
                    status: "completed".into(),
                    ..RoundScript::default()
                },
                round_of,
            ),
            rounds,
            first: v.get("first").map(round_of),
        })
    }

    /// The scenario directory's script, when it has one.
    pub fn load(dir: &Path) -> Result<Option<Self>, String> {
        match std::fs::read_to_string(dir.join("goal.json")) {
            Ok(t) => Self::parse(&t).map(Some),
            Err(_) => Ok(None),
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

/// What a run of the loop needs the main loop to know.
#[derive(Debug)]
pub enum Flow {
    Go,
    /// The client closed the connection.
    Exit,
}

#[derive(Debug)]
pub struct GoalSim {
    script: GoalScript,
    data_dir: PathBuf,
    config_dir: PathBuf,
    /// The first prompt has been answered.
    first_done: bool,
    claimed: bool,
    /// The session that claimed the goal: the plugin claims goals per session.
    claimed_sid: String,
    next: usize,
    due: Option<Instant>,
    queue: VecDeque<RoundScript>,
    /// `session/cancel` ends the round that runs now (the run of the first prompt) instead of being ignored.
    honor_cancel: bool,
    cancelled: bool,
    starts: u64,
    ends: u64,
    ids: u64,
}

impl GoalSim {
    pub fn new(script: GoalScript, data_dir: PathBuf, config_dir: PathBuf) -> Self {
        Self {
            script,
            data_dir,
            config_dir,
            first_done: false,
            claimed: false,
            claimed_sid: String::new(),
            next: 0,
            due: None,
            queue: VecDeque::new(),
            honor_cancel: false,
            cancelled: false,
            starts: 0,
            ends: 0,
            ids: 0,
        }
    }

    fn state_path(&self) -> PathBuf {
        self.data_dir.join("plugins/goal/goals.json")
    }

    fn rounds_path(&self) -> PathBuf {
        self.data_dir.join("plugins/harness-rounds/rounds.jsonl")
    }

    /// The goal plugin is placed in the configuration directory (agentd put it there).
    fn plugin_present(&self) -> bool {
        self.config_dir.join("plugins/goal/index.js").is_file()
    }

    fn observer_present(&self) -> bool {
        self.script.observer
            && self
                .config_dir
                .join("plugins/harness-rounds/index.js")
                .is_file()
    }

    /// The goal loop is engaged: the plugin is placed and the goal was claimed.
    pub fn engaged(&self) -> bool {
        self.first_done && self.claimed
    }

    /// The loop is engaged for the session `sid`: it is the session that claimed the goal. A prompt of another session (the harness
    /// started a fresh one) begins a first run of its own.
    pub fn engaged_for(&self, sid: &str) -> bool {
        self.engaged() && self.claimed_sid == sid
    }

    pub fn next_due(&self) -> Option<Instant> {
        self.due
    }

    fn read_state(&self) -> Value {
        std::fs::read_to_string(self.state_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({"version": 1, "activeGoals": {}}))
    }

    fn write_state(&self, v: &Value) {
        let path = self.state_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Like the plugin: written in place, no lock, no rename.
        let _ = std::fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(v).unwrap_or_default()),
        );
    }

    /// The plugin's `readActiveGoal`: an active goal of the session, else the pending one.
    fn goal_active(&self, sid: &str) -> bool {
        let s = self.read_state();
        let open = |g: &Value| g.get("goal").is_some() && g.get("completedAt").is_none();
        s["activeGoals"].get(sid).is_some_and(open) || s.get("pendingGoal").is_some_and(open)
    }

    fn observer_line(&self, v: &Value) {
        if !self.observer_present() {
            return;
        }
        let path = self.rounds_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{v}");
        }
    }

    fn start_line(&mut self) {
        self.starts += 1;
        self.observer_line(&json!({"ev": "start", "n": self.starts, "t": now_ms()}));
    }

    fn end_line(&mut self, status: &str, iterations: u32, text: &str, error: Option<&str>) {
        self.ends += 1;
        let tail: String = {
            let chars: Vec<char> = text.chars().collect();
            chars[chars.len().saturating_sub(1500)..].iter().collect()
        };
        let mut line = json!({"ev": "end", "n": self.ends, "t": now_ms(), "runId": format!("run-{}", self.ends), "status": status,
            "iterations": iterations, "inputTokens": 100, "outputTokens": 10, "text": tail});
        if let Some(e) = error {
            line["error"] = json!(e);
        }
        self.observer_line(&line);
    }

    /// The plugin's `beforeRun` of the first run: claim the pending goal for the session.
    pub fn begin_first_run(&mut self, sid: &str, io: &mut dyn SimIo) {
        // A session that has not claimed a goal starts over: the plugin looks for a pending goal at the start of each run of a session
        // without one, and a fresh session has no rounds behind it.
        self.first_done = false;
        self.claimed = false;
        self.due = None;
        self.queue.clear();
        self.next = 0;
        if !self.plugin_present() {
            return;
        }
        if self.script.claim {
            let mut s = self.read_state();
            if let Some(mut pending) = s.as_object_mut().and_then(|o| o.remove("pendingGoal"))
                && pending.get("completedAt").is_none()
            {
                let goal = pending["goal"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(60)
                    .collect::<String>();
                pending["sessionId"] = json!(sid);
                s["activeGoals"][sid] = pending;
                self.write_state(&s);
                self.claimed = true;
                self.claimed_sid = sid.to_string();
                io.log(&format!("goal claimed for {sid}: {goal}"));
            }
        } else {
            io.log("goal not claimed (the scenario's plugin never loads)");
        }
        self.start_line();
    }

    /// The first run ended and its messages were written: the plugin's `afterRun`.
    pub fn end_first_run(&mut self, sid: &str, out: &[Value], io: &mut dyn SimIo) {
        self.first_done = true;
        if !self.plugin_present() {
            return;
        }
        let tools = out
            .iter()
            .filter(|m| {
                m.pointer("/params/update/sessionUpdate")
                    .and_then(Value::as_str)
                    == Some("tool_call")
            })
            .count();
        let text = out
            .iter()
            .rev()
            .find_map(|m| {
                (m.pointer("/params/update/sessionUpdate")
                    .and_then(Value::as_str)
                    == Some("agent_message_chunk"))
                .then(|| {
                    m.pointer("/params/update/content/text")
                        .and_then(Value::as_str)
                        .map(String::from)
                })
                .flatten()
            })
            .unwrap_or_default();
        let stop = out
            .iter()
            .rev()
            .find_map(|m| {
                m.pointer("/result/stopReason")
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .unwrap_or_else(|| "end_turn".into());
        let status = if stop == "end_turn" {
            "completed"
        } else {
            "aborted"
        };
        self.end_line(status, u32::try_from(tools).unwrap_or(0) + 1, &text, None);
        self.after_run(sid, status, io);
    }

    /// The plugin's `afterRun`: a completed run with a goal still active queues the verification prompt.
    fn after_run(&mut self, sid: &str, status: &str, io: &mut dyn SimIo) {
        if status != "completed" || !self.claimed || !self.goal_active(sid) {
            self.due = None;
            if status != "completed" {
                io.log("the loop stopped: a run that did not complete queues nothing");
            }
            return;
        }
        let mut s = self.read_state();
        if let Some(g) = s["activeGoals"].get_mut(sid) {
            g["awaitingVerification"] = json!(true);
            self.write_state(&s);
        }
        self.due = Some(Instant::now() + Duration::from_millis(self.script.gap_ms));
    }

    fn pick(&mut self) -> Option<RoundScript> {
        if let Some(q) = self.queue.pop_front() {
            return Some(q);
        }
        if self.next < self.script.rounds.len() {
            self.next += 1;
            return self.script.rounds.get(self.next - 1).cloned();
        }
        if self.script.repeat_last {
            return self.script.rounds.last().cloned();
        }
        None
    }

    fn update(&self, sid: &str, u: Value) -> Value {
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid, "update": u}})
    }

    /// Services the input for `wait`: a prompt is answered at once and queued, a cancel is ignored. `Closed` when the client is gone.
    fn busy(&mut self, wait: Duration, io: &mut dyn SimIo) -> Flow {
        let end = Instant::now() + wait;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match io.recv(left) {
                Recv::Timeout => return Flow::Go,
                Recv::Closed => return Flow::Exit,
                Recv::Line(l) => {
                    let Ok(v) = serde_json::from_str::<Value>(&l) else {
                        continue;
                    };
                    match v["method"].as_str() {
                        Some("session/prompt") => {
                            io.log("prompt queued during a round");
                            io.emit(json!({"jsonrpc": "2.0", "id": v["id"], "result": {"stopReason": "end_turn"}}));
                            self.queue.push_back(self.script.queued.clone());
                        }
                        Some("session/cancel") if self.honor_cancel => {
                            io.log("session/cancel ends the round");
                            self.cancelled = true;
                            return Flow::Go;
                        }
                        Some("session/cancel") => io.log("session/cancel ignored by the goal loop"),
                        _ => {}
                    }
                }
            }
            if left.is_zero() {
                return Flow::Go;
            }
        }
    }

    /// Emits the edit of a round as a tool call that carries the diff; returns the number of calls made.
    fn emit_edit(&mut self, sid: &str, path: &str, text: &str, io: &mut dyn SimIo) -> u32 {
        self.ids += 1;
        let id = format!("g-{}", self.ids);
        io.emit(self.update(sid, json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": format!("Edit {path}"), "kind": "edit", "status": "pending"})));
        io.emit(self.update(
            sid,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed",
                "content": [{"type": "diff", "path": path, "oldText": null, "newText": text}]}),
        ));
        1
    }

    /// Runs one follow-up round (or the run a client prompt asked for) and returns how it ended.
    fn exec_round(&mut self, sid: &str, r: &RoundScript, io: &mut dyn SimIo) -> (String, Flow) {
        self.cancelled = false;
        self.start_line();
        io.log(&format!("round {} starts", self.starts));
        if r.delay_ms > 0 && matches!(self.busy(Duration::from_millis(r.delay_ms), io), Flow::Exit)
        {
            return (r.status.clone(), Flow::Exit);
        }
        let mut calls = 0u32;
        let edit_at = if r.edit_first {
            Some(0)
        } else {
            r.edit_after_calls
        };
        let mut edited = false;
        for (i, (title, kind)) in r.tools.iter().enumerate() {
            if self.cancelled {
                break;
            }
            if let (Some(at), Some((path, text))) = (edit_at, &r.edit)
                && !edited
                && u32::try_from(i).is_ok_and(|i| i == at)
            {
                calls += self.emit_edit(sid, path, text, io);
                edited = true;
            }
            if i > 0
                && r.tool_gap_ms > 0
                && matches!(
                    self.busy(Duration::from_millis(r.tool_gap_ms), io),
                    Flow::Exit
                )
            {
                return (r.status.clone(), Flow::Exit);
            }
            if self.cancelled {
                break;
            }
            self.ids += 1;
            let id = format!("g-{}", self.ids);
            let carried = r.tool_io.get(i).cloned().unwrap_or_default();
            let mut call = json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": kind, "status": "pending"});
            if let Some(input) = carried.input {
                call["rawInput"] = input;
            }
            io.emit(self.update(sid, call));
            let status = if carried.failed {
                "failed"
            } else {
                "completed"
            };
            let mut done =
                json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": status});
            if let Some(output) = carried.output {
                done["rawOutput"] = output;
            }
            io.emit(self.update(sid, done));
            calls += 1;
        }
        if !edited
            && !self.cancelled
            && let Some((path, text)) = &r.edit
        {
            calls += self.emit_edit(sid, path, text, io);
        }
        if let Some(summary) = r.complete.as_ref().filter(|_| !self.cancelled) {
            self.ids += 1;
            let id = format!("g-{}", self.ids);
            let title = format!("mark_goal_complete: {}", json!({"summary": summary}));
            io.emit(self.update(sid, json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": "other", "status": "pending", "rawInput": {"summary": summary}})));
            let mut s = self.read_state();
            let awaiting = s["activeGoals"]
                .get(sid)
                .is_some_and(|g| g["awaitingVerification"] == json!(true));
            let out = if r.refuse || !awaiting {
                io.log("completion refused (not after a verification prompt)");
                json!({"completed": false, "message": "Do not call mark_goal_complete until the follow-up verification prompt asks whether the goal is complete."})
            } else {
                let goal = s["activeGoals"][sid]["goal"].clone();
                if let Some(o) = s["activeGoals"].as_object_mut() {
                    o.remove(sid);
                }
                self.write_state(&s);
                json!({"completed": true, "goal": goal, "summary": summary})
            };
            io.emit(self.update(sid, json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed", "rawOutput": out})));
            calls += 1;
        }
        // the model's words come after its tool calls, so they are the run's final message
        if !r.text.is_empty() && !self.cancelled {
            io.emit(self.update(sid, json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": r.text}})));
        }
        if r.hold_ms > 0
            && !self.cancelled
            && matches!(self.busy(Duration::from_millis(r.hold_ms), io), Flow::Exit)
        {
            return (r.status.clone(), Flow::Exit);
        }
        let status = if self.cancelled {
            "aborted".to_string()
        } else {
            r.status.clone()
        };
        let iterations = if r.iterations > 0 {
            r.iterations
        } else {
            calls + 1
        };
        self.end_line(&status, iterations, &r.text, r.error.as_deref());
        io.log(&format!("round {} ends {}", self.ends, status));
        self.after_run(sid, &status, io);
        (status, Flow::Go)
    }

    /// The next follow-up round is due: run it. When the script has no more rounds the loop simply stops.
    pub fn run_due(&mut self, sid: &str, io: &mut dyn SimIo) -> Flow {
        self.due = None;
        if !self.goal_active(sid) {
            io.log("the goal is no longer active: the loop ends");
            return Flow::Go;
        }
        let Some(r) = self.pick() else {
            io.log("the scenario has no more rounds: the loop ends");
            return Flow::Go;
        };
        self.exec_round(sid, &r, io).1
    }

    /// A client prompt after the first. While a round runs or is queued it is answered at once and queued; when the loop is dead it runs
    /// as a normal prompt and is answered when it ends.
    pub fn client_prompt(&mut self, sid: &str, id: &Value, io: &mut dyn SimIo) -> Flow {
        if self.due.is_some() {
            io.log("prompt queued during a round");
            io.emit(json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}}));
            self.queue.push_back(self.script.queued.clone());
            return Flow::Go;
        }
        io.log("prompt runs on a dead loop");
        let r = self.pick().unwrap_or_else(|| self.script.queued.clone());
        let (status, flow) = self.exec_round(sid, &r, io);
        io.emit(reply_for(&status, id));
        flow
    }

    /// The script names a run for the first prompt.
    pub fn first_scripted(&self) -> bool {
        self.script.first.is_some()
    }

    /// The first prompt of a script with a `first` run: the run is the scripted round, `session/cancel` ends it, and the prompt is answered
    /// by how it ended. Call `begin_first_run` first (the plugin claims the goal at the start of the run).
    pub fn first_prompt_scripted(&mut self, sid: &str, id: &Value, io: &mut dyn SimIo) -> Flow {
        let r = self.script.first.clone().unwrap_or_default();
        self.honor_cancel = true;
        let (status, flow) = self.exec_round(sid, &r, io);
        self.honor_cancel = false;
        self.first_done = true;
        if matches!(flow, Flow::Go) {
            io.emit(reply_for(&status, id));
        }
        flow
    }

    /// The client closed or a request was a cancel with no round to cancel.
    pub fn cancel_seen(&mut self, io: &mut dyn SimIo) {
        io.log("session/cancel ignored by the goal loop");
    }
}

/// The answer to a prompt, by how its run ended.
fn reply_for(status: &str, id: &Value) -> Value {
    match status {
        "completed" => json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}}),
        "aborted" => json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "cancelled"}}),
        _ => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "data": {"errorName": "ServiceUnavailableException", "isRetryable": true, "statusCode": 503}, "message": "Internal error: Service unavailable"}})
        }
    }
}
