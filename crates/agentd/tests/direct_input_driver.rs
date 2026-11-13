//! The stage loop of the direct driver: when a stage ends in a way agentd cannot mend, the
//! run's user is asked what to do, and the answer starts the next stage. These tests run the real driver over in-memory streams with
//! a scripted engine (several sessions, several processes) and a paused clock, so a 31-minute wait takes no wall time. They pin the
//! per-stage hygiene the design lists: the silence watchdog stays quiet during a person's wait and works again in the next stage, the
//! deadline is one absolute time for the whole run, `canContinue` is false when the prompt cannot be continued, the usage sums over
//! sessions, the attempt numbers continue, and a user's prompt starts the ladder on the route the engine is really on.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::acp::{AcpClient, Activity, Watched};
use agentd::direct::classify::classify;
use agentd::direct::collector::DirectCollector;
use agentd::direct::context::ContextDir;
use agentd::direct::driver::{
    AuditSink, DriverEnd, InputWiring, NoProbe, PromptPlan, Respawn, run_direct_stages,
};
use agentd::direct::failover::{FailoverPlan, Route};
use agentd::direct::input::{ChangedFiles, DigestSource, DigestSources, InputPolicy, TreeSource};
use agentd::direct::retry::RetryPolicy;
use agentd::events::{EventRecorder, Instruct};
use harness_protocol::common::Usage;
use harness_protocol::direct::{DirectAttemptKind, DirectOutcome, InstructAction, InstructOrigin};
use harness_protocol::envelope::{Envelope, Payload};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

const MODEL: &str = "us.anthropic.claude-opus-5-5";
const MODEL_B: &str = "global.anthropic.claude-opus-5-5";
const MODEL_C: &str = "us.anthropic.claude-sonnet-5-5";
const TOP: &str = "/mnt/workspace/run1/direct/repo";
const PROMPT: &str = "Add percent_change\nwith tests\n";

// ---- the scripted engine ------------------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Step {
    Update(Value),
    Sleep(u64),
    Finish(&'static str, Option<Value>),
    ErrorWith(i64, &'static str, Value),
    /// Nothing more, ever (the process stays open).
    Silent,
    /// The process dies: its output closes.
    Die,
}

fn chunk(text: &str) -> Step {
    Step::Update(
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
    )
}

fn tool(id: &str, status: &str) -> Step {
    Step::Update(
        json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": "run_commands: make test", "kind": "execute", "status": status}),
    )
}

fn end_turn(text: &str) -> Vec<Step> {
    vec![chunk(text), Step::Finish("end_turn", None)]
}

fn frame(v: Value) -> String {
    format!("{v}\n")
}

type Received = Arc<Mutex<Vec<Value>>>;

/// One engine process: per session (in the order `session/new` arrives), one list of steps per `session/prompt`.
struct Script {
    model: &'static str,
    sessions: Vec<Vec<Vec<Step>>>,
    /// `session/set_config_option` answers with the model the engine is on, not the one asked for.
    refuse_model: bool,
}

impl Script {
    fn new(sessions: Vec<Vec<Vec<Step>>>) -> Self {
        Self {
            model: MODEL,
            sessions,
            refuse_model: false,
        }
    }
}

struct Proc {
    r: DuplexStream,
    w: DuplexStream,
    received: Received,
}

fn spawn_engine(script: Script) -> Proc {
    let (client_w, engine_r) = tokio::io::duplex(1 << 16);
    let (mut out, client_r) = tokio::io::duplex(1 << 16);
    let received: Received = Arc::default();
    let seen = received.clone();
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let mut lines = BufReader::new(engine_r).lines();
        while let Ok(Some(raw)) = lines.next_line().await {
            let msg: Value = serde_json::from_str(&raw).unwrap();
            seen.lock().unwrap().push(msg.clone());
            let _ = tx.send(msg);
        }
    });
    tokio::spawn(async move {
        let mut model = script.model.to_string();
        let mut session_no = 0usize;
        let mut prompt_no: Vec<usize> = Vec::new();
        while let Some(msg) = rx.recv().await {
            let id = msg.get("id").cloned();
            let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
            match msg["method"].as_str().unwrap_or_default() {
                "initialize" => {
                    out.write_all(frame(reply(json!({"protocolVersion": 1}))).as_bytes())
                        .await
                        .unwrap();
                }
                "session/new" => {
                    session_no += 1;
                    prompt_no.push(0);
                    let options = json!([{"id": "model", "currentValue": model, "options": []}]);
                    out.write_all(
                        frame(reply(
                            json!({"sessionId": format!("s-{session_no}"), "configOptions": options}),
                        ))
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                }
                "session/set_config_option" => {
                    if !script.refuse_model {
                        model = msg["params"]["value"].as_str().unwrap().to_string();
                    }
                    let options = json!([{"id": "model", "currentValue": model, "options": []}]);
                    out.write_all(frame(reply(json!({"configOptions": options}))).as_bytes())
                        .await
                        .unwrap();
                }
                "session/prompt" => {
                    let sid = msg["params"]["sessionId"].as_str().unwrap().to_string();
                    let s: usize = sid.trim_start_matches("s-").parse().unwrap();
                    let n = prompt_no[s - 1];
                    prompt_no[s - 1] += 1;
                    let steps = script
                        .sessions
                        .get(s - 1)
                        .and_then(|p| p.get(n))
                        .cloned()
                        .unwrap_or_default();
                    for step in steps {
                        match step {
                            Step::Update(u) => out
                                .write_all(
                                    frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid, "update": u}}))
                                        .as_bytes(),
                                )
                                .await
                                .unwrap(),
                            Step::Sleep(secs) => tokio::time::sleep(Duration::from_secs(secs)).await,
                            Step::Finish(reason, usage) => {
                                let mut result = json!({"stopReason": reason});
                                if let Some(u) = usage {
                                    result["_meta"] = json!({"usage": u});
                                }
                                out.write_all(frame(reply(result)).as_bytes()).await.unwrap();
                            }
                            Step::ErrorWith(code, message, data) => out
                                .write_all(
                                    frame(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message, "data": data}}))
                                        .as_bytes(),
                                )
                                .await
                                .unwrap(),
                            Step::Silent => std::future::pending::<()>().await,
                            Step::Die => return,
                        }
                        out.flush().await.unwrap();
                    }
                }
                _ => {}
            }
            out.flush().await.unwrap();
        }
    });
    Proc {
        r: client_r,
        w: client_w,
        received,
    }
}

/// The host of a run that may restart its engine: each restart is the next script.
struct Host {
    next: VecDeque<Script>,
    spawned: Arc<Mutex<Vec<Received>>>,
    audit: Option<Arc<dyn AuditSink>>,
}

impl Respawn for Host {
    type R = DuplexStream;
    type W = DuplexStream;
    fn respawn(
        &mut self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(DuplexStream, DuplexStream), String>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let script = self.next.pop_front().ok_or("no engine left to start")?;
            let p = spawn_engine(script);
            self.spawned.lock().unwrap().push(p.received.clone());
            Ok((p.r, p.w))
        })
    }

    fn audit(&self) -> Option<Arc<dyn AuditSink>> {
        self.audit.clone()
    }
}

/// An audit sink that keeps what it was given; `fail` makes every upload fail.
#[derive(Default)]
struct Recorded {
    puts: Mutex<Vec<(String, Vec<u8>)>>,
    fail: bool,
}

impl AuditSink for Recorded {
    fn put<'a>(
        &'a self,
        file: &'a str,
        bytes: Vec<u8>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.puts.lock().unwrap().push((file.to_string(), bytes));
            if self.fail {
                Err("the upload failed".to_string())
            } else {
                Ok(())
            }
        })
    }
}

// ---- the doubles of the work tree and the digest -------------------------------------------------------------------------------

/// The number of changed files the tree reports, one entry per stage; the last repeats.
struct Tree(Mutex<VecDeque<Option<u32>>>, Mutex<Option<u32>>);

impl TreeSource for Tree {
    fn changed_files(
        &self,
        _limit: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<ChangedFiles>> + Send + '_>>
    {
        Box::pin(async move {
            let mut q = self.0.lock().unwrap();
            let mut last = self.1.lock().unwrap();
            if let Some(next) = q.pop_front() {
                *last = next;
            }
            (*last).map(|total| ChangedFiles {
                paths: (0..total).map(|i| format!("src/file{i}.py")).collect(),
                total,
            })
        })
    }
}

struct Source;

impl DigestSource for Source {
    fn gather(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = DigestSources> + Send + '_>> {
        Box::pin(async {
            DigestSources {
                task: PROMPT.to_string(),
                ..DigestSources::default()
            }
        })
    }
}

fn plan(timeout_s: u64, silence_s: u64) -> PromptPlan {
    PromptPlan {
        cwd: TOP.into(),
        engine: "cline".into(),
        model: MODEL.into(),
        prompt: PROMPT.into(),
        nudge: None,
        cancel: None,
        timeout: Duration::from_secs(timeout_s),
        silence: Duration::from_secs(silence_s),
        retry: RetryPolicy::off(),
        failover: None,
        engine_stop_resumes: 0,
    }
}

fn policy(wait_s: u64) -> InputPolicy {
    InputPolicy {
        wait: Duration::from_secs(wait_s),
        max_requests: 3,
        margin: Duration::from_secs(900),
        min_window: Duration::from_secs(300),
        max_auto_sessions: 1,
        max_user_sessions: 3,
    }
}

fn route(id: &str) -> Route {
    Route {
        id: id.into(),
        region: "us-east-1".into(),
        context_tokens: 200_000,
        max_output_tokens: 32_000,
    }
}

// ---- running the driver ------------------------------------------------------------------------------------------------------

struct Live {
    tx: Option<mpsc::Sender<Instruct>>,
    cancel: Option<watch::Sender<bool>>,
    rec: Arc<EventRecorder>,
    seen: Vec<Envelope>,
    handle: tokio::task::JoinHandle<DriverEnd>,
    first: Received,
    later: Arc<Mutex<Vec<Received>>>,
}

struct Setup {
    first: Script,
    later: Vec<Script>,
    plan: PromptPlan,
    policy: InputPolicy,
    /// Changed files per stage.
    tree: Vec<Option<u32>>,
    routes: Vec<harness_protocol::direct::NeedsInputModel>,
    audit: Option<Arc<Recorded>>,
}

impl Setup {
    fn new(first: Script) -> Self {
        Self {
            first,
            later: Vec::new(),
            plan: plan(30_000, 900),
            policy: policy(1800),
            tree: vec![Some(0), Some(3)],
            routes: Vec::new(),
            audit: None,
        }
    }
}

fn start(s: Setup) -> Live {
    let p0 = spawn_engine(s.first);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let (tx, rx) = mpsc::channel::<Instruct>(16);
    let rec = EventRecorder::new("run1", "direct");
    let activity = Activity::new();
    let client = AcpClient::new(p0.r, p0.w)
        .with_activity(activity.clone())
        .with_cancel(cancel_rx.clone(), Duration::from_secs(10));
    let mut plan = s.plan;
    plan.cancel = Some(cancel_rx.clone());
    let spawned: Arc<Mutex<Vec<Received>>> = Arc::default();
    let host = Host {
        next: s.later.into(),
        spawned: spawned.clone(),
        audit: s.audit.clone().map(|a| a as Arc<dyn AuditSink>),
    };
    let dir = common::scratch("input-driver");
    let wiring = InputWiring {
        policy: s.policy,
        instructions: rx,
        cancel: cancel_rx,
        tree: Arc::new(Tree(Mutex::new(s.tree.into()), Mutex::new(None))),
        sources: Arc::new(Source),
        context: ContextDir::create(&dir).unwrap(),
        task: PROMPT.to_string(),
        engine_label: "Cline".to_string(),
        routes: s.routes,
        rec: rec.clone(),
        audit: None,
        log: None,
        review: None,
    };
    let rec2 = rec.clone();
    let handle = tokio::spawn(async move {
        let mut client = client;
        let mut host = host;
        let mut collector = DirectCollector::new(rec2);
        run_direct_stages(
            &mut client,
            &mut collector,
            &activity,
            &plan,
            &NoProbe,
            Some(wiring),
            &mut host,
        )
        .await
    });
    Live {
        tx: Some(tx),
        cancel: Some(cancel_tx),
        rec,
        seen: Vec::new(),
        handle,
        first: p0.received,
        later: spawned,
    }
}

fn kind_of(e: &Envelope) -> Option<(String, Value)> {
    let Payload::Inline { value } = &e.payload else {
        return None;
    };
    value
        .get("kind")
        .and_then(Value::as_str)
        .map(|k| (k.to_string(), value.clone()))
}

impl Live {
    fn pull(&mut self) {
        self.seen.extend(self.rec.drain());
    }

    /// Every progress payload of this kind seen so far.
    fn events(&mut self, kind: &str) -> Vec<Value> {
        self.pull();
        self.seen
            .iter()
            .filter_map(kind_of)
            .filter(|(k, _)| k == kind)
            .map(|(_, v)| v)
            .collect()
    }

    /// Waits (in virtual time) for the n-th `needs-input` event and returns its payload.
    async fn needs_input(&mut self, n: usize) -> Value {
        for _ in 0..20_000 {
            let found = self.events("needs-input");
            if let Some(v) = found.get(n - 1) {
                return v.clone();
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        panic!("no needs-input event number {n}");
    }

    async fn send(&self, i: Instruct) {
        self.tx.as_ref().unwrap().send(i).await.unwrap();
    }

    async fn answer(
        &self,
        request_id: &str,
        action: InstructAction,
        text: Option<&str>,
        model: Option<&str>,
    ) {
        self.send(Instruct {
            request_id: request_id.to_string(),
            action,
            text: text.map(str::to_string),
            model: model.map(str::to_string),
            origin: InstructOrigin::User,
            cancelled_in_batch: false,
        })
        .await;
    }

    async fn finish(mut self) -> (DriverEnd, Vec<Value>, Vec<Received>) {
        let end = (&mut self.handle).await.unwrap();
        self.pull();
        let all: Vec<Value> = self
            .seen
            .iter()
            .filter_map(kind_of)
            .map(|(_, v)| v)
            .collect();
        let mut procs = vec![self.first.clone()];
        procs.extend(self.later.lock().unwrap().iter().cloned());
        (end, all, procs)
    }
}

fn methods(r: &Received) -> Vec<String> {
    r.lock()
        .unwrap()
        .iter()
        .filter_map(|m| m["method"].as_str().map(str::to_string))
        .collect()
}

fn kinds(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|v| v["kind"].as_str().map(str::to_string))
        .collect()
}

// ---- tests --------------------------------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_31_minute_wait_neither_stalls_nor_cancels_and_the_answer_continues_the_session() {
    let mut s = Setup::new(Script::new(vec![vec![
        end_turn("I looked at the code."),
        end_turn("Added the function."),
    ]]));
    s.policy = policy(3600);
    let mut live = start(s);
    let q = live.needs_input(1).await;
    assert_eq!(q["reason"], "no_changes");
    assert_eq!(q["blocking"], true);
    let id = q["requestId"].as_str().unwrap().to_string();
    // The person takes 31 minutes; the silence budget is 15.
    tokio::time::sleep(Duration::from_secs(31 * 60)).await;
    live.answer(
        &id,
        InstructAction::ContinueWithPrompt,
        Some("Add percent_change with tests, please."),
        None,
    )
    .await;
    let (end, events, procs) = live.finish().await;
    assert!(
        matches!(end.raw.watched, Watched::Done(Ok(_))),
        "the run ends on the second prompt, not on a stall: {:?}",
        end.raw.watched
    );
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(
        methods(&procs[0]),
        [
            "initialize",
            "session/new",
            "session/prompt",
            "session/prompt"
        ]
    );
    let sent = procs[0].lock().unwrap().clone();
    assert_eq!(
        sent[3]["params"]["prompt"][0]["text"],
        "Add percent_change with tests, please."
    );
    // The wait is the person's: it is not silence of the provider.
    assert!(
        end.max_silent_gap_s.unwrap_or(0) < 60,
        "longest silence {:?}",
        end.max_silent_gap_s
    );
    assert!(kinds(&events).contains(&"input-received".to_string()));
    // The attempt numbers continue and the second send is an instruction.
    let kinds_n: Vec<(u32, DirectAttemptKind)> =
        end.attempts.iter().map(|a| (a.n, a.kind)).collect();
    assert_eq!(
        kinds_n,
        [
            (1, DirectAttemptKind::Prompt),
            (2, DirectAttemptKind::Instruction)
        ]
    );
    assert_eq!(end.interventions.len(), 1);
    assert_eq!(end.interventions[0].outcome, "answered");
    assert_eq!(end.interventions[0].at_attempt, 2);
}

#[tokio::test(start_paused = true)]
async fn a_closed_pipe_cannot_continue_and_a_continue_answer_is_ignored() {
    let mut s = Setup::new(Script::new(vec![vec![vec![
        tool("t1", "in_progress"),
        Step::Die,
    ]]]));
    s.policy.max_auto_sessions = 0;
    let mut live = start(s);
    let q = live.needs_input(1).await;
    assert_eq!(q["reason"], "engine_died");
    assert_eq!(q["canContinue"], false);
    assert_eq!(q["options"], json!(["new_session_with_prompt", "finish"]));
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(&id, InstructAction::ContinueWithPrompt, Some("go on"), None)
        .await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let ignored = live.events("input-ignored");
    assert_eq!(ignored.len(), 1);
    // The stop cannot take a continue: the word is the window's own for a session that is gone.
    assert_eq!(ignored[0]["why"], "session-dead");
    live.answer(&id, InstructAction::Finish, None, None).await;
    let (end, events, _) = live.finish().await;
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::StoppedEngineDied);
    let closed: Vec<&Value> = events
        .iter()
        .filter(|v| v["kind"] == "input-closed")
        .collect();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["how"], "finish");
    assert!(matches!(
        end.input_note,
        Some(agentd::direct::summary::InputNote::ChoseFinish)
    ));
}

#[tokio::test(start_paused = true)]
async fn a_stall_cannot_continue_and_the_engine_is_asked_to_cancel() {
    let mut s = Setup::new(Script::new(vec![vec![vec![
        chunk("thinking"),
        Step::Silent,
    ]]]));
    s.policy.max_auto_sessions = 0;
    let mut live = start(s);
    let q = live.needs_input(1).await;
    assert_eq!(q["reason"], "silence");
    assert_eq!(q["canContinue"], false);
    assert!(
        !q["options"]
            .as_array()
            .unwrap()
            .contains(&json!("continue_with_prompt"))
    );
    assert!(
        !q["options"]
            .as_array()
            .unwrap()
            .contains(&json!("try_model"))
    );
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(&id, InstructAction::Finish, None, None).await;
    let (end, _, procs) = live.finish().await;
    assert!(matches!(end.raw.watched, Watched::Stalled { .. }));
    assert!(methods(&procs[0]).contains(&"session/cancel".to_string()));
}

#[tokio::test(start_paused = true)]
async fn a_tool_left_running_by_a_dead_engine_does_not_switch_off_the_next_stages_watchdog() {
    let mut s = Setup::new(Script::new(vec![vec![vec![
        tool("t1", "in_progress"),
        Step::Die,
    ]]]));
    s.policy.max_auto_sessions = 0;
    // The restarted engine takes the prompt and says nothing more.
    s.later = vec![Script::new(vec![vec![vec![
        chunk("working"),
        Step::Silent,
    ]]])];
    let mut live = start(s);
    let q1 = live.needs_input(1).await;
    assert_eq!(q1["reason"], "engine_died");
    let id1 = q1["requestId"].as_str().unwrap().to_string();
    live.answer(
        &id1,
        InstructAction::NewSessionWithPrompt,
        Some("start over"),
        None,
    )
    .await;
    // Stage two stalls at the silence budget: the dead engine's open tool call must not hide it.
    let q2 = live.needs_input(2).await;
    assert_eq!(q2["reason"], "silence");
    let id2 = q2["requestId"].as_str().unwrap().to_string();
    live.answer(&id2, InstructAction::Finish, None, None).await;
    let (end, events, procs) = live.finish().await;
    assert!(matches!(end.raw.watched, Watched::Stalled { .. }));
    assert_eq!(end.fresh_sessions.len(), 1);
    assert!(end.fresh_sessions[0].restarted_process);
    // The restarted process was started with initialize, then a new session and the prompt; its silence was cancelled, resumed once
    // (A5: the recovery ladder's class 7) and cancelled again before the question.
    assert_eq!(
        methods(&procs[1]),
        [
            "initialize",
            "session/new",
            "session/prompt",
            "session/cancel",
            "session/prompt",
            "session/cancel"
        ]
    );
    let ns: Vec<&Value> = events
        .iter()
        .filter(|v| v["kind"] == "new-session")
        .collect();
    assert_eq!(ns.len(), 1);
    assert_eq!(ns[0]["restartedProcess"], true);
    assert_eq!(ns[0]["trigger"], "user");
}

#[tokio::test(start_paused = true)]
async fn the_deadline_is_one_absolute_time_for_all_stages() {
    let mut s = Setup::new(Script::new(vec![vec![
        vec![
            Step::Sleep(1000),
            chunk("I read the code."),
            Step::Finish("end_turn", None),
        ],
        vec![
            Step::Sleep(3500),
            chunk("Added it."),
            Step::Finish("end_turn", None),
        ],
    ]]));
    s.plan = plan(4000, 100_000);
    let t0 = Instant::now();
    let mut live = start(s);
    let q = live.needs_input(1).await;
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(&id, InstructAction::ContinueWithPrompt, Some("go on"), None)
        .await;
    let (end, _, _) = live.finish().await;
    assert!(
        matches!(end.raw.watched, Watched::TimedOut),
        "{:?}",
        end.raw.watched
    );
    let elapsed = t0.elapsed().as_secs();
    assert!(
        (3998..=4003).contains(&elapsed),
        "the second stage got what was left of one deadline, not a new budget: {elapsed} s"
    );
}

#[tokio::test(start_paused = true)]
async fn a_fresh_session_in_the_live_process_sums_the_usage_and_starts_clean() {
    let u1 = json!({"inputTokens": 100, "outputTokens": 10});
    let u2 = json!({"inputTokens": 50, "outputTokens": 5});
    let s = Setup::new(Script::new(vec![
        vec![vec![
            chunk("I read the files."),
            Step::Finish("end_turn", Some(u1)),
        ]],
        vec![vec![chunk("Added it."), Step::Finish("end_turn", Some(u2))]],
    ]));
    let mut live = start(s);
    let q = live.needs_input(1).await;
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(
        &id,
        InstructAction::NewSessionWithPrompt,
        Some("try again carefully"),
        None,
    )
    .await;
    let (end, events, procs) = live.finish().await;
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
    // One process: the second session is a `session/new`, no second initialize, no model switch.
    assert_eq!(
        methods(&procs[0]),
        [
            "initialize",
            "session/new",
            "session/prompt",
            "session/new",
            "session/prompt"
        ]
    );
    let sent = procs[0].lock().unwrap().clone();
    assert_eq!(sent[4]["params"]["sessionId"], "s-2");
    let text = sent[4]["params"]["prompt"][0]["text"].as_str().unwrap();
    assert!(text.contains("try again carefully"), "{text}");
    assert_ne!(text, PROMPT);
    assert_eq!(end.session_id.as_deref(), Some("s-2"));
    assert_eq!(
        end.usage,
        Some(Usage {
            input_tokens: 150,
            output_tokens: 15,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0
        })
    );
    assert_eq!(end.fresh_sessions.len(), 1);
    let f = &end.fresh_sessions[0];
    assert_eq!(
        (f.n, f.trigger.as_str(), f.reason.as_str()),
        (2, "user", "no_changes")
    );
    assert!(!f.restarted_process);
    let kinds_n: Vec<(u32, DirectAttemptKind)> =
        end.attempts.iter().map(|a| (a.n, a.kind)).collect();
    assert_eq!(
        kinds_n,
        [
            (1, DirectAttemptKind::Prompt),
            (2, DirectAttemptKind::FreshSession)
        ]
    );
    let ns: Vec<&Value> = events
        .iter()
        .filter(|v| v["kind"] == "new-session")
        .collect();
    assert_eq!(ns.len(), 1);
    assert_eq!(ns[0]["session"], 2);
    assert_eq!(ns[0]["restartedProcess"], false);
}

#[tokio::test(start_paused = true)]
async fn a_prompt_after_a_switch_starts_the_ladder_on_the_route_the_engine_is_on() {
    let transient = || {
        Step::ErrorWith(
            -32603,
            "Internal error: service unavailable",
            json!({"statusCode": 503}),
        )
    };
    let mut s = Setup::new(Script::new(vec![vec![
        // Stage one: the user's route fails, the ladder moves to the next route and the work ends there.
        vec![transient()],
        end_turn("I looked at the code."),
        // Stage two: the same route fails again.
        vec![transient()],
        vec![transient()],
        end_turn("Added it."),
    ]]));
    s.plan.retry = RetryPolicy::new(0, Duration::from_secs(1));
    let mut f = FailoverPlan::new(vec![route(MODEL), route(MODEL_B), route(MODEL_C)]);
    f.fallback_retries = 1;
    s.plan.failover = Some(f);
    let mut live = start(s);
    let q = live.needs_input(1).await;
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(&id, InstructAction::ContinueWithPrompt, Some("go on"), None)
        .await;
    let (end, _, procs) = live.finish().await;
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
    let switches: Vec<String> = procs[0]
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m["method"] == "session/set_config_option")
        .map(|m| m["params"]["value"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(switches.len(), 2, "{switches:?}");
    assert_eq!(switches[0], MODEL_B);
    assert_ne!(
        switches[1], MODEL_B,
        "the engine is on {MODEL_B}: the second stage's first move is to another route"
    );
}

#[tokio::test(start_paused = true)]
async fn the_users_cancel_ends_the_wait_as_the_users_stop() {
    let s = Setup::new(Script::new(vec![vec![end_turn("I looked at the code.")]]));
    let mut live = start(s);
    let q = live.needs_input(1).await;
    let id = q["requestId"].as_str().unwrap().to_string();
    live.cancel.as_ref().unwrap().send_replace(true);
    let (end, events, _) = live.finish().await;
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::StoppedByUser);
    let closed: Vec<&Value> = events
        .iter()
        .filter(|v| v["kind"] == "input-closed")
        .collect();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["how"], "cancelled");
    assert_eq!(closed[0]["requestId"], id);
    assert_eq!(end.interventions[0].outcome, "cancelled");
}

#[tokio::test(start_paused = true)]
async fn nobody_answering_ends_the_run_with_what_exists_and_says_so() {
    let s = Setup::new(Script::new(vec![vec![end_turn("I looked at the code.")]]));
    let mut live = start(s);
    let q = live.needs_input(1).await;
    assert_eq!(q["waitS"], 1800);
    let (end, events, _) = live.finish().await;
    // The stop that was held is the stop the run ends with.
    assert!(matches!(end.raw.watched, Watched::Done(Ok(_))));
    let closed: Vec<&Value> = events
        .iter()
        .filter(|v| v["kind"] == "input-closed")
        .collect();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["how"], "expired");
    match end.input_note {
        Some(agentd::direct::summary::InputNote::NobodyAnswered { waited_s }) => {
            assert!((1799..=1802).contains(&waited_s), "{waited_s}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(end.interventions[0].outcome, "expired");
}

#[tokio::test(start_paused = true)]
async fn an_engine_that_dies_during_the_wait_supersedes_the_question_and_asks_again() {
    let mut s = Setup::new(Script::new(vec![vec![vec![
        chunk("I looked at the code."),
        Step::Finish("end_turn", None),
        // The process dies after it answered, while the question is open.
        Step::Die,
    ]]]));
    s.policy.max_auto_sessions = 0;
    let mut live = start(s);
    let q1 = live.needs_input(1).await;
    assert_eq!(q1["reason"], "no_changes");
    let q2 = live.needs_input(2).await;
    assert_eq!(q2["reason"], "engine_died");
    assert_eq!(q2["canContinue"], false);
    let closed = live.events("input-closed");
    assert_eq!(closed[0]["how"], "superseded");
    assert_eq!(closed[0]["requestId"], q1["requestId"]);
    let id2 = q2["requestId"].as_str().unwrap().to_string();
    live.answer(&id2, InstructAction::Finish, None, None).await;
    let (end, _, _) = live.finish().await;
    assert_eq!(end.interventions.len(), 2);
    assert_eq!(end.interventions[0].outcome, "superseded");
}

#[tokio::test(start_paused = true)]
async fn a_run_with_no_answer_window_ends_after_its_first_stage_as_it_always_did() {
    let mut s = Setup::new(Script::new(vec![vec![end_turn("I looked at the code.")]]));
    s.policy = policy(0);
    let live = start(s);
    let (end, events, procs) = live.finish().await;
    assert_eq!(
        methods(&procs[0]),
        ["initialize", "session/new", "session/prompt"]
    );
    assert!(!kinds(&events).contains(&"needs-input".to_string()));
    assert!(end.interventions.is_empty());
    assert!(end.fresh_sessions.is_empty());
    assert!(end.input_note.is_none());
}

// ---- the pure parts ------------------------------------------------------------------------------------------------------------

fn usage(i: u64, o: u64) -> Usage {
    Usage {
        input_tokens: i,
        output_tokens: o,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
    }
}

#[test]
fn the_usage_tally_adds_what_every_prompt_of_every_session_reported() {
    let mut t = agentd::direct::driver::UsageTally::default();
    assert_eq!(t.total(), None);
    t.after_prompt(Some(usage(100, 10)), None);
    t.after_prompt(Some(usage(20, 2)), None);
    assert_eq!(t.total(), Some(usage(120, 12)));
    t.new_session(None);
    assert_eq!(t.total(), Some(usage(120, 12)));
    t.after_prompt(Some(usage(50, 5)), None);
    assert_eq!(t.total(), Some(usage(170, 17)));
}

#[test]
fn the_usage_tally_does_not_credit_the_new_session_with_the_figure_that_was_streamed_before_it() {
    let mut t = agentd::direct::driver::UsageTally::default();
    // The first session streamed a figure and its last prompt reported nothing.
    t.after_prompt(None, Some(usage(100, 10)));
    assert_eq!(t.total(), Some(usage(100, 10)));
    t.new_session(Some(usage(100, 10)));
    // The collector still holds the old session's figure; the new session's prompt reports nothing either.
    t.after_prompt(None, Some(usage(100, 10)));
    assert_eq!(t.total(), Some(usage(100, 10)), "counted once");
    // Once the new session streams its own figure it is added.
    t.after_prompt(None, Some(usage(7, 1)));
    assert_eq!(t.total(), Some(usage(107, 11)));
}

#[tokio::test]
async fn a_run_that_cannot_restart_its_engine_says_so() {
    let mut host = agentd::direct::driver::NoRespawn::<DuplexStream, DuplexStream>::new();
    let err = host.respawn().await.err().unwrap();
    assert!(err.contains("cannot restart"), "{err}");
}

// ---- the digest of every stop ------------------------------------------------------------------------------------------------

fn digest_of(puts: &[(String, Vec<u8>)], n: u32) -> Value {
    let (_, bytes) = puts
        .iter()
        .find(|(f, _)| *f == format!("stop-digest-{n}.json"))
        .unwrap_or_else(|| {
            panic!(
                "no stop-digest-{n}.json in {:?}",
                puts.iter().map(|p| &p.0).collect::<Vec<_>>()
            )
        });
    assert!(bytes.len() <= 64 * 1024, "{} bytes", bytes.len());
    serde_json::from_slice(bytes).unwrap()
}

#[tokio::test(start_paused = true)]
async fn the_digest_of_every_stop_is_written_with_its_number_and_outcome() {
    let mut s = Setup::new(Script::new(vec![vec![
        end_turn("I looked at the code."),
        end_turn("Added the function."),
    ]]));
    let sink = Arc::new(Recorded::default());
    s.audit = Some(sink.clone());
    let mut live = start(s);
    let q = live.needs_input(1).await;
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(&id, InstructAction::ContinueWithPrompt, Some("go on"), None)
        .await;
    let (_end, _, _) = live.finish().await;
    let puts = sink.puts.lock().unwrap().clone();
    let names: Vec<&str> = puts.iter().map(|p| p.0.as_str()).collect();
    assert_eq!(names, ["stop-digest-1.json", "stop-digest-2.json"]);
    let first = digest_of(&puts, 1);
    assert_eq!(first["n"], 1);
    assert_eq!(first["harness"]["outcome"], "stopped_no_changes");
    assert_eq!(first["harness"]["session"], 1);
    assert_eq!(first["harness"]["filesChanged"], 0);
    let second = digest_of(&puts, 2);
    assert_eq!(second["harness"]["outcome"], "reported_done");
    assert_eq!(second["harness"]["filesChanged"], 3);
}

#[tokio::test(start_paused = true)]
async fn a_failed_digest_upload_never_stops_the_run() {
    let mut s = Setup::new(Script::new(vec![vec![
        end_turn("I looked at the code."),
        end_turn("Added the function."),
    ]]));
    let sink = Arc::new(Recorded {
        fail: true,
        ..Recorded::default()
    });
    s.audit = Some(sink.clone());
    let mut live = start(s);
    let q = live.needs_input(1).await;
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(&id, InstructAction::ContinueWithPrompt, Some("go on"), None)
        .await;
    let (end, _, procs) = live.finish().await;
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(
        methods(&procs[0])
            .iter()
            .filter(|m| *m == "session/prompt")
            .count(),
        2
    );
    assert_eq!(
        sink.puts.lock().unwrap().len(),
        2,
        "both uploads were tried"
    );
}

#[tokio::test(start_paused = true)]
async fn a_run_with_no_answer_window_writes_the_digest_of_its_one_stop() {
    let p = spawn_engine(Script::new(vec![vec![vec![
        tool("t1", "completed"),
        chunk("Done."),
        Step::Finish("end_turn", None),
    ]]]));
    let activity = Activity::new();
    let mut client = AcpClient::new(p.r, p.w).with_activity(activity.clone());
    let rec = EventRecorder::new("run1", "direct");
    let mut collector = DirectCollector::new(rec);
    let sink = Arc::new(Recorded::default());
    let mut host = Host {
        next: VecDeque::new(),
        spawned: Arc::default(),
        audit: Some(sink.clone() as Arc<dyn AuditSink>),
    };
    let end = run_direct_stages(
        &mut client,
        &mut collector,
        &activity,
        &plan(1000, 900),
        &NoProbe,
        None,
        &mut host,
    )
    .await;
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
    let puts = sink.puts.lock().unwrap().clone();
    assert_eq!(puts.len(), 1);
    let d = digest_of(&puts, 1);
    assert_eq!(d["harness"]["outcome"], "reported_done");
    assert_eq!(d["harness"]["stopReason"], "end_turn");
    assert!(
        d["harness"]["filesChanged"].is_null(),
        "no tree is measured without a window"
    );
}

#[test]
fn the_audit_keys_are_the_protocols_keys() {
    use harness_protocol::direct::s3;
    assert_eq!(
        agentd::direct::audit_key("01RUN", "stop-digest-3.json"),
        s3::stop_digest_key("01RUN", 3)
    );
    assert_eq!(
        agentd::direct::audit_key("01RUN", "fresh-session-2.md"),
        s3::fresh_session_key("01RUN", 2)
    );
}

// ---- the corners of the wait and of the fresh session ---------------------------------------------------------------------------

fn model(id: &str) -> harness_protocol::direct::NeedsInputModel {
    harness_protocol::direct::NeedsInputModel {
        id: id.into(),
        name: id.into(),
        context_tokens: 200_000,
    }
}

#[tokio::test(start_paused = true)]
async fn an_answer_that_arrives_after_its_question_closed_is_acknowledged_when_the_stage_ends() {
    let mut s = Setup::new(Script::new(vec![vec![
        end_turn("I looked at the code."),
        vec![
            Step::Sleep(5),
            chunk("Added it."),
            Step::Finish("end_turn", None),
        ],
    ]]));
    s.policy.max_requests = 3;
    let mut live = start(s);
    let q = live.needs_input(1).await;
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(&id, InstructAction::ContinueWithPrompt, Some("go on"), None)
        .await;
    // While the second stage runs, the plane hands out the same answer again and an answer for a question nobody asked.
    tokio::time::sleep(Duration::from_secs(2)).await;
    live.answer(&id, InstructAction::ContinueWithPrompt, Some("go on"), None)
        .await;
    live.answer(
        "01NOSUCHREQUEST000000000000",
        InstructAction::Finish,
        None,
        None,
    )
    .await;
    let (_end, events, _) = live.finish().await;
    let ignored: Vec<(&str, &str)> = events
        .iter()
        .filter(|v| v["kind"] == "input-ignored")
        .map(|v| (v["requestId"].as_str().unwrap(), v["why"].as_str().unwrap()))
        .collect();
    assert_eq!(
        ignored,
        [
            (id.as_str(), "already-answered"),
            ("01NOSUCHREQUEST000000000000", "unknown-request")
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn an_answer_marked_as_sent_with_the_cancel_is_ignored_and_the_wait_goes_on() {
    let s = Setup::new(Script::new(vec![vec![end_turn("I looked at the code.")]]));
    let mut live = start(s);
    let q = live.needs_input(1).await;
    let id = q["requestId"].as_str().unwrap().to_string();
    live.send(Instruct {
        request_id: id.clone(),
        action: InstructAction::Finish,
        text: None,
        model: None,
        origin: InstructOrigin::User,
        cancelled_in_batch: true,
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let ignored = live.events("input-ignored");
    assert_eq!(ignored.len(), 1);
    assert_eq!(ignored[0]["why"], "window-closed");
    // The question is still open: a real answer is taken.
    live.answer(&id, InstructAction::Finish, None, None).await;
    let (end, _, _) = live.finish().await;
    assert_eq!(end.interventions[0].outcome, "finish");
}

#[tokio::test(start_paused = true)]
async fn closed_channels_leave_the_question_to_its_window() {
    let s = Setup::new(Script::new(vec![vec![end_turn("I looked at the code.")]]));
    let mut live = start(s);
    let _ = live.needs_input(1).await;
    // Nobody can answer or cancel any more (both ends are gone): the window alone decides.
    live.tx = None;
    live.cancel = None;
    let (end, events, _) = live.finish().await;
    assert!(
        events
            .iter()
            .any(|v| v["kind"] == "input-closed" && v["how"] == "expired")
    );
    assert_eq!(end.interventions[0].outcome, "expired");
}

#[tokio::test(start_paused = true)]
async fn an_analyzers_text_goes_into_the_notes_slot_and_the_session_is_the_analyzers() {
    let s = Setup::new(Script::new(vec![
        vec![end_turn("I looked at the code.")],
        vec![end_turn("Added it.")],
    ]));
    let mut live = start(s);
    let q = live.needs_input(1).await;
    let id = q["requestId"].as_str().unwrap().to_string();
    live.send(Instruct {
        request_id: id,
        action: InstructAction::NewSessionWithPrompt,
        text: Some("the loop is in the poll".into()),
        model: None,
        origin: InstructOrigin::Analyzer,
        cancelled_in_batch: false,
    })
    .await;
    let (end, events, procs) = live.finish().await;
    assert_eq!(end.fresh_sessions[0].trigger, "analyzer");
    let ns: Vec<&Value> = events
        .iter()
        .filter(|v| v["kind"] == "new-session")
        .collect();
    assert_eq!(ns[0]["trigger"], "analyzer");
    let sent = procs[0].lock().unwrap().clone();
    let text = sent.last().unwrap()["params"]["prompt"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(text.contains("the loop is in the poll"), "{text}");
}

#[tokio::test(start_paused = true)]
async fn a_fresh_session_that_cannot_start_ends_the_run_when_no_question_is_left() {
    let mut s = Setup::new(Script::new(vec![vec![vec![chunk("working"), Step::Die]]]));
    s.policy.max_auto_sessions = 0;
    s.policy.max_requests = 1;
    // No engine is left to start.
    let mut live = start(s);
    let q = live.needs_input(1).await;
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(&id, InstructAction::NewSessionWithPrompt, None, None)
        .await;
    let (end, events, _) = live.finish().await;
    assert!(matches!(
        end.input_note,
        Some(agentd::direct::summary::InputNote::FreshSessionFailed)
    ));
    assert!(end.fresh_sessions.is_empty());
    assert!(!events.iter().any(|v| v["kind"] == "new-session"));
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::StoppedEngineDied);
}

#[tokio::test(start_paused = true)]
async fn a_restarted_engine_on_another_model_is_not_a_fresh_session_and_the_run_asks_again() {
    let mut s = Setup::new(Script::new(vec![vec![vec![chunk("working"), Step::Die]]]));
    s.policy.max_auto_sessions = 0;
    let mut other = Script::new(vec![vec![end_turn("Added it.")]]);
    other.model = MODEL_C;
    s.later = vec![other];
    let mut live = start(s);
    let q = live.needs_input(1).await;
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(&id, InstructAction::NewSessionWithPrompt, None, None)
        .await;
    // The engine took the wrong model: no session starts, and the engine that cannot be used is a dead one.
    let q2 = live.needs_input(2).await;
    assert_eq!(q2["reason"], "engine_died");
    let id2 = q2["requestId"].as_str().unwrap().to_string();
    live.answer(&id2, InstructAction::Finish, None, None).await;
    let (end, events, procs) = live.finish().await;
    assert!(end.fresh_sessions.is_empty());
    assert!(!events.iter().any(|v| v["kind"] == "new-session"));
    assert!(!methods(&procs[1]).contains(&"session/prompt".to_string()));
}

#[tokio::test(start_paused = true)]
async fn a_model_the_engine_refuses_ends_the_stage_as_a_dead_engine_and_the_run_asks_again() {
    let mut script = Script::new(vec![vec![
        end_turn("I looked at the code."),
        end_turn("Added it."),
    ]]);
    script.refuse_model = true;
    let mut s = Setup::new(script);
    s.policy.max_auto_sessions = 0;
    s.routes = vec![model(MODEL), model(MODEL_B)];
    s.plan.failover = Some(FailoverPlan::new(vec![route(MODEL), route(MODEL_B)]));
    let mut live = start(s);
    let q = live.needs_input(1).await;
    assert_eq!(q["models"][0]["id"], MODEL_B);
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(
        &id,
        InstructAction::TryModel,
        Some("try the other"),
        Some(MODEL_B),
    )
    .await;
    let q2 = live.needs_input(2).await;
    assert_eq!(q2["reason"], "engine_died");
    let id2 = q2["requestId"].as_str().unwrap().to_string();
    live.answer(&id2, InstructAction::Finish, None, None).await;
    let (end, _, procs) = live.finish().await;
    assert!(end.switches.is_empty(), "a refused switch is no switch");
    assert_eq!(
        methods(&procs[0])
            .iter()
            .filter(|m| *m == "session/prompt")
            .count(),
        1
    );
}

#[test]
fn the_words_of_a_sessions_trigger() {
    use agentd::direct::driver::SessionTrigger;
    assert_eq!(SessionTrigger::Harness.as_str(), "harness");
    assert_eq!(SessionTrigger::User.as_str(), "user");
    assert_eq!(SessionTrigger::Analyzer.as_str(), "analyzer");
    let _ = agentd::direct::driver::NoRespawn::<DuplexStream, DuplexStream>::default();
}

#[test]
fn the_engine_is_named_by_its_label_and_an_unknown_one_is_the_engine() {
    assert_eq!(agentd::engine::direct::engine_label("cline"), "Cline");
    assert_eq!(agentd::engine::direct::engine_label("other"), "the engine");
}

#[tokio::test(start_paused = true)]
async fn an_answer_that_arrives_after_the_last_stop_is_told_the_run_is_over() {
    let s = Setup::new(Script::new(vec![vec![end_turn("I looked at the code.")]]));
    let mut live = start(s);
    let q = live.needs_input(1).await;
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(&id, InstructAction::Finish, None, None).await;
    let tx = live.tx.clone().unwrap();
    let rec = live.rec.clone();
    let (end, _, _) = live.finish().await;
    // The run's last stage is over; the plane still hands out answers (a repeat of the finish, and one for a question never asked).
    for request_id in [id.as_str(), "01NEVERASKED000000000000000"] {
        tx.send(Instruct {
            request_id: request_id.to_string(),
            action: InstructAction::Finish,
            text: None,
            model: None,
            origin: InstructOrigin::User,
            cancelled_in_batch: false,
        })
        .await
        .unwrap();
    }
    let mut late = end.late_answers.expect("the channel is handed back");
    assert_eq!(late.drain(), 2);
    let ignored: Vec<(String, String)> = rec
        .drain()
        .iter()
        .filter_map(kind_of)
        .filter(|(k, _)| k == "input-ignored")
        .map(|(_, v)| {
            (
                v["requestId"].as_str().unwrap().to_string(),
                v["why"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        ignored,
        [
            (id.clone(), "already-answered".to_string()),
            (
                "01NEVERASKED000000000000000".to_string(),
                "window-closed".to_string()
            )
        ]
    );
    assert_eq!(late.drain(), 0, "nothing is acknowledged twice");
}
