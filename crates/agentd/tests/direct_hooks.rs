//! The module seam of the direct driver, whole (package AG0, `src/direct/hooks.rs`): a fake module is registered through `Deps.hooks` and
//! the whole agent runs in process against the fake AWS and the fake ACP engine. The tests pin where each hook is called, what a module
//! can change (the launch environment, the session directory, the repository the run restores, the `turn-complete` request, the next
//! prompt, the end of the run) and that a module which fails stops nothing but itself. The default, with no module, is pinned by the
//! golden streams of `direct_golden.rs`.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::BoxFuture;
use agentd::acp::Dir;
use agentd::direct::Deps;
use agentd::direct::hooks::{
    DirectHook, DriverCtl, Event, Finalize, Hooks, LadderEvent, Limits, RestoreCtx, SessionEnd,
    Shutdown, StartupCtx, SummaryCtx, TurnEnd, TurnVote, Wire,
};
use agentd::events::TurnControls;
use direct_support::{Direct, FakeAws, Opts, PROMPT, RUN};
use harness_protocol::api::internal::PendingControl;
use harness_protocol::common::Protocol;
use harness_protocol::direct::{
    DirectAttemptKind, DirectModelSpec, DirectOutcome, DirectSkillRef, s3 as keys,
};
use harness_protocol::envelope::EventType;
use harness_protocol::runtime::{InvocationResponse, TurnOutcome};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

type StartupFn = Box<dyn Fn(&mut StartupCtx<'_>) -> Result<(), String> + Send + Sync>;

/// When the fake module interrupts the first prompt.
#[derive(Clone, Copy)]
enum InterruptWhen {
    /// This long after the prompt was sent.
    After(Duration),
    /// As soon as the user's cancel is up.
    AfterUserCancel,
}

/// A module that writes down what it is told and does what a test scripted.
struct Probe {
    name: &'static str,
    log: Arc<Mutex<Vec<String>>>,
    /// The driver's controls and the user's cancel, kept from startup.
    ctl: Mutex<Option<(DriverCtl, Arc<TurnControls>)>>,
    interrupt: Option<InterruptWhen>,
    /// Try an interrupt at startup, when no prompt is open.
    interrupt_at_startup: bool,
    /// The user's cancel is raised inside the vote, which then comes back with its answer.
    cancel_in_vote: bool,
    restore_err: Option<String>,
    panic_in_restore: bool,
    panic_in_shutdown: bool,
    panic_in_finalize: bool,
    panic_in_summary: bool,
    hang_in_summary: bool,
    summary_skills: Option<Vec<DirectSkillRef>>,
    wire: Mutex<Vec<(u64, Dir, Value, bool)>>,
    turn_end: Mutex<VecDeque<TurnVote>>,
    seal: Mutex<VecDeque<TurnVote>>,
    startup: Option<StartupFn>,
    panic_in_startup: bool,
    panic_on_wire: bool,
    restore_to: Option<String>,
    finalize_summary: Option<String>,
    /// The fake AWS, to read what has been uploaded or posted at the moment a call arrives.
    world: FakeAws,
}

impl Probe {
    fn new(d: &Direct) -> Self {
        Self {
            name: "probe",
            log: Arc::new(Mutex::new(Vec::new())),
            ctl: Mutex::new(None),
            interrupt: None,
            interrupt_at_startup: false,
            cancel_in_vote: false,
            restore_err: None,
            panic_in_restore: false,
            panic_in_shutdown: false,
            panic_in_finalize: false,
            panic_in_summary: false,
            hang_in_summary: false,
            summary_skills: None,
            wire: Mutex::new(Vec::new()),
            turn_end: Mutex::new(VecDeque::new()),
            seal: Mutex::new(VecDeque::new()),
            startup: None,
            panic_in_startup: false,
            panic_on_wire: false,
            restore_to: None,
            finalize_summary: None,
            world: FakeAws {
                base: d.aws.base.clone(),
                state: d.aws.state.clone(),
            },
        }
    }

    fn votes(self, v: Vec<TurnVote>) -> Self {
        *self.turn_end.lock().unwrap() = v.into();
        self
    }

    fn seals(self, v: Vec<TurnVote>) -> Self {
        *self.seal.lock().unwrap() = v.into();
        self
    }

    fn note(&self, s: impl Into<String>) {
        self.log.lock().unwrap().push(s.into());
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    /// The calls other than the wire, in order.
    fn calls(&self) -> Vec<String> {
        self.log()
    }
}

impl DirectHook for Probe {
    fn name(&self) -> &'static str {
        self.name
    }

    fn on_restore<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut RestoreCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.note("restore");
            if self.panic_in_restore {
                panic!("a module bug at restore");
            }
            if let Some(why) = &self.restore_err {
                return Err(why.clone());
            }
            assert!(ctx.request.prompt.starts_with("Add a percent_change"));
            if let Some(uri) = &self.restore_to {
                *ctx.repo_uri = uri.clone();
            }
            Ok(())
        })
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.note("startup");
            *self.ctl.lock().unwrap() = Some((ctx.env.ctl.clone(), ctx.env.controls.clone()));
            if self.interrupt_at_startup {
                let sent = ctx.env.ctl.interrupt();
                self.note(format!(
                    "interrupt at startup sent={sent} open={}",
                    ctx.env.ctl.prompt_open()
                ));
            }
            if self.panic_in_startup {
                panic!("a module bug at startup");
            }
            match &self.startup {
                Some(f) => f(ctx),
                None => Ok(()),
            }
        })
    }

    fn on_wire(&self, w: &Wire<'_>) {
        if self.panic_on_wire {
            panic!("a module bug on the wire");
        }
        self.wire
            .lock()
            .unwrap()
            .push((w.seq, w.dir, w.msg.clone(), w.stale));
    }

    fn on_event(&self, e: &Event<'_>) {
        let line = match e {
            Event::SessionStarted(s) => format!("event session-started {}", s.number),
            Event::SessionEnded(s) => format!(
                "event session-ended {} {}",
                s.number,
                match s.why {
                    SessionEnd::Replaced => "replaced",
                    SessionEnd::RunEnded => "run-ended",
                }
            ),
            Event::PromptSent(p) => {
                if p.no == 1
                    && let Some(when) = self.interrupt
                    && let Some((ctl, controls)) = self.ctl.lock().unwrap().clone()
                {
                    let cancel = controls.subscribe();
                    let log = self.log.clone();
                    tokio::spawn(async move {
                        match when {
                            InterruptWhen::After(d) => tokio::time::sleep(d).await,
                            InterruptWhen::AfterUserCancel => {
                                while !*cancel.borrow() {
                                    tokio::time::sleep(Duration::from_millis(10)).await;
                                }
                            }
                        }
                        let sent = ctl.interrupt();
                        log.lock().unwrap().push(format!("interrupt sent={sent}"));
                    });
                }
                format!("event prompt-sent {}", p.no)
            }
            Event::PromptReplied(p) => {
                format!(
                    "event prompt-replied {} {}",
                    p.no,
                    p.stop_reason.unwrap_or("-")
                )
            }
            Event::Ladder(l) => format!(
                "event ladder {}",
                match l {
                    LadderEvent::ProviderRetry { .. } => "provider-retry",
                    LadderEvent::Switch(_) => "switch",
                    _ => "other",
                }
            ),
            other => format!("event {}", other.name()),
        };
        self.note(line);
    }

    fn on_turn_end<'a>(&'a self, end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move {
            self.note(format!(
                "turn-end {} {}{}",
                end.n,
                end.classified.outcome.as_str(),
                if end.interrupted { " interrupted" } else { "" }
            ));
            if self.cancel_in_vote {
                // The user's cancel arrives while the module is still deciding.
                let controls = self.ctl.lock().unwrap().clone().map(|(_, c)| c);
                if let Some(controls) = controls {
                    controls.deliver(&[PendingControl::Cancel]);
                }
            }
            self.turn_end
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(TurnVote::Done)
        })
    }

    fn on_seal<'a>(&'a self, end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move {
            self.note(format!("seal {}", end.n));
            self.seal
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(TurnVote::Done)
        })
    }

    fn on_shutdown<'a>(&'a self, _end: &'a Shutdown<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if self.panic_in_shutdown {
                panic!("a module bug at shutdown");
            }
            // The engine is gone and nothing is delivered yet: the bundle is not in the bucket.
            let delivered = self.world.get(&keys::bundle_key(RUN)).is_some();
            self.note(format!("shutdown bundle-uploaded={delivered}"));
        })
    }

    fn on_summary<'a>(&'a self, ctx: SummaryCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if self.panic_in_summary {
                panic!("a module bug at summary");
            }
            if self.hang_in_summary {
                std::future::pending::<()>().await;
            }
            // The summary is built and not yet uploaded.
            let written = self.world.get(&keys::summary_key(RUN)).is_some();
            self.note(format!(
                "summary written={written} outcome={}",
                ctx.summary.outcome.as_str()
            ));
            if let Some(skills) = &self.summary_skills {
                ctx.summary.skills.clone_from(skills);
            }
        })
    }

    fn on_finalize<'a>(&'a self, fin: Finalize<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if self.panic_in_finalize {
                panic!("a module bug at finalize");
            }
            let posted = self.world.complete().len();
            self.note(format!("finalize complete-posted={posted}"));
            if let Some(s) = &self.finalize_summary {
                fin.complete.summary = Some(s.clone());
            }
        })
    }
}

async fn run_with(d: &Direct, probe: &Arc<Probe>) -> InvocationResponse {
    let deps = Deps {
        hooks: Some(Hooks::new(vec![probe.clone() as Arc<dyn DirectHook>])),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await
}

fn route_spec(id: &str) -> DirectModelSpec {
    DirectModelSpec {
        id: id.into(),
        region: "us-east-1".into(),
        protocol: Protocol::Converse,
        context_tokens: 1_000_000,
        max_output_tokens: 128_000,
        default_reasoning_effort: None,
    }
}

// ---- where each hook is called -------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_plain_run_tells_the_module_everything_in_order() {
    let d = Direct::start(Opts::default()).await;
    let p = Arc::new(Probe::new(&d));
    let resp = run_with(&d, &p).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(
        p.calls(),
        [
            "restore",
            "startup",
            "event session-started 1",
            "event prompt-sent 1",
            "event prompt-replied 1 end_turn",
            "turn-end 1 reported_done",
            "seal 1",
            "event session-ended 1 run-ended",
            "event engine-stopped",
            "shutdown bundle-uploaded=false",
            "summary written=false outcome=reported_done",
            "finalize complete-posted=0",
            "event run-ended",
        ]
    );
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_wire_shows_both_directions_with_the_session_new_answer_and_the_prompt_reply() {
    let d = Direct::start(Opts::default()).await;
    let p = Arc::new(Probe::new(&d));
    run_with(&d, &p).await;
    let wire = p.wire.lock().unwrap().clone();
    assert!(wire.len() > 6, "{}", wire.len());
    // The sequence counts every message from 1, without a gap.
    let seqs: Vec<u64> = wire.iter().map(|w| w.0).collect();
    assert_eq!(seqs, (1..=wire.len() as u64).collect::<Vec<_>>());
    let method = |w: &(u64, Dir, Value, bool)| w.2["method"].as_str().map(String::from);
    // agentd's own messages, in order.
    let to_engine: Vec<String> = wire
        .iter()
        .filter(|w| w.1 == Dir::ToEngine)
        .filter_map(method)
        .collect();
    assert_eq!(to_engine, ["initialize", "session/new", "session/prompt"]);
    // The answer to session/new carries the session id; the prompt text went out as typed; the reply carries the stop reason.
    assert!(
        wire.iter()
            .any(|w| w.1 == Dir::FromEngine && w.2["result"]["sessionId"].is_string())
    );
    assert!(wire.iter().any(
        |w| w.1 == Dir::ToEngine && w.2["params"]["prompt"][0]["text"].as_str() == Some(PROMPT)
    ));
    assert!(
        wire.iter()
            .any(|w| w.1 == Dir::FromEngine && w.2["result"]["stopReason"] == "end_turn")
    );
    // Updates arrive as notifications, and a message of the current session is not stale.
    assert!(
        wire.iter()
            .any(|w| w.1 == Dir::FromEngine && w.2["method"] == "session/update" && !w.3)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_from_the_user_is_told_and_the_stop_is_not_put_to_the_vote() {
    let d = Direct::start(Opts {
        scenario: "end_turn_done",
        engine_env: vec![("FAKE_ACP_HOLD_PROMPTS", "1".into())],
        timeout_s: 60,
        ..Opts::default()
    })
    .await;
    d.aws.cancel_on(EventType::TurnStarted);
    let p = Arc::new(Probe::new(&d).votes(vec![TurnVote::Prompt("go on".into())]));
    let resp = run_with(&d, &p).await;
    assert_eq!(resp.outcome, TurnOutcome::Cancelled, "{resp:?}");
    let calls = p.calls();
    assert!(calls.contains(&"event cancel".to_string()), "{calls:?}");
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("turn-end") || c.starts_with("seal")),
        "the user's cancel is final: {calls:?}"
    );
    // Whatever the end, the module is told, so it can close what it opened.
    assert!(calls.iter().any(|c| c.starts_with("shutdown")), "{calls:?}");
    assert!(calls.iter().any(|c| c.starts_with("finalize")), "{calls:?}");
    assert_eq!(d.summary().outcome, DirectOutcome::StoppedByUser);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_ladder_tells_the_module_each_retry_and_switch() {
    let d = Direct::start(Opts {
        scenario: "provider_down_then_done",
        edit_cfg: Some(Box::new(|c| {
            c.direct.provider_retries = 3;
            c.direct.provider_retry_pause = Duration::from_millis(20);
            c.direct.failover_wait = Duration::from_millis(100);
            c.direct.failover_wait_cap = Duration::from_millis(200);
            c.direct.failover_probe = false;
        })),
        failover: vec![route_spec("global.anthropic.claude-opus-5-5")],
        ..Opts::default()
    })
    .await;
    let p = Arc::new(Probe::new(&d));
    run_with(&d, &p).await;
    let calls = p.calls();
    let retries = calls
        .iter()
        .filter(|c| *c == "event ladder provider-retry")
        .count();
    let switches = calls.iter().filter(|c| *c == "event ladder switch").count();
    assert_eq!((retries, switches), (3, 1), "{calls:?}");
    // Five prompts went out and five were answered.
    assert_eq!(
        calls
            .iter()
            .filter(|c| c.starts_with("event prompt-sent"))
            .count(),
        5
    );
}

// ---- what a module can change --------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_end_vote_for_a_prompt_sends_it_in_the_same_session_as_an_instruction() {
    let d = Direct::start(Opts {
        scenario: "nudge_done",
        ..Opts::default()
    })
    .await;
    let p = Arc::new(Probe::new(&d).votes(vec![TurnVote::Prompt(
        "Reply DONE if it is all done.".into(),
    )]));
    let resp = run_with(&d, &p).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(
        d.prompts_received(),
        [
            PROMPT.to_string(),
            "Reply DONE if it is all done.".to_string()
        ]
    );
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    assert_eq!(
        s.cline_message.as_deref(),
        Some("DONE: everything is finished.")
    );
    let kinds: Vec<_> = s.attempts.iter().map(|a| a.kind).collect();
    assert_eq!(
        kinds,
        [DirectAttemptKind::Prompt, DirectAttemptKind::Instruction]
    );
    assert_eq!(
        d.engine_sessions().len(),
        0,
        "one session: no fresh session was started"
    );
    // The vote was put twice (after each stop) and the seal once, after the second.
    let calls = p.calls();
    let asked: Vec<&String> = calls
        .iter()
        .filter(|c| c.starts_with("turn-end") || c.starts_with("seal"))
        .collect();
    assert_eq!(
        asked,
        [
            "turn-end 1 reported_done",
            "turn-end 2 reported_done",
            "seal 2"
        ]
    );
    assert_eq!(d.events_of(EventType::TurnStarted).len(), 1);
    assert_eq!(d.events_of(EventType::TurnEnded).len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_seal_that_finds_a_pending_message_continues_the_run() {
    let d = Direct::start(Opts {
        scenario: "nudge_done",
        ..Opts::default()
    })
    .await;
    let p = Arc::new(Probe::new(&d).seals(vec![TurnVote::Prompt(
        "A message that arrived at the last moment.".into(),
    )]));
    let resp = run_with(&d, &p).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(
        d.prompts_received(),
        [
            PROMPT.to_string(),
            "A message that arrived at the last moment.".to_string()
        ]
    );
    let calls = p.calls();
    let asked: Vec<&String> = calls
        .iter()
        .filter(|c| c.starts_with("turn-end") || c.starts_with("seal"))
        .collect();
    assert_eq!(
        asked,
        [
            "turn-end 1 reported_done",
            "seal 1",
            "turn-end 2 reported_done",
            "seal 2"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_vote_ends_the_run_as_a_cline_stopped_row_with_the_modules_words() {
    let d = Direct::start(Opts::default()).await;
    let p = Arc::new(Probe::new(&d).votes(vec![TurnVote::Stop {
        detail: "the goal loop ended: BLOCKED twice (missing credentials)".into(),
    }]));
    let resp = run_with(&d, &p).await;
    // The work is delivered whatever the label.
    assert!(resp.bundle_uri.is_some(), "{resp:?}");
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedOther, "{s:?}");
    assert!(s.label.starts_with("Cline stopped:"), "{}", s.label);
    assert!(
        s.detail
            .as_deref()
            .unwrap()
            .contains("BLOCKED twice (missing credentials)"),
        "{:?}",
        s.detail
    );
    let ended = d.events_of(EventType::TurnEnded);
    assert_eq!(ended[0]["directOutcome"], "stopped_other");
    // A stop is final: the seal is not asked.
    assert!(
        !p.calls().iter().any(|c| c.starts_with("seal")),
        "{:?}",
        p.calls()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_can_set_engine_environment_and_the_session_directory() {
    let d = Direct::start(Opts::default()).await;
    let mut probe = Probe::new(&d);
    probe.startup = Some(Box::new(|ctx| {
        ctx.engine_env
            .insert("AG0_PROBE".into(), "from-a-module".into());
        *ctx.session_cwd = Some(ctx.repo.join("src").display().to_string());
        // What a module that lists the engine's skills needs: the engine's own command and a one-off command as the engine would run it.
        assert!(ctx.engine_argv().len() > 1, "{:?}", ctx.engine_argv());
        let spec = ctx.launch_spec(vec!["list".into()], "listing-stderr.log");
        assert_eq!(spec.argv, ["list"]);
        assert_eq!(
            spec.env.get("AG0_PROBE").map(String::as_str),
            Some("from-a-module")
        );
        assert_eq!(spec.cwd, ctx.repo);
        assert!(spec.stderr_log.ends_with("listing-stderr.log"));
        assert_eq!(spec.run_as, ctx.engine_uid);
        assert!(format!("{:?}", ctx.env).contains(&ctx.env.run_id));
        Ok(())
    }));
    let p = Arc::new(probe);
    let resp = run_with(&d, &p).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        d.engine_probe()
            .lines()
            .any(|l| l == "AG0_PROBE=from-a-module"),
        "{}",
        d.engine_probe()
    );
    let want = d.worktree().join("src").display().to_string();
    let new_session = d
        .fake_engine_log()
        .lines()
        .filter_map(|l| l.strip_prefix("in "))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["method"] == "session/new")
        .unwrap();
    assert_eq!(new_session["params"]["cwd"], want.as_str());
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_can_name_another_repository_and_the_run_restores_that_one() {
    let d = Direct::start(Opts::default()).await;
    let mut probe = Probe::new(&d);
    probe.restore_to = Some(keys::uri(
        direct_support::BUCKET,
        "runs/none/missing.bundle",
    ));
    let p = Arc::new(probe);
    run_with(&d, &p).await;
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed, "{s:?}");
    assert!(
        s.detail.as_deref().unwrap().starts_with("worktree:"),
        "{:?}",
        s.detail
    );
    assert!(d.prompts_received().is_empty(), "the engine never ran");
}

#[tokio::test(flavor = "multi_thread")]
async fn finalize_runs_before_turn_complete_and_can_change_the_request() {
    let d = Direct::start(Opts::default()).await;
    let mut probe = Probe::new(&d);
    probe.finalize_summary = Some("a note from a module".into());
    let p = Arc::new(probe);
    run_with(&d, &p).await;
    let complete = d.aws.complete();
    assert_eq!(complete.len(), 1);
    assert_eq!(complete[0].summary.as_deref(), Some("a note from a module"));
    assert!(
        p.calls()
            .contains(&"finalize complete-posted=0".to_string()),
        "turn-complete was not posted yet: {:?}",
        p.calls()
    );
}

// ---- a module that fails -------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_startup_error_stops_the_run_before_any_model_call_and_finalize_still_runs() {
    let d = Direct::start(Opts::default()).await;
    let mut probe = Probe::new(&d);
    probe.startup = Some(Box::new(|_| {
        Err("skills: pdf-tools is not a valid name".into())
    }));
    let p = Arc::new(probe);
    run_with(&d, &p).await;
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed, "{s:?}");
    assert_eq!(s.label, "Cline did not start");
    assert!(
        s.detail
            .as_deref()
            .unwrap()
            .contains("skills: pdf-tools is not a valid name"),
        "{:?}",
        s.detail
    );
    assert!(d.prompts_received().is_empty());
    assert!(
        p.calls().iter().any(|c| c.starts_with("finalize")),
        "{:?}",
        p.calls()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panic_at_startup_also_stops_the_run_and_names_the_module() {
    let d = Direct::start(Opts::default()).await;
    let mut probe = Probe::new(&d);
    probe.panic_in_startup = true;
    let p = Arc::new(probe);
    run_with(&d, &p).await;
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed, "{s:?}");
    let detail = s.detail.unwrap();
    assert!(
        detail.contains("probe") && detail.contains("panicked"),
        "{detail}"
    );
    assert!(d.prompts_received().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_module_that_panics_on_the_wire_changes_nothing_about_the_run() {
    let d = Direct::start(Opts::default()).await;
    let mut probe = Probe::new(&d);
    probe.panic_on_wire = true;
    let p = Arc::new(probe);
    let resp = run_with(&d, &p).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("probe") && l.contains("disabled") && l.contains("panicked")),
        "{:?}",
        d.log.lines()
    );
    // The module is off for the rest of the run: it was told what came before its first message on the wire, and nothing after.
    assert_eq!(p.calls(), ["restore", "startup"]);
}

// ---- the interrupt of an open prompt --------------------------------------------------------------------------------------------

/// The engine holds its first prompt until `session/cancel`, then answers `cancelled`; the second prompt is answered from the script.
fn held_first_prompt(extra: Vec<(&'static str, String)>) -> Opts {
    let mut engine_env = vec![("FAKE_ACP_HOLD_PROMPTS", "1".to_string())];
    engine_env.extend(extra);
    Opts {
        scenario: "nudge_done",
        engine_env,
        timeout_s: 90,
        ..Opts::default()
    }
}

fn cancels_written(d: &Direct) -> usize {
    d.fake_engine_log()
        .lines()
        .filter(|l| *l == "notification session/cancel")
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupt_cancels_the_open_prompt_and_the_module_goes_on_with_a_prompt_of_its_own() {
    let d = Direct::start(held_first_prompt(vec![])).await;
    let mut probe = Probe::new(&d).votes(vec![TurnVote::Prompt("Do this instead.".into())]);
    probe.interrupt = Some(InterruptWhen::After(Duration::from_millis(300)));
    let p = Arc::new(probe);
    let resp = run_with(&d, &p).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let calls = p.calls();
    assert!(
        calls.contains(&"interrupt sent=true".to_string()),
        "{calls:?}"
    );
    assert!(
        calls.contains(&"turn-end 1 stopped_by_user interrupted".to_string()),
        "the module is told the stage ended because of its interrupt: {calls:?}"
    );
    assert!(calls.contains(&"event cancel".to_string()));
    assert_eq!(cancels_written(&d), 1);
    // The held prompt was answered cancelled, then the module's text ran in the same session.
    assert_eq!(
        d.prompts_received(),
        [PROMPT.to_string(), "Do this instead.".to_string()]
    );
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone, "{s:?}");
    let kinds: Vec<_> = s.attempts.iter().map(|a| a.kind).collect();
    assert_eq!(
        kinds,
        [DirectAttemptKind::Prompt, DirectAttemptKind::Instruction]
    );
    assert_eq!(d.events_of(EventType::TurnEnded).len(), 1);
    // The first stage's cancel did not leak into the second: that stage was not an interrupted one.
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("turn-end 2") && c.contains("interrupted")),
        "{calls:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_engine_that_ignores_the_interrupt_leaves_the_decision_to_the_module() {
    // The engine answers the cancel after 30 s; agentd's grace is 10 s.
    let d = Direct::start(held_first_prompt(vec![(
        "FAKE_ACP_CANCEL_DELAY_MS",
        "30000".into(),
    )]))
    .await;
    let mut probe = Probe::new(&d).votes(vec![TurnVote::Stop {
        detail: "the engine did not answer the interrupt".into(),
    }]);
    probe.interrupt = Some(InterruptWhen::After(Duration::from_millis(300)));
    let p = Arc::new(probe);
    run_with(&d, &p).await;
    let calls = p.calls();
    assert!(
        calls.contains(&"turn-end 1 stopped_by_user interrupted".to_string()),
        "the module is asked although the stage ended in an error: {calls:?}"
    );
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedOther, "{s:?}");
    assert!(
        s.detail
            .as_deref()
            .unwrap()
            .contains("did not answer the interrupt"),
        "{:?}",
        s.detail
    );
    assert_eq!(d.prompts_received().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_users_cancel_wins_over_an_interrupt_and_only_one_cancel_is_written() {
    let d = Direct::start(held_first_prompt(vec![])).await;
    d.aws.cancel_on(EventType::TurnStarted);
    let mut probe = Probe::new(&d).votes(vec![TurnVote::Prompt("Do this instead.".into())]);
    probe.interrupt = Some(InterruptWhen::AfterUserCancel);
    let p = Arc::new(probe);
    let resp = run_with(&d, &p).await;
    assert_eq!(resp.outcome, TurnOutcome::Cancelled, "{resp:?}");
    assert_eq!(cancels_written(&d), 1);
    assert_eq!(
        d.prompts_received().len(),
        1,
        "the module's prompt was never sent"
    );
    assert!(
        !p.calls()
            .iter()
            .any(|c| c.starts_with("turn-end") || c.starts_with("seal")),
        "{:?}",
        p.calls()
    );
    assert_eq!(d.summary().outcome, DirectOutcome::StoppedByUser);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupt_is_refused_when_no_prompt_is_open() {
    let d = Direct::start(Opts::default()).await;
    let mut probe = Probe::new(&d);
    probe.interrupt_at_startup = true;
    let p = Arc::new(probe);
    let resp = run_with(&d, &p).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        p.calls()
            .contains(&"interrupt at startup sent=false open=false".to_string()),
        "{:?}",
        p.calls()
    );
    assert_eq!(cancels_written(&d), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_seal_that_stops_ends_the_run_as_a_stop_with_the_modules_words() {
    let d = Direct::start(Opts::default()).await;
    let p = Arc::new(Probe::new(&d).seals(vec![TurnVote::Stop {
        detail: "a message arrived and the run may not take it".into(),
    }]));
    run_with(&d, &p).await;
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedOther, "{s:?}");
    assert!(
        s.detail.as_deref().unwrap().contains("may not take it"),
        "{:?}",
        s.detail
    );
    assert_eq!(
        p.calls().iter().filter(|c| c.starts_with("seal")).count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_vote_that_comes_back_after_the_users_cancel_is_dropped() {
    // The prompt ends normally; the user's cancel arrives while the module decides what comes next.
    let d = Direct::start(Opts {
        timeout_s: 60,
        ..Opts::default()
    })
    .await;
    let mut probe = Probe::new(&d).votes(vec![TurnVote::Prompt("Do this instead.".into())]);
    probe.cancel_in_vote = true;
    let p = Arc::new(probe);
    run_with(&d, &p).await;
    assert_eq!(
        d.prompts_received().len(),
        1,
        "the cancel is final: the module's prompt was not sent"
    );
    assert!(
        !p.calls().iter().any(|c| c.starts_with("turn-end 2")),
        "{:?}",
        p.calls()
    );
}

// ---- restore, shutdown and finalize that fail, and a module that implements nothing ------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_restore_error_ends_the_run_before_the_repository_is_fetched() {
    let d = Direct::start(Opts::default()).await;
    let mut probe = Probe::new(&d);
    probe.restore_err = Some("continue: the source run's head moved".into());
    let p = Arc::new(probe);
    run_with(&d, &p).await;
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed, "{s:?}");
    assert!(
        s.detail
            .as_deref()
            .unwrap()
            .contains("the source run's head moved"),
        "{:?}",
        s.detail
    );
    assert!(d.prompts_received().is_empty());
    assert!(
        !p.calls().contains(&"startup".to_string()),
        "{:?}",
        p.calls()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panic_at_restore_also_ends_the_run_and_names_the_module() {
    let d = Direct::start(Opts::default()).await;
    let mut probe = Probe::new(&d);
    probe.panic_in_restore = true;
    let p = Arc::new(probe);
    run_with(&d, &p).await;
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed, "{s:?}");
    let detail = s.detail.unwrap();
    assert!(
        detail.contains("probe") && detail.contains("panicked"),
        "{detail}"
    );
}

/// A module that implements only `name`: every default does nothing, and the run is the run of a set with no module.
struct Silent;

impl DirectHook for Silent {
    fn name(&self) -> &'static str {
        "silent"
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_module_that_implements_nothing_changes_nothing_and_a_panic_at_shutdown_or_finalize_costs_only_that_module()
 {
    let d = Direct::start(Opts::default()).await;
    let mut shutdown_bug = Probe::new(&d);
    shutdown_bug.name = "shutdown-bug";
    shutdown_bug.panic_in_shutdown = true;
    let mut finalize_bug = Probe::new(&d);
    finalize_bug.name = "finalize-bug";
    finalize_bug.panic_in_finalize = true;
    let good = Arc::new(Probe::new(&d));
    let deps = Deps {
        hooks: Some(Hooks::new(vec![
            Arc::new(Silent) as Arc<dyn DirectHook>,
            Arc::new(shutdown_bug),
            Arc::new(finalize_bug),
            good.clone(),
        ])),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    let lines = d.log.lines();
    for who in ["shutdown-bug", "finalize-bug"] {
        assert!(
            lines
                .iter()
                .any(|l| l.contains(who) && l.contains("disabled") && l.contains("panicked")),
            "{who}: {lines:?}"
        );
    }
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("silent") && l.contains("disabled"))
    );
    // The module that did nothing wrong was told everything, and turn-complete went out.
    assert!(good.calls().iter().any(|c| c.starts_with("finalize")));
    assert_eq!(d.aws.complete().len(), 1);
}

// ---- the summary hook: a module adds to summary.json before it is checked and uploaded -------------------------------

fn skill_ref(name: &str) -> DirectSkillRef {
    DirectSkillRef {
        name: name.into(),
        sha256: "ab".repeat(32),
        files: 2,
        bytes: 123,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn summary_runs_after_shutdown_and_before_the_summary_is_uploaded_and_a_module_can_list_skills()
 {
    let d = Direct::start(Opts::default()).await;
    let mut probe = Probe::new(&d);
    probe.summary_skills = Some(vec![
        skill_ref("lifecycle-tdd"),
        skill_ref("lifecycle-loop"),
    ]);
    let p = Arc::new(probe);
    let resp = run_with(&d, &p).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let calls = p.calls();
    assert!(
        calls.contains(&"summary written=false outcome=reported_done".to_string()),
        "the summary was not in the bucket yet: {calls:?}"
    );
    let at = |what: &str| calls.iter().position(|c| c.starts_with(what)).unwrap();
    assert!(at("shutdown") < at("summary"), "{calls:?}");
    assert!(at("summary") < at("finalize"), "{calls:?}");
    // What the module set is in the uploaded file.
    let s = d.summary();
    assert_eq!(s.skills.len(), 2, "{s:?}");
    assert_eq!(s.skills[0], skill_ref("lifecycle-tdd"));
    assert_eq!(s.skills[1].name, "lifecycle-loop");
    s.validate().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_module_that_adds_nothing_leaves_skills_out_of_the_summary_file() {
    let d = Direct::start(Opts::default()).await;
    let p = Arc::new(Probe::new(&d));
    run_with(&d, &p).await;
    assert!(
        p.calls()
            .iter()
            .any(|c| c.starts_with("summary written=false")),
        "{:?}",
        p.calls()
    );
    let raw = d.aws.get(&keys::summary_key(RUN)).unwrap();
    let json: Value = serde_json::from_slice(&raw).unwrap();
    assert!(json.get("skills").is_none(), "{json}");
    assert!(d.summary().skills.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panic_or_a_hang_in_summary_costs_only_that_module() {
    let d = Direct::start(Opts::default()).await;
    let mut bug = Probe::new(&d);
    bug.name = "summary-bug";
    bug.panic_in_summary = true;
    let mut hang = Probe::new(&d);
    hang.name = "summary-hang";
    hang.hang_in_summary = true;
    let mut good = Probe::new(&d);
    good.summary_skills = Some(vec![skill_ref("kept")]);
    let good = Arc::new(good);
    let deps = Deps {
        hooks: Some(
            Hooks::new(vec![
                Arc::new(Silent) as Arc<dyn DirectHook>,
                Arc::new(bug),
                Arc::new(hang),
                good.clone(),
            ])
            .with_limits(Limits {
                finalize: Duration::from_secs(1),
                ..Limits::default()
            }),
        ),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    let lines = d.log.lines();
    assert!(
        lines.iter().any(|l| l.contains("summary-bug")
            && l.contains("disabled")
            && l.contains("panicked in on_summary")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("summary-hang")
            && l.contains("disabled")
            && l.contains("no answer within 1 s in on_summary")),
        "{lines:?}"
    );
    // The module that did nothing wrong was heard, the summary was written, and turn-complete went out.
    assert_eq!(d.summary().skills, vec![skill_ref("kept")]);
    assert_eq!(d.aws.complete().len(), 1);
}
