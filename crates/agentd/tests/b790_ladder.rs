//! A tool call that never returns, through the recovery ladder: the first engine hangs inside a helper-agent call, the
//! tool-idle limit cancels the prompt, the process is started again and the fresh session finishes the work. Nobody is asked.
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
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::DuplexStream;

const PROMPT: &str = "Add percent_change\nwith tests\n";

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

/// The host of the run: each restart is the next script.
struct Host {
    next: VecDeque<Script>,
    started: Arc<Mutex<Vec<Received>>>,
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

async fn run(
    first: Script,
    later: Vec<Script>,
    auto_sessions: u32,
) -> (
    agentd::direct::driver::DriverEnd,
    Received,
    Arc<Mutex<Vec<Received>>>,
) {
    let p0 = spawn(first);
    let (_tx, rx) = tokio::sync::mpsc::channel(16);
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let rec = EventRecorder::new("run1", "direct");
    let activity = Activity::new();
    activity.set_tool_idle(Some(Duration::from_secs(3600)));
    let mut client = AcpClient::new(p0.r, p0.w).with_activity(activity.clone());
    let started: Arc<Mutex<Vec<Received>>> = Arc::default();
    let mut host = Host {
        next: later.into(),
        started: started.clone(),
    };
    let dir = b790_support::scratch("ladder");
    let wiring = InputWiring {
        policy: InputPolicy {
            wait: Duration::from_secs(1800),
            max_requests: 3,
            margin: Duration::from_secs(900),
            min_window: Duration::from_secs(300),
            max_auto_sessions: auto_sessions,
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
    let mut collector = DirectCollector::new(rec);
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
    (end, p0.received, started)
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
async fn b790_a_hung_tool_call_ends_with_a_restarted_engine_and_a_finished_fresh_session() {
    let (end, first, later) = run(hanging(), vec![finishing()], 1).await;
    // The first engine was told to cancel, then replaced; the second one ran the fresh session to its end.
    assert!(
        methods(&first).contains(&"session/cancel".to_string()),
        "{:?}",
        methods(&first)
    );
    assert_eq!(later.lock().unwrap().len(), 1, "one restart");
    assert_eq!(
        classify(&end.raw).outcome,
        DirectOutcome::ReportedDone,
        "{:?}",
        end.raw.watched
    );
    assert_eq!(end.fresh_sessions.len(), 1);
    assert_eq!(
        end.fresh_sessions[0].trigger, "harness",
        "the harness started it, nobody was asked"
    );
    assert!(end.interventions.is_empty(), "{:?}", end.interventions);
}

#[tokio::test(start_paused = true)]
async fn b790_with_no_automatic_session_left_the_user_is_asked_and_the_run_does_not_loop() {
    // No automatic session allowed: the stage ends as an engine that is gone, and the run asks (the window runs out unanswered).
    let (end, first, later) = run(hanging(), vec![], 0).await;
    assert!(methods(&first).contains(&"session/cancel".to_string()));
    assert!(
        later.lock().unwrap().is_empty(),
        "no restart without a session left"
    );
    let outcome = classify(&end.raw).outcome;
    assert_eq!(
        outcome,
        DirectOutcome::StoppedEngineDied,
        "{:?}",
        end.raw.watched
    );
    assert_eq!(end.interventions.len(), 1, "one question, then the end");
}
