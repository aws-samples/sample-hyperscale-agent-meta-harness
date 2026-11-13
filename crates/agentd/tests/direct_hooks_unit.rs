//! The module seam of the direct driver, pure part (package AG0, `src/direct/hooks.rs`): how `Hooks` fans a call out to the registered
//! modules, how it votes, and how it keeps a module from failing or hanging the run. No AWS, no engine: the modules here are fakes.
#![allow(clippy::unwrap_used)]

use agentd::BoxFuture;
use agentd::acp::{Dir, Watched, WireTap};
use agentd::direct::classify::{Classified, PromptDone, RawEnd, classify, stopped_by_module};
use agentd::direct::collector::DirectCollector;
use agentd::direct::hooks::{
    DirectHook, EngineStop, Event, Hooks, LadderEvent, Limits, PromptReplied, PromptSent,
    SessionEnd, SessionEnded, SessionStarted, TurnEnd, TurnVote, Wire,
};
use agentd::events::TurnControls;
use agentd::logging::MemoryLog;
use harness_protocol::api::internal::PendingControl;
use harness_protocol::direct::{DirectOutcome, DirectSwitch, DirectSwitchKind};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A fake module: what it was asked, in order, and what it answers.
struct Fake {
    name: &'static str,
    calls: Arc<Mutex<Vec<String>>>,
    vote: Mutex<Vec<TurnVote>>,
    panic_in: Option<&'static str>,
    hang_in_vote: bool,
}

impl Fake {
    fn new(name: &'static str, calls: &Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            name,
            calls: calls.clone(),
            vote: Mutex::new(Vec::new()),
            panic_in: None,
            hang_in_vote: false,
        }
    }

    fn voting(self, v: TurnVote) -> Self {
        self.vote.lock().unwrap().push(v);
        self
    }

    fn panicking_in(mut self, what: &'static str) -> Self {
        self.panic_in = Some(what);
        self
    }

    fn hanging(mut self) -> Self {
        self.hang_in_vote = true;
        self
    }

    fn note(&self, what: &str) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{}:{what}", self.name));
    }
}

impl DirectHook for Fake {
    fn name(&self) -> &'static str {
        self.name
    }

    fn on_wire(&self, w: &Wire<'_>) {
        if self.panic_in == Some("wire") {
            panic!("a module bug");
        }
        let dir = match w.dir {
            Dir::ToEngine => "to",
            Dir::FromEngine => "from",
        };
        self.note(&format!("wire#{} {dir}", w.seq));
    }

    fn on_event(&self, e: &Event<'_>) {
        match e {
            Event::Ladder(l) => self.note(&format!(
                "event ladder {}",
                match l {
                    LadderEvent::ProviderRetry { .. } => "provider-retry",
                    LadderEvent::Switch(_) => "switch",
                    LadderEvent::EngineStop { .. } => "engine-stop",
                    LadderEvent::Wait { .. } => "wait",
                    LadderEvent::Outage { .. } => "outage",
                    LadderEvent::OutageNotice { .. } => "outage-notice",
                    LadderEvent::OutageRecovered { .. } => "outage-recovered",
                    LadderEvent::Notify { .. } => "notify",
                    LadderEvent::SilenceResume { .. } => "silence-resume",
                }
            )),
            other => self.note(&format!("event {}", other.name())),
        }
    }

    fn on_controls(&self, controls: &[PendingControl]) {
        self.note(&format!("controls {}", controls.len()));
    }

    fn on_turn_end<'a>(&'a self, end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move {
            self.note(&format!("turn-end {}", end.n));
            if self.panic_in == Some("turn_end") {
                panic!("a module bug");
            }
            if self.hang_in_vote {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
            let mut v = self.vote.lock().unwrap();
            if v.is_empty() {
                TurnVote::Done
            } else {
                v.remove(0)
            }
        })
    }

    fn on_seal<'a>(&'a self, end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move {
            self.note(&format!("seal {}", end.n));
            TurnVote::Done
        })
    }
}

fn hooks(list: Vec<Fake>) -> (Hooks, Arc<MemoryLog>) {
    let log = MemoryLog::new();
    let h = Hooks::new(
        list.into_iter()
            .map(|f| Arc::new(f) as Arc<dyn DirectHook>)
            .collect(),
    )
    .with_log(log.clone());
    (h, log)
}

fn answered_end_turn() -> RawEnd {
    RawEnd {
        watched: Watched::Done(Ok(PromptDone {
            stop_reason: "end_turn".into(),
            usage: None,
        })),
        cancel_sent: false,
        message: "All done.".into(),
        setup_error: None,
    }
}

async fn ask(h: &Hooks, seal: bool) -> TurnVote {
    let raw = answered_end_turn();
    let classified: Classified = classify(&raw);
    let end = TurnEnd {
        n: 1,
        session_id: Some("s1"),
        session: 1,
        raw: &raw,
        classified: &classified,
        files_changed: Some(2),
        interrupted: false,
        time_left: Duration::from_secs(600),
        tool_calls: 3,
    };
    if seal {
        h.seal(&end).await
    } else {
        h.turn_end(&end).await
    }
}

fn calls(c: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    c.lock().unwrap().clone()
}

// ---- no module ---------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn no_module_means_an_empty_set_that_votes_done_and_asks_nobody() {
    let h = Hooks::none().with_limits(Limits::default());
    assert!(h.is_empty());
    assert!(Hooks::default().is_empty());
    assert!(h.names().is_empty());
    assert!(h.tap().is_none(), "a run with no module installs no tap");
    assert_eq!(format!("{h:?}"), "Hooks { modules: [] }");
    assert_eq!(ask(&h, false).await, TurnVote::Done);
    assert_eq!(ask(&h, true).await, TurnVote::Done);
    // The calls that do not wait do nothing either.
    h.event(&Event::RunEnded);
    h.controls(&[PendingControl::Cancel]);
}

// ---- the vote ----------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn modules_are_asked_in_registration_order_and_done_changes_nothing() {
    let c = Arc::new(Mutex::new(Vec::new()));
    let (h, _) = hooks(vec![
        Fake::new("a", &c),
        Fake::new("b", &c),
        Fake::new("c", &c),
    ]);
    assert_eq!(h.names(), ["a", "b", "c"]);
    assert_eq!(ask(&h, false).await, TurnVote::Done);
    assert_eq!(calls(&c), ["a:turn-end 1", "b:turn-end 1", "c:turn-end 1"]);
}

#[tokio::test]
async fn the_first_stop_ends_the_poll_and_later_modules_are_not_asked() {
    let c = Arc::new(Mutex::new(Vec::new()));
    let stop = TurnVote::Stop {
        detail: "blocked twice".into(),
    };
    let (h, _) = hooks(vec![
        Fake::new("goal", &c).voting(stop.clone()),
        Fake::new("messages", &c).voting(TurnVote::Prompt("a pending message".into())),
    ]);
    assert_eq!(ask(&h, false).await, stop);
    assert_eq!(
        calls(&c),
        ["goal:turn-end 1"],
        "messages was never asked, so it claimed nothing"
    );
}

#[tokio::test]
async fn a_later_prompt_replaces_an_earlier_one_so_the_last_module_has_priority() {
    let c = Arc::new(Mutex::new(Vec::new()));
    let (h, _) = hooks(vec![
        Fake::new("goal", &c).voting(TurnVote::Prompt("continue the goal".into())),
        Fake::new("messages", &c).voting(TurnVote::Prompt("the user's message".into())),
    ]);
    assert_eq!(
        ask(&h, false).await,
        TurnVote::Prompt("the user's message".into())
    );
    assert_eq!(calls(&c), ["goal:turn-end 1", "messages:turn-end 1"]);
}

#[tokio::test]
async fn a_later_done_does_not_take_back_an_earlier_prompt() {
    let c = Arc::new(Mutex::new(Vec::new()));
    let (h, _) = hooks(vec![
        Fake::new("goal", &c).voting(TurnVote::Prompt("continue".into())),
        Fake::new("messages", &c),
    ]);
    assert_eq!(ask(&h, false).await, TurnVote::Prompt("continue".into()));
}

#[tokio::test]
async fn a_stop_after_a_prompt_wins() {
    let c = Arc::new(Mutex::new(Vec::new()));
    let stop = TurnVote::Stop {
        detail: "brake".into(),
    };
    let (h, _) = hooks(vec![
        Fake::new("a", &c).voting(TurnVote::Prompt("go on".into())),
        Fake::new("b", &c).voting(stop.clone()),
    ]);
    assert_eq!(ask(&h, false).await, stop);
}

// ---- the hold -------------------------------------

fn wait(s: u64) -> TurnVote {
    TurnVote::Wait {
        until: Duration::from_secs(s),
    }
}

#[tokio::test]
async fn a_wait_is_kept_when_every_other_module_has_no_opinion() {
    let c = Arc::new(Mutex::new(Vec::new()));
    let (h, _) = hooks(vec![
        Fake::new("goal", &c).voting(wait(2)),
        Fake::new("messages", &c),
    ]);
    assert_eq!(ask(&h, false).await, wait(2));
    assert_eq!(
        calls(&c),
        ["goal:turn-end 1", "messages:turn-end 1"],
        "a wait does not stop the poll: messages is still asked"
    );
}

#[tokio::test]
async fn a_prompt_replaces_a_wait_whichever_module_votes_first() {
    for (first, second) in [
        (wait(1), TurnVote::Prompt("hello".into())),
        (TurnVote::Prompt("hello".into()), wait(1)),
    ] {
        let c = Arc::new(Mutex::new(Vec::new()));
        let (h, _) = hooks(vec![
            Fake::new("a", &c).voting(first),
            Fake::new("b", &c).voting(second),
        ]);
        assert_eq!(ask(&h, false).await, TurnVote::Prompt("hello".into()));
    }
}

#[tokio::test]
async fn a_stop_beats_a_wait_whichever_module_votes_first() {
    let stop = TurnVote::Stop {
        detail: "brake".into(),
    };
    let c = Arc::new(Mutex::new(Vec::new()));
    let (h, _) = hooks(vec![
        Fake::new("a", &c).voting(wait(1)),
        Fake::new("b", &c).voting(stop.clone()),
    ]);
    assert_eq!(ask(&h, false).await, stop);
    let c = Arc::new(Mutex::new(Vec::new()));
    let (h, _) = hooks(vec![
        Fake::new("a", &c).voting(stop.clone()),
        Fake::new("b", &c).voting(wait(1)),
    ]);
    assert_eq!(ask(&h, false).await, stop);
    assert_eq!(calls(&c), ["a:turn-end 1"]);
}

#[tokio::test]
async fn of_two_waits_the_shorter_is_kept_and_a_wait_of_zero_is_at_least_a_tick() {
    let c = Arc::new(Mutex::new(Vec::new()));
    let (h, _) = hooks(vec![
        Fake::new("a", &c).voting(wait(5)),
        Fake::new("b", &c).voting(wait(1)),
    ]);
    assert_eq!(ask(&h, false).await, wait(1));
    let c = Arc::new(Mutex::new(Vec::new()));
    let (h, _) = hooks(vec![Fake::new("a", &c).voting(wait(0))]);
    assert_eq!(
        ask(&h, false).await,
        TurnVote::Wait {
            until: Duration::from_millis(10)
        }
    );
}

#[tokio::test]
async fn a_wait_at_the_seal_is_read_as_no_opinion() {
    struct SealWaiter;
    impl DirectHook for SealWaiter {
        fn name(&self) -> &'static str {
            "sealwaiter"
        }
        fn on_seal<'a>(&'a self, _end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
            Box::pin(async { wait(1) })
        }
    }
    let h = Hooks::new(vec![Arc::new(SealWaiter)]);
    assert_eq!(ask(&h, true).await, TurnVote::Done);
}

#[tokio::test]
async fn the_seal_is_asked_separately_and_only_by_the_caller() {
    let c = Arc::new(Mutex::new(Vec::new()));
    let (h, _) = hooks(vec![Fake::new("messages", &c)]);
    assert_eq!(ask(&h, true).await, TurnVote::Done);
    assert_eq!(calls(&c), ["messages:seal 1"]);
}

// ---- a module that fails -------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_module_that_panics_in_the_vote_counts_as_done_and_is_not_asked_again() {
    let c = Arc::new(Mutex::new(Vec::new()));
    let (h, log) = hooks(vec![
        Fake::new("bad", &c).panicking_in("turn_end"),
        Fake::new("good", &c).voting(TurnVote::Prompt("go on".into())),
    ]);
    assert_eq!(ask(&h, false).await, TurnVote::Prompt("go on".into()));
    assert!(
        log.lines()
            .iter()
            .any(|l| l.contains("bad") && l.contains("disabled") && l.contains("panicked")),
        "{:?}",
        log.lines()
    );
    c.lock().unwrap().clear();
    assert_eq!(ask(&h, false).await, TurnVote::Done);
    assert_eq!(
        calls(&c),
        ["good:turn-end 1"],
        "the disabled module is skipped"
    );
}

#[tokio::test(start_paused = true)]
async fn a_module_that_hangs_in_the_vote_is_cut_off_at_the_limit_and_disabled() {
    let c = Arc::new(Mutex::new(Vec::new()));
    let (h, log) = hooks(vec![
        Fake::new("slow", &c).hanging(),
        Fake::new("good", &c).voting(TurnVote::Prompt("go on".into())),
    ]);
    let h = h.with_limits(Limits {
        decision: Duration::from_secs(5),
        ..Limits::default()
    });
    assert_eq!(ask(&h, false).await, TurnVote::Prompt("go on".into()));
    assert!(
        log.lines()
            .iter()
            .any(|l| l.contains("slow") && l.contains("disabled") && l.contains("5 s")),
        "{:?}",
        log.lines()
    );
}

#[test]
fn a_module_that_panics_on_the_wire_is_disabled_and_the_others_still_see_every_message() {
    let c = Arc::new(Mutex::new(Vec::new()));
    let (h, log) = hooks(vec![
        Fake::new("bad", &c).panicking_in("wire"),
        Fake::new("good", &c),
    ]);
    let m = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
    h.wire(Dir::ToEngine, &m, false);
    h.wire(
        Dir::FromEngine,
        &json!({"jsonrpc": "2.0", "id": 1, "result": {}}),
        false,
    );
    assert_eq!(
        calls(&c),
        ["good:wire#1 to", "good:wire#2 from"],
        "bad panicked once and was skipped from then on; the sequence counts every message"
    );
    let panics = log
        .lines()
        .iter()
        .filter(|l| l.contains("panicked"))
        .count();
    assert_eq!(panics, 1, "{:?}", log.lines());
}

#[test]
fn writing_session_cancel_raises_a_cancel_event_after_the_wire_message() {
    let c = Arc::new(Mutex::new(Vec::new()));
    let (h, _) = hooks(vec![Fake::new("m", &c)]);
    h.wire(
        Dir::ToEngine,
        &json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "s1"}}),
        false,
    );
    h.wire(
        Dir::FromEngine,
        &json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "s1"}}),
        false,
    );
    assert_eq!(
        calls(&c),
        ["m:wire#1 to", "m:event cancel", "m:wire#2 from"],
        "only a message agentd writes is a cancel agentd wrote"
    );
}

#[test]
fn events_reach_every_module_in_order() {
    let c = Arc::new(Mutex::new(Vec::new()));
    let (h, _) = hooks(vec![Fake::new("a", &c), Fake::new("b", &c)]);
    h.event(&Event::RunEnded);
    assert_eq!(calls(&c), ["a:event run-ended", "b:event run-ended"]);
}

// ---- the stop a module asks for -------------------------------------------------------------------------------------------------

#[test]
fn a_stop_a_module_asked_for_is_the_cline_stopped_row_with_the_modules_words() {
    let c = stopped_by_module("the goal said BLOCKED twice: missing credentials\u{7}");
    assert_eq!(c.outcome, DirectOutcome::StoppedOther);
    assert_eq!(
        c.detail.as_deref(),
        Some("the goal said BLOCKED twice: missing credentials"),
        "a detail is one scrubbed line"
    );
    assert!(c.fault_hint.is_none());
    assert!(
        c.outcome.label().starts_with("Cline stopped:"),
        "{}",
        c.outcome.label()
    );
}

// ---- controls --------------------------------------------------------------------------------------------------------------------

#[test]
fn every_batch_of_controls_is_shown_to_the_modules_after_the_driver_acted_on_it() {
    let c = Arc::new(Mutex::new(Vec::new()));
    let (h, _) = hooks(vec![Fake::new("messages", &c)]);
    let (controls, _cancel) = TurnControls::new();
    controls.install_hooks(&h);
    let report = controls.deliver(&[
        PendingControl::Cancel,
        PendingControl::Steer {
            text: "a kind the driver ignores".into(),
            now: false,
        },
    ]);
    assert!(report.cancel, "the driver acted on the cancel");
    assert_eq!(
        calls(&c),
        ["messages:controls 2"],
        "and the module saw both"
    );
    // An idle poll carries no control: nothing is shown.
    controls.deliver(&[]);
    assert_eq!(calls(&c).len(), 1);
}

#[test]
fn controls_with_no_module_are_handled_as_before() {
    let (controls, mut cancel) = TurnControls::new();
    controls.install_hooks(&Hooks::none());
    let report = controls.deliver(&[PendingControl::Cancel]);
    assert!(report.cancel);
    assert!(*cancel.borrow_and_update());
}

// ---- the recovery ladder ---------------------------------------------------------------------------------------------------------

#[test]
fn the_collector_tells_the_modules_each_ladder_step_from_the_functions_that_already_record_it() {
    let c = Arc::new(Mutex::new(Vec::new()));
    let (h, _) = hooks(vec![Fake::new("m", &c)]);
    let collector = DirectCollector::new(agentd::events::EventRecorder::new("run", "direct"))
        .with_hooks(h.clone());
    assert_eq!(collector.hooks().names(), ["m"]);
    collector.record_provider_retry(1, 4, "transient", 5, "a 429 from the provider");
    collector.record_provider_switch(&DirectSwitch {
        n: 1,
        at: "2026-10-16T12:00:00Z".into(),
        from: "model-a".into(),
        to: "model-b".into(),
        kind: DirectSwitchKind::Route,
        class: "transient".into(),
        detail: None,
    });
    collector.record_engine_stop("loop-guard", 1, 1, 5, "five identical calls in a row");
    collector.record_provider_wait(1, 30, "every route failed");
    collector.record_provider_outage(900);
    let id = collector.record_outage_notice(1800);
    collector.record_outage_recovered(&id, 1900);
    collector.record_notify("history_unusable", None, "the provider refuses the history");
    let kinds: Vec<String> = calls(&c)
        .into_iter()
        .map(|l| l.trim_start_matches("m:event ladder ").to_string())
        .collect();
    assert_eq!(
        kinds,
        [
            "provider-retry",
            "switch",
            "engine-stop",
            "wait",
            "outage",
            "outage-notice",
            "outage-recovered",
            "notify"
        ]
    );
}

// ---- a module that implements nothing, and the words of the events -------------------------------------------------------------

/// Implements only `name`: every default does nothing and votes Done.
struct Silent;

impl DirectHook for Silent {
    fn name(&self) -> &'static str {
        "silent"
    }
}

#[tokio::test]
async fn a_module_that_implements_only_its_name_is_asked_everything_and_answers_nothing() {
    let h = Hooks::new(vec![Arc::new(Silent) as Arc<dyn DirectHook>]);
    assert_eq!(h.names(), ["silent"]);
    assert!(!h.is_empty());
    assert!(h.tap().is_some());
    h.wire(Dir::ToEngine, &json!({"method": "initialize"}), false);
    h.event(&Event::RunEnded);
    h.controls(&[PendingControl::Cancel]);
    assert_eq!(ask(&h, false).await, TurnVote::Done);
    assert_eq!(ask(&h, true).await, TurnVote::Done);
}

#[test]
fn every_event_has_its_word() {
    let response = json!({});
    let words: Vec<&str> = [
        Event::SessionStarted(SessionStarted {
            session_id: "s",
            number: 1,
            response: &response,
            cwd: "/",
            trigger: None,
        }),
        Event::SessionEnded(SessionEnded {
            session_id: "s",
            number: 1,
            why: SessionEnd::Replaced,
        }),
        Event::PromptSent(PromptSent {
            no: 1,
            session_id: "s",
            text: "go",
        }),
        Event::PromptReplied(PromptReplied {
            no: 1,
            session_id: "s",
            stop_reason: Some("end_turn"),
            usage: None,
            error: None,
        }),
        Event::Ladder(LadderEvent::SilenceResume { detail: "quiet" }),
        Event::Cancel { session_id: "s" },
        Event::EngineStopped(EngineStop::Restarted),
        Event::RunEnded,
    ]
    .iter()
    .map(Event::name)
    .collect();
    assert_eq!(
        words,
        [
            "session-started",
            "session-ended",
            "prompt-sent",
            "prompt-replied",
            "ladder",
            "cancel",
            "engine-stopped",
            "run-ended"
        ]
    );
}
