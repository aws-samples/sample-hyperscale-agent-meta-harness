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
use agentd::events::{EventRecorder, Instruct};
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

/// One engine process; `prompts` holds the steps of each `session/prompt` of its one session, in order.
fn spawn_engine(prompts: Vec<Vec<Step>>) -> Proc {
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
        let mut model = MODEL.to_string();
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
}

impl Setup {
    fn new(prompts: Vec<Vec<Step>>, retry: RetryPolicy) -> Self {
        Self {
            prompts,
            retry,
            failover: None,
            policy: policy(0),
            hooks: Vec::new(),
        }
    }
}

struct Live {
    tx: Option<mpsc::Sender<Instruct>>,
    _cancel: watch::Sender<bool>,
    rec: Arc<EventRecorder>,
    seen: Vec<Envelope>,
    handle: tokio::task::JoinHandle<DriverEnd>,
    received: Received,
}

fn start(s: Setup) -> Live {
    let p0 = spawn_engine(s.prompts);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let (tx, rx) = mpsc::channel::<Instruct>(16);
    let rec = EventRecorder::new("run1", "direct");
    let activity = Activity::new();
    let client = AcpClient::new(p0.r, p0.w)
        .with_activity(activity.clone())
        .with_cancel(cancel_rx.clone(), Duration::from_secs(10));
    let plan = PromptPlan {
        cwd: TOP.into(),
        model: MODEL.into(),
        prompt: PROMPT.into(),
        nudge: None,
        cancel: Some(cancel_rx.clone()),
        timeout: Duration::from_secs(30_000),
        silence: Duration::from_secs(900),
        retry: s.retry,
        failover: s.failover,
        engine_stop_resumes: 0,
    };
    let dir = common::scratch("disk-stop");
    let wiring = InputWiring {
        policy: s.policy,
        instructions: rx,
        cancel: cancel_rx,
        tree: Arc::new(Tree),
        sources: Arc::new(Source),
        context: ContextDir::create(&dir).unwrap(),
        task: PROMPT.to_string(),
        engine_label: "Cline".to_string(),
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
    assert!(
        label.starts_with(&format!(
            "Cline stopped: the harness ended the run ({cause}, workspace 40 MiB"
        )),
        "{label}"
    );
    // One notice names the cause (read-only or full) and what the person can do.
    let notes = notices(&events, "disk_full");
    assert_eq!(notes.len(), 1, "{events:?}");
    let detail = notes[0]["detail"].as_str().unwrap();
    assert!(detail.starts_with(title), "{detail}");
    assert!(
        detail.contains("ham say") && detail.contains("ham finish"),
        "{detail}"
    );
    assert!(
        detail.contains(&format!("The engine's error says: {}.", signal.word())),
        "{detail}"
    );
}

const FULL_TITLE: &str = "The session disk is full";
const READ_ONLY_TITLE: &str = "The session disk is read-only or full";
const FULL: &str = "the session disk is full";
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
async fn the_text_no_space_left_on_device_stops_the_call_and_names_a_full_disk() {
    the_text_stops_the_call(
        "Internal error: No space left on device (os error 28)",
        Value::Null,
        Signal::NoSpace,
        FULL,
        FULL_TITLE,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn the_text_os_error_28_in_the_data_stops_the_call() {
    the_text_stops_the_call(
        "Internal error",
        json!({"cause": "write failed: os error 28"}),
        Signal::NoSpace,
        FULL,
        FULL_TITLE,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn the_text_enospc_stops_the_call() {
    the_text_stops_the_call(
        "Internal error: ENOSPC: no space left, write",
        Value::Null,
        Signal::NoSpace,
        FULL,
        FULL_TITLE,
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
