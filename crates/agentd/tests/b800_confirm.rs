//! The look for the engine's own sign that the command time limit is in force, in the driver. After `session/new` and the model pin,
//! the first session and every fresh session ask the host ([`Respawn::confirm`]). A session without the sign is told once, with the reason
//! `command_limit`, and goes on: the prompt is sent and the run ends as its engine ends it. Nothing ends on a missing sign.
#![allow(clippy::unwrap_used)]

mod b790_support;

use agentd::acp::{AcpClient, Activity};
use agentd::direct::classify::classify;
use agentd::direct::collector::DirectCollector;
use agentd::direct::context::ContextDir;
use agentd::direct::driver::{InputWiring, NoProbe, PromptPlan, Respawn, run_direct_stages};
use agentd::direct::input::{ChangedFiles, DigestSource, DigestSources, InputPolicy, TreeSource};
use agentd::direct::retry::RetryPolicy;
use agentd::events::EventRecorder;
use b790_support::{MODEL, Received, Script, Step, TOP, chunk, methods, spawn, tool};
use harness_protocol::direct::DirectOutcome;
use harness_protocol::envelope::{EventType, Payload};
use serde_json::Value;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::DuplexStream;

const PROMPT: &str = "Add percent_change\nwith tests\n";
const SENTENCE: &str = "OpenCode has not confirmed this run's command time limit of 600 seconds. If it is not in force, commands that set no limit of their own stop after 120 seconds.";

struct Tree;

impl TreeSource for Tree {
    fn changed_files(
        &self,
        _limit: usize,
    ) -> Pin<Box<dyn Future<Output = Option<ChangedFiles>> + Send + '_>> {
        Box::pin(async {
            Some(ChangedFiles {
                paths: vec!["src/calc.py".into()],
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

/// The host of the run: each restart is the next script, and each look for the engine's sign is the next answer (none left: confirmed).
struct Host {
    next: VecDeque<Script>,
    started: Arc<Mutex<Vec<Received>>>,
    confirms: VecDeque<Option<String>>,
    asked: Arc<Mutex<u32>>,
}

impl Respawn for Host {
    type R = DuplexStream;
    type W = DuplexStream;
    fn respawn(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(DuplexStream, DuplexStream), String>> + Send + '_>>
    {
        Box::pin(async move {
            let script = self.next.pop_front().ok_or("no engine left to start")?;
            let p = spawn(script);
            self.started.lock().unwrap().push(p.received.clone());
            Ok((p.r, p.w))
        })
    }

    fn confirm(&mut self) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
        Box::pin(async move {
            *self.asked.lock().unwrap() += 1;
            self.confirms.pop_front().flatten()
        })
    }
}

fn plan() -> PromptPlan {
    PromptPlan {
        cwd: TOP.into(),
        model: MODEL.into(),
        prompt: PROMPT.into(),
        nudge: None,
        cancel: None,
        timeout: Duration::from_secs(30_000),
        silence: Duration::from_secs(900),
        retry: RetryPolicy::off(),
        failover: None,
        engine_stop_resumes: 0,
        engine: "opencode".into(),
    }
}

struct Ran {
    end: agentd::direct::driver::DriverEnd,
    first: Received,
    later: Arc<Mutex<Vec<Received>>>,
    asked: u32,
    notices: Vec<Value>,
}

async fn run(first: Script, later: Vec<Script>, confirms: Vec<Option<String>>) -> Ran {
    let p0 = spawn(first);
    let (_tx, rx) = tokio::sync::mpsc::channel(16);
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let rec = EventRecorder::new("run1", "direct");
    let activity = Activity::new();
    activity.set_tool_idle(Some(Duration::from_secs(3600)));
    let mut client = AcpClient::new(p0.r, p0.w).with_activity(activity.clone());
    let started: Arc<Mutex<Vec<Received>>> = Arc::default();
    let asked: Arc<Mutex<u32>> = Arc::default();
    let mut host = Host {
        next: later.into(),
        started: started.clone(),
        confirms: confirms.into(),
        asked: asked.clone(),
    };
    let dir = b790_support::scratch("confirm");
    let wiring = InputWiring {
        delivery: None,
        policy: InputPolicy {
            wait: Duration::from_secs(1800),
            max_requests: 3,
            margin: Duration::from_secs(900),
            min_window: Duration::from_secs(300),
            max_auto_sessions: 1,
            max_user_sessions: 3,
        },
        instructions: rx,
        cancel: cancel_rx,
        tree: Arc::new(Tree),
        sources: Arc::new(Source),
        context: ContextDir::create(&dir).unwrap(),
        task: PROMPT.to_string(),
        engine_label: "OpenCode".to_string(),
        routes: Vec::new(),
        rec: rec.clone(),
        audit: None,
        log: None,
        review: None,
    };
    let mut collector = DirectCollector::new(rec.clone()).with_engine("opencode");
    let end = run_direct_stages(
        &mut client,
        &mut collector,
        &activity,
        &plan(),
        &NoProbe,
        Some(wiring),
        &mut host,
    )
    .await;
    let notices = rec
        .drain()
        .into_iter()
        .filter(|e| e.event_type == EventType::TurnProgress)
        .filter_map(|e| match e.payload {
            Payload::Inline { value } => Some(value),
            _ => None,
        })
        .filter(|v| v["kind"] == "notify" && v["reason"] == "command_limit")
        .collect();
    let asked = *asked.lock().unwrap();
    Ran {
        end,
        first: p0.received,
        later: started,
        asked,
        notices,
    }
}

fn finishing() -> Script {
    Script {
        prompts: vec![vec![chunk("Done."), Step::Finish("end_turn")]],
        ..Script::default()
    }
}

fn hanging() -> Script {
    Script {
        prompts: vec![vec![tool("t1", "task", "in_progress"), Step::Silent]],
        ..Script::default()
    }
}

#[tokio::test(start_paused = true)]
async fn b800_a_missing_confirmation_is_told_and_the_prompt_is_sent() {
    let ran = run(finishing(), vec![], vec![Some(SENTENCE.to_string())]).await;
    assert_eq!(ran.asked, 1, "the first session asks once");
    assert_eq!(ran.notices.len(), 1, "{:?}", ran.notices);
    assert_eq!(ran.notices[0]["detail"], SENTENCE);
    assert_eq!(ran.notices[0]["outageMinutes"], Value::Null);
    // The run goes on: the prompt is written and the run ends as its engine ends it, not as a failed setup.
    assert!(
        methods(&ran.first).contains(&"session/prompt".to_string()),
        "{:?}",
        methods(&ran.first)
    );
    assert_eq!(
        classify(&ran.end.raw).outcome,
        DirectOutcome::ReportedDone,
        "{:?}",
        ran.end.raw.watched
    );
}

#[tokio::test(start_paused = true)]
async fn b800_a_confirmed_session_is_told_nothing() {
    let ran = run(finishing(), vec![], vec![None]).await;
    assert_eq!(ran.asked, 1);
    assert!(ran.notices.is_empty(), "{:?}", ran.notices);
    assert_eq!(classify(&ran.end.raw).outcome, DirectOutcome::ReportedDone);
}

#[tokio::test(start_paused = true)]
async fn b800_a_fresh_session_asks_for_the_confirmation_again() {
    // The first engine hangs in a tool call, the ladder starts the process again and the fresh session finishes. The first session was
    // confirmed; the fresh one is not, is told, and goes on.
    let ran = run(
        hanging(),
        vec![finishing()],
        vec![None, Some(SENTENCE.to_string())],
    )
    .await;
    assert_eq!(ran.later.lock().unwrap().len(), 1, "one restart");
    assert_eq!(ran.asked, 2, "the first session and the fresh one");
    assert_eq!(ran.notices.len(), 1, "{:?}", ran.notices);
    assert_eq!(ran.notices[0]["detail"], SENTENCE);
    assert_eq!(
        classify(&ran.end.raw).outcome,
        DirectOutcome::ReportedDone,
        "{:?}",
        ran.end.raw.watched
    );
    assert_eq!(ran.end.fresh_sessions.len(), 1);
}
