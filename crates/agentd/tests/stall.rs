//! Provider-stall watchdog: the single turn timeout becomes a select of
//! the turn and a ticker. After `session/prompt` is sent, engine silence of `providerSilenceS`
//! (default 900 s) with no tool call running ends the turn as a stall,
//! which is distinct from the whole-turn timeout. The engine here is in-process and the clock is
//! paused, so 900 s of silence takes no wall time.
#![allow(clippy::unwrap_used)]

use agentd::acp::{
    AcpClient, AcpError, Activity, DEFAULT_PROVIDER_SILENCE_S, STALL_TICK, WatchLimits, Watched,
    watch,
};
use agentd::direct::collector::DirectCollector;
use agentd::events::EventRecorder;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::Instant;

const WT: &str = "/mnt/workspace/run1/a1/repo";

#[derive(Clone, Debug)]
enum Step {
    /// A `session/update` notification carrying this `update`.
    Update(Value),
    /// Virtual seconds of silence.
    Sleep(u64),
    /// The `session/prompt` response.
    Finish,
    /// Wait for the client's `session/cancel`, then answer the prompt `cancelled`.
    AwaitCancel,
    /// The engine's own idle-timeout error as the answer to the prompt.
    TimeoutError,
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

fn tool_done(id: &str, status: &str) -> Step {
    Step::Update(json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": status}))
}

fn frame(v: Value) -> String {
    format!("{v}\n")
}

type Client = AcpClient<tokio::io::DuplexStream, tokio::io::DuplexStream>;

/// An in-process engine: it answers the handshake at once and runs `steps` for `session/prompt`.
/// With `hang_initialize` it never answers anything.
fn engine(steps: Vec<Step>, hang_initialize: bool) -> Client {
    let (client_w, engine_r) = tokio::io::duplex(1 << 16);
    let (mut out, client_r) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        let mut lines = BufReader::new(engine_r).lines();
        while let Ok(Some(received)) = lines.next_line().await {
            let msg: Value = serde_json::from_str(&received).unwrap();
            if hang_initialize {
                continue;
            }
            let id = msg.get("id").cloned();
            let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
            let method = msg["method"].as_str().unwrap_or_default().to_string();
            match method.as_str() {
                "initialize" => {
                    let l = frame(reply(
                        json!({"protocolVersion": 1, "agentInfo": {"name": "scripted", "version": "9.9.9"}}),
                    ));
                    out.write_all(l.as_bytes()).await.unwrap();
                }
                "session/new" => {
                    let l = frame(reply(
                        json!({"sessionId": "s-1", "configOptions": [{"id": "model", "currentValue": "m", "options": []}]}),
                    ));
                    out.write_all(l.as_bytes()).await.unwrap();
                }
                "session/set_config_option" => {
                    let l = frame(reply(
                        json!({"configOptions": [{"id": "model", "currentValue": "us.anthropic.claude-sonnet-5"}]}),
                    ));
                    out.write_all(l.as_bytes()).await.unwrap();
                }
                "session/prompt" => {
                    for step in steps.clone() {
                        match step {
                            Step::Update(u) => {
                                let l = frame(
                                    json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s-1", "update": u}}),
                                );
                                out.write_all(l.as_bytes()).await.unwrap();
                            }
                            Step::Sleep(s) => tokio::time::sleep(Duration::from_secs(s)).await,
                            Step::AwaitCancel => {
                                while let Ok(Some(l)) = lines.next_line().await {
                                    let m: Value = serde_json::from_str(&l).unwrap();
                                    if m["method"] == "session/cancel" {
                                        break;
                                    }
                                }
                                let l = frame(reply(json!({"stopReason": "cancelled"})));
                                out.write_all(l.as_bytes()).await.unwrap();
                            }
                            Step::TimeoutError => {
                                let l = frame(json!({"jsonrpc": "2.0", "id": id, "error": {
                                    "code": -32603, "message": "Internal error: The operation timed out.",
                                    "data": {"message": "The operation timed out."}}}));
                                out.write_all(l.as_bytes()).await.unwrap();
                            }
                            Step::Finish => {
                                let l = frame(reply(json!({"stopReason": "end_turn"})));
                                out.write_all(l.as_bytes()).await.unwrap();
                            }
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
    AcpClient::new(client_r, client_w)
}

/// What a run sends: the working directory and the prompt text.
struct Plan {
    cwd: String,
    prompt: String,
}

fn plan() -> Plan {
    Plan {
        cwd: WT.into(),
        prompt: "Add percent_change".into(),
    }
}

/// What the exchange ended with.
#[derive(Debug)]
struct TurnResult {
    stop_reason: String,
}

/// The exchange of a run as `direct::driver` sends it, without its model check: `initialize`, `session/new`, one `session/prompt`.
async fn run_acp_turn(
    client: &mut Client,
    plan: &Plan,
    collector: &mut DirectCollector,
) -> Result<TurnResult, AcpError> {
    client
        .request("initialize", json!({"protocolVersion": 1}), collector)
        .await?;
    let new = client
        .request(
            "session/new",
            json!({ "cwd": plan.cwd, "mcpServers": [] }),
            collector,
        )
        .await?;
    let session_id = new["sessionId"].as_str().unwrap().to_string();
    let p = client
        .request(
            "session/prompt",
            json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": plan.prompt }] }),
            collector,
        )
        .await?;
    Ok(TurnResult {
        stop_reason: p["stopReason"].as_str().unwrap_or("unknown").to_string(),
    })
}

fn limits(timeout_s: u64, silence_s: u64) -> WatchLimits {
    WatchLimits::new(
        Duration::from_secs(timeout_s),
        Duration::from_secs(silence_s),
    )
}

/// Runs one turn against `steps` under `limits`; returns the outcome, the virtual seconds it took
/// and the activity record.
async fn run(
    steps: Vec<Step>,
    limits: WatchLimits,
    collector: DirectCollector,
) -> (
    Watched<Result<TurnResult, AcpError>>,
    Duration,
    Arc<Activity>,
) {
    let activity = Activity::new();
    let mut client = engine(steps, false).with_activity(activity.clone());
    let mut collector = collector;
    let t0 = Instant::now();
    let out = watch(
        run_acp_turn(&mut client, &plan(), &mut collector),
        limits,
        &activity,
    )
    .await;
    (out, t0.elapsed(), activity)
}

fn collector() -> DirectCollector {
    DirectCollector::new(EventRecorder::new("run1", "a1"))
}

#[tokio::test(start_paused = true)]
async fn a_silent_engine_stalls_the_turn_at_the_budget() {
    let (out, took, activity) = run(vec![Step::Silent], limits(5400, 900), collector()).await;
    let Watched::Stalled { silent } = out else {
        panic!("expected a stall, got {out:?}");
    };
    assert!(
        (900..906).contains(&silent.as_secs()),
        "silent for {silent:?}"
    );
    assert!(
        (900..910).contains(&took.as_secs()),
        "the stall came after {took:?}"
    );
    assert!(activity.silent_for() >= Duration::from_secs(900));
}

#[tokio::test(start_paused = true)]
async fn a_long_tool_call_is_not_a_stall() {
    let steps = vec![
        chunk("Running the suite. "),
        tool("c1", "in_progress"),
        Step::Sleep(3000),
        tool_done("c1", "completed"),
        Step::Finish,
    ];
    let (out, took, activity) = run(steps, limits(5400, 900), collector()).await;
    let Watched::Done(Ok(r)) = out else {
        panic!("a tool call for 3000 s must not stall: {out:?}");
    };
    assert_eq!(r.stop_reason, "end_turn");
    assert!(took >= Duration::from_secs(3000));
    assert_eq!(activity.tools_in_flight(), 0);
    assert!(
        activity.max_silent_gap() < Duration::from_secs(5),
        "tool time is not provider silence: {:?}",
        activity.max_silent_gap()
    );
}

#[tokio::test(start_paused = true)]
async fn silence_counts_again_from_the_end_of_the_last_tool_call() {
    let steps = vec![
        tool("c1", "in_progress"),
        Step::Sleep(2000),
        tool_done("c1", "completed"),
        Step::Silent,
    ];
    let (out, took, _) = run(steps, limits(5400, 900), collector()).await;
    let Watched::Stalled { silent } = out else {
        panic!("expected a stall after the tool call: {out:?}");
    };
    assert!(
        (900..906).contains(&silent.as_secs()),
        "silent for {silent:?}"
    );
    assert!(
        (2900..2912).contains(&took.as_secs()),
        "the stall came {took:?} into the turn: the clock restarts when the tool ends"
    );
}

#[tokio::test(start_paused = true)]
async fn steady_progress_is_not_a_stall_however_long_the_turn_runs() {
    let mut steps = Vec::new();
    for i in 0..5 {
        steps.push(Step::Sleep(800));
        steps.push(chunk(&format!("part {i}. ")));
    }
    steps.push(Step::Finish);
    let (out, took, activity) = run(steps, limits(5400, 900), collector()).await;
    assert!(matches!(out, Watched::Done(Ok(_))), "{out:?}");
    assert!(took >= Duration::from_secs(4000));
    assert_eq!(
        activity.max_silent_gap().as_secs(),
        800,
        "the longest gap between two engine updates"
    );
}

#[tokio::test(start_paused = true)]
async fn the_longest_gap_between_updates_is_recorded() {
    let steps = vec![
        Step::Sleep(100),
        chunk("a"),
        Step::Sleep(120),
        chunk("b"),
        Step::Sleep(50),
        Step::Finish,
    ];
    let (out, _, activity) = run(steps, limits(5400, 900), collector()).await;
    assert!(matches!(out, Watched::Done(Ok(_))), "{out:?}");
    assert_eq!(activity.max_silent_gap().as_secs(), 120);
}

#[tokio::test(start_paused = true)]
async fn the_whole_turn_timeout_keeps_its_own_outcome() {
    // A tool call outlasts the 1000 s turn timeout: that is a timeout, not a stall.
    let steps = vec![tool("c1", "in_progress"), Step::Sleep(6000), Step::Finish];
    let (out, took, _) = run(steps, limits(1000, 900), collector()).await;
    assert!(matches!(out, Watched::TimedOut), "{out:?}");
    assert!((1000..1010).contains(&took.as_secs()), "took {took:?}");
}

#[tokio::test(start_paused = true)]
async fn an_engine_that_never_answers_the_handshake_is_not_a_provider_stall() {
    // Nothing has asked the model yet, so the silence is an engine hang: the whole-turn timeout
    // bounds it, as before.
    let activity = Activity::new();
    let mut client = engine(vec![], true).with_activity(activity.clone());
    let mut c = collector();
    let out = watch(
        run_acp_turn(&mut client, &plan(), &mut c),
        limits(2000, 900),
        &activity,
    )
    .await;
    assert!(matches!(out, Watched::TimedOut), "{out:?}");
}

#[tokio::test(start_paused = true)]
async fn a_finished_turn_wins_over_a_due_tick() {
    let (out, _, _) = run(vec![Step::Finish], limits(5400, 900), collector()).await;
    assert!(matches!(out, Watched::Done(Ok(_))), "{out:?}");
}

#[tokio::test(start_paused = true)]
async fn the_activity_clock_resets_on_a_touch_and_only_runs_after_a_prompt() {
    let a = Activity::new();
    tokio::time::advance(Duration::from_secs(2000)).await;
    assert_eq!(a.silent_for(), Duration::ZERO, "no prompt yet");
    a.prompt_sent();
    tokio::time::advance(Duration::from_secs(800)).await;
    assert_eq!(a.silent_for().as_secs(), 800);
    a.touch();
    assert_eq!(a.silent_for(), Duration::ZERO);
    tokio::time::advance(Duration::from_secs(10)).await;
    assert_eq!(a.silent_for().as_secs(), 10);
    // A nudge sends another prompt: the clock restarts.
    a.prompt_sent();
    assert_eq!(a.silent_for(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn only_a_running_tool_call_holds_the_watchdog_open() {
    // ACP `pending` is a call whose input is still streaming or awaiting approval; OpenCode
    // announces `tool-input-start` that way and relays no input deltas, so a provider hang there is
    // the provider's silence. Only `in_progress` is a tool running.
    let a = Activity::new();
    a.prompt_sent();
    let update = |v: Value| a.observe_update(&v);
    update(json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "status": "pending"}));
    update(json!({"sessionUpdate": "tool_call", "toolCallId": "c2"}));
    assert_eq!(
        a.tools_in_flight(),
        0,
        "announced, not started: pending, and no status is pending"
    );
    tokio::time::advance(Duration::from_secs(600)).await;
    assert_eq!(
        a.silent_for().as_secs(),
        600,
        "the clock runs while a call is only announced"
    );
    update(
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "in_progress"}),
    );
    assert_eq!(a.tools_in_flight(), 1);
    assert_eq!(
        a.silent_for(),
        Duration::ZERO,
        "a running call holds the clock"
    );
    update(json!({"sessionUpdate": "tool_call", "toolCallId": "c3", "status": "completed"}));
    assert_eq!(
        a.tools_in_flight(),
        1,
        "a call created finished is not in flight"
    );
    update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "title": "renamed"}));
    assert_eq!(
        a.tools_in_flight(),
        1,
        "an update with no status changes nothing"
    );
    update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "failed"}));
    assert_eq!(a.tools_in_flight(), 0, "failed closes it");
    update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "zz", "status": "completed"}));
    assert_eq!(a.tools_in_flight(), 0, "an unknown id ends nothing");
    update(
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "x"}}),
    );
    assert_eq!(a.tools_in_flight(), 0);
}

#[test]
fn a_status_never_goes_back_to_an_earlier_one() {
    // Cline sends `tool_call_update in_progress` before the `tool_call pending` announcement of the
    // same id (engine fault probe, ok_tool): the late `pending` must not end the running call, and
    // a `completed` that arrives before its announcement stays completed.
    let a = Activity::new();
    a.prompt_sent();
    let update = |v: Value| a.observe_update(&v);
    update(
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "in_progress"}),
    );
    update(json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "status": "pending"}));
    assert_eq!(
        a.tools_in_flight(),
        1,
        "the late pending announcement does not regress it"
    );
    update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed"}));
    assert_eq!(a.tools_in_flight(), 0);
    update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "t2", "status": "completed"}));
    update(json!({"sessionUpdate": "tool_call", "toolCallId": "t2", "status": "pending"}));
    assert_eq!(a.tools_in_flight(), 0);
    assert_eq!(a.tool_result(&["t2".into()]), "completed");
    a.prompt_ended();
}

#[test]
fn the_result_of_watched_tool_calls_is_the_best_status_any_of_them_reached() {
    let a = Activity::new();
    a.prompt_sent();
    let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        a.tool_result(&ids(&["a", "b"])),
        "absent",
        "never announced"
    );
    a.observe_update(
        &json!({"sessionUpdate": "tool_call", "toolCallId": "a", "status": "pending"}),
    );
    assert_eq!(a.tool_result(&ids(&["a", "b"])), "pending");
    a.observe_update(
        &json!({"sessionUpdate": "tool_call_update", "toolCallId": "b", "status": "in_progress"}),
    );
    assert_eq!(a.tool_result(&ids(&["a", "b"])), "in_progress");
    a.observe_update(
        &json!({"sessionUpdate": "tool_call_update", "toolCallId": "a", "status": "failed"}),
    );
    assert_eq!(a.tool_result(&ids(&["a", "b"])), "failed");
    a.observe_update(
        &json!({"sessionUpdate": "tool_call_update", "toolCallId": "b", "status": "completed"}),
    );
    assert_eq!(
        a.tool_result(&ids(&["a", "b"])),
        "completed",
        "one that ran is enough"
    );
    a.prompt_ended();
    assert_eq!(
        a.tool_result(&ids(&["a", "b"])),
        "completed",
        "the record outlives the prompt: it is read at the turn's end"
    );
}

#[tokio::test(start_paused = true)]
async fn a_stall_after_a_tool_was_announced_but_never_started_is_detected() {
    // The provider hangs while the model streams a tool call's input: OpenCode has sent `tool_call
    // pending` and nothing after it.
    let (out, took, _) = run(
        vec![tool("t1", "pending"), Step::Silent],
        limits(5400, 900),
        collector(),
    )
    .await;
    let Watched::Stalled { silent } = out else {
        panic!("a call that is only announced must not hold the watchdog open: {out:?}");
    };
    assert!(
        (900..906).contains(&silent.as_secs()),
        "silent for {silent:?}"
    );
    assert!((900..910).contains(&took.as_secs()), "took {took:?}");
}

#[tokio::test(start_paused = true)]
async fn a_tool_that_started_still_runs_past_the_budget_after_a_late_announcement() {
    // Cline's order: `in_progress` first, then the `pending` announcement.
    let steps = vec![
        Step::Update(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "in_progress"}),
        ),
        tool("t1", "pending"),
        Step::Sleep(3000),
        tool_done("t1", "completed"),
        Step::Finish,
    ];
    let (out, took, activity) = run(steps, limits(5400, 900), collector()).await;
    assert!(matches!(out, Watched::Done(Ok(_))), "{out:?}");
    assert!(took >= Duration::from_secs(3000));
    assert_eq!(activity.tools_in_flight(), 0);
}

#[tokio::test(start_paused = true)]
async fn the_silence_of_an_engine_timeout_is_the_gap_before_the_notification_that_precedes_it() {
    // Both real engines send a notification about 1 ms before the timeout error (Cline:
    // session_info_update at 302.9342 s, the error at 302.9351 s), so the gap the error line ended
    // is about 1 ms and the silence is the gap the notification ended.
    let steps = vec![
        Step::Sleep(300),
        chunk("x"),
        Step::Sleep(0),
        Step::TimeoutError,
    ];
    let (out, _, activity) = run(steps, limits(5400, 900), collector()).await;
    assert!(matches!(out, Watched::Done(Err(_))), "{out:?}");
    assert_eq!(activity.error_gap().as_secs(), 300);
    assert!(
        activity.last_gap() < Duration::from_secs(1),
        "the error line itself ended a short gap"
    );
    // With no notification the error line's own gap is the silence.
    let (_, _, activity) = run(
        vec![Step::Sleep(250), Step::TimeoutError],
        limits(5400, 900),
        collector(),
    )
    .await;
    assert_eq!(activity.error_gap().as_secs(), 250);
}

#[tokio::test(start_paused = true)]
async fn an_unfinished_tool_call_is_closed_when_the_prompt_answers() {
    // The engine reports a call and never its end, then answers the prompt: the next prompt of
    // the session (a nudge) must not inherit an outstanding tool.
    let steps = vec![tool("c1", "in_progress"), Step::Finish];
    let (out, _, activity) = run(steps, limits(5400, 900), collector()).await;
    assert!(matches!(out, Watched::Done(Ok(_))), "{out:?}");
    assert_eq!(activity.tools_in_flight(), 0);
}

#[test]
fn the_budget_is_900_s_and_the_ticker_is_5_s() {
    assert_eq!(DEFAULT_PROVIDER_SILENCE_S, 900);
    assert_eq!(STALL_TICK, Duration::from_secs(5));
    let l = limits(5400, 900);
    assert_eq!(
        (l.timeout.as_secs(), l.silence.as_secs(), l.tick),
        (5400, 900, STALL_TICK)
    );
    // A short budget ticks four times inside the budget, never faster than 100 ms.
    assert_eq!(limits(60, 2).tick, Duration::from_millis(500));
    assert_eq!(
        WatchLimits::new(Duration::from_secs(60), Duration::from_millis(10)).tick,
        Duration::from_millis(100)
    );
}

#[tokio::test(start_paused = true)]
async fn the_gap_that_the_last_engine_line_ended_is_kept() {
    let steps = vec![Step::Sleep(100), chunk("a"), Step::Sleep(120), Step::Finish];
    let (out, _, activity) = run(steps, limits(5400, 900), collector()).await;
    assert!(matches!(out, Watched::Done(Ok(_))), "{out:?}");
    assert_eq!(
        activity.last_gap().as_secs(),
        120,
        "the answer ended a 120 s silence"
    );
    // Tool time is not silence.
    let steps = vec![
        tool("c1", "in_progress"),
        Step::Sleep(500),
        tool_done("c1", "completed"),
        Step::Finish,
    ];
    let (_, _, activity) = run(steps, limits(5400, 900), collector()).await;
    assert_eq!(activity.last_gap(), Duration::ZERO);
}

#[test]
fn a_cancel_is_noted_for_the_prompt_it_was_written_in() {
    let a = Activity::new();
    a.prompt_sent();
    assert!(!a.cancel_sent());
    a.cancel_written();
    assert!(a.cancel_sent());
    a.prompt_sent(); // a nudge's prompt starts clean
    assert!(!a.cancel_sent());
}

#[tokio::test(start_paused = true)]
async fn the_client_notes_the_cancel_it_sends_and_only_that() {
    // A cancel signal raised while the prompt is pending makes the client write session/cancel.
    let activity = Activity::new();
    let (tx, rx) = tokio::sync::watch::channel(false);
    let mut client = engine(vec![Step::AwaitCancel], false)
        .with_cancel(rx, Duration::from_secs(10))
        .with_activity(activity.clone());
    let mut c = collector();
    let raise = async {
        tokio::time::sleep(Duration::from_secs(30)).await;
        tx.send_replace(true);
    };
    let p = plan();
    let (r, ()) = tokio::join!(run_acp_turn(&mut client, &p, &mut c), raise);
    assert_eq!(r.unwrap().stop_reason, "cancelled");
    assert!(activity.cancel_sent());
    // No cancel: the same stop reason would be the engine's own doing.
    let activity = Activity::new();
    let mut client = engine(vec![Step::Finish], false).with_activity(activity.clone());
    let mut c = collector();
    run_acp_turn(&mut client, &plan(), &mut c).await.unwrap();
    assert!(!activity.cancel_sent());
}
