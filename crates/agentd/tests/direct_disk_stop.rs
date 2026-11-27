//! A full session disk is named, not reported as a provider error.
//!
//! The checkout is on the session's managed storage: 1 GiB, and `df` and `statvfs` report 0 used on it. In a live run it filled, the engine
//! answered error -32603 "Internal error" on every turn, and the failover ladder cycled all model routes six times and sent 12 prompts with no
//! new tool call before it gave up ("Automatic recovery reached its prompt cap"). The routes were healthy.
//!
//! These tests run the real driver over in-memory streams with a scripted engine, a scripted disk and a paused clock. They pin that a -32603 with
//! the disk short is a cause of its own (no route cycling, no second prompt, the run's label names the disk, a question that offers a message or
//! the end), that the same error with room behaves exactly as before, and that the early notice comes once per crossing.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::BoxFuture;
use agentd::acp::{AcpClient, Activity, Watched};
use agentd::direct::classify::{Classified, classify, with_disk};
use agentd::direct::collector::DirectCollector;
use agentd::direct::context::ContextDir;
use agentd::direct::disk::{DiskCheck, DiskFull, DiskProbe, Limits, Reading, Signal};
use agentd::direct::driver::{
    DriverEnd, InputWiring, NoProbe, PromptPlan, Respawn, run_direct_stages,
};
use agentd::direct::failover::{FailoverPlan, Route};
use agentd::direct::hooks::{DirectHook, Hooks, TurnEnd, TurnVote};
use agentd::direct::input::{
    AssessFacts, Assessment, ChangedFiles, DigestSource, DigestSources, InputPolicy, TreeSource,
    assess, reason_word,
};
use agentd::direct::retry::RetryPolicy;
use agentd::direct::storage_watch::{
    StorageWatch, StorageWrites, WatchSetup, WatchTask, WriteError,
};
use agentd::events::{EventRecorder, Instruct};
use agentd::logging::MemoryLog;
use harness_protocol::direct::{DirectOutcome, InstructAction, InstructOrigin};
use harness_protocol::envelope::{Envelope, Payload};
use harness_protocol::runtime::TurnOutcome;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::{mpsc, watch};

const MODEL: &str = "us.anthropic.claude-opus-5-5";
const MODEL_B: &str = "global.anthropic.claude-opus-5-5";
const MODEL_C: &str = "us.anthropic.claude-sonnet-5-5";
const TOP: &str = "/mnt/workspace/run1/direct/repo";
const PROMPT: &str = "Add percent_change\nwith tests\n";

// ---- the disks ---------------------------------------------------------------------------------------------------------------

fn ample() -> Reading {
    Reading {
        root_free_mb: Some(5700),
        workspace_used_mb: Some(40),
        workspace_partial: false,
        writable: None,
    }
}

/// The workspace is over its mark: 975 MiB of a 960 MiB mark (the mount holds 1024).
fn full() -> Reading {
    Reading {
        root_free_mb: Some(2400),
        workspace_used_mb: Some(975),
        workspace_partial: false,
        writable: None,
    }
}

/// The workspace is past the early mark (864 MiB) and short of the stop mark (960 MiB).
fn nearly(used: u64) -> Reading {
    Reading {
        root_free_mb: Some(2400),
        workspace_used_mb: Some(used),
        workspace_partial: false,
        writable: None,
    }
}

/// A disk that answers the readings in order, then the last one again, and counts the measurements.
#[derive(Debug)]
struct Scripted {
    readings: Mutex<VecDeque<Reading>>,
    last: Mutex<Reading>,
    calls: AtomicU32,
}

impl Scripted {
    fn new(readings: Vec<Reading>) -> Arc<Self> {
        Arc::new(Self {
            readings: Mutex::new(readings.into()),
            last: Mutex::new(Reading::default()),
            calls: AtomicU32::new(0),
        })
    }

    fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
}

impl DiskProbe for Scripted {
    fn measure(&self) -> BoxFuture<'_, Reading> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut q = self.readings.lock().unwrap();
            let mut last = self.last.lock().unwrap();
            if let Some(next) = q.pop_front() {
                *last = next;
            }
            *last
        })
    }

    /// The size of the files for the notice of a refusal: what the newest reading said.
    fn storage_used(&self) -> BoxFuture<'_, (Option<u64>, bool)> {
        Box::pin(async move {
            let last = self.last.lock().unwrap();
            (last.workspace_used_mb, last.workspace_partial)
        })
    }
}

fn check(probe: &Arc<Scripted>) -> DiskCheck {
    DiskCheck::new(probe.clone(), Limits::default())
}

// ---- the scripted engine -----------------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Step {
    Update(Value),
    Finish(&'static str),
    ErrorWith(i64, &'static str, Value),
}

fn chunk(text: &str) -> Step {
    Step::Update(
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
    )
}

fn end_turn(text: &str) -> Vec<Step> {
    vec![chunk(text), Step::Finish("end_turn")]
}

/// What the live run saw: error -32603 with the message `Internal error` and no data.
fn internal_error() -> Vec<Step> {
    vec![Step::ErrorWith(-32603, "Internal error", Value::Null)]
}

/// A provider fault the engine names: a 503 the provider marked retryable.
fn provider_503() -> Vec<Step> {
    vec![Step::ErrorWith(
        -32603,
        "Internal error: Service unavailable",
        json!({"isRetryable": true, "statusCode": 503, "errorName": "ServiceUnavailableException"}),
    )]
}

fn frame(v: Value) -> String {
    format!("{v}\n")
}

type Received = Arc<Mutex<Vec<Value>>>;

struct Proc {
    r: DuplexStream,
    w: DuplexStream,
    received: Received,
}

/// One engine process; `prompts` holds the steps of each `session/prompt` of its one session, in order. `model_value` is the model
/// it reports for its session: an engine names a model in its own form.
fn spawn_engine(prompts: Vec<Vec<Step>>, model_value: String) -> Proc {
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
        let mut model = model_value;
        let mut prompt_no = 0usize;
        while let Some(msg) = rx.recv().await {
            let id = msg.get("id").cloned();
            let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
            match msg["method"].as_str().unwrap_or_default() {
                "initialize" => out
                    .write_all(frame(reply(json!({"protocolVersion": 1}))).as_bytes())
                    .await
                    .unwrap(),
                "session/new" => {
                    let options = json!([{"id": "model", "currentValue": model, "options": []}]);
                    out.write_all(
                        frame(reply(json!({"sessionId": "s-1", "configOptions": options})))
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
                    let steps = prompts.get(prompt_no).cloned().unwrap_or_default();
                    prompt_no += 1;
                    for step in steps {
                        match step {
                            Step::Update(u) => out
                                .write_all(
                                    frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s-1", "update": u}}))
                                        .as_bytes(),
                                )
                                .await
                                .unwrap(),
                            Step::Finish(reason) => out
                                .write_all(frame(reply(json!({"stopReason": reason}))).as_bytes())
                                .await
                                .unwrap(),
                            Step::ErrorWith(code, message, data) => out
                                .write_all(
                                    frame(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message, "data": data}}))
                                        .as_bytes(),
                                )
                                .await
                                .unwrap(),
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

/// A run that never restarts its engine.
struct NoRestart;

impl Respawn for NoRestart {
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
        Box::pin(async { Err("no engine left to start".to_string()) })
    }
}

struct Tree;

impl TreeSource for Tree {
    fn changed_files(
        &self,
        _limit: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<ChangedFiles>> + Send + '_>>
    {
        Box::pin(async {
            Some(ChangedFiles {
                paths: vec!["src/lib.rs".into()],
                total: 1,
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

fn route(id: &str) -> Route {
    Route {
        id: id.into(),
        region: "us-east-1".into(),
        context_tokens: 200_000,
        max_output_tokens: 32_000,
    }
}

fn three_routes() -> FailoverPlan {
    FailoverPlan::new(vec![route(MODEL), route(MODEL_B), route(MODEL_C)])
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

struct Setup {
    prompts: Vec<Vec<Step>>,
    retry: RetryPolicy,
    failover: Option<FailoverPlan>,
    policy: InputPolicy,
    hooks: Vec<Arc<dyn DirectHook>>,
    /// The writes and the size of a watch of the session's storage; `None` runs without one.
    storage: Option<(Arc<dyn StorageWrites>, Arc<dyn DiskProbe>)>,
    /// `finish` waits until the watch made every test write it was asked for, as the session's end does. A test whose storage
    /// never answers switches the wait off.
    storage_wait: bool,
    /// The profile name of the engine that runs; every test but the one that names another runs the default engine.
    engine: &'static str,
}

impl Setup {
    fn new(prompts: Vec<Vec<Step>>, retry: RetryPolicy) -> Self {
        Self {
            prompts,
            retry,
            failover: None,
            policy: policy(0),
            hooks: Vec::new(),
            storage: None,
            storage_wait: true,
            engine: "cline",
        }
    }
}

struct Live {
    /// The task of the storage watch, when the run has one, and the watch itself when `finish` waits for it.
    _storage: Option<WatchTask>,
    storage_watch: Option<StorageWatch>,
    tx: Option<mpsc::Sender<Instruct>>,
    _cancel: watch::Sender<bool>,
    rec: Arc<EventRecorder>,
    seen: Vec<Envelope>,
    handle: tokio::task::JoinHandle<DriverEnd>,
    received: Received,
}

fn start(s: Setup) -> Live {
    let p0 = spawn_engine(
        s.prompts,
        agentd::engine::config::acp_model_value(s.engine, MODEL),
    );
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let (tx, rx) = mpsc::channel::<Instruct>(16);
    let rec = EventRecorder::new("run1", "direct");
    let activity = Activity::new();
    let client = AcpClient::new(p0.r, p0.w)
        .with_activity(activity.clone())
        .with_cancel(cancel_rx.clone(), Duration::from_secs(10));
    let mut retry = s.retry;
    let mut storage_task = None;
    let mut storage_watch = None;
    // The collector gets the watch as a session's collector does: a tool result that names no space wakes it.
    let mut collector_watch = None;
    let storage_wait = s.storage_wait;
    if let Some((writes, probe)) = s.storage {
        let watch = StorageWatch::new(WatchSetup {
            writes,
            probe,
            rec: rec.clone(),
            log: MemoryLog::new(),
            run_id: "run1".to_string(),
            session: 1,
            run_dir: common::scratch("disk-stop-run"),
            limits: Limits::default(),
            test_refusal: false,
        });
        storage_task = Some(watch.start());
        if storage_wait {
            storage_watch = Some(watch.clone());
        }
        collector_watch = Some(watch.clone());
        retry = retry.with_storage(watch);
    }
    let plan = PromptPlan {
        cwd: TOP.into(),
        engine: s.engine.into(),
        model: MODEL.into(),
        prompt: PROMPT.into(),
        nudge: None,
        cancel: Some(cancel_rx.clone()),
        timeout: Duration::from_secs(30_000),
        silence: Duration::from_secs(900),
        retry,
        failover: s.failover,
        engine_stop_resumes: 0,
    };
    let dir = common::scratch("disk-stop");
    let wiring = InputWiring {
        delivery: None,
        policy: s.policy,
        instructions: rx,
        cancel: cancel_rx,
        tree: Arc::new(Tree),
        sources: Arc::new(Source),
        context: ContextDir::create(&dir).unwrap(),
        task: PROMPT.to_string(),
        engine_label: agentd::engine::direct::engine_label(s.engine).to_string(),
        routes: Vec::new(),
        rec: rec.clone(),
        audit: None,
        log: None,
        review: None,
    };
    let rec2 = rec.clone();
    let hooks = s.hooks;
    let handle = tokio::spawn(async move {
        let mut client = client;
        let mut host = NoRestart;
        let mut collector = DirectCollector::new(rec2);
        if !hooks.is_empty() {
            collector = collector.with_hooks(Hooks::new(hooks));
        }
        if let Some(watch) = collector_watch {
            collector = collector.with_storage_watch(watch);
        }
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
        _storage: storage_task,
        storage_watch,
        tx: Some(tx),
        _cancel: cancel_tx,
        rec,
        seen: Vec::new(),
        handle,
        received: p0.received,
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

    fn events(&mut self, kind: &str) -> Vec<Value> {
        self.pull();
        self.seen
            .iter()
            .filter_map(kind_of)
            .filter(|(k, _)| k == kind)
            .map(|(_, v)| v)
            .collect()
    }

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

    async fn answer(&self, request_id: &str, action: InstructAction, text: Option<&str>) {
        self.tx
            .as_ref()
            .unwrap()
            .send(Instruct {
                request_id: request_id.to_string(),
                action,
                text: text.map(str::to_string),
                model: None,
                origin: InstructOrigin::User,
                cancelled_in_batch: false,
            })
            .await
            .unwrap();
    }

    async fn finish(mut self) -> (DriverEnd, Vec<Value>, Received) {
        let end = (&mut self.handle).await.unwrap();
        // The session's end waits for the test write a last prompt boundary or tool result asked for.
        if let Some(watch) = &self.storage_watch {
            watch.idle().await;
        }
        self.pull();
        let all: Vec<Value> = self
            .seen
            .iter()
            .filter_map(kind_of)
            .map(|(_, v)| v)
            .collect();
        (end, all, self.received.clone())
    }
}

fn count(r: &Received, method: &str) -> usize {
    r.lock()
        .unwrap()
        .iter()
        .filter(|m| m["method"] == method)
        .count()
}

fn kinds(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|v| v["kind"].as_str().map(str::to_string))
        .collect()
}

fn notices(events: &[Value], reason: &str) -> Vec<Value> {
    events
        .iter()
        .filter(|v| v["kind"] == "notify" && v["reason"] == reason)
        .cloned()
        .collect()
}

// ---- a -32603 with the disk short is the disk -----------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_32603_with_the_disk_full_stops_at_once_no_second_prompt_and_no_route_switch() {
    let disk = Scripted::new(vec![ample(), full()]);
    let mut s = Setup::new(
        vec![internal_error(), end_turn("never sent")],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    s.failover = Some(three_routes());
    let (end, events, received) = start(s).finish().await;
    // One prompt reached the engine: no resume, and no switch to another route.
    assert_eq!(count(&received, "session/prompt"), 1);
    assert_eq!(count(&received, "session/set_config_option"), 0);
    // The cause is recorded with the numbers.
    let cause = end.disk_full.as_ref().expect("the stop names the disk");
    assert_eq!(cause.workspace_used_mb, Some(975));
    assert_eq!(cause.root_free_mb, Some(2400));
    assert!(cause.workspace_short && !cause.root_short);
    // The run ended on the engine's error, which agentd now reads as the disk.
    assert!(matches!(end.raw.watched, Watched::Done(Err(_))));
    let named = with_disk(classify(&end.raw), end.disk_full.as_ref());
    assert_eq!(named.outcome, DirectOutcome::StoppedOther);
    assert_eq!(named.turn_outcome, TurnOutcome::Failed);
    assert!(named.fault_hint.is_none());
    assert_eq!(
        DirectOutcome::StoppedOther.label_for(named.detail.as_deref()),
        "Cline stopped: the harness ended the run (the session disk is full, workspace 975 MiB against a stop mark of 960 MiB, root 2400 MiB free)"
    );
    // One notice names it and says what the user can do; no retry, no wait, no switch was announced.
    let notes = notices(&events, "disk_full");
    assert_eq!(notes.len(), 1, "{events:?}");
    let detail = notes[0]["detail"].as_str().unwrap();
    assert!(detail.starts_with("The session disk is full"), "{detail}");
    assert!(detail.contains("975"), "{detail}");
    assert!(
        detail.contains("ham say") && detail.contains("ham finish"),
        "{detail}"
    );
    for k in kinds(&events) {
        assert!(
            !matches!(
                k.as_str(),
                "provider-retry" | "provider-wait" | "provider-switch" | "provider-outage"
            ),
            "{k} in {events:?}"
        );
    }
    // Measured twice: at the prompt boundary and, fresh, after the error.
    assert_eq!(disk.calls(), 2);
}

#[tokio::test(start_paused = true)]
async fn the_attempt_that_found_the_disk_full_says_so_and_costs_the_ladder_nothing() {
    let disk = Scripted::new(vec![ample(), full()]);
    let mut s = Setup::new(
        vec![internal_error(), internal_error(), end_turn("never")],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    s.failover = Some(three_routes());
    let (end, _, _) = start(s).finish().await;
    // A single try lists no attempts; the disk stop is the first.
    assert!(end.attempts.len() <= 1);
    assert!(end.switches.is_empty());
    assert_eq!(end.final_model, None, "the ladder never moved");
}

/// What a run without the check does with the same errors, for the comparison below.
async fn the_ladder_run(disk: Option<Arc<Scripted>>) -> (DriverEnd, Received) {
    let mut retry = RetryPolicy::new(3, Duration::from_secs(1));
    if let Some(d) = &disk {
        retry = retry.with_disk(check(d));
    }
    let mut s = Setup::new(
        vec![internal_error(), internal_error(), end_turn("Done.")],
        retry,
    );
    s.failover = Some(three_routes());
    let (end, _, received) = start(s).finish().await;
    (end, received)
}

#[tokio::test(start_paused = true)]
async fn a_32603_with_room_behaves_exactly_as_it_did_before() {
    let (plain, plain_wire) = the_ladder_run(None).await;
    let disk = Scripted::new(vec![ample()]);
    let (checked, checked_wire) = the_ladder_run(Some(disk.clone())).await;
    // The same messages in the same order, the same tries and the same switches.
    let wire = |r: &Received| -> Vec<(String, Value)> {
        r.lock()
            .unwrap()
            .iter()
            .map(|m| {
                (
                    m["method"].as_str().unwrap_or_default().to_string(),
                    m["params"].clone(),
                )
            })
            .collect()
    };
    assert_eq!(wire(&plain_wire), wire(&checked_wire));
    let tries = |e: &DriverEnd| -> Vec<(u32, String, String, Option<String>)> {
        e.attempts
            .iter()
            .map(|a| {
                (
                    a.n,
                    format!("{:?}", a.kind),
                    format!("{:?}", a.result),
                    a.retry_class.clone(),
                )
            })
            .collect()
    };
    assert_eq!(tries(&plain), tries(&checked));
    assert!(
        tries(&plain).len() >= 3,
        "the ladder really ran: {:?}",
        tries(&plain)
    );
    assert_eq!(plain.switches.len(), checked.switches.len());
    assert!(!plain.switches.is_empty(), "a route was cycled");
    assert_eq!(classify(&plain.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(classify(&checked.raw).outcome, DirectOutcome::ReportedDone);
    assert!(checked.disk_full.is_none());
    assert!(disk.calls() >= 2, "it measured, and found room");
}

#[tokio::test(start_paused = true)]
async fn what_was_not_measured_is_never_the_disk() {
    let disk = Scripted::new(vec![Reading::default()]);
    let (end, _) = the_ladder_run(Some(disk)).await;
    assert!(end.disk_full.is_none());
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
}

#[tokio::test(start_paused = true)]
async fn an_error_that_names_its_own_cause_is_not_measured_as_the_disk() {
    // The provider's filter blocked the answer: the cause is stated, the disk is not asked.
    let disk = Scripted::new(vec![ample(), full()]);
    let s = Setup::new(
        vec![vec![Step::ErrorWith(
            -32603,
            "Internal error: Model returned no content because the response was blocked by a content filter.",
            Value::Null,
        )]],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    let (end, events, _) = start(s).finish().await;
    assert!(end.disk_full.is_none());
    assert!(notices(&events, "disk_full").is_empty());
    assert_eq!(
        classify(&end.raw).outcome,
        DirectOutcome::StoppedProviderError
    );
}

#[tokio::test(start_paused = true)]
async fn a_provider_fault_the_engine_names_is_measured_too_and_a_short_disk_wins() {
    // A -32603 is read as the disk when the disk is short, also when it carries provider markers; with room it is the
    // provider's, as before.
    let disk = Scripted::new(vec![ample(), full()]);
    let mut s = Setup::new(
        vec![provider_503(), end_turn("never")],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    s.failover = Some(three_routes());
    let (end, _, received) = start(s).finish().await;
    assert!(end.disk_full.is_some());
    assert_eq!(count(&received, "session/prompt"), 1);
    let room = Scripted::new(vec![ample()]);
    let mut s = Setup::new(
        vec![provider_503(), end_turn("Done.")],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&room)),
    );
    s.failover = Some(three_routes());
    let (end, _, received) = start(s).finish().await;
    assert!(end.disk_full.is_none());
    assert_eq!(count(&received, "session/prompt"), 2, "resumed as before");
}

// ---- no module may send another prompt ------------------------------------------------------------------------------------

/// A module that votes one kick prompt at a turn end, as the goal loop does after a loop that stopped.
struct Kicker {
    votes: AtomicU32,
}

impl DirectHook for Kicker {
    fn name(&self) -> &'static str {
        "kicker"
    }

    fn on_turn_end<'a>(&'a self, _end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move {
            if self.votes.fetch_add(1, Ordering::SeqCst) == 0 {
                TurnVote::Prompt("kick".to_string())
            } else {
                TurnVote::Done
            }
        })
    }

    fn on_seal<'a>(&'a self, _end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async { TurnVote::Done })
    }
}

#[tokio::test(start_paused = true)]
async fn a_module_votes_its_kick_when_the_disk_has_room_and_is_not_asked_when_it_is_full() {
    // The control: the module's kick is a second prompt.
    let room = Scripted::new(vec![ample()]);
    let mut s = Setup::new(
        vec![end_turn("one"), end_turn("two")],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&room)),
    );
    s.hooks = vec![Arc::new(Kicker {
        votes: AtomicU32::new(0),
    })];
    let (_, _, received) = start(s).finish().await;
    assert_eq!(count(&received, "session/prompt"), 2);
    // With the disk full the stage ends on the disk and the module is not asked for a prompt.
    let disk = Scripted::new(vec![ample(), full()]);
    let kicker = Arc::new(Kicker {
        votes: AtomicU32::new(0),
    });
    let mut s = Setup::new(
        vec![internal_error(), end_turn("never")],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    s.hooks = vec![kicker.clone()];
    let (end, _, received) = start(s).finish().await;
    assert_eq!(count(&received, "session/prompt"), 1);
    assert!(end.disk_full.is_some());
    assert_eq!(kicker.votes.load(Ordering::SeqCst), 0, "no vote was asked");
}

// ---- the question ---------------------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_full_disk_asks_with_a_message_or_the_end_and_names_itself() {
    let disk = Scripted::new(vec![ample(), full()]);
    let mut s = Setup::new(
        vec![internal_error(), end_turn("never")],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    s.failover = Some(three_routes());
    s.policy = policy(1800);
    let mut live = start(s);
    let q = live.needs_input(1).await;
    assert_eq!(q["reason"], "disk_full");
    assert_eq!(q["title"], "The session disk is full");
    assert_eq!(q["blocking"], true);
    assert_eq!(q["options"], json!(["continue_with_prompt", "finish"]));
    assert_eq!(q["models"], json!([]));
    assert_eq!(q["canContinue"], true);
    let text = q["text"].as_str().unwrap();
    assert_eq!(
        text,
        "Needs your input: The session disk is full (ham say or ham finish)"
    );
    assert!(text.chars().count() <= harness_protocol::direct::MAX_NEEDS_INPUT_TEXT_CHARS);
    let question = q["question"].as_str().unwrap();
    assert!(
        question.contains("ham say") && question.contains("ham finish"),
        "{question}"
    );
    let last = q["evidence"]["lastError"].as_str().unwrap();
    assert!(
        last.contains("the session disk is full") && last.contains("975"),
        "{last}"
    );
    // Until the person answers, nothing else reaches the engine.
    assert_eq!(count(&live.received, "session/prompt"), 1);
    assert_eq!(count(&live.received, "session/set_config_option"), 0);
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(&id, InstructAction::Finish, None).await;
    let (end, events, received) = live.finish().await;
    assert_eq!(count(&received, "session/prompt"), 1);
    assert!(end.disk_full.is_some());
    assert!(matches!(
        end.input_note,
        Some(agentd::direct::summary::InputNote::ChoseFinish)
    ));
    // The record of the question says the same word as the event.
    assert_eq!(end.interventions.len(), 1);
    assert_eq!(end.interventions[0].reason, "disk_full");
    assert_eq!(end.interventions[0].outcome, "finish");
    let closed: Vec<&Value> = events
        .iter()
        .filter(|v| v["kind"] == "input-closed")
        .collect();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["how"], "finish");
}

/// A run of another engine whose session disk is full is asked about the engine that ran, in the question of the card and
/// at the end of the notice.
#[tokio::test(start_paused = true)]
async fn b885_a_full_disk_asks_a_run_of_another_engine_about_that_engine() {
    let disk = Scripted::new(vec![ample(), full()]);
    // The engine's own text names the disk, so the stop does not hang on how an engine's bare error is classed.
    let mut s = Setup::new(
        vec![
            vec![Step::ErrorWith(
                -32603,
                "Internal error: No space left on device",
                Value::Null,
            )],
            end_turn("never"),
        ],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    s.failover = Some(three_routes());
    s.policy = policy(1800);
    s.engine = "opencode";
    let mut live = start(s);
    let q = live.needs_input(1).await;
    assert_eq!(q["reason"], "disk_full");
    let question = q["question"].as_str().unwrap().to_string();
    assert_eq!(
        question,
        "Send a message with `ham say` that asks OpenCode to delete build output or temporary files (OpenCode may not be able to answer while the disk is full), or end the run with `ham finish` and keep what exists."
    );
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(&id, InstructAction::Finish, None).await;
    let (_end, events, _received) = live.finish().await;
    let notes = notices(&events, "disk_full");
    assert_eq!(notes.len(), 1, "{events:?}");
    let detail = notes[0]["detail"].as_str().unwrap();
    assert!(detail.ends_with(&question), "{detail}");
    assert!(!detail.contains("Cline"), "{detail}");
}

/// The title of a question names the engine that ran. The client shows the title as the head line of the question's card and
/// in the line of `ham watch`.
#[tokio::test(start_paused = true)]
async fn b885_the_title_of_a_question_names_the_engine_that_ran() {
    for (engine, name) in [("opencode", "OpenCode"), ("cline", "Cline")] {
        // The engine ends its turn with no message: a question whose title names the engine.
        let mut s = Setup::new(
            vec![vec![Step::Finish("end_turn")]],
            RetryPolicy::new(3, Duration::from_secs(1)),
        );
        s.policy = policy(1800);
        s.engine = engine;
        let mut live = start(s);
        let q = live.needs_input(1).await;
        assert_eq!(q["reason"], "no_message", "{engine}");
        assert_eq!(q["title"], format!("{name} sent an empty message"));
        let text = q["text"].as_str().unwrap();
        assert!(
            text.starts_with(&format!("Needs your input: {name} sent an empty message")),
            "{text}"
        );
        let id = q["requestId"].as_str().unwrap().to_string();
        live.answer(&id, InstructAction::Finish, None).await;
        live.finish().await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_message_after_space_was_freed_goes_on_and_the_run_is_no_longer_the_disk() {
    // Full at the first prompt; after the person's message the disk has room (Cline deleted the build output).
    let disk = Scripted::new(vec![ample(), full(), ample()]);
    let mut s = Setup::new(
        vec![internal_error(), end_turn("Deleted target/, done.")],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    s.policy = policy(1800);
    let mut live = start(s);
    let q = live.needs_input(1).await;
    assert_eq!(q["reason"], "disk_full");
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(
        &id,
        InstructAction::ContinueWithPrompt,
        Some("delete the build output and go on"),
    )
    .await;
    let (end, _, received) = live.finish().await;
    assert_eq!(count(&received, "session/prompt"), 2);
    let sent = received.lock().unwrap().clone();
    let prompts: Vec<&Value> = sent
        .iter()
        .filter(|m| m["method"] == "session/prompt")
        .collect();
    assert_eq!(
        prompts[1]["params"]["prompt"][0]["text"],
        "delete the build output and go on"
    );
    assert!(
        end.disk_full.is_none(),
        "the last stage did not end on the disk"
    );
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(end.interventions[0].reason, "disk_full");
}

#[tokio::test(start_paused = true)]
async fn nobody_answering_ends_the_run_on_the_disk_without_another_prompt() {
    let disk = Scripted::new(vec![ample(), full()]);
    let mut s = Setup::new(
        vec![internal_error(), end_turn("never")],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    s.policy = policy(1800);
    let (end, _, received) = start(s).finish().await;
    assert_eq!(count(&received, "session/prompt"), 1);
    assert!(end.disk_full.is_some());
    assert!(matches!(
        end.input_note,
        Some(agentd::direct::summary::InputNote::NobodyAnswered { .. })
    ));
}

// ---- before it is full ----------------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_early_notice_comes_once_per_crossing_at_the_prompt_boundaries() {
    // A provider fault twice, then success: three prompts, three boundaries. The workspace is past the early mark but short of the stop
    // mark, comes back under, and crosses again.
    let disk = Scripted::new(vec![
        nearly(900), // boundary 1: crossing, notice 1
        nearly(900), // after the error: not full
        nearly(910), // boundary 2: still crossed, no notice
        nearly(910), // after the error
        nearly(100), // boundary 3: recovered
    ]);
    let s = Setup::new(
        vec![provider_503(), provider_503(), end_turn("Done.")],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    let (end, events, received) = start(s).finish().await;
    assert_eq!(count(&received, "session/prompt"), 3);
    assert!(end.disk_full.is_none());
    let low = notices(&events, "disk_low");
    assert_eq!(low.len(), 1, "{events:?}");
    let detail = low[0]["detail"].as_str().unwrap();
    assert!(
        detail.contains("workspace") && detail.contains("900"),
        "{detail}"
    );
    assert!(detail.contains("Nothing is deleted"), "{detail}");
}

#[tokio::test(start_paused = true)]
async fn the_early_notice_comes_again_after_a_recovery() {
    let disk = Scripted::new(vec![
        nearly(900), // boundary 1: notice 1
        nearly(900),
        nearly(100), // boundary 2: recovered
        nearly(100),
        nearly(920), // boundary 3: crossed again, notice 2
    ]);
    let s = Setup::new(
        vec![provider_503(), provider_503(), end_turn("Done.")],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    let (_, events, _) = start(s).finish().await;
    assert_eq!(notices(&events, "disk_low").len(), 2, "{events:?}");
}

#[tokio::test(start_paused = true)]
async fn a_run_with_room_raises_no_notice_and_a_run_without_a_check_measures_nothing() {
    let disk = Scripted::new(vec![ample()]);
    let s = Setup::new(
        vec![provider_503(), end_turn("Done.")],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    let (_, events, _) = start(s).finish().await;
    assert!(notices(&events, "disk_low").is_empty());
    assert!(notices(&events, "disk_full").is_empty());
    let s = Setup::new(
        vec![provider_503(), end_turn("Done.")],
        RetryPolicy::new(3, Duration::from_secs(1)),
    );
    let (_, events, _) = start(s).finish().await;
    assert!(notices(&events, "disk_low").is_empty());
}

// ---- the words and the rules, without the driver --------------------------------------------------------------------------

fn sample_full() -> DiskFull {
    Limits::default().full(&full()).unwrap()
}

fn disk_classified() -> Classified {
    with_disk(
        Classified {
            outcome: DirectOutcome::StoppedProviderError,
            turn_outcome: TurnOutcome::Failed,
            detail: Some("engine error -32603: Internal error".into()),
            fault_hint: None,
        },
        Some(&sample_full()),
    )
}

#[test]
fn only_a_provider_error_end_is_read_as_the_disk() {
    let cause = sample_full();
    let c = disk_classified();
    assert_eq!(c.outcome, DirectOutcome::StoppedOther);
    assert_eq!(c.turn_outcome, TurnOutcome::Failed);
    assert_eq!(c.detail.as_deref(), Some(cause.detail().as_str()));
    // Any other end passes through, and so does a provider error with no disk cause.
    for outcome in [
        DirectOutcome::ReportedDone,
        DirectOutcome::StoppedByUser,
        DirectOutcome::StoppedTimeout,
        DirectOutcome::StoppedEngineDied,
        DirectOutcome::StoppedSilence,
        DirectOutcome::StoppedEngineCancelled,
    ] {
        let same = Classified {
            outcome,
            turn_outcome: TurnOutcome::Completed,
            detail: Some("x".into()),
            fault_hint: None,
        };
        assert_eq!(with_disk(same.clone(), Some(&cause)), same, "{outcome:?}");
    }
    let provider = Classified {
        outcome: DirectOutcome::StoppedProviderError,
        turn_outcome: TurnOutcome::Failed,
        detail: Some("engine error -32603: Internal error".into()),
        fault_hint: None,
    };
    assert_eq!(with_disk(provider.clone(), None), provider);
}

fn facts_with_disk() -> AssessFacts {
    AssessFacts {
        client_alive: true,
        prompt_answered: true,
        attempts: 1,
        tool_calls: 41,
        files_changed: Some(3),
        disk_full: Some(sample_full()),
        run_s: 412,
        ..AssessFacts::default()
    }
}

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

#[test]
fn a_full_disk_offers_a_message_and_the_end_and_nothing_that_would_fail_the_same_way() {
    let routes = vec![harness_protocol::direct::NeedsInputModel {
        id: MODEL_B.into(),
        name: "Claude Opus 5.5".into(),
        context_tokens: 200_000,
    }];
    let facts = AssessFacts {
        routes,
        ..facts_with_disk()
    };
    let st = agentd::direct::input::RunInputState::default();
    let a = assess(
        &disk_classified(),
        &facts,
        &st,
        &InputPolicy::new(secs(1800)),
        secs(27_000),
    );
    let Assessment::Ask(ask) = a else {
        panic!("a full disk asks: {a:?}")
    };
    // Another session or another model would meet the same disk.
    assert_eq!(
        ask.options,
        [InstructAction::ContinueWithPrompt, InstructAction::Finish]
    );
    assert!(ask.models.is_empty());
    assert!(ask.can_continue);
}

#[test]
fn a_full_disk_with_a_dead_engine_can_only_end() {
    let facts = AssessFacts {
        client_alive: false,
        prompt_answered: false,
        ..facts_with_disk()
    };
    let st = agentd::direct::input::RunInputState::default();
    let a = assess(
        &disk_classified(),
        &facts,
        &st,
        &InputPolicy::new(secs(1800)),
        secs(27_000),
    );
    let Assessment::Ask(ask) = a else {
        panic!("{a:?}")
    };
    assert_eq!(ask.options, [InstructAction::Finish]);
    assert!(!ask.can_continue);
}

#[test]
fn with_no_answer_window_a_full_disk_ends_the_run() {
    let st = agentd::direct::input::RunInputState::default();
    let a = assess(
        &disk_classified(),
        &facts_with_disk(),
        &st,
        &InputPolicy::new(Duration::ZERO),
        secs(27_000),
    );
    assert_eq!(a, Assessment::Finish);
}

#[test]
fn the_users_own_stop_still_wins_over_a_full_disk() {
    let st = agentd::direct::input::RunInputState::default();
    let stopped = Classified {
        outcome: DirectOutcome::StoppedByUser,
        turn_outcome: TurnOutcome::Cancelled,
        detail: None,
        fault_hint: None,
    };
    assert_eq!(
        assess(
            &stopped,
            &facts_with_disk(),
            &st,
            &InputPolicy::new(secs(1800)),
            secs(27_000)
        ),
        Assessment::Finish
    );
}

#[test]
fn one_word_names_the_reason_in_the_event_the_record_and_the_log() {
    use harness_protocol::direct::NeedsInputReason as R;
    let cause = sample_full();
    assert_eq!(reason_word(R::ProviderError, Some(&cause)), "disk_full");
    assert_eq!(reason_word(R::ProviderError, None), "provider_error");
    assert_eq!(reason_word(R::CapHit, None), "cap_hit");
}

// ---- the engine's own error text names the disk -----------------------------------------------
//
// The managed session storage turns read-only once it is full: the engine's -32603 answers on every route carried "Read-only file system
// (os error 30)", and not only "No space left on device". The text stops the call by itself, also when both numbers show room; the
// measurement stays the second signal and its numbers are in the words.

/// The engine answers a -32603 with `message` and `data` while the disk shows room: the call stops at once on the text.
async fn the_text_stops_the_call(
    message: &'static str,
    data: Value,
    signal: Signal,
    cause: &str,
    title: &str,
) {
    let disk = Scripted::new(vec![ample()]);
    let mut s = Setup::new(
        vec![
            vec![Step::ErrorWith(-32603, message, data.clone())],
            end_turn("never sent"),
        ],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    s.failover = Some(three_routes());
    let (end, events, received) = start(s).finish().await;
    // One prompt reached the engine: no resume, no switch of route.
    assert_eq!(count(&received, "session/prompt"), 1, "{message}");
    assert_eq!(
        count(&received, "session/set_config_option"),
        0,
        "{message}"
    );
    let found = end.disk_full.as_ref().expect("the text names the disk");
    assert_eq!(found.signal, Some(signal));
    assert!(
        !found.root_short && !found.workspace_short,
        "the numbers show room"
    );
    assert_eq!(found.cause(), cause);
    // The measured numbers are in the words, with the room they show.
    assert_eq!(found.workspace_used_mb, Some(40));
    assert_eq!(found.root_free_mb, Some(5700));
    let named = with_disk(classify(&end.raw), end.disk_full.as_ref());
    assert_eq!(named.outcome, DirectOutcome::StoppedOther);
    let label = DirectOutcome::StoppedOther.label_for(named.detail.as_deref());
    // A text that says no space while both numbers show room is not called a full disk: the label says what the engine
    // reported and how much the files add up to against the storage's own size.
    let with_room = signal == Signal::NoSpace;
    if with_room {
        assert_eq!(
            label,
            "Cline stopped: the harness ended the run (the engine reported no space left on device, but the files add up to 40 MiB of 1024 MiB, root 5700 MiB free)"
        );
    } else {
        assert!(
            label.starts_with(&format!(
                "Cline stopped: the harness ended the run ({cause}, workspace 40 MiB"
            )),
            "{label}"
        );
    }
    // One notice names the cause and what the person can do.
    let notes = notices(&events, "disk_full");
    assert_eq!(notes.len(), 1, "{events:?}");
    let detail = notes[0]["detail"].as_str().unwrap();
    assert!(detail.starts_with(title), "{detail}");
    assert!(
        detail.contains("ham say") && detail.contains("ham finish"),
        "{detail}"
    );
    if with_room {
        assert!(!detail.contains("The engine's error says"), "{detail}");
        assert!(!detail.contains("disk is full"), "{detail}");
        assert!(
            detail.contains("Prompts can fail while the storage refuses writes."),
            "{detail}"
        );
    } else {
        assert!(
            detail.contains(&format!("The engine's error says: {}.", signal.word())),
            "{detail}"
        );
    }
}

/// The title and the cause of a text that says no space while the numbers show room.
const NO_SPACE_TITLE: &str = "The engine reported no space left on device";
const READ_ONLY_TITLE: &str = "The session disk is read-only or full";
const NO_SPACE: &str = "the engine reported no space left on device";
const READ_ONLY: &str = "the session disk is read-only or full";

#[tokio::test(start_paused = true)]
async fn the_text_read_only_file_system_stops_the_call_and_names_both_causes() {
    the_text_stops_the_call(
        "Internal error: Read-only file system (os error 30)",
        Value::Null,
        Signal::ReadOnly,
        READ_ONLY,
        READ_ONLY_TITLE,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn the_text_os_error_30_in_the_data_stops_the_call() {
    the_text_stops_the_call(
        "Internal error",
        json!({"detail": "failed to create directory: os error 30"}),
        Signal::ReadOnly,
        READ_ONLY,
        READ_ONLY_TITLE,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn the_text_erofs_stops_the_call() {
    the_text_stops_the_call(
        "Internal error: EROFS: read-only file system, open '/mnt/workspace/run/direct/repo/x'",
        Value::Null,
        Signal::ReadOnly,
        READ_ONLY,
        READ_ONLY_TITLE,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn the_text_no_space_left_on_device_stops_the_call_and_names_what_the_engine_reported() {
    the_text_stops_the_call(
        "Internal error: No space left on device (os error 28)",
        Value::Null,
        Signal::NoSpace,
        NO_SPACE,
        NO_SPACE_TITLE,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn the_text_os_error_28_in_the_data_stops_the_call() {
    the_text_stops_the_call(
        "Internal error",
        json!({"cause": "write failed: os error 28"}),
        Signal::NoSpace,
        NO_SPACE,
        NO_SPACE_TITLE,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn the_text_enospc_stops_the_call() {
    the_text_stops_the_call(
        "Internal error: ENOSPC: no space left, write",
        Value::Null,
        Signal::NoSpace,
        NO_SPACE,
        NO_SPACE_TITLE,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn the_text_of_an_error_that_is_not_a_32603_is_not_read() {
    // Another code is another kind of failure: today's path, whatever the text says.
    let disk = Scripted::new(vec![ample()]);
    let mut s = Setup::new(
        vec![vec![Step::ErrorWith(
            -32000,
            "Read-only file system (os error 30)",
            Value::Null,
        )]],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    s.failover = Some(three_routes());
    let (end, events, _) = start(s).finish().await;
    assert!(end.disk_full.is_none());
    assert!(notices(&events, "disk_full").is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_run_without_the_disk_check_does_not_read_the_text() {
    // Both settings at 0 leave no check: the error goes down the ladder as before.
    let s = Setup::new(
        vec![
            vec![Step::ErrorWith(
                -32603,
                "Internal error: Read-only file system (os error 30)",
                Value::Null,
            )],
            end_turn("Done."),
        ],
        RetryPolicy::new(3, Duration::from_secs(1)),
    );
    let (end, _, received) = start(s).finish().await;
    assert!(end.disk_full.is_none());
    assert_eq!(count(&received, "session/prompt"), 2, "resumed as before");
}

#[tokio::test(start_paused = true)]
async fn the_question_for_a_read_only_disk_says_read_only_or_full_and_offers_a_message_or_the_end()
{
    let disk = Scripted::new(vec![ample()]);
    let mut s = Setup::new(
        vec![
            vec![Step::ErrorWith(
                -32603,
                "Internal error: Read-only file system (os error 30)",
                Value::Null,
            )],
            end_turn("never"),
        ],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    s.policy = policy(1800);
    let mut live = start(s);
    let q = live.needs_input(1).await;
    assert_eq!(q["reason"], "disk_full");
    assert_eq!(q["title"], READ_ONLY_TITLE);
    assert_eq!(
        q["text"],
        "Needs your input: The session disk is read-only or full (ham say or ham finish)"
    );
    assert_eq!(q["options"], json!(["continue_with_prompt", "finish"]));
    let last = q["evidence"]["lastError"].as_str().unwrap();
    assert!(last.starts_with(READ_ONLY), "{last}");
    let id = q["requestId"].as_str().unwrap().to_string();
    live.answer(&id, InstructAction::Finish, None).await;
    let (end, _, received) = live.finish().await;
    assert_eq!(count(&received, "session/prompt"), 1);
    assert!(end.disk_full.is_some());
}

// ---- a refused test write is the disk, whatever the free space shows ------------------------------------------------

fn refuses_writes() -> Reading {
    Reading {
        root_free_mb: Some(744),
        workspace_used_mb: Some(59),
        workspace_partial: false,
        writable: Some(false),
    }
}

/// A probe whose report is a fixed text, to see when the log gets it.
#[derive(Debug)]
struct Reporting {
    inner: Arc<Scripted>,
    reports: AtomicU32,
}

impl DiskProbe for Reporting {
    fn measure(&self) -> BoxFuture<'_, Reading> {
        self.inner.measure()
    }

    fn diagnose(&self) -> BoxFuture<'_, String> {
        Box::pin(async move {
            self.reports.fetch_add(1, Ordering::SeqCst);
            "write test /var/lib/agentd/run1/direct: failed: Read-only file system (os error 30)"
                .to_string()
        })
    }
}

#[tokio::test(start_paused = true)]
async fn a_32603_with_a_refused_test_write_stops_at_once_though_the_free_space_is_above_the_mark() {
    let disk = Scripted::new(vec![ample(), refuses_writes()]);
    let mut s = Setup::new(
        vec![internal_error(), end_turn("never sent")],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    s.failover = Some(three_routes());
    let (end, events, received) = start(s).finish().await;
    // One prompt reached the engine: no resume, and no walk over the routes.
    assert_eq!(count(&received, "session/prompt"), 1);
    assert_eq!(count(&received, "session/set_config_option"), 0);
    let cause = end.disk_full.as_ref().expect("the stop names the disk");
    assert!(cause.unwritable, "{cause:?}");
    assert!(!cause.root_short && !cause.workspace_short && cause.signal.is_none());
    let named = with_disk(classify(&end.raw), end.disk_full.as_ref());
    assert_eq!(named.outcome, DirectOutcome::StoppedOther);
    assert_eq!(
        DirectOutcome::StoppedOther.label_for(named.detail.as_deref()),
        "Cline stopped: the harness ended the run (the session disk is read-only or full, workspace 59 MiB against a stop mark of 960 MiB, root 744 MiB free)"
    );
    // The one notice says that a test write failed, and the question is the disk's.
    let notes = notices(&events, "disk_full");
    assert_eq!(notes.len(), 1, "{events:?}");
    let detail = notes[0]["detail"].as_str().unwrap();
    assert!(detail.contains("A test write"), "{detail}");
    assert!(detail.contains("ham finish"), "{detail}");
    assert_eq!(disk.calls(), 2);
}

#[tokio::test(start_paused = true)]
async fn the_first_bare_32603_puts_the_machines_report_in_the_log_once() {
    let disk = Scripted::new(vec![ample()]);
    let probe = Arc::new(Reporting {
        inner: disk,
        reports: AtomicU32::new(0),
    });
    let log = agentd::logging::MemoryLog::new();
    let mut s = Setup::new(
        vec![internal_error(), internal_error(), end_turn("Done.")],
        RetryPolicy::new(3, Duration::from_secs(1))
            .with_log(log.clone())
            .with_disk(DiskCheck::new(probe.clone(), Limits::default())),
    );
    s.failover = Some(three_routes());
    let _ = start(s).finish().await;
    // Two bare errors reached the check; the report is asked for once and logged once.
    assert_eq!(probe.reports.load(Ordering::SeqCst), 1);
    let lines = log.lines();
    let found: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("names no cause; the machine reports"))
        .collect();
    assert_eq!(found.len(), 1, "{lines:?}");
    assert!(found[0].contains("Read-only file system"), "{}", found[0]);
}

#[tokio::test(start_paused = true)]
async fn an_error_that_names_the_disk_does_not_need_the_machines_report() {
    let disk = Scripted::new(vec![ample(), full()]);
    let probe = Arc::new(Reporting {
        inner: disk,
        reports: AtomicU32::new(0),
    });
    let mut s = Setup::new(
        vec![vec![Step::ErrorWith(
            -32603,
            "Internal error: Read-only file system (os error 30)",
            Value::Null,
        )]],
        RetryPolicy::new(3, Duration::from_secs(1))
            .with_disk(DiskCheck::new(probe.clone(), Limits::default())),
    );
    s.failover = Some(three_routes());
    let (end, _, _) = start(s).finish().await;
    assert!(end.disk_full.is_some());
    assert_eq!(probe.reports.load(Ordering::SeqCst), 0);
}

// ----: the session's storage refuses writes while its files leave room -------------------------------------------------------
//
// The storage was seen to answer "No space left on device" with files of 16 percent of its size. The worker makes a test write of its
// own at each prompt boundary and beside the fresh measurement after a -32603: a refused write is announced once per stretch, and a
// stop whose numbers show room names the refusal and not a full disk.

/// A gate a held write waits at until the test opens it.
type Gate = Arc<(Mutex<bool>, std::sync::Condvar)>;

/// Writes that answer from a list (then are taken) and count the test writes.
#[derive(Debug, Default)]
struct StorageScript {
    answers: Mutex<VecDeque<Option<i32>>>,
    held: Mutex<Option<Gate>>,
    tests: AtomicU32,
}

impl StorageScript {
    /// `Some(28)` refuses a write with that errno; `None` takes it.
    fn new(answers: Vec<Option<i32>>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into()),
            ..Self::default()
        })
    }

    /// Every test write waits until the gate opens.
    fn held(gate: Gate) -> Arc<Self> {
        Arc::new(Self {
            held: Mutex::new(Some(gate)),
            ..Self::default()
        })
    }

    fn tests(&self) -> u32 {
        self.tests.load(Ordering::SeqCst)
    }
}

impl StorageWrites for StorageScript {
    fn test(&self, _run_dir: &std::path::Path) -> Result<(), WriteError> {
        self.tests.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = self.held.lock().unwrap().clone() {
            let (open, cv) = &*gate;
            let mut open = open.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
        }
        match self.answers.lock().unwrap().pop_front().flatten() {
            None => Ok(()),
            Some(errno) => Err(WriteError {
                step: "write",
                errno: Some(errno),
                text: std::io::Error::from_raw_os_error(errno).to_string(),
                test_switch: false,
            }),
        }
    }

    fn marker(&self, _run_dir: &std::path::Path, _bytes: &[u8]) -> Result<(), WriteError> {
        Ok(())
    }
}

/// The disk rule's own test write failed too, and both numbers show room.
fn room_unwritable() -> Reading {
    Reading {
        writable: Some(false),
        ..ample()
    }
}

#[tokio::test(start_paused = true)]
async fn b881_prompt_boundaries_inside_a_refusal_announce_it() {
    let disk = Scripted::new(vec![ample()]);
    // Four prompts: three end in a provider fault that is retried, the fourth ends the turn. Each prompt boundary makes one test write,
    // and so does each fresh measurement after an error. At the boundaries the storage answers refused, refused, taken, refused.
    let storage = StorageScript::new(vec![
        Some(28), // boundary 1: stretch 1 opens
        Some(28), // error 1
        Some(28), // boundary 2
        Some(28), // error 2
        None,     // boundary 3: the stretch closes
        None,     // error 3
        Some(28), // boundary 4: stretch 2 opens
    ]);
    let mut s = Setup::new(
        vec![
            provider_503(),
            provider_503(),
            provider_503(),
            end_turn("done"),
        ],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    s.storage = Some((storage.clone(), disk.clone()));
    let (end, events, received) = start(s).finish().await;
    assert_eq!(count(&received, "session/prompt"), 4);
    assert!(
        end.disk_full.is_none(),
        "a provider fault with room is no disk stop"
    );
    assert_eq!(storage.tests(), 7);
    let notes = notices(&events, "storage_refused");
    assert_eq!(notes.len(), 2, "{events:?}");
    assert_eq!(notes[0]["storage"]["stretch"], 1);
    assert_eq!(notes[0]["storage"]["seen"], "prompt");
    assert_eq!(notes[0]["storage"]["usedMb"], 40);
    assert!(
        notes[0]["detail"].as_str().unwrap().starts_with(
            "The session's storage refused a write while its files add up to 40 MiB of 1024 MiB (no space left on device)."
        ),
        "{}",
        notes[0]["detail"]
    );
    assert_eq!(notes[1]["storage"]["stretch"], 2);
    assert!(
        notes[1]["detail"]
            .as_str()
            .unwrap()
            .starts_with("The session's storage refused a write again (no space left on device); ham has seen this 2 times in this session."),
        "{}",
        notes[1]["detail"]
    );
    // The engine was told nothing: every prompt after the first is the fixed resume sentence.
    let prompts: Vec<String> = received
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m["method"] == "session/prompt")
        .map(|m| m["params"]["prompt"].to_string())
        .collect();
    for p in &prompts {
        assert!(
            !p.to_lowercase().contains("storage") && !p.to_lowercase().contains("no space"),
            "{p}"
        );
    }
    // No disk notice: the numbers show room.
    assert!(notices(&events, "disk_full").is_empty() && notices(&events, "disk_low").is_empty());
}

#[tokio::test(start_paused = true)]
async fn b881_a_32603_that_says_no_space_with_room_stops_with_the_new_words() {
    for own_write_refused in [false, true] {
        let disk = Scripted::new(vec![ample()]);
        // The boundary's write is taken; the write beside the fresh measurement is taken or refused.
        let storage = StorageScript::new(vec![None, own_write_refused.then_some(28)]);
        let mut s = Setup::new(
            vec![
                vec![Step::ErrorWith(
                    -32603,
                    "Internal error: No space left on device (os error 28)",
                    Value::Null,
                )],
                end_turn("never sent"),
            ],
            RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
        );
        s.failover = Some(three_routes());
        s.storage = Some((storage.clone(), disk.clone()));
        let (end, events, received) = start(s).finish().await;
        // The stop decision is the one it was: one prompt, no resume, no switch of route.
        assert_eq!(count(&received, "session/prompt"), 1);
        assert_eq!(count(&received, "session/set_config_option"), 0);
        let found = end.disk_full.as_ref().expect("the text stops the call");
        assert_eq!(found.storage_refused, own_write_refused);
        let line = "the engine reported no space left on device, but the files add up to 40 MiB of 1024 MiB, root 5700 MiB free";
        assert_eq!(found.line(), line);
        let named = with_disk(classify(&end.raw), end.disk_full.as_ref());
        assert_eq!(named.outcome, DirectOutcome::StoppedOther);
        assert_eq!(
            DirectOutcome::StoppedOther.label_for(named.detail.as_deref()),
            format!("Cline stopped: the harness ended the run ({line})")
        );
        let notes = notices(&events, "disk_full");
        assert_eq!(notes.len(), 1, "{events:?}");
        let own = if own_write_refused {
            "ham's own write to the session's storage was refused too. "
        } else {
            ""
        };
        assert_eq!(
            notes[0]["detail"],
            format!(
                "The engine reported no space left on device, but the files add up to 40 MiB of 1024 MiB, root 5700 MiB free. Prompts can fail while the storage refuses writes. {own}Send a message with `ham say` to go on once the storage takes writes again (Cline may not be able to answer while it refuses them), or end the run with `ham finish` and keep what exists."
            )
        );
        // The storage notice follows the worker's own write, not the engine's text.
        let storage_notes = notices(&events, "storage_refused");
        assert_eq!(
            storage_notes.len(),
            usize::from(own_write_refused),
            "{events:?}"
        );
        if own_write_refused {
            assert_eq!(storage_notes[0]["storage"]["seen"], "error");
        }
        assert_eq!(storage.tests(), 2);
    }
}

#[tokio::test(start_paused = true)]
async fn b881_a_bare_32603_during_a_refusal_stops_with_the_refusal_words_and_one_storage_notice() {
    let disk = Scripted::new(vec![room_unwritable()]);
    // The boundary's write is refused (the notice), and so is the write beside the fresh measurement (nothing new).
    let storage = StorageScript::new(vec![Some(28), Some(28)]);
    let mut s = Setup::new(
        vec![internal_error(), end_turn("never sent")],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    s.failover = Some(three_routes());
    s.storage = Some((storage.clone(), disk.clone()));
    let (end, events, received) = start(s).finish().await;
    assert_eq!(count(&received, "session/prompt"), 1);
    let found = end
        .disk_full
        .as_ref()
        .expect("the failed test write stops the call as it did");
    assert!(found.storage_refused && found.unwritable);
    let line = "the session's storage refused a write while its files add up to 40 MiB of 1024 MiB, root 5700 MiB free";
    assert_eq!(found.line(), line);
    assert_eq!(found.title(), "The session's storage refused a write");
    let named = with_disk(classify(&end.raw), end.disk_full.as_ref());
    let detail = named.detail.clone().unwrap();
    assert_eq!(
        DirectOutcome::StoppedOther.label_for(Some(&detail)),
        format!("Cline stopped: the harness ended the run ({line})")
    );
    for wrong in ["read-only or full", "until space is freed", "disk is full"] {
        assert!(!detail.contains(wrong), "{detail}");
    }
    let stop = notices(&events, "disk_full");
    assert_eq!(stop.len(), 1, "{events:?}");
    let text = stop[0]["detail"].as_str().unwrap();
    assert!(
        text.starts_with("The session's storage refused a write while its files add up to 40 MiB of 1024 MiB, root 5700 MiB free. Prompts can fail while the storage refuses writes. Send a message with `ham say` to go on once the storage takes writes again (Cline may not be able to answer"),
        "{text}"
    );
    assert!(
        !text.contains("read-only or full") && !text.contains("until space is freed"),
        "{text}"
    );
    // One storage notice for the one stretch, however many writes were refused in it.
    let storage_notes = notices(&events, "storage_refused");
    assert_eq!(storage_notes.len(), 1, "{events:?}");
    assert_eq!(storage_notes[0]["storage"]["seen"], "prompt");
    assert_eq!(storage.tests(), 2);
    assert_eq!(
        found.question(),
        agentd::engine::direct_words::storage_refused_question("cline")
    );
}

#[tokio::test(start_paused = true)]
async fn b881_a_bare_32603_with_room_and_working_writes_is_a_provider_fault_as_before() {
    // The worker's own write is taken and the disk rule finds nothing: the error is retried like any provider fault.
    let disk = Scripted::new(vec![ample()]);
    let storage = StorageScript::new(Vec::new());
    let mut s = Setup::new(
        vec![internal_error(), end_turn("done")],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    s.storage = Some((storage.clone(), disk.clone()));
    let (end, events, received) = start(s).finish().await;
    assert_eq!(count(&received, "session/prompt"), 2);
    assert!(end.disk_full.is_none());
    assert!(notices(&events, "storage_refused").is_empty());
    assert!(notices(&events, "disk_full").is_empty());
}

#[tokio::test]
async fn b881_a_prompt_is_sent_and_answered_while_the_test_write_is_still_out() {
    // The storage does not answer the boundary's test write. The prompt is sent, answered and the run ends all the same: no prompt
    // waits for the storage. The clock is the real one: a paused clock does not move while a write is out on its thread, and the
    // driver's own timers would then stand still, which is the test's doing and not the product's.
    let gate: Gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let disk = Scripted::new(vec![ample()]);
    let storage = StorageScript::held(gate.clone());
    let mut s = Setup::new(
        vec![end_turn("done")],
        RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
    );
    s.storage = Some((storage.clone(), disk.clone()));
    s.storage_wait = false;
    let done = tokio::time::timeout(Duration::from_secs(60), start(s).finish()).await;
    let Ok((end, events, received)) = done else {
        let (open, cv) = &*gate;
        *open.lock().unwrap() = true;
        cv.notify_all();
        panic!("the run did not end while the storage's test write was out");
    };
    assert_eq!(count(&received, "session/prompt"), 1);
    assert!(end.disk_full.is_none());
    for _ in 0..5000 {
        if storage.tests() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(storage.tests(), 1, "the write started and is still out");
    assert!(notices(&events, "storage_refused").is_empty());
    let (open, cv) = &*gate;
    *open.lock().unwrap() = true;
    cv.notify_all();
}

// ----: what the watch costs the work in front of it (the brief's section 7; measurements, run by hand) ------------------------

/// The real writes, counted.
#[derive(Debug, Default)]
struct CountedHost {
    tests: AtomicU32,
}

impl StorageWrites for CountedHost {
    fn test(&self, run_dir: &std::path::Path) -> Result<(), WriteError> {
        self.tests.fetch_add(1, Ordering::SeqCst);
        agentd::direct::storage_watch::HostWrites.test(run_dir)
    }

    fn marker(&self, run_dir: &std::path::Path, bytes: &[u8]) -> Result<(), WriteError> {
        agentd::direct::storage_watch::HostWrites.marker(run_dir, bytes)
    }
}

/// One prompt with `n` tool calls whose results are `text`, then the end of the turn.
fn tool_results(n: usize, text: &str) -> Vec<Step> {
    let mut steps = Vec::new();
    for i in 0..n {
        let id = format!("call-{i}");
        steps.push(Step::Update(json!({
            "sessionUpdate": "tool_call", "toolCallId": id, "title": "run_commands: make", "kind": "execute",
            "status": "pending", "rawInput": {"commands": ["make"]}
        })));
        steps.push(Step::Update(json!({
            "sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed",
            "rawOutput": Value::String(json!([{"query": "make", "result": text, "success": true}]).to_string())
        })));
    }
    steps.push(chunk("done"));
    steps.push(Step::Finish("end_turn"));
    steps
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

/// Measurement 1 of the brief's section 7, as far as the driver alone shows it: a turn of 300 tool results that each name no space left
/// against the same turn without the words, ten runs of each in turn, with the real test write in a folder of the build host. Arm A
/// is run twice so that the noise (A against A) stands beside the difference (B against A). Prints its numbers; asserts only that the
/// watch never made more test writes than it was asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a measurement: run with --run-ignored only --no-capture"]
async fn b881_cost_of_300_tool_results_that_name_no_space() {
    const N: usize = 300;
    let quiet = "make: Nothing to be done for 'all'.";
    let named = "cp: error writing 'out.bin': No space left on device";
    let mut a1 = Vec::new();
    let mut a2 = Vec::new();
    let mut b = Vec::new();
    let mut writes_b = Vec::new();
    let mut writes_a = Vec::new();
    for round in 0..10 {
        for arm in ["a1", "b", "a2"] {
            let disk = Scripted::new(vec![ample()]);
            let host = Arc::new(CountedHost::default());
            let text = if arm == "b" { named } else { quiet };
            let mut s = Setup::new(
                vec![tool_results(N, text)],
                RetryPolicy::new(3, Duration::from_secs(1)).with_disk(check(&disk)),
            );
            s.storage = Some((host.clone(), disk.clone()));
            let began = std::time::Instant::now();
            let (end, events, received) = start(s).finish().await;
            let took = began.elapsed();
            assert_eq!(count(&received, "session/prompt"), 1);
            assert!(end.disk_full.is_none());
            assert!(
                notices(&events, "storage_refused").is_empty(),
                "round {round}"
            );
            let n = host.tests.load(Ordering::SeqCst);
            match arm {
                "a1" => {
                    a1.push(took);
                    writes_a.push(n);
                }
                "a2" => a2.push(took),
                _ => {
                    b.push(took);
                    writes_b.push(n);
                }
            }
            // One at the prompt boundary, and at most one for each result that named no space.
            assert!(
                n as usize <= 1 + if arm == "b" { N } else { 0 },
                "{arm}: {n} test writes"
            );
        }
    }
    let per = |d: Duration| d.as_secs_f64() * 1e6 / N as f64;
    println!("b881 cost, {N} tool results in one turn, 10 runs of each arm in turn (a1, b, a2):");
    println!(
        "  arm A (no words), first:  median turn {:?} ({:.1} us a result), all {:?}",
        median(a1.clone()),
        per(median(a1.clone())),
        a1
    );
    println!(
        "  arm A (no words), second: median turn {:?} ({:.1} us a result), all {:?}",
        median(a2.clone()),
        per(median(a2.clone())),
        a2
    );
    println!(
        "  arm B (each result names no space): median turn {:?} ({:.1} us a result), all {:?}",
        median(b.clone()),
        per(median(b.clone())),
        b
    );
    println!("  test writes in a turn: arm A {writes_a:?}, arm B {writes_b:?}");
    println!(
        "  B against A: {:+.1} us a result; noise A against A: {:+.1} us a result",
        per(median(b)) - per(median(a1.clone())),
        per(median(a2)) - per(median(a1))
    );
}

/// The test write alone on the build host's own disk (in a session the storage is a network mount: 1.9 to 3.6 ms a write were
/// measured there, median per run), and the two calls the foreground makes: the collector taking a final tool result, with and
/// without the words, and the wake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a measurement: run with --run-ignored only --no-capture"]
async fn b881_cost_of_one_test_write_one_result_and_one_wake() {
    use agentd::acp::AcpHandler;
    let dir = common::scratch("b881-cost");
    let mut writes: Vec<Duration> = (0..500)
        .map(|_| {
            let t = std::time::Instant::now();
            agentd::direct::storage_watch::HostWrites
                .test(&dir)
                .unwrap();
            t.elapsed()
        })
        .collect();
    writes.sort();
    println!(
        "b881 cost, one test write on the build host (500 runs): median {:?}, p95 {:?}, max {:?}",
        writes[250], writes[475], writes[499]
    );
    // The collector: a final tool result of 256 KiB and of 200 bytes, with the words at its end and without them.
    let rec = EventRecorder::new("run1", "direct");
    let watch = StorageWatch::new(WatchSetup {
        writes: Arc::new(CountedHost::default()),
        probe: Scripted::new(vec![ample()]),
        rec: rec.clone(),
        log: MemoryLog::new(),
        run_id: "run1".to_string(),
        session: 1,
        run_dir: dir.clone(),
        limits: Limits::default(),
        test_refusal: false,
    });
    let task = watch.start();
    for (label, with_watch) in [("with the watch", true), ("without a watch", false)] {
        for (size, bytes) in [("256 KiB", 256 * 1024usize), ("200 bytes", 200usize)] {
            for (words, tail) in [
                ("no words", "ok\n"),
                ("the words", "No space left on device\n"),
            ] {
                let body = format!("{}{tail}", "x".repeat(bytes.saturating_sub(tail.len())));
                let mut c = DirectCollector::new(rec.clone()).with_engine("cline");
                if with_watch {
                    c = c.with_storage_watch(watch.clone());
                }
                let mut times = Vec::new();
                for i in 0..200 {
                    let id = format!("c{i}");
                    let call = json!({"sessionId": "s", "update": {"sessionUpdate": "tool_call", "toolCallId": id, "title": "run_commands: make", "kind": "execute", "status": "pending", "rawInput": {"commands": ["make"]}}});
                    let done = json!({"sessionId": "s", "update": {"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed", "rawOutput": body}});
                    c.on_notification("session/update", &call);
                    let t = std::time::Instant::now();
                    c.on_notification("session/update", &done);
                    times.push(t.elapsed());
                }
                times.sort();
                println!(
                    "b881 cost, the collector takes one final result of {size} ({words}, {label}): median {:?}, p95 {:?}",
                    times[100], times[190]
                );
                rec.drain();
            }
        }
    }
    let mut wakes: Vec<Duration> = (0..10_000)
        .map(|_| {
            let t = std::time::Instant::now();
            watch.wake(agentd::direct::storage_watch::Seen::PromptBoundary);
            t.elapsed()
        })
        .collect();
    wakes.sort();
    println!(
        "b881 cost, one wake (10,000 runs): median {:?}, p95 {:?}, max {:?}",
        wakes[5_000], wakes[9_500], wakes[9_999]
    );
    watch.idle().await;
    println!(
        "b881 cost, test writes the 10,000 wakes and the results started: {}",
        watch.test_writes_started()
    );
    task.stop().await;
}

// ----: the note about a large workspace folder, through the real driver ---------------------------------------------------
//
// The events are read as the service gets them: from the batches a flusher handed to a sink (`EventSink::post_events`), not from
// the recorder's queue. Began with a notice that was only known to have been queued.

const B889_W4_920: &str = "The workspace folder is nearly full: 920 MiB used against a stop mark of 960 MiB, over 90 percent of it. Nothing is deleted.";
const B889_N1_920: &str = "The workspace folder holds 920 MiB. In measurements this storage at times refused writes from 300 MiB up and none up to 200 MiB. The run goes on.";
const B889_N2: &str = "A new run with --compute instances, where the stage has such machines, keeps the workspace folder on the machine's own disk. That machine runs no containers.";
const B889_ENGINE_WORDS: &str = "b889 the engine's first words";

/// What a flusher posted, batch by batch.
#[derive(Default)]
struct Posted {
    batches: Mutex<Vec<harness_protocol::api::internal::EventsBatchRequest>>,
}

impl agentd::events::EventSink for Posted {
    fn post_events<'a>(
        &'a self,
        batch: harness_protocol::api::internal::EventsBatchRequest,
    ) -> BoxFuture<'a, Result<harness_protocol::api::internal::EventsBatchResponse, String>> {
        let n = batch.events.len() as u32;
        self.batches.lock().unwrap().push(batch);
        Box::pin(async move {
            Ok(harness_protocol::api::internal::EventsBatchResponse {
                accepted: n,
                pending: vec![],
            })
        })
    }

    fn heartbeat<'a>(
        &'a self,
        _req: harness_protocol::api::internal::HeartbeatRequest,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

impl Posted {
    /// The posted events in the order they were posted.
    fn envelopes(&self) -> Vec<Envelope> {
        self.batches
            .lock()
            .unwrap()
            .iter()
            .flat_map(|b| b.events.iter().cloned())
            .collect()
    }
}

/// A module that listens to the ladder: the reasons of the `Notify` events it was told.
#[derive(Default)]
struct Listener {
    told: Mutex<Vec<String>>,
}

impl DirectHook for Listener {
    fn name(&self) -> &'static str {
        "listener"
    }

    fn on_event(&self, event: &agentd::direct::hooks::Event<'_>) {
        if let agentd::direct::hooks::Event::Ladder(agentd::direct::hooks::LadderEvent::Notify {
            reason,
            ..
        }) = event
        {
            self.told.lock().unwrap().push((*reason).to_string());
        }
    }
}

struct B889Run {
    end: DriverEnd,
    posted: Vec<Envelope>,
    received: Received,
    log: Vec<String>,
    told: Vec<String>,
}

impl B889Run {
    /// The posted `notify` events in order: reason and detail.
    fn notify(&self) -> Vec<(String, String)> {
        self.posted
            .iter()
            .filter_map(kind_of)
            .filter(|(k, _)| k == "notify")
            .map(|(_, v)| {
                (
                    v["reason"].as_str().unwrap_or_default().to_string(),
                    v["detail"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    fn notes(&self) -> Vec<String> {
        self.notify()
            .into_iter()
            .filter(|(r, _)| r == "disk_note")
            .map(|(_, d)| d)
            .collect()
    }

    fn lines(&self, part: &str) -> Vec<String> {
        self.log
            .iter()
            .filter(|l| l.contains(part))
            .cloned()
            .collect()
    }
}

/// One session of the worker: the real driver, the scripted engine, `disk` under `limits`, a module that listens to the ladder, a log,
/// and a flusher that posts to a sink.
async fn b889_run(prompts: Vec<Vec<Step>>, disk: &Arc<Scripted>, limits: Limits) -> B889Run {
    let log = MemoryLog::new();
    let listener = Arc::new(Listener::default());
    let mut s = Setup::new(
        prompts,
        RetryPolicy::new(3, Duration::from_secs(1))
            .with_disk(DiskCheck::new(disk.clone(), limits))
            .with_log(log.clone()),
    );
    let hook: Arc<dyn DirectHook> = listener.clone();
    s.hooks = vec![hook];
    let mut live = start(s);
    let sink = Arc::new(Posted::default());
    let (cancel, _cancelled) = watch::channel(false);
    let flusher = agentd::events::spawn_flusher(
        live.rec.clone(),
        sink.clone(),
        Duration::from_millis(50),
        Duration::from_secs(60),
        1,
        MemoryLog::new(),
        cancel,
    );
    let end = (&mut live.handle).await.unwrap();
    flusher.stop().await;
    B889Run {
        end,
        posted: sink.envelopes(),
        received: live.received.clone(),
        log: log.lines(),
        told: listener.told.lock().unwrap().clone(),
    }
}

/// A prompt whose answer is `calls` tool calls and then the engine's words.
fn b889_prompt(calls: usize) -> Vec<Step> {
    let mut steps = tool_results(calls, "ok");
    // `tool_results` ends with a chunk and the finish; the engine's own words go in front of them.
    let tail = steps.split_off(steps.len() - 2);
    steps.push(chunk(B889_ENGINE_WORDS));
    steps.extend(tail);
    steps
}

/// T7: a whole session with a folder of 920 MiB. The service is posted the nearly-full notice and the two rows of the note, in
/// that order, before anything of the engine's answer; only the nearly-full notice is a ladder event.
#[tokio::test(start_paused = true)]
async fn b889_a_session_on_a_large_folder_posts_the_note_before_the_engines_answer() {
    let disk = Scripted::new(vec![nearly(920)]);
    let run = b889_run(vec![b889_prompt(2)], &disk, Limits::default()).await;
    assert!(
        disk.calls() >= 1,
        "TRIGGER ABSENT: the probe was not asked for a reading"
    );
    println!(
        "TRIGGER PRESENT: reading 920 MiB above the mark {} MiB",
        agentd::direct::disk::NOTE_ABOVE_MB
    );
    assert_eq!(count(&run.received, "session/prompt"), 1);
    assert!(run.end.disk_full.is_none());
    let notes = run.notes();
    assert_eq!(
        notes.len(),
        2,
        "{} events with reason disk_note, expected 2: {:?}",
        notes.len(),
        run.notify()
    );
    assert_eq!(
        run.notify(),
        vec![
            ("disk_low".to_string(), B889_W4_920.to_string()),
            ("disk_note".to_string(), B889_N1_920.to_string()),
            ("disk_note".to_string(), B889_N2.to_string()),
        ]
    );
    // The three are posted before the first tool call and before the engine's words.
    let is_notify = |e: &Envelope| kind_of(e).is_some_and(|(k, _)| k == "notify");
    let last_notice = run.posted.iter().rposition(is_notify).unwrap();
    let text = |e: &Envelope| serde_json::to_string(e).unwrap();
    let first_engine = run
        .posted
        .iter()
        .position(|e| {
            e.event_type == harness_protocol::envelope::EventType::ToolSummary
                || text(e).contains(B889_ENGINE_WORDS)
                || text(e).contains("run_commands: make")
        })
        .expect("the engine's tool calls are posted too");
    assert!(
        last_notice < first_engine,
        "notice at {last_notice}, the engine's first event at {first_engine}: {:?}",
        run.posted.iter().map(|e| e.event_type).collect::<Vec<_>>()
    );
    // The note is no ladder event: no module hears of it, so none can turn it into a prompt. The nearly-full notice keeps the
    // ladder event it had.
    assert_eq!(run.told, vec!["disk_low".to_string()]);
    // The log: the session's first reading, the crossing, the two rows, in that order.
    let disk_lines = run.lines("direct turn disk ");
    assert_eq!(
        disk_lines,
        vec![
            "direct turn disk reading: workspace 920 MiB used".to_string(),
            format!("direct turn disk low: {B889_W4_920}"),
            format!("direct turn disk note: {B889_N1_920}"),
            format!("direct turn disk note: {B889_N2}"),
        ]
    );
}

/// T6 (the order): with the root volume low as well, its sentence and the nearly-full sentence come first, then the two rows.
#[tokio::test(start_paused = true)]
async fn b889_the_order_is_the_older_notices_and_then_the_two_rows() {
    let disk = Scripted::new(vec![Reading {
        root_free_mb: Some(400),
        workspace_used_mb: Some(920),
        workspace_partial: false,
        writable: None,
    }]);
    let run = b889_run(vec![end_turn("Done.")], &disk, Limits::default()).await;
    assert_eq!(
        run.notify(),
        vec![
            (
                "disk_low".to_string(),
                "The session's root volume is running low: 400 MiB free, below the 512 MiB mark. Nothing is deleted.".to_string()
            ),
            ("disk_low".to_string(), B889_W4_920.to_string()),
            ("disk_note".to_string(), B889_N1_920.to_string()),
            ("disk_note".to_string(), B889_N2.to_string()),
        ]
    );
    // A folder between the two marks says the note alone, with its own size.
    let disk = Scripted::new(vec![nearly(250)]);
    let run = b889_run(vec![end_turn("Done.")], &disk, Limits::default()).await;
    assert_eq!(
        run.notify(),
        vec![
            (
                "disk_note".to_string(),
                "The workspace folder holds 250 MiB. In measurements this storage at times refused writes from 300 MiB up and none up to 200 MiB. The run goes on.".to_string()
            ),
            ("disk_note".to_string(), B889_N2.to_string()),
        ]
    );
    assert!(run.told.is_empty(), "{:?}", run.told);
}

/// T8, the control: a folder of 100 MiB says nothing, and a folder of 920 MiB with the workspace check off says nothing (T4 through
/// the driver: the Instances runtime sets that limit to 0).
#[tokio::test(start_paused = true)]
async fn b889_a_small_folder_and_a_session_without_the_workspace_check_say_nothing() {
    let disk = Scripted::new(vec![nearly(100)]);
    let run = b889_run(vec![b889_prompt(2)], &disk, Limits::default()).await;
    assert_eq!(disk.calls(), 1);
    println!("TRIGGER ABSENT (control): reading 100 MiB, the mark is 200 MiB");
    assert!(run.notify().is_empty(), "{:?}", run.notify());
    assert_eq!(
        run.lines("direct turn disk "),
        vec!["direct turn disk reading: workspace 100 MiB used".to_string()]
    );
    let disk = Scripted::new(vec![nearly(920)]);
    let off = Limits {
        root_low_mb: 512,
        workspace_limit_mb: 0,
    };
    let run = b889_run(vec![b889_prompt(2)], &disk, off).await;
    assert_eq!(disk.calls(), 1);
    assert!(run.notify().is_empty(), "{:?}", run.notify());
}

/// T1 and T9 through the driver: a later prompt boundary of the session says nothing again, also when the folder has grown past the
/// mark by then; a second session of the same run, which restores the work again, says the note again.
#[tokio::test(start_paused = true)]
async fn b889_the_note_is_said_once_for_a_session_and_again_by_the_next_session() {
    // Three boundaries in one session (a provider fault twice, then the answer); the folder is measured at each and after each fault.
    let disk = Scripted::new(vec![nearly(920)]);
    let run = b889_run(
        vec![provider_503(), provider_503(), end_turn("Done.")],
        &disk,
        Limits::default(),
    )
    .await;
    assert_eq!(count(&run.received, "session/prompt"), 3);
    assert_eq!(
        run.notes(),
        vec![B889_N1_920.to_string(), B889_N2.to_string()]
    );
    assert_eq!(run.lines("direct turn disk reading").len(), 1);
    // A folder that is small at the session's first reading and large at a later boundary brings no note.
    let grown = Scripted::new(vec![nearly(100), nearly(100), nearly(700)]);
    let run = b889_run(
        vec![provider_503(), end_turn("Done.")],
        &grown,
        Limits::default(),
    )
    .await;
    assert_eq!(count(&run.received, "session/prompt"), 2);
    assert!(run.notes().is_empty(), "{:?}", run.notify());
    // The next session of the run: its own first reading, its own note. It is the third session in this one process, so a
    // "said once" state that belongs to the process and not to the worker's session fails here, while a test of one session
    // still passes (seen with such a state put into `FolderNote::first_reading` on purpose).
    let again = Scripted::new(vec![nearly(920)]);
    let run = b889_run(vec![end_turn("Done.")], &again, Limits::default()).await;
    assert_eq!(
        run.notes().len(),
        2,
        "{} events with reason disk_note in the second session, expected 2",
        run.notes().len()
    );
}

/// One prompt boundary is one reading, with a folder above the mark and with one below it, and a prompt with 50 tool calls adds
/// none.
#[tokio::test(start_paused = true)]
async fn b889_the_note_adds_no_reading_to_a_prompt_boundary() {
    for used in [920, 100] {
        let disk = Scripted::new(vec![nearly(used)]);
        let run = b889_run(vec![b889_prompt(50)], &disk, Limits::default()).await;
        assert_eq!(count(&run.received, "session/prompt"), 1);
        assert_eq!(
            disk.calls(),
            1,
            "measure was called {} times, expected 1 (first reading {used} MiB, 50 tool calls)",
            disk.calls()
        );
        assert_eq!(run.notes().len(), if used > 200 { 2 } else { 0 });
    }
    // Two boundaries with a fault between them: the boundary's reading twice and the reading after the fault, as before the note.
    let disk = Scripted::new(vec![nearly(920)]);
    let run = b889_run(
        vec![provider_503(), b889_prompt(50)],
        &disk,
        Limits::default(),
    )
    .await;
    assert_eq!(count(&run.received, "session/prompt"), 2);
    assert_eq!(
        disk.calls(),
        3,
        "measure was called {} times, expected 3",
        disk.calls()
    );
}
