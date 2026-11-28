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
use agentd::direct::driver::{NoProbe, NoWait, OwnEnd, OwnStop, PromptPlan, run_direct_prompt};
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

// ---- the run's deadline at a stop where the engine works by itself and no vote comes back --------------------------------

/// The facts of a stop at which the driver held the run for an engine that works by itself, the engine had answered the stop's prompt,
/// and the question after the hold brought no vote.
fn no_vote_after_a_hold() -> NoWait {
    NoWait {
        no_vote: true,
        worked_alone: true,
        still_alone: true,
        answered: true,
        cancel_up: false,
        cancel_sent: false,
        deadline_passed: false,
    }
}

/// The rule: the question after a hold brought no vote because the run's deadline had passed (no time was left when the question
/// read the clock, so no module was asked; or the vote came back after the deadline and was dropped). The driver ends the run as its time
/// limit and writes `session/cancel` once, as a hold does. Before, nothing ended the run there: it kept the outcome of the last answered
/// prompt and nothing told the engine.
#[test]
fn b902_no_vote_after_a_hold_with_the_deadline_passed_is_the_time_limits_end_with_one_cancel() {
    let stop = NoWait {
        deadline_passed: true,
        ..no_vote_after_a_hold()
    };
    assert_eq!(
        stop.end(),
        Some(OwnEnd {
            stop: OwnStop::TimeLimit,
            write_cancel: true
        }),
        "{stop:?}"
    );
}

/// The user's cancel keeps its precedence over the deadline, and a cancel that was written to the prompt
/// already is not written a second time.
#[test]
fn b902_the_users_cancel_comes_before_the_deadline_and_is_never_written_twice() {
    for deadline_passed in [false, true] {
        let stop = NoWait {
            cancel_up: true,
            deadline_passed,
            ..no_vote_after_a_hold()
        };
        assert_eq!(
            stop.end(),
            Some(OwnEnd {
                stop: OwnStop::User,
                write_cancel: true
            }),
            "{stop:?}"
        );
        let written = NoWait {
            cancel_sent: true,
            ..stop
        };
        assert_eq!(
            written.end(),
            Some(OwnEnd {
                stop: OwnStop::User,
                write_cancel: false
            }),
            "{written:?}"
        );
    }
}

/// Where the driver makes no end of its own, as before. The deadline has not passed and no cancel is up; a vote came back; the
/// prompt was not answered; the engine was not working by itself when the driver last looked (a prompt that was answered just before
/// the limit at a stop with no hold: the engine ended its turn in time, and the run keeps that outcome). And the deadline alone at a
/// stage the driver started itself after a held stop that the modules ended with no prompt of theirs (the review round, a person's
/// answer): that prompt was answered in time too. The user's cancel ends the run there as before.
#[test]
fn b902_with_a_vote_an_unanswered_prompt_or_no_work_alone_the_driver_makes_no_end_of_its_own() {
    assert_eq!(no_vote_after_a_hold().end(), None);
    for deadline_passed in [false, true] {
        for cancel_up in [false, true] {
            let stop = NoWait {
                deadline_passed,
                cancel_up,
                ..no_vote_after_a_hold()
            };
            for other in [
                NoWait {
                    no_vote: false,
                    ..stop
                },
                NoWait {
                    answered: false,
                    ..stop
                },
                NoWait {
                    worked_alone: false,
                    still_alone: false,
                    ..stop
                },
            ] {
                assert_eq!(other.end(), None, "{other:?}");
            }
        }
    }
    let own_stage = NoWait {
        still_alone: false,
        deadline_passed: true,
        ..no_vote_after_a_hold()
    };
    assert_eq!(own_stage.end(), None, "{own_stage:?}");
    let cancelled = NoWait {
        cancel_up: true,
        ..own_stage
    };
    assert_eq!(
        cancelled.end(),
        Some(OwnEnd {
            stop: OwnStop::User,
            write_cancel: true
        }),
        "{cancelled:?}"
    );
}

/// A module that hands out its votes in order. In the poll after the last of them it stays until the run's deadline has passed and then
/// gives its late vote; in every later poll it has no opinion.
struct ThenLate {
    votes: Mutex<VecDeque<TurnVote>>,
    late: TurnVote,
    /// What was left of the run's time when the late poll was asked.
    left_when_asked: Mutex<Option<Duration>>,
}

impl ThenLate {
    fn new(votes: Vec<TurnVote>, late: TurnVote) -> Arc<Self> {
        Arc::new(Self {
            votes: Mutex::new(votes.into()),
            late,
            left_when_asked: Mutex::new(None),
        })
    }

    /// TRIGGER PRESENT: the late poll was asked while time was left of the run, so its vote came back after the deadline.
    fn left_when_asked(&self) -> Duration {
        let left = self
            .left_when_asked
            .lock()
            .unwrap()
            .expect("the late poll was never asked: the test proves nothing");
        assert!(!left.is_zero(), "the late poll was asked with no time left");
        left
    }
}

impl DirectHook for ThenLate {
    fn name(&self) -> &'static str {
        "then-late"
    }

    fn on_turn_end<'a>(&'a self, end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move {
            let next = self.votes.lock().unwrap().pop_front();
            if let Some(vote) = next {
                return vote;
            }
            let first = self
                .left_when_asked
                .lock()
                .unwrap()
                .replace(end.time_left)
                .is_none();
            if first {
                tokio::time::sleep(end.time_left + Duration::from_secs(1)).await;
                return self.late.clone();
            }
            TurnVote::Done
        })
    }
}

/// Through the driver: the driver holds the run for an engine that works by itself and asks again while time is left; the answer
/// comes back after the run's deadline and is dropped, as every vote after the deadline is that is no `Wait` and no prompt beside a
/// `Wait`: no opinion, a stop, and a prompt of a poll in which no module said that the engine works. So the question after the
/// hold brings no vote and the deadline has passed. The run ends as its time limit, the engine is told once, and the late prompt is
/// not sent. Before, the run kept the outcome of the answered prompt (`reported_done` here) and nothing was written to the engine.
#[tokio::test(start_paused = true)]
async fn b902_no_vote_after_a_hold_past_the_deadline_ends_the_run_as_its_time_limit_and_tells_the_engine_once()
 {
    for late_vote in [
        TurnVote::Done,
        TurnVote::Stop {
            detail: "a stop that came back too late".into(),
        },
        TurnVote::Prompt("a prompt that came back too late".into()),
    ] {
        let late = ThenLate::new(
            vec![TurnVote::Wait {
                until: Duration::from_secs(1),
            }],
            late_vote.clone(),
        );
        let run = drive(
            vec![goes_on_by_itself()],
            plan(20, 900, None),
            vec![late.clone()],
            None,
        )
        .await;
        // the hold of 1 s came first: the late poll was the question after it
        assert_eq!(late.left_when_asked(), Duration::from_secs(19));
        assert_eq!(count(&run.received, "session/prompt"), 1, "{late_vote:?}");
        assert!(run.end.hook_stop.is_none(), "{late_vote:?}");
        assert_eq!(
            classify(&run.end.raw).outcome,
            DirectOutcome::StoppedTimeout,
            "the run kept the outcome of the answered prompt though its time ran out while the driver held it ({late_vote:?})"
        );
        assert_eq!(
            count(&run.received, "session/cancel"),
            1,
            "the engine is told once ({late_vote:?})"
        );
    }
}

/// The stop after a hold: the driver held the run, a module's prompt went out to the working engine and was answered (the engine
/// took it behind its round), and at that stop the modules' answer comes back after the run's deadline. The driver did not hold the run
/// at this stop, and the engine was working by itself when it last looked: the run ends as its time limit and the engine is told once,
/// as the user's cancel ends it there. Before, the run kept the outcome of that prompt and nothing told the engine.
#[tokio::test(start_paused = true)]
async fn b902_no_vote_at_the_stop_after_a_hold_past_the_deadline_ends_the_run_as_its_time_limit() {
    let late = ThenLate::new(
        vec![
            TurnVote::Wait {
                until: Duration::from_secs(1),
            },
            TurnVote::Prompt("go on".into()),
        ],
        TurnVote::Done,
    );
    let run = drive(
        vec![
            vec![
                chunk("first"),
                Step::Finish,
                Step::Sleep(2),
                chunk("still in the round"),
            ],
            vec![chunk("second"), Step::Finish],
        ],
        plan(20, 900, None),
        vec![late.clone()],
        None,
    )
    .await;
    // the prompt went out after the hold of 1 s and was answered when the engine's round of 2 s was over
    assert_eq!(late.left_when_asked(), Duration::from_secs(18));
    assert_eq!(count(&run.received, "session/prompt"), 2);
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedTimeout,
        "the run kept the outcome of the prompt the working engine answered though its time ran out"
    );
    assert_eq!(
        count(&run.received, "session/cancel"),
        1,
        "the engine is told once"
    );
}

/// Gives the deadline a second place to end a run that is held: the question after a hold. So the outcome alone no longer shows
/// a hold that misses the deadline, because the run would end as its time limit all the same, at the hold's own end. The clock shows it:
/// a hold that outlasts the time left is ended at the deadline.
#[tokio::test(start_paused = true)]
async fn b902_a_hold_that_outlasts_the_time_left_ends_at_the_deadline_and_not_at_its_own_end() {
    let long = ThenLate::new(
        vec![TurnVote::Wait {
            until: Duration::from_secs(600),
        }],
        TurnVote::Done,
    );
    let t0 = tokio::time::Instant::now();
    let run = drive(
        vec![goes_on_by_itself()],
        plan(20, 900, None),
        vec![long],
        None,
    )
    .await;
    let took = t0.elapsed();
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedTimeout
    );
    assert_eq!(count(&run.received, "session/cancel"), 1);
    assert!(
        took >= Duration::from_secs(20) && took < Duration::from_secs(25),
        "the hold of 600 s was not ended at the deadline of 20 s: the run took {took:?}"
    );
}

/// A module that says once that the engine works by itself. In its second poll (the question after the hold) it raises the user's
/// cancel and stays until the run's deadline has passed.
struct CancelThenLate {
    tx: tokio::sync::watch::Sender<bool>,
    polls: AtomicU32,
    left_at_second: Mutex<Option<Duration>>,
}

impl DirectHook for CancelThenLate {
    fn name(&self) -> &'static str {
        "cancel-then-late"
    }

    fn on_turn_end<'a>(&'a self, end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        let n = self.polls.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move {
            match n {
                1 => TurnVote::Wait {
                    until: Duration::from_secs(1),
                },
                2 => {
                    *self.left_at_second.lock().unwrap() = Some(end.time_left);
                    let _ = self.tx.send(true);
                    tokio::time::sleep(end.time_left + Duration::from_secs(1)).await;
                    TurnVote::Done
                }
                _ => TurnVote::Done,
            }
        })
    }
}

/// Beside, through the driver: at the question after a hold the user's cancel comes up and the run's deadline
/// passes before the vote is back, so both are up at one stop. The user's cancel comes first (`NoWait::end`): the run ends as the
/// user's stop and the engine is told once. With the deadline asked first the run would end as its time limit.
#[tokio::test(start_paused = true)]
async fn b902_the_cancel_and_the_deadline_both_up_after_a_hold_end_the_run_as_the_users_stop_with_one_cancel()
 {
    let (tx, rx) = tokio::sync::watch::channel(false);
    let both = Arc::new(CancelThenLate {
        tx,
        polls: AtomicU32::new(0),
        left_at_second: Mutex::new(None),
    });
    let run = drive(
        vec![goes_on_by_itself()],
        plan(20, 900, Some(rx)),
        vec![both.clone()],
        None,
    )
    .await;
    // TRIGGER PRESENT: the second poll was asked with time left, after the hold of 1 s
    assert_eq!(
        both.left_at_second
            .lock()
            .unwrap()
            .expect("the second poll was never asked"),
        Duration::from_secs(19)
    );
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::StoppedByUser);
    assert_eq!(count(&run.received, "session/cancel"), 1);
    assert_eq!(count(&run.received, "session/prompt"), 1);
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

// ---- the run's deadline inside a poll -------------------------------------------------------------------------------------

/// A module that hands out its votes in order and then has no opinion.
struct Votes {
    name: &'static str,
    votes: Mutex<VecDeque<TurnVote>>,
}

impl Votes {
    fn new(name: &'static str, votes: Vec<TurnVote>) -> Arc<Self> {
        Arc::new(Self {
            name,
            votes: Mutex::new(votes.into()),
        })
    }
}

impl DirectHook for Votes {
    fn name(&self) -> &'static str {
        self.name
    }

    fn on_turn_end<'a>(&'a self, _end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move {
            self.votes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(TurnVote::Done)
        })
    }
}

/// A module that stays in the first poll until the run's deadline has passed, and notes what the poll told it about the engine.
struct PastTheDeadline {
    done: AtomicBool,
    engine_working: AtomicBool,
}

impl PastTheDeadline {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            done: AtomicBool::new(false),
            engine_working: AtomicBool::new(false),
        })
    }
}

impl DirectHook for PastTheDeadline {
    fn name(&self) -> &'static str {
        "past-the-deadline"
    }

    fn on_turn_end<'a>(&'a self, end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move {
            if !self.done.swap(true, Ordering::SeqCst) {
                self.engine_working
                    .store(end.engine_working, Ordering::SeqCst);
                tokio::time::sleep(end.time_left + Duration::from_secs(1)).await;
            }
            TurnVote::Done
        })
    }
}

/// An engine that answers the prompt and goes on by itself for a long time.
fn goes_on_by_itself() -> Vec<Step> {
    std::iter::once(chunk("first"))
        .chain(std::iter::once(Step::Finish))
        .chain((0..30).flat_map(|i| [Step::Sleep(3), chunk(&format!("line {i}"))]))
        .collect()
}

/// The run's deadline comes up while the modules are asked, in a poll in which one module says that the engine still works
/// by itself and a later one votes a prompt. The prompt is not sent. The run is held as for the wait the poll heard, and the hold
/// ends it for the deadline at once: the cancel is written to the engine once and the stop is the time limit's. Before, nothing
/// was kept of what the poll heard, nothing told the engine, and the run ended with the outcome of the answered prompt.
#[tokio::test(start_paused = true)]
async fn b980_the_deadline_in_a_poll_with_a_wait_and_a_prompt_ends_the_run_as_its_time_limit() {
    let goal = Votes::new(
        "goal",
        vec![TurnVote::Wait {
            until: Duration::from_secs(1),
        }],
    );
    let messages = Votes::new(
        "messages",
        vec![TurnVote::Prompt("a message of the user".into())],
    );
    let late = PastTheDeadline::new();
    let run = drive(
        vec![goes_on_by_itself()],
        plan(20, 900, None),
        vec![goal, messages, late.clone()],
        None,
    )
    .await;
    // TRIGGER PRESENT: the module that stayed past the deadline was told in that poll that the engine works by itself.
    assert!(
        late.engine_working.load(Ordering::SeqCst),
        "no module said in that poll that the engine works by itself: the test proves nothing"
    );
    assert_eq!(
        count(&run.received, "session/prompt"),
        1,
        "no prompt goes out after the deadline"
    );
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedTimeout,
        "the run kept the outcome of the answered prompt though the engine was working when the time ran out"
    );
    assert_eq!(
        count(&run.received, "session/cancel"),
        1,
        "the engine is told once"
    );
}

/// The other side: a prompt that comes back after the deadline from a poll in which no module said that the engine works
/// is dropped as before, and nothing is kept: the run ends with the outcome of the answered prompt and nothing is written to the
/// engine.
#[tokio::test(start_paused = true)]
async fn b980_a_prompt_after_the_deadline_from_a_poll_with_no_wait_is_dropped_and_nothing_is_kept()
{
    let messages = Votes::new(
        "messages",
        vec![TurnVote::Prompt("a message of the user".into())],
    );
    let late = PastTheDeadline::new();
    let run = drive(
        vec![vec![chunk("first"), Step::Finish]],
        plan(20, 900, None),
        vec![messages, late.clone()],
        None,
    )
    .await;
    assert!(late.done.load(Ordering::SeqCst), "the poll was not made");
    assert!(!late.engine_working.load(Ordering::SeqCst));
    assert_eq!(count(&run.received, "session/prompt"), 1);
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(count(&run.received, "session/cancel"), 0);
}
