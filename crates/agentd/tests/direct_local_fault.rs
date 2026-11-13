//! A fault of the machine is not reported as a fault of the model provider.
//!
//! Bedrock showed no error and no throttle; the session's file system had stopped taking writes. The ladder refused eight routes in a pass,
//! waited, walked them again up to seven times, and ended under "Cline stopped: the model provider failed". These tests run the real driver
//! over in-memory streams with a scripted engine (which refuses `session/set_config_option`, the way the live engine did), a probe that
//! answers healthy for every route and a paused clock, and pin that one pass with that signature ends the run at once under a label that says
//! what was seen.
//!
//! A real provider outage is not touched: its error text names the provider ("Service unavailable"), the engine takes the switch, or a probe
//! does not answer. The tests for those stay in `direct_retry.rs` and `direct_failover.rs`.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::BoxFuture;
use agentd::acp::{AcpClient, Activity, Watched};
use agentd::direct::classify::{classify, with_disk};
use agentd::direct::collector::DirectCollector;
use agentd::direct::context::ContextDir;
use agentd::direct::disk::{DiskCheck, DiskProbe, Limits, Reading};
use agentd::direct::driver::{
    DriverEnd, InputWiring, NoProbe, PromptPlan, Respawn, run_direct_stages,
};
use agentd::direct::failover::{FailoverPlan, ProbeAnswer, Route, RouteProbe};
use agentd::direct::input::{ChangedFiles, DigestSource, DigestSources, InputPolicy, TreeSource};
use agentd::direct::local_fault::{Tally, bare_internal_error};
use agentd::direct::retry::RetryPolicy;
use agentd::events::{EventRecorder, Instruct};
use harness_protocol::direct::DirectOutcome;
use harness_protocol::envelope::{Envelope, Payload};
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
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

// ---- the disk and the probe -----------------------------------------------------------------------------------------------

/// A disk with room that takes a write: nothing about it explains the engine's error.
#[derive(Debug)]
struct RoomyDisk(AtomicU32);

impl DiskProbe for RoomyDisk {
    fn measure(&self) -> BoxFuture<'_, Reading> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Reading {
                root_free_mb: Some(5700),
                workspace_used_mb: Some(40),
                ..Reading::default()
            }
        })
    }
}

/// The small test request of a route: every answer is the one scripted, and the routes asked are kept.
#[derive(Debug)]
struct Answers {
    answer: ProbeAnswer,
    asked: Mutex<Vec<String>>,
}

impl RouteProbe for Answers {
    fn probe<'a>(
        &'a self,
        route: &'a Route,
    ) -> Pin<Box<dyn Future<Output = ProbeAnswer> + Send + 'a>> {
        Box::pin(async move {
            self.asked.lock().unwrap().push(route.id.clone());
            self.answer.clone()
        })
    }
}

// ---- the scripted engine --------------------------------------------------------------------------------------------------

fn frame(v: Value) -> String {
    format!("{v}\n")
}

type Received = Arc<Mutex<Vec<Value>>>;

struct Proc {
    r: DuplexStream,
    w: DuplexStream,
    received: Received,
}

/// What the live engine did: every `session/prompt` ends in error -32603 "Internal error" with no data, and `session/set_config_option` is
/// refused with the same error when `refuse_switch` is set (the engine writes its state before it takes a model).
fn spawn_engine(refuse_switch: bool, prompt_error: (&'static str, Value)) -> Proc {
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
        while let Some(msg) = rx.recv().await {
            let id = msg.get("id").cloned();
            let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
            let error = |message: &str, data: Value| json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": message, "data": data}});
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
                "session/set_config_option" if refuse_switch => out
                    .write_all(frame(error("Internal error", Value::Null)).as_bytes())
                    .await
                    .unwrap(),
                "session/set_config_option" => {
                    model = msg["params"]["value"].as_str().unwrap().to_string();
                    let options = json!([{"id": "model", "currentValue": model, "options": []}]);
                    out.write_all(frame(reply(json!({"configOptions": options}))).as_bytes())
                        .await
                        .unwrap();
                }
                "session/prompt" => out
                    .write_all(frame(error(prompt_error.0, prompt_error.1.clone())).as_bytes())
                    .await
                    .unwrap(),
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

struct NoRestart;

impl Respawn for NoRestart {
    type R = DuplexStream;
    type W = DuplexStream;
    fn respawn(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(DuplexStream, DuplexStream), String>> + Send + '_>>
    {
        Box::pin(async { Err("no engine left to start".to_string()) })
    }
}

struct Tree;

impl TreeSource for Tree {
    fn changed_files(
        &self,
        _limit: usize,
    ) -> Pin<Box<dyn Future<Output = Option<ChangedFiles>> + Send + '_>> {
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
    fn gather(&self) -> Pin<Box<dyn Future<Output = DigestSources> + Send + '_>> {
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

fn policy() -> InputPolicy {
    InputPolicy {
        wait: Duration::from_secs(0),
        max_requests: 3,
        margin: Duration::from_secs(900),
        min_window: Duration::from_secs(300),
        max_auto_sessions: 1,
        max_user_sessions: 3,
    }
}

struct Run {
    end: DriverEnd,
    events: Vec<Value>,
    received: Received,
    asked: Vec<String>,
    measured: u32,
}

/// One run: three routes, a probe that answers `answer` for every route, a disk with room.
async fn run(refuse_switch: bool, prompt_error: (&'static str, Value), answer: ProbeAnswer) -> Run {
    let p0 = spawn_engine(refuse_switch, prompt_error);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let (_tx, rx) = mpsc::channel::<Instruct>(16);
    let rec = EventRecorder::new("run1", "direct");
    let activity = Activity::new();
    let client = AcpClient::new(p0.r, p0.w)
        .with_activity(activity.clone())
        .with_cancel(cancel_rx.clone(), Duration::from_secs(10));
    let probes = Arc::new(Answers {
        answer,
        asked: Mutex::default(),
    });
    let disk = Arc::new(RoomyDisk(AtomicU32::new(0)));
    let plan = PromptPlan {
        cwd: TOP.into(),
        engine: "cline".into(),
        model: MODEL.into(),
        prompt: PROMPT.into(),
        nudge: None,
        cancel: Some(cancel_rx.clone()),
        timeout: Duration::from_secs(30_000),
        silence: Duration::from_secs(900),
        retry: RetryPolicy::new(3, Duration::from_secs(1))
            .with_disk(DiskCheck::new(disk.clone(), Limits::default())),
        failover: Some(
            FailoverPlan::new(vec![route(MODEL), route(MODEL_B), route(MODEL_C)])
                .with_probe(probes.clone()),
        ),
        engine_stop_resumes: 0,
    };
    let dir = common::scratch("local-fault");
    let wiring = InputWiring {
        policy: policy(),
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
    let handle = tokio::spawn(async move {
        let mut client = client;
        let mut host = NoRestart;
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
    let end = handle.await.unwrap();
    let events: Vec<Value> = rec
        .drain()
        .iter()
        .filter_map(kind_of)
        .map(|(_, v)| v)
        .collect();
    drop(cancel_tx);
    Run {
        end,
        events,
        received: p0.received,
        asked: probes.asked.lock().unwrap().clone(),
        measured: disk.0.load(Ordering::SeqCst),
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

// ---- the signature: every route refused, every probe healthy ---------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn one_pass_of_refused_switches_with_healthy_probes_ends_the_run_and_says_what_was_seen() {
    let r = run(true, ("Internal error", Value::Null), ProbeAnswer::Healthy).await;
    // The two other routes were asked once each and refused the switch; no second pass, no wait, no later prompt.
    assert_eq!(count(&r.received, "session/set_config_option"), 2);
    assert_eq!(r.asked.len(), 2, "{:?}", r.asked);
    for k in kinds(&r.events) {
        assert!(
            !matches!(k.as_str(), "provider-wait" | "provider-outage"),
            "{k} in {:?}",
            r.events
        );
    }
    // The cause is the harness's own stop, with numbers and without a model provider in it.
    let cause = r.end.disk_full.as_ref().expect("the stop names a cause");
    assert!(!cause.root_short && !cause.workspace_short, "{cause:?}");
    assert!(matches!(r.end.raw.watched, Watched::Done(Err(_))));
    let named = with_disk(classify(&r.end.raw), r.end.disk_full.as_ref());
    assert_eq!(named.outcome, DirectOutcome::StoppedOther);
    let label = DirectOutcome::StoppedOther.label_for(named.detail.as_deref());
    assert_eq!(
        label,
        "Cline stopped: the harness ended the run (the engine failed on every route while every route answered a test request, workspace 40 MiB used, root 5700 MiB free)"
    );
    assert!(!label.contains("provider"), "{label}");
    // One notice, with a word of its own, not the disk's.
    let notes: Vec<&Value> = r
        .events
        .iter()
        .filter(|v| v["kind"] == "notify" && v["reason"] == "engine_fault")
        .collect();
    assert_eq!(notes.len(), 1, "{:?}", r.events);
    assert!(r.measured >= 2, "measured {} times", r.measured);
}

#[tokio::test(start_paused = true)]
async fn a_route_that_takes_the_switch_is_not_the_signature() {
    // The engine takes the switch, so the machine is not what is failing: the walk goes on as before. The routes then fail on their own
    // prompts and the run ends the way it did before this rule.
    let r = run(false, ("Internal error", Value::Null), ProbeAnswer::Healthy).await;
    assert!(
        r.end
            .disk_full
            .as_ref()
            .is_none_or(|c| !c.line().contains("engine failed")),
        "{:?}",
        r.end.disk_full
    );
    assert!(count(&r.received, "session/set_config_option") >= 2);
}

#[tokio::test(start_paused = true)]
async fn a_probe_that_does_not_answer_is_not_the_signature() {
    // A route whose small test request goes unanswered is a route in trouble, so the pass is not proof of a local fault.
    let r = run(
        true,
        ("Internal error", Value::Null),
        ProbeAnswer::Down("503".into()),
    )
    .await;
    assert!(
        r.end
            .disk_full
            .as_ref()
            .is_none_or(|c| !c.line().contains("engine failed")),
        "{:?}",
        r.end.disk_full
    );
    assert_eq!(count(&r.received, "session/set_config_option"), 0);
}

#[tokio::test(start_paused = true)]
async fn an_error_that_names_the_provider_is_not_the_signature() {
    let r = run(
        true,
        (
            "Internal error: Service unavailable",
            json!({"isRetryable": true, "statusCode": 503, "errorName": "ServiceUnavailableException"}),
        ),
        ProbeAnswer::Healthy,
    )
    .await;
    assert!(
        r.end
            .disk_full
            .as_ref()
            .is_none_or(|c| !c.line().contains("engine failed")),
        "{:?}",
        r.end.disk_full
    );
}

// ---- the pieces ------------------------------------------------------------------------------------------------------------

#[test]
fn only_the_bare_internal_error_is_bare() {
    assert!(bare_internal_error("engine error -32603: Internal error"));
    assert!(bare_internal_error(
        "  engine error -32603: internal error \n"
    ));
    for named in [
        "engine error -32603: Internal error: Service unavailable",
        "engine error -32603: Internal error: throttled",
        "engine error -32602: Internal error",
        "engine error -32603: The operation timed out.",
        "engine I/O: broken pipe",
        "the engine closed its output",
        "",
    ] {
        assert!(!bare_internal_error(named), "{named:?}");
    }
}

#[test]
fn a_walk_is_a_local_fault_only_when_every_route_it_met_refused_the_switch_the_bare_way() {
    let mut t = Tally::default();
    assert!(!t.is_local_fault(), "an empty walk shows nothing");
    t.local_refusal();
    assert!(t.is_local_fault());
    t.local_refusal();
    assert!(t.is_local_fault());
    assert_eq!(t.routes(), 2);
    // One route that did anything else clears it: a probe that did not answer, a switch the engine took, another refusal.
    t.other();
    assert!(!t.is_local_fault());
}
