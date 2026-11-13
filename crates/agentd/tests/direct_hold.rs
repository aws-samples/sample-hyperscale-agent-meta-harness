//! The hold of the module seam. A module votes `TurnVote::Wait { until }` at a turn end when the engine is
//! still working on its own (the engine's goal plugin queues follow-up prompts itself, so the engine goes on after the prompt's answer with
//! no request open). The driver keeps the run open: it reads the engine's pipe for `until`, feeds what it reads to the collector and the
//! wire tap, applies the deadline, the silence limit and the user's cancel as it does for a prompt, and asks again. Over in-memory streams
//! with a scripted engine and a paused clock.
#![allow(clippy::unwrap_used)]

use agentd::BoxFuture;
use agentd::acp::{AcpClient, Activity, Dir};
use agentd::direct::classify::classify;
use agentd::direct::collector::DirectCollector;
use agentd::direct::driver::{NoProbe, PromptPlan, run_direct_prompt};
use agentd::direct::hooks::{DirectHook, Event, Hooks, TurnEnd, TurnVote, Wire};
use agentd::direct::retry::RetryPolicy;
use agentd::events::{EventRecorder, TurnControls};
use harness_protocol::api::internal::PendingControl;
use harness_protocol::direct::DirectOutcome;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const MODEL: &str = "us.anthropic.claude-opus-5-5";
const TOP: &str = "/mnt/workspace/run1/direct/repo";
const PROMPT: &str = "Add percent_change with tests";

#[derive(Clone, Debug)]
enum Step {
    Update(Value),
    Sleep(u64),
    Finish,
    /// The engine's output ends (the process died).
    Close,
    /// Nothing more, ever.
    Silent,
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

fn frame(v: Value) -> String {
    format!("{v}\n")
}

type Client = AcpClient<tokio::io::DuplexStream, tokio::io::DuplexStream>;
type Received = Arc<Mutex<Vec<Value>>>;

/// A client wired to an in-process engine; `prompts` holds the steps after each `session/prompt`, in order. The steps after `Finish`
/// run with no request open: the engine going on by itself.
fn engine(prompts: Vec<Vec<Step>>) -> (Client, Received) {
    let (client_w, engine_r) = tokio::io::duplex(1 << 16);
    let (mut out, client_r) = tokio::io::duplex(1 << 16);
    let received: Received = Arc::default();
    let seen = received.clone();
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
        while let Some(msg) = rx.recv().await {
            let id = msg.get("id").cloned();
            let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
            match msg["method"].as_str().unwrap_or_default() {
                "initialize" => out
                    .write_all(frame(reply(json!({"protocolVersion": 1, "agentInfo": {"name": "scripted", "version": "9.9.9"}}))).as_bytes())
                    .await
                    .unwrap(),
                "session/new" => out
                    .write_all(frame(reply(json!({"sessionId": "s-1", "configOptions": [{"id": "model", "currentValue": MODEL, "options": []}]}))).as_bytes())
                    .await
                    .unwrap(),
                "session/prompt" => {
                    let steps = prompts.get(prompt_no).cloned().unwrap_or_default();
                    prompt_no += 1;
                    for step in steps {
                        match step {
                            Step::Update(u) => out
                                .write_all(frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s-1", "update": u}})).as_bytes())
                                .await
                                .unwrap(),
                            Step::Sleep(s) => tokio::time::sleep(Duration::from_secs(s)).await,
                            Step::Finish => out
                                .write_all(frame(reply(json!({"stopReason": "end_turn"}))).as_bytes())
                                .await
                                .unwrap(),
                            Step::Close => return,
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
    (AcpClient::new(client_r, client_w), received)
}

fn plan(
    timeout_s: u64,
    silence_s: u64,
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> PromptPlan {
    PromptPlan {
        cwd: TOP.into(),
        engine: "cline".into(),
        model: MODEL.into(),
        prompt: PROMPT.into(),
        nudge: None,
        cancel,
        timeout: Duration::from_secs(timeout_s),
        silence: Duration::from_secs(silence_s),
        retry: RetryPolicy::off(),
        failover: None,
        engine_stop_resumes: 0,
    }
}

/// A module that asks for the hold until a mark shows on the wire, and writes down what it is shown.
struct Holder {
    name: &'static str,
    /// The wire text that ends the hold.
    mark: &'static str,
    saw_mark: AtomicBool,
    asks: Mutex<Vec<String>>,
    ask_times: Mutex<Vec<tokio::time::Instant>>,
    events: Mutex<Vec<String>>,
    replies: AtomicU32,
    /// Votes handed out first, in order; then the hold or `Done`.
    votes: Mutex<VecDeque<TurnVote>>,
    until: Duration,
}

impl Holder {
    fn new(name: &'static str, mark: &'static str) -> Arc<Self> {
        Arc::new(Self {
            name,
            mark,
            saw_mark: AtomicBool::new(false),
            asks: Mutex::new(Vec::new()),
            ask_times: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            replies: AtomicU32::new(0),
            votes: Mutex::new(VecDeque::new()),
            until: Duration::from_secs(1),
        })
    }

    fn with_votes(self: Arc<Self>, v: Vec<TurnVote>) -> Arc<Self> {
        *self.votes.lock().unwrap() = v.into();
        self
    }
}

impl DirectHook for Holder {
    fn name(&self) -> &'static str {
        self.name
    }

    fn on_wire(&self, w: &Wire<'_>) {
        if w.dir == Dir::FromEngine
            && !self.mark.is_empty()
            && w.msg.to_string().contains(self.mark)
        {
            self.saw_mark.store(true, Ordering::SeqCst);
        }
    }

    fn on_event(&self, e: &Event<'_>) {
        if let Event::PromptReplied(_) = e {
            self.replies.fetch_add(1, Ordering::SeqCst);
        }
        self.events.lock().unwrap().push(e.name().to_string());
    }

    fn on_turn_end<'a>(&'a self, end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move {
            self.asks.lock().unwrap().push(end.raw.message.clone());
            self.ask_times
                .lock()
                .unwrap()
                .push(tokio::time::Instant::now());
            if let Some(v) = self.votes.lock().unwrap().pop_front() {
                return v;
            }
            if self.saw_mark.load(Ordering::SeqCst) {
                TurnVote::Done
            } else {
                TurnVote::Wait { until: self.until }
            }
        })
    }

    fn on_seal<'a>(&'a self, _end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move { TurnVote::Done })
    }
}

struct Run {
    end: agentd::direct::driver::DriverEnd,
    received: Received,
    collector: DirectCollector,
}

async fn drive(
    prompts: Vec<Vec<Step>>,
    plan: PromptPlan,
    modules: Vec<Arc<dyn DirectHook>>,
    controls: Option<Arc<TurnControls>>,
) -> Run {
    let (client, received) = engine(prompts);
    let activity = Activity::new();
    let hooks = Hooks::new(modules);
    let mut client = client
        .with_activity(activity.clone())
        .with_tap(hooks.tap().unwrap());
    if let Some(c) = &controls {
        client = client.with_cancel(c.subscribe(), Duration::from_secs(10));
    }
    let rec = EventRecorder::new("run1", "direct");
    let mut collector = DirectCollector::new(rec).with_hooks(hooks);
    let end = run_direct_prompt(&mut client, &mut collector, &activity, &plan, &NoProbe).await;
    // the engine's reader task records what was written; let it catch up before a test counts
    tokio::time::sleep(Duration::from_millis(50)).await;
    Run {
        end,
        received,
        collector,
    }
}

fn methods(received: &Received) -> Vec<String> {
    received
        .lock()
        .unwrap()
        .iter()
        .filter_map(|m| m["method"].as_str().map(str::to_string))
        .collect()
}

fn count(received: &Received, method: &str) -> usize {
    methods(received).iter().filter(|m| *m == method).count()
}

// ---- the hold ----------------------------------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_wait_vote_keeps_the_run_open_reads_the_pipe_and_ends_with_the_message_the_engine_said_last()
 {
    let holder = Holder::new("goal", "LOOP-END");
    let run = drive(
        vec![vec![
            chunk("first"),
            Step::Finish,
            Step::Sleep(2),
            tool("t1", "pending"),
            chunk("middle"),
            Step::Sleep(2),
            tool("t2", "pending"),
            chunk("LOOP-END final words"),
        ]],
        plan(1000, 900, None),
        vec![holder.clone()],
        None,
    )
    .await;
    // one prompt only: the engine went on by itself
    assert_eq!(
        methods(&run.received),
        ["initialize", "session/new", "session/prompt"]
    );
    let asks = holder.asks.lock().unwrap().clone();
    assert!(
        asks.len() >= 4,
        "asked again and again while the engine worked: {asks:?}"
    );
    assert_eq!(asks[0], "first");
    // what the pipe held after the answer reached the collector and the module
    assert_eq!(run.collector.tool_calls(), 2);
    assert!(asks.contains(&"middle".to_string()), "{asks:?}");
    assert_eq!(asks.last().unwrap(), "LOOP-END final words", "{asks:?}");
    assert_eq!(run.end.raw.message, "LOOP-END final words");
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::ReportedDone);
    assert!(run.end.hook_stop.is_none());
}

#[tokio::test(start_paused = true)]
async fn the_deadline_ends_a_hold_like_it_ends_a_prompt_and_the_cancel_is_written() {
    let holder = Holder::new("goal", "NEVER");
    let steps = std::iter::once(chunk("first"))
        .chain(std::iter::once(Step::Finish))
        .chain((0..30).flat_map(|i| [Step::Sleep(3), chunk(&format!("line {i}"))]))
        .collect();
    let run = drive(vec![steps], plan(20, 900, None), vec![holder], None).await;
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedTimeout
    );
    assert_eq!(count(&run.received, "session/cancel"), 1);
}

#[tokio::test(start_paused = true)]
async fn silence_ends_a_hold_after_the_silence_limit_like_it_ends_a_prompt() {
    let holder = Holder::new("goal", "NEVER");
    let run = drive(
        vec![vec![chunk("first"), Step::Finish, Step::Silent]],
        plan(10_000, 30, None),
        vec![holder.clone()],
        None,
    )
    .await;
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedSilence
    );
    assert_eq!(count(&run.received, "session/cancel"), 1);
    assert!(
        holder.asks.lock().unwrap().len() >= 20,
        "asked every second for the 30 s of silence"
    );
}

#[tokio::test(start_paused = true)]
async fn a_tool_that_runs_keeps_the_silence_clock_stopped_during_a_hold() {
    let holder = Holder::new("goal", "NEVER");
    let run = drive(
        vec![vec![
            chunk("first"),
            Step::Finish,
            tool("long", "in_progress"),
            Step::Silent,
        ]],
        plan(100, 30, None),
        vec![holder],
        None,
    )
    .await;
    // the deadline, not the silence: a running tool is not the provider being quiet
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedTimeout
    );
}

#[tokio::test(start_paused = true)]
async fn the_users_cancel_during_a_hold_writes_one_cancel_and_the_stop_is_the_users() {
    let holder = Holder::new("goal", "NEVER");
    let (controls, _) = TurnControls::new();
    let c = controls.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        c.deliver(&[PendingControl::Cancel]);
    });
    let t0 = tokio::time::Instant::now();
    let run = drive(
        vec![vec![chunk("first"), Step::Finish, Step::Silent]],
        plan(10_000, 900, Some(controls.subscribe())),
        vec![holder.clone()],
        Some(controls),
    )
    .await;
    assert!(
        t0.elapsed() < Duration::from_secs(8),
        "{:?}: no grace is waited for, no prompt is open",
        t0.elapsed()
    );
    let classified = classify(&run.end.raw);
    assert_eq!(classified.outcome, DirectOutcome::StoppedByUser);
    assert!(run.end.raw.cancel_sent);
    assert_eq!(count(&run.received, "session/cancel"), 1);
    // the module is told, so it can stop the engine's own loop
    assert!(
        holder
            .events
            .lock()
            .unwrap()
            .contains(&"cancel".to_string())
    );
}

#[tokio::test(start_paused = true)]
async fn an_engine_that_dies_during_a_hold_is_a_dead_engine() {
    let holder = Holder::new("goal", "NEVER");
    let run = drive(
        vec![vec![
            chunk("first"),
            Step::Finish,
            Step::Sleep(2),
            Step::Close,
        ]],
        plan(10_000, 900, None),
        vec![holder],
        None,
    )
    .await;
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedEngineDied
    );
}

// ---- who wins ---------------------------------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_prompt_from_a_later_module_goes_out_while_the_hold_goes_on_and_the_hold_asks_again_after_it()
 {
    let goal = Holder::new("goal", "second answer");
    let messages = Holder::new("messages", "").with_votes(vec![TurnVote::Prompt("hello".into())]);
    let run = drive(
        vec![
            vec![chunk("first"), Step::Finish],
            vec![chunk("second answer"), Step::Finish],
        ],
        plan(1000, 900, None),
        vec![goal.clone(), messages],
        None,
    )
    .await;
    assert_eq!(count(&run.received, "session/prompt"), 2);
    let prompts: Vec<String> = run
        .received
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m["method"] == "session/prompt")
        .map(|m| {
            m["params"]["prompt"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(prompts, [PROMPT, "hello"]);
    assert_eq!(run.end.raw.message, "second answer");
    assert!(goal.asks.lock().unwrap().len() >= 2);
}

#[tokio::test(start_paused = true)]
async fn a_stop_from_any_module_beats_the_hold_and_ends_the_run_at_once() {
    let goal = Holder::new("goal", "NEVER");
    let brake = Holder::new("brake", "").with_votes(vec![TurnVote::Stop {
        detail: "The harness ended the run: test".into(),
    }]);
    let run = drive(
        vec![vec![chunk("first"), Step::Finish, Step::Silent]],
        plan(1000, 900, None),
        vec![goal, brake],
        None,
    )
    .await;
    assert_eq!(
        run.end.hook_stop.as_deref(),
        Some("The harness ended the run: test")
    );
}

#[tokio::test(start_paused = true)]
async fn a_wait_at_the_seal_counts_as_no_opinion() {
    struct SealWaiter;
    impl DirectHook for SealWaiter {
        fn name(&self) -> &'static str {
            "sealwaiter"
        }
        fn on_seal<'a>(&'a self, _end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
            Box::pin(async {
                TurnVote::Wait {
                    until: Duration::from_secs(1),
                }
            })
        }
    }
    let run = drive(
        vec![vec![chunk("done"), Step::Finish]],
        plan(1000, 900, None),
        vec![Arc::new(SealWaiter)],
        None,
    )
    .await;
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(count(&run.received, "session/prompt"), 1);
}

#[tokio::test(start_paused = true)]
async fn the_shortest_wait_of_several_modules_is_the_one_kept() {
    let a = Holder::new("a", "ENDMARK").with_votes(vec![TurnVote::Wait {
        until: Duration::from_secs(5),
    }]);
    let b = Holder::new("b", "ENDMARK").with_votes(vec![TurnVote::Wait {
        until: Duration::from_secs(1),
    }]);
    let run = drive(
        vec![vec![
            chunk("first"),
            Step::Finish,
            Step::Sleep(3),
            chunk("ENDMARK"),
        ]],
        plan(1000, 900, None),
        vec![a.clone(), b.clone()],
        None,
    )
    .await;
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::ReportedDone);
    // a asked for 5 s and b for 1 s: the second ask came after about 1 s, not 5
    let times = b.ask_times.lock().unwrap().clone();
    assert!(times.len() >= 2, "{times:?}");
    let gap = times[1] - times[0];
    assert!(
        gap >= Duration::from_secs(1) && gap < Duration::from_secs(2),
        "{gap:?}"
    );
}
