//! The automatic triggers of the correction path at the stage loop:
//! what a run does by itself when a stage ends in a way agentd can mend, and when it asks instead. The real driver
//! runs over in-memory streams with a scripted engine (several sessions, several processes) and a paused clock, so an outage of
//! half an hour or a silence of 15 minutes takes no wall time.
//!
//! Covered: class 4 (a replayed history the provider refuses, a context that no longer fits) and a dead engine each get one automatic fresh
//! session and then ask; a silence is cancelled and resumed once and then asks; the ladder's cap of fruitless prompts asks at once; the outage
//! notice closes `recovered` when a route answers again and is no question; the engine's second own stop asks with the counters of the stage.
#![allow(clippy::unwrap_used, dead_code)]

mod common;

use agentd::acp::{AcpClient, Activity, Watched};
use agentd::direct::classify::classify;
use agentd::direct::collector::DirectCollector;
use agentd::direct::context::ContextDir;
use agentd::direct::driver::{
    DriverEnd, InputWiring, NoProbe, PromptPlan, Respawn, run_direct_stages,
};
use agentd::direct::failover::{FRUITLESS_PROMPTS_MAX, FailoverPlan, Route};
use agentd::direct::input::{ChangedFiles, DigestSource, DigestSources, InputPolicy, TreeSource};
use agentd::direct::retry::{RESUME_PROMPT_INTERRUPTED, RetryPolicy};
use agentd::events::{EventRecorder, Instruct};
use agentd::logging::MemoryLog;
use harness_protocol::direct::{
    DirectAttemptKind, DirectOutcome, InstructAction, InstructOrigin, NeedsInputModel,
};
use harness_protocol::envelope::{Envelope, Payload};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::{mpsc, watch};

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
}

impl Script {
    fn new(sessions: Vec<Vec<Vec<Step>>>) -> Self {
        Self {
            model: MODEL,
            sessions,
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
                    model = msg["params"]["value"].as_str().unwrap().to_string();
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
    tx: mpsc::Sender<Instruct>,
    cancel: watch::Sender<bool>,
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
        tx,
        cancel: cancel_tx,
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

    async fn answer(
        &self,
        request_id: &str,
        action: InstructAction,
        text: Option<&str>,
        model: Option<&str>,
    ) {
        self.tx
            .send(Instruct {
                request_id: request_id.to_string(),
                action,
                text: text.map(str::to_string),
                model: model.map(str::to_string),
                origin: InstructOrigin::User,
                cancelled_in_batch: false,
            })
            .await
            .unwrap();
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

// ---- helpers of these tests ----------------------------------------------------------------------------------------------------

const A15: &str = "Internal error: The format of the value at messages.7.content.0.toolUse.input is invalid. Provide a json object for the field and try again.";
/// A context overflow in the provider's words.
const OVERFLOW: &str = "Internal error: Input is too long for requested model.";

fn refused_history(text: &'static str) -> Step {
    Step::ErrorWith(
        -32603,
        text,
        json!({"statusCode": 400, "errorName": "ValidationException", "isRetryable": false}),
    )
}

fn transient() -> Step {
    Step::ErrorWith(
        -32603,
        "Internal error: service unavailable",
        json!({"statusCode": 503}),
    )
}

/// A completed tool call as Cline reports it: the call, then its update.
fn completed_call(id: &str, title: &str) -> Vec<Step> {
    vec![
        Step::Update(
            json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": "execute", "status": "pending"}),
        ),
        Step::Update(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed"}),
        ),
    ]
}

/// Cline's loop guard from outside: five completed calls with the same title, then `cancelled` with nobody having asked.
fn loop_guard_stop() -> Vec<Step> {
    let mut v = Vec::new();
    for i in 0..5 {
        v.extend(completed_call(&format!("c{i}"), "run_commands: make test"));
    }
    v.push(Step::Finish("cancelled", None));
    v
}

fn others() -> Vec<NeedsInputModel> {
    [MODEL_B, MODEL_C]
        .iter()
        .map(|id| NeedsInputModel {
            id: (*id).to_string(),
            name: (*id).to_string(),
            context_tokens: 1_000_000,
        })
        .collect()
}

fn three_routes() -> FailoverPlan {
    let mut f = FailoverPlan::new(vec![route(MODEL), route(MODEL_B), route(MODEL_C)]);
    f.fallback_retries = 1;
    f
}

/// The same ladder with the outage notice out of the way (it has its own test).
fn three_routes_no_notice() -> FailoverPlan {
    let mut f = three_routes();
    f.notice_after = Duration::from_secs(1_000_000);
    f
}

fn events_of(all: &[Value], kind: &str) -> Vec<Value> {
    all.iter().filter(|v| v["kind"] == kind).cloned().collect()
}

fn prompts_sent(r: &Received) -> usize {
    methods(r).iter().filter(|m| *m == "session/prompt").count()
}

fn texts_sent(r: &Received) -> Vec<String> {
    r.lock()
        .unwrap()
        .iter()
        .filter(|m| m["method"] == "session/prompt")
        .map(|m| {
            m["params"]["prompt"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

// ---- class 4: the provider refuses the replayed history ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_history_the_provider_refuses_gets_one_automatic_fresh_session_and_no_model_switch() {
    let mut s = Setup::new(Script::new(vec![
        vec![vec![refused_history(A15)]],
        vec![end_turn("Added percent_change with tests.")],
    ]));
    s.plan.retry = RetryPolicy::new(3, Duration::from_secs(1));
    s.plan.failover = Some(three_routes());
    s.routes = others();
    let live = start(s);
    let (end, events, procs) = live.finish().await;
    // No question: the automatic remedy comes first.
    assert!(events_of(&events, "needs-input").is_empty(), "{events:?}");
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
    // One process, a second session in it, the model left alone (no set_config_option), no switch recorded.
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
    assert!(end.switches.is_empty(), "{:?}", end.switches);
    assert_eq!(end.final_model, None);
    // The refused first prompt was not resent: the history is what the provider refuses.
    assert_eq!(prompts_sent(&procs[0]), 2);
    let sent = procs[0].lock().unwrap().clone();
    assert_eq!(sent[4]["params"]["sessionId"], "s-2");
    // The harness started it, for the reason of the stop.
    assert_eq!(end.fresh_sessions.len(), 1);
    let f = &end.fresh_sessions[0];
    assert_eq!(
        (f.trigger.as_str(), f.reason.as_str(), f.restarted_process),
        ("harness", "replay_rejected", false)
    );
    assert!(f.request_id.is_none());
    let ns = events_of(&events, "new-session");
    assert_eq!(ns.len(), 1);
    assert_eq!(ns[0]["trigger"], "harness");
    assert_eq!(ns[0]["reason"], "replay_rejected");
    assert_eq!(ns[0]["restartedProcess"], false);
    // The attempts continue across the sessions and the new session's first send is a fresh-session try.
    let kinds_n: Vec<(u32, DirectAttemptKind)> =
        end.attempts.iter().map(|a| (a.n, a.kind)).collect();
    assert_eq!(
        kinds_n,
        [
            (1, DirectAttemptKind::Prompt),
            (2, DirectAttemptKind::FreshSession)
        ]
    );
    assert_eq!(
        end.attempts[0].retry_class.as_deref(),
        Some("history_unusable")
    );
}

#[tokio::test(start_paused = true)]
async fn a_context_overflow_takes_the_same_automatic_path() {
    let mut s = Setup::new(Script::new(vec![
        vec![vec![refused_history(OVERFLOW)]],
        vec![end_turn("Added percent_change with tests.")],
    ]));
    s.plan.retry = RetryPolicy::new(3, Duration::from_secs(1));
    s.plan.failover = Some(three_routes());
    let live = start(s);
    let (end, events, procs) = live.finish().await;
    assert!(events_of(&events, "needs-input").is_empty(), "{events:?}");
    assert!(end.switches.is_empty());
    assert!(!methods(&procs[0]).contains(&"session/set_config_option".to_string()));
    assert_eq!(end.fresh_sessions.len(), 1);
    assert_eq!(end.fresh_sessions[0].reason, "context_full");
    assert_eq!(end.fresh_sessions[0].trigger, "harness");
}

#[tokio::test(start_paused = true)]
async fn when_the_shape_recurs_in_the_fresh_session_the_run_asks_with_all_four_options() {
    for (shape, reason, words) in [
        (A15, "replay_rejected", "messages.7"),
        (OVERFLOW, "context_full", "too long for requested model"),
    ] {
        let mut s = Setup::new(Script::new(vec![
            vec![vec![refused_history(shape)]],
            vec![vec![refused_history(shape)]],
        ]));
        s.plan.retry = RetryPolicy::new(3, Duration::from_secs(1));
        s.plan.failover = Some(three_routes());
        s.routes = others();
        let mut live = start(s);
        let q = live.needs_input(1).await;
        assert_eq!(q["reason"], reason);
        assert_eq!(q["blocking"], true);
        // The error answered the prompt, so the engine can take another: all four options.
        assert_eq!(q["canContinue"], true, "{q}");
        assert_eq!(
            q["options"],
            json!([
                "continue_with_prompt",
                "new_session_with_prompt",
                "try_model",
                "finish"
            ])
        );
        assert_eq!(q["models"].as_array().unwrap().len(), 2);
        let last = q["evidence"]["lastError"].as_str().unwrap();
        assert!(last.contains(words), "{last}");
        // The automatic session was spent on the first occurrence, and it is the only one.
        assert_eq!(q["evidence"]["sessions"], 2);
        let id = q["requestId"].as_str().unwrap().to_string();
        live.answer(&id, InstructAction::Finish, None, None).await;
        let (end, events, procs) = live.finish().await;
        assert_eq!(
            classify(&end.raw).outcome,
            DirectOutcome::StoppedProviderError
        );
        assert_eq!(end.fresh_sessions.len(), 1, "one automatic session per run");
        assert_eq!(events_of(&events, "new-session").len(), 1);
        assert!(end.switches.is_empty());
        assert!(!methods(&procs[0]).contains(&"session/set_config_option".to_string()));
    }
}

// ---- a dead engine ----------------------------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_dead_engine_is_restarted_once_without_asking_and_the_run_goes_on() {
    let mut s = Setup::new(Script::new(vec![vec![vec![
        tool("t1", "in_progress"),
        Step::Die,
    ]]]));
    s.later = vec![Script::new(vec![vec![end_turn("Added percent_change.")]])];
    let live = start(s);
    let (end, events, procs) = live.finish().await;
    assert!(events_of(&events, "needs-input").is_empty(), "{events:?}");
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
    // The second process was started like the first: initialize, a new session, the prompt.
    assert_eq!(procs.len(), 2);
    assert_eq!(
        methods(&procs[1]),
        ["initialize", "session/new", "session/prompt"]
    );
    let ns = events_of(&events, "new-session");
    assert_eq!(ns.len(), 1);
    assert_eq!(ns[0]["trigger"], "harness");
    assert_eq!(ns[0]["reason"], "engine_died");
    assert_eq!(ns[0]["restartedProcess"], true);
    assert_eq!(end.fresh_sessions.len(), 1);
    assert!(end.fresh_sessions[0].restarted_process);
    assert!(end.switches.is_empty());
}

#[tokio::test(start_paused = true)]
async fn when_the_restarted_engine_dies_too_the_run_asks_and_there_is_no_continue() {
    let mut s = Setup::new(Script::new(vec![vec![vec![
        tool("t1", "in_progress"),
        Step::Die,
    ]]]));
    s.later = vec![Script::new(vec![vec![vec![Step::Die]]])];
    s.routes = others();
    let mut live = start(s);
    let q = live.needs_input(1).await;
    assert_eq!(q["reason"], "engine_died");
    assert_eq!(q["canContinue"], false);
    assert_eq!(q["options"], json!(["new_session_with_prompt", "finish"]));
    assert_eq!(q["models"], json!([]));
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(&id, InstructAction::Finish, None, None).await;
    let (end, events, _) = live.finish().await;
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::StoppedEngineDied);
    // Exactly one automatic restart was made before the question.
    assert_eq!(events_of(&events, "new-session").len(), 1);
    assert_eq!(end.fresh_sessions.len(), 1);
}

// ---- silence ------------------------------------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_silence_is_cancelled_and_resumed_once_and_the_run_goes_on_without_asking() {
    // The engine hangs for 150 s on the first prompt; the silence limit is 100 s. After the cancel it is back and takes the resume.
    let mut s = Setup::new(Script::new(vec![vec![
        vec![chunk("thinking"), Step::Sleep(150)],
        end_turn("Added percent_change."),
    ]]));
    s.plan = plan(30_000, 100);
    let live = start(s);
    let (end, events, procs) = live.finish().await;
    assert!(events_of(&events, "needs-input").is_empty(), "{events:?}");
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(
        methods(&procs[0]),
        [
            "initialize",
            "session/new",
            "session/prompt",
            "session/cancel",
            "session/prompt"
        ]
    );
    // The same session, the fixed resume sentence, no fresh session and no switch.
    let texts = texts_sent(&procs[0]);
    assert_eq!(texts[0], PROMPT);
    assert_eq!(texts[1], RESUME_PROMPT_INTERRUPTED);
    let sent = procs[0].lock().unwrap().clone();
    assert_eq!(sent[4]["params"]["sessionId"], "s-1");
    assert!(end.fresh_sessions.is_empty());
    assert!(end.switches.is_empty());
    // The attempts say so: the stalled try, then the resume.
    let kinds_n: Vec<(u32, DirectAttemptKind)> =
        end.attempts.iter().map(|a| (a.n, a.kind)).collect();
    assert_eq!(kinds_n.last(), Some(&(2, DirectAttemptKind::Resume)));
}

#[tokio::test(start_paused = true)]
async fn the_second_silence_asks_and_cannot_continue() {
    // The engine is away for 1,000 s: the resume gets no answer either.
    let mut s = Setup::new(Script::new(vec![vec![
        vec![chunk("thinking"), Step::Sleep(1_000)],
        vec![chunk("still thinking"), Step::Sleep(1_000)],
    ]]));
    s.plan = plan(30_000, 100);
    s.routes = others();
    let mut live = start(s);
    let q = live.needs_input(1).await;
    assert_eq!(q["reason"], "silence");
    assert_eq!(q["canContinue"], false);
    assert_eq!(q["options"], json!(["new_session_with_prompt", "finish"]));
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(&id, InstructAction::Finish, None, None).await;
    let (end, _, procs) = live.finish().await;
    assert!(matches!(end.raw.watched, Watched::Stalled { .. }));
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::StoppedSilence);
    // One resume only, and a cancel after each silence.
    assert_eq!(prompts_sent(&procs[0]), 2);
    assert_eq!(
        methods(&procs[0])
            .iter()
            .filter(|m| *m == "session/cancel")
            .count(),
        2
    );
}

#[tokio::test(start_paused = true)]
async fn an_engine_that_dies_after_the_silence_cancel_is_restarted_without_asking() {
    // The engine is away for 150 s, takes the resume's bytes and then its process ends.
    let mut s = Setup::new(Script::new(vec![vec![vec![
        chunk("thinking"),
        Step::Sleep(150),
        Step::Die,
    ]]]));
    s.plan = plan(30_000, 100);
    s.later = vec![Script::new(vec![vec![end_turn("Added percent_change.")]])];
    let live = start(s);
    let (end, events, procs) = live.finish().await;
    assert!(events_of(&events, "needs-input").is_empty(), "{events:?}");
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(procs.len(), 2);
    let ns = events_of(&events, "new-session");
    assert_eq!(ns.len(), 1);
    assert_eq!(ns[0]["reason"], "engine_died");
    assert_eq!(ns[0]["restartedProcess"], true);
}

#[tokio::test(start_paused = true)]
async fn the_silence_resume_is_one_for_the_whole_run_a_later_silence_asks_at_once() {
    let mut s = Setup::new(Script::new(vec![vec![
        // Stage one: a silence of 150 s, resumed; the resume ends without a changed file, which asks.
        vec![chunk("thinking"), Step::Sleep(150)],
        end_turn("I looked at the code."),
        // Stage two (the person's answer): the engine goes quiet again.
        vec![chunk("thinking again"), Step::Sleep(1_000)],
    ]]));
    s.plan = plan(30_000, 100);
    s.tree = vec![Some(0)];
    let mut live = start(s);
    let q1 = live.needs_input(1).await;
    assert_eq!(q1["reason"], "no_changes");
    let id1 = q1["requestId"].as_str().unwrap().to_string();
    live.answer(
        &id1,
        InstructAction::ContinueWithPrompt,
        Some("go on"),
        None,
    )
    .await;
    let q2 = live.needs_input(2).await;
    assert_eq!(q2["reason"], "silence");
    assert_eq!(q2["canContinue"], false);
    let id2 = q2["requestId"].as_str().unwrap().to_string();
    live.answer(&id2, InstructAction::Finish, None, None).await;
    let (end, _, procs) = live.finish().await;
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::StoppedSilence);
    // Prompt, the resume of the first silence, the person's text; the second silence was not resumed.
    let texts = texts_sent(&procs[0]);
    assert_eq!(texts.len(), 3, "{texts:?}");
    assert_eq!(texts[1], RESUME_PROMPT_INTERRUPTED);
    assert_eq!(texts[2], "go on");
}

#[tokio::test(start_paused = true)]
async fn the_answer_to_the_cancelled_prompt_is_read_before_the_resume_is_written() {
    // The engine answers the cancel at once, as the real one does: the late response of the dropped prompt is consumed, then the resume goes.
    let mut s = Setup::new(Script::new(vec![vec![
        vec![
            chunk("thinking"),
            Step::Sleep(101),
            Step::Finish("cancelled", None),
        ],
        end_turn("Added percent_change."),
    ]]));
    s.plan = plan(30_000, 100);
    let live = start(s);
    let (end, events, procs) = live.finish().await;
    assert!(events_of(&events, "needs-input").is_empty(), "{events:?}");
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(texts_sent(&procs[0]), [PROMPT, RESUME_PROMPT_INTERRUPTED]);
}

// ---- the cap of fruitless prompts ------------------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_ladders_cap_of_fruitless_prompts_asks_at_once_when_the_run_can_ask() {
    let mut s = Setup::new(Script::new(vec![
        (0..40).map(|_| vec![transient()]).collect(),
    ]));
    let log = MemoryLog::new();
    s.plan.retry = RetryPolicy::new(0, Duration::from_secs(1)).with_log(log.clone());
    s.plan.failover = Some(three_routes_no_notice());
    s.routes = others();
    let mut live = start(s);
    let q = live.needs_input(1).await;
    assert_eq!(q["reason"], "cap_hit");
    assert!(
        log.lines()
            .iter()
            .any(|l| l.contains("stops at the cap: 12 prompts")),
        "{:?}",
        log.lines()
    );
    assert_eq!(q["canContinue"], true);
    assert_eq!(
        q["options"],
        json!([
            "continue_with_prompt",
            "new_session_with_prompt",
            "try_model",
            "finish"
        ])
    );
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(&id, InstructAction::Finish, None, None).await;
    let (end, _, procs) = live.finish().await;
    assert_eq!(
        classify(&end.raw).outcome,
        DirectOutcome::StoppedProviderError
    );
    // The ladder stopped on the twelfth prompt that achieved nothing.
    assert_eq!(
        prompts_sent(&procs[0]),
        usize::try_from(FRUITLESS_PROMPTS_MAX).unwrap()
    );
}

#[tokio::test(start_paused = true)]
async fn after_the_cap_question_a_continue_starts_the_ladder_afresh() {
    let mut prompts: Vec<Vec<Step>> = (0..12).map(|_| vec![transient()]).collect();
    prompts.push(end_turn("Added percent_change."));
    let mut s = Setup::new(Script::new(vec![prompts]));
    s.plan.retry = RetryPolicy::new(0, Duration::from_secs(1));
    s.plan.failover = Some(three_routes_no_notice());
    s.routes = others();
    let mut live = start(s);
    let q = live.needs_input(1).await;
    assert_eq!(q["reason"], "cap_hit");
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(
        &id,
        InstructAction::ContinueWithPrompt,
        Some("try again"),
        None,
    )
    .await;
    let (end, events, procs) = live.finish().await;
    // The answer's prompt got the thirteenth send and the stage's facts were reset: no second cap question.
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(events_of(&events, "needs-input").len(), 1, "{events:?}");
    assert_eq!(prompts_sent(&procs[0]), 13);
    assert_eq!(texts_sent(&procs[0]).last().unwrap(), "try again");
}

#[tokio::test(start_paused = true)]
async fn without_an_answer_window_the_ladder_still_holds_and_waits_for_the_deadline() {
    let mut s = Setup::new(Script::new(vec![
        (0..60).map(|_| vec![transient()]).collect(),
    ]));
    s.plan.retry = RetryPolicy::new(0, Duration::from_secs(1));
    s.plan.failover = Some(three_routes_no_notice());
    s.plan.timeout = Duration::from_secs(5_000);
    s.policy = policy(0);
    let live = start(s);
    let (end, events, procs) = live.finish().await;
    assert!(events_of(&events, "needs-input").is_empty());
    assert!(
        prompts_sent(&procs[0]) > usize::try_from(FRUITLESS_PROMPTS_MAX).unwrap(),
        "the cap is a question only for a run that can ask"
    );
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::StoppedTimeout);
}

// ---- the outage notice --------------------------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_outage_notice_closes_recovered_when_a_route_answers_and_is_no_question() {
    // Two routes, one submission each, both down; the wait is 600 s and the notice is due after 300 s. The next pass is answered.
    let mut s = Setup::new(Script::new(vec![vec![
        vec![transient()],
        vec![transient()],
        end_turn("I looked at the code."),
    ]]));
    s.plan.retry = RetryPolicy::new(0, Duration::from_secs(1));
    let mut f = FailoverPlan::new(vec![route(MODEL), route(MODEL_B)]);
    f.fallback_retries = 0;
    f.wait_base = Duration::from_secs(600);
    f.wait_cap = Duration::from_secs(600);
    f.notice_after = Duration::from_secs(300);
    s.plan.failover = Some(f);
    s.tree = vec![Some(0)];
    let mut live = start(s);
    // The stage ends without a changed file: the real question is the first one, so the notice was not counted.
    let q = live.needs_input(2).await;
    assert_eq!(q["reason"], "no_changes");
    assert_eq!(q["n"], 1);
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(&id, InstructAction::Finish, None, None).await;
    let (_, events, _) = live.finish().await;
    let all = events_of(&events, "needs-input");
    assert_eq!(all.len(), 2, "{all:?}");
    let notice = &all[0];
    assert_eq!(notice["reason"], "provider_outage");
    assert_eq!(notice["blocking"], false);
    assert_eq!(notice["n"], 0);
    let closed: Vec<&Value> = events
        .iter()
        .filter(|v| v["kind"] == "input-closed" && v["requestId"] == notice["requestId"])
        .collect();
    assert_eq!(closed.len(), 1, "{events:?}");
    assert_eq!(closed[0]["how"], "recovered");
    assert_eq!(closed[0]["text"], "A model route answered again.");
    // It closed before the real question opened.
    let order = kinds(&events);
    let pos = |kind: &str, nth: usize| {
        order
            .iter()
            .enumerate()
            .filter(|(_, k)| *k == kind)
            .nth(nth)
            .map(|(i, _)| i)
            .unwrap()
    };
    assert!(pos("needs-input", 0) < pos("input-closed", 0));
    assert!(pos("input-closed", 0) < pos("needs-input", 1));
}

#[tokio::test(start_paused = true)]
async fn a_failure_that_ran_as_long_as_the_longest_wait_closes_the_notice_and_starts_the_count_afresh()
 {
    // Both routes are down, the wait is 600 s and the notice is due after 300 s. The next pass is tried on the first route and fails after
    // 700 s: a submission that ran that long ended the outage, so the notice closes `recovered`; the second route then answers.
    let mut s = Setup::new(Script::new(vec![vec![
        vec![transient()],
        vec![transient()],
        vec![Step::Sleep(700), transient()],
        end_turn("Added percent_change."),
    ]]));
    s.plan.retry = RetryPolicy::new(0, Duration::from_secs(1));
    let mut f = FailoverPlan::new(vec![route(MODEL), route(MODEL_B)]);
    f.fallback_retries = 0;
    f.wait_base = Duration::from_secs(600);
    f.wait_cap = Duration::from_secs(600);
    f.notice_after = Duration::from_secs(300);
    s.plan.failover = Some(f);
    s.tree = vec![Some(3)];
    let live = start(s);
    let (end, events, _) = live.finish().await;
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
    let notices = events_of(&events, "needs-input");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["reason"], "provider_outage");
    let closed = events_of(&events, "input-closed");
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0]["requestId"], notices[0]["requestId"]);
    assert_eq!(closed[0]["how"], "recovered");
}

// ---- the engine's own stop, read through the one mapping place ------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_second_stop_of_the_engine_asks_with_the_counters_of_the_stage() {
    let mut s = Setup::new(Script::new(vec![vec![
        loop_guard_stop(),
        loop_guard_stop(),
    ]]));
    s.plan.retry = RetryPolicy::new(3, Duration::from_secs(1));
    s.plan.engine_stop_resumes = 1;
    s.plan.failover = Some(three_routes());
    s.routes = others();
    let mut live = start(s);
    let q = live.needs_input(1).await;
    assert_eq!(q["reason"], "engine_stop");
    assert_eq!(q["canContinue"], true);
    assert_eq!(q["evidence"]["identicalCalls"], 5);
    assert_eq!(q["evidence"]["failedTurns"], 0);
    assert_eq!(q["evidence"]["attempts"], 2);
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(&id, InstructAction::Finish, None, None).await;
    let (end, events, procs) = live.finish().await;
    assert_eq!(
        classify(&end.raw).outcome,
        DirectOutcome::StoppedEngineCancelled
    );
    // The one resume was sent and the engine's own stop never moved the model or started a session.
    assert_eq!(prompts_sent(&procs[0]), 2);
    assert!(end.switches.is_empty());
    assert!(events_of(&events, "new-session").is_empty());
}
