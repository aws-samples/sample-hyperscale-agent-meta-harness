//! The second slice of provider failover over in-memory streams with a scripted engine and a paused clock: after the
//! resumes on the user's model are spent, agentd switches the SAME engine session to the next route (`session/set_config_option`),
//! records the switch, waits and probes when every route has failed, returns to the preferred route when it may, and stops at the
//! user's cancel. The decisions themselves are pinned in `direct_ladder.rs`; here the wire and the records.
#![allow(clippy::unwrap_used)]

use agentd::acp::{AcpClient, Activity};
use agentd::direct::classify::classify;
use agentd::direct::collector::DirectCollector;
use agentd::direct::driver::{DriverEnd, NoProbe, PromptPlan, run_direct_prompt};
use agentd::direct::failover::{
    FailoverPlan, FailoverRecord, OUTAGE_NOTICE_AFTER_S, ProbeAnswer, Route, RouteProbe,
};
use agentd::direct::hooks::{DirectHook, Event, Hooks, LadderEvent};
use agentd::direct::retry::{
    RESUME_PROMPT, RESUME_PROMPT_INTERRUPTED, RetryPolicy, prompt_with_retry,
    prompt_with_retry_from,
};
use agentd::events::{EventRecorder, TurnControls};
use harness_protocol::api::internal::PendingControl;
use harness_protocol::direct::{
    DirectAttemptKind, DirectAttemptResult, DirectOutcome, DirectSwitchKind,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const OPUS_US: &str = "us.anthropic.claude-opus-5-5";
const OPUS_GLOBAL: &str = "global.anthropic.claude-opus-5-5";
const SONNET_GLOBAL: &str = "global.anthropic.claude-sonnet-5-5";
const SOL_US: &str = "us.openai.gpt-6.1-sol";
const TOP: &str = "/mnt/workspace/run1/direct/repo";
const PROMPT: &str = "Add percent_change with tests";

#[derive(Clone, Debug)]
enum Step {
    Update(Value),
    Finish,
    /// The prompt's response is a JSON-RPC error with `data`.
    Fail(&'static str, Value),
    /// The prompt's response is `cancelled`, though nobody asked: Cline ending the run itself.
    Cancelled,
    /// The prompt's response is this stop reason.
    Stop(&'static str),
    /// The engine does nothing for this many seconds (the clock is paused: it only moves time).
    Pause(u64),
    /// The engine never answers this prompt.
    Hang,
    /// The engine ends its output: a closed pipe.
    Close,
}

fn say(text: &str) -> Step {
    Step::Update(
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
    )
}

/// 503 from the provider: a transient failure.
fn transient() -> Vec<Step> {
    vec![Step::Fail(
        "Internal error: Service unavailable",
        json!({"isRetryable": true, "statusCode": 503, "errorName": "ServiceUnavailableException"}),
    )]
}

/// The provider refuses the route itself.
fn unavailable() -> Vec<Step> {
    vec![Step::Fail(
        "Internal error: The provided model identifier is invalid.",
        json!({"statusCode": 400, "errorName": "ValidationException", "isRetryable": false}),
    )]
}

/// A request the provider refuses: permanent.
fn permanent() -> Vec<Step> {
    vec![Step::Fail(
        "Internal error: Malformed input request",
        json!({"statusCode": 400, "errorName": "ValidationException", "isRetryable": false}),
    )]
}

fn done(text: &str) -> Vec<Step> {
    vec![say(text), Step::Finish]
}

/// How the engine answers `session/set_config_option`.
#[derive(Clone, Debug, Default)]
enum Setting {
    /// Accept every id and report it back.
    #[default]
    Accept,
    /// Refuse the ids in the list with an error; accept the rest.
    Refuse(Vec<&'static str>),
    /// Accept, but report the first model the session had.
    Mismatch,
    /// Never answer.
    Silent,
}

struct Script {
    prompts: Vec<Vec<Step>>,
    setting: Setting,
}

impl Script {
    fn new(prompts: Vec<Vec<Step>>) -> Self {
        Self {
            prompts,
            setting: Setting::Accept,
        }
    }
}

type Client = AcpClient<tokio::io::DuplexStream, tokio::io::DuplexStream>;
/// `(method, params)` of every message the engine received, and `(prompt text, model the session was on)` of every prompt.
type Wire = Arc<Mutex<Vec<(String, Value)>>>;
type Prompts = Arc<Mutex<Vec<(String, String)>>>;

fn frame(v: Value) -> String {
    format!("{v}\n")
}

fn engine(script: Script, first_model: &'static str) -> (Client, Wire, Prompts) {
    let (client_w, engine_r) = tokio::io::duplex(1 << 16);
    let (mut out, client_r) = tokio::io::duplex(1 << 16);
    let wire: Wire = Arc::default();
    let prompts: Prompts = Arc::default();
    let (seen, seen_prompts) = (wire.clone(), prompts.clone());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let mut lines = BufReader::new(engine_r).lines();
        while let Ok(Some(raw)) = lines.next_line().await {
            let msg: Value = serde_json::from_str(&raw).unwrap();
            seen.lock().unwrap().push((
                msg["method"].as_str().unwrap_or_default().to_string(),
                msg["params"].clone(),
            ));
            let _ = tx.send(msg);
        }
    });
    tokio::spawn(async move {
        let mut current = first_model.to_string();
        let mut prompt_no = 0usize;
        while let Some(msg) = rx.recv().await {
            let id = msg.get("id").cloned();
            let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
            let options = |m: &str| json!([{"id": "model", "currentValue": m, "options": []}]);
            match msg["method"].as_str().unwrap_or_default() {
                "initialize" => out
                    .write_all(frame(reply(json!({"protocolVersion": 1, "agentInfo": {"name": "scripted", "version": "9.9.9"}}))).as_bytes())
                    .await
                    .unwrap(),
                "session/new" => out
                    .write_all(frame(reply(json!({"sessionId": "s-1", "configOptions": options(&current)}))).as_bytes())
                    .await
                    .unwrap(),
                "session/set_config_option" => {
                    let want = msg["params"]["value"].as_str().unwrap_or_default().to_string();
                    match &script.setting {
                        Setting::Silent => {}
                        Setting::Refuse(ids) if ids.contains(&want.as_str()) => out
                            .write_all(frame(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": format!("Model \"{want}\" cannot be routed")}})).as_bytes())
                            .await
                            .unwrap(),
                        Setting::Mismatch => out
                            .write_all(frame(reply(json!({"configOptions": options(&current)}))).as_bytes())
                            .await
                            .unwrap(),
                        _ => {
                            current = want;
                            out.write_all(frame(reply(json!({"configOptions": options(&current)}))).as_bytes())
                                .await
                                .unwrap();
                        }
                    }
                }
                "session/prompt" => {
                    let text = msg["params"]["prompt"][0]["text"].as_str().unwrap_or_default().to_string();
                    seen_prompts.lock().unwrap().push((text, current.clone()));
                    let steps = script.prompts.get(prompt_no).cloned().unwrap_or_else(|| done("Done."));
                    prompt_no += 1;
                    for step in steps {
                        match step {
                            Step::Update(u) => out
                                .write_all(frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s-1", "update": u}})).as_bytes())
                                .await
                                .unwrap(),
                            Step::Finish => out
                                .write_all(frame(reply(json!({"stopReason": "end_turn"}))).as_bytes())
                                .await
                                .unwrap(),
                            Step::Fail(message, data) => out
                                .write_all(frame(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": message, "data": data}})).as_bytes())
                                .await
                                .unwrap(),
                            Step::Cancelled => out
                                .write_all(frame(reply(json!({"stopReason": "cancelled"}))).as_bytes())
                                .await
                                .unwrap(),
                            Step::Stop(reason) => out
                                .write_all(frame(reply(json!({"stopReason": reason}))).as_bytes())
                                .await
                                .unwrap(),
                            Step::Pause(secs) => tokio::time::sleep(Duration::from_secs(secs)).await,
                            Step::Hang => std::future::pending::<()>().await,
                            Step::Close => return,
                        }
                        out.flush().await.unwrap();
                    }
                }
                _ => {}
            }
            out.flush().await.unwrap();
        }
    });
    (AcpClient::new(client_r, client_w), wire, prompts)
}

fn route(id: &str) -> Route {
    Route {
        id: id.into(),
        region: "us-east-1".into(),
        context_tokens: 1_000_000,
        max_output_tokens: 128_000,
    }
}

/// Opus us (the user's), Opus global, Sonnet global, Sol us.
fn four() -> Vec<Route> {
    vec![
        route(OPUS_US),
        route(OPUS_GLOBAL),
        route(SONNET_GLOBAL),
        route(SOL_US),
    ]
}

fn plan(routes: Vec<Route>) -> PromptPlan {
    PromptPlan {
        cwd: TOP.into(),
        engine: "cline".into(),
        model: OPUS_US.into(),
        prompt: PROMPT.into(),
        nudge: None,
        cancel: None,
        timeout: Duration::from_secs(27_900),
        silence: Duration::from_secs(900),
        retry: RetryPolicy::new(3, Duration::from_secs(5)),
        failover: Some(FailoverPlan::new(routes)),
        engine_stop_resumes: 1,
    }
}

struct Run {
    end: DriverEnd,
    wire: Wire,
    prompts: Prompts,
    rec: Arc<EventRecorder>,
}

async fn drive(script: Script, plan: PromptPlan, controls: Option<Arc<TurnControls>>) -> Run {
    drive_with(script, plan, controls, Hooks::none()).await
}

async fn drive_with(
    script: Script,
    plan: PromptPlan,
    controls: Option<Arc<TurnControls>>,
    hooks: Hooks,
) -> Run {
    let (client, wire, prompts) = engine(script, OPUS_US);
    let activity = Activity::new();
    let mut client = client.with_activity(activity.clone());
    if let Some(c) = &controls {
        client = client.with_cancel(c.subscribe(), Duration::from_secs(10));
    }
    let rec = EventRecorder::new("run1", "direct");
    let mut collector = DirectCollector::new(rec.clone()).with_hooks(hooks);
    let end = run_direct_prompt(&mut client, &mut collector, &activity, &plan, &NoProbe).await;
    Run {
        end,
        wire,
        prompts,
        rec,
    }
}

/// The minutes an outage notice says (its one-line text: "Every model route has been down for N minutes. ...").
fn minutes_in(notice: &Value) -> u64 {
    let text = notice["text"].as_str().unwrap();
    text.split_whitespace()
        .zip(text.split_whitespace().skip(1))
        .find(|(_, unit)| unit.starts_with("minutes"))
        .and_then(|(n, _)| n.parse().ok())
        .unwrap_or_else(|| panic!("no minutes in {text:?}"))
}

fn events_of(run: &Run, kind: &str) -> Vec<Value> {
    run.rec
        .drain()
        .into_iter()
        .filter_map(|e| match e.payload {
            harness_protocol::envelope::Payload::Inline { value } if value["kind"] == kind => {
                Some(value)
            }
            _ => None,
        })
        .collect()
}

fn models_by_prompt(run: &Run) -> Vec<String> {
    run.prompts
        .lock()
        .unwrap()
        .iter()
        .map(|(_, m)| m.clone())
        .collect()
}

fn texts(run: &Run) -> Vec<String> {
    run.prompts
        .lock()
        .unwrap()
        .iter()
        .map(|(t, _)| t.clone())
        .collect()
}

fn set_calls(run: &Run) -> Vec<String> {
    run.wire
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| m == "session/set_config_option")
        .map(|(_, p)| p["value"].as_str().unwrap().to_string())
        .collect()
}

// ---- the route switch ------------------------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn after_the_resumes_on_the_model_the_engine_is_switched_to_the_other_profile_and_the_work_goes_on()
 {
    // The user's prompt and three resumes fail; the fifth submission is on the global profile of the same model.
    let script = Script::new(vec![
        transient(),
        transient(),
        transient(),
        transient(),
        done("All done."),
    ]);
    let run = drive(script, plan(four()), None).await;
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(run.end.raw.message, "All done.");
    assert_eq!(
        models_by_prompt(&run),
        [OPUS_US, OPUS_US, OPUS_US, OPUS_US, OPUS_GLOBAL],
        "the model of each submission"
    );
    assert_eq!(
        set_calls(&run),
        [OPUS_GLOBAL],
        "one switch, on the same session"
    );
    assert_eq!(
        texts(&run).last().map(String::as_str),
        Some(RESUME_PROMPT),
        "a switch sends the same fixed sentence a retry sends"
    );
    let methods: Vec<String> = run
        .wire
        .lock()
        .unwrap()
        .iter()
        .map(|(m, _)| m.clone())
        .collect();
    assert_eq!(
        methods.iter().filter(|m| *m == "session/new").count(),
        1,
        "no second session"
    );
    // The records.
    assert_eq!(run.end.switches.len(), 1);
    let sw = &run.end.switches[0];
    assert_eq!(
        (sw.n, sw.from.as_str(), sw.to.as_str()),
        (1, OPUS_US, OPUS_GLOBAL)
    );
    assert_eq!(sw.kind, DirectSwitchKind::Route);
    assert_eq!(sw.class, "transient");
    assert!(
        sw.detail
            .as_deref()
            .unwrap()
            .contains("Service unavailable")
    );
    assert_eq!(run.end.final_model.as_deref(), Some(OPUS_GLOBAL));
    let note = run.end.failover.as_ref().unwrap();
    assert_eq!((note.routes, note.passes, note.probes), (4, 0, 0));
    assert_eq!(
        (
            note.effective_max_output_tokens,
            note.effective_context_tokens
        ),
        (128_000, 1_000_000)
    );
    let a = &run.end.attempts;
    assert_eq!(a.len(), 5);
    assert_eq!(a[0].model_id.as_deref(), Some(OPUS_US));
    assert_eq!(a[4].model_id.as_deref(), Some(OPUS_GLOBAL));
}

#[tokio::test(start_paused = true)]
async fn each_switch_is_one_turn_progress_event() {
    let script = Script::new(vec![
        transient(),
        transient(),
        transient(),
        transient(),
        done("ok"),
    ]);
    let run = drive(script, plan(four()), None).await;
    let events = events_of(&run, "provider-switch");
    assert_eq!(events.len(), 1);
    let e = &events[0];
    assert_eq!(
        (e["n"].as_u64(), e["from"].as_str(), e["to"].as_str()),
        (Some(1), Some(OPUS_US), Some(OPUS_GLOBAL))
    );
    assert_eq!(e["switchKind"], "route");
    assert_eq!(e["class"], "transient");
}

#[tokio::test(start_paused = true)]
async fn when_the_other_profile_fails_too_the_next_model_is_next() {
    // Four on the user's model, two on the global profile (the switch and one resume), then Sonnet answers.
    let script = Script::new(vec![
        transient(),
        transient(),
        transient(),
        transient(), // opus us
        transient(),
        transient(), // opus global
        done("Sonnet finished."),
    ]);
    let run = drive(script, plan(four()), None).await;
    assert_eq!(run.end.raw.message, "Sonnet finished.");
    assert_eq!(set_calls(&run), [OPUS_GLOBAL, SONNET_GLOBAL]);
    let kinds: Vec<DirectSwitchKind> = run.end.switches.iter().map(|s| s.kind).collect();
    assert_eq!(kinds, [DirectSwitchKind::Route, DirectSwitchKind::Model]);
    assert_eq!(run.end.final_model.as_deref(), Some(SONNET_GLOBAL));
    assert_eq!(
        models_by_prompt(&run).last().map(String::as_str),
        Some(SONNET_GLOBAL)
    );
}

#[tokio::test(start_paused = true)]
async fn a_route_the_provider_refuses_is_left_without_a_resume() {
    let script = Script::new(vec![unavailable(), done("On the other profile.")]);
    let run = drive(script, plan(four()), None).await;
    assert_eq!(run.end.raw.message, "On the other profile.");
    assert_eq!(
        models_by_prompt(&run),
        [OPUS_US, OPUS_GLOBAL],
        "no resume on the refused route"
    );
    assert_eq!(run.end.switches[0].class, "unavailable");
    let a = &run.end.attempts;
    assert_eq!(a[0].retry_class.as_deref(), Some("unavailable"));
}

#[tokio::test(start_paused = true)]
async fn a_failure_the_work_causes_is_not_answered_with_another_route() {
    let script = Script::new(vec![vec![Step::Fail(
        "Internal error: Model returned no content because the response was blocked by a content filter.",
        json!({"isRetryable": true}),
    )]]);
    let run = drive(script, plan(four()), None).await;
    assert!(set_calls(&run).is_empty());
    assert_eq!(texts(&run).len(), 1);
    assert!(run.end.switches.is_empty());
    assert_ne!(classify(&run.end.raw).outcome, DirectOutcome::ReportedDone);
}

#[tokio::test(start_paused = true)]
async fn a_permanent_failure_on_the_model_gets_one_resume_then_the_switch() {
    let script = Script::new(vec![permanent(), permanent(), done("fine")]);
    let run = drive(script, plan(four()), None).await;
    assert_eq!(models_by_prompt(&run), [OPUS_US, OPUS_US, OPUS_GLOBAL]);
    assert_eq!(run.end.switches[0].class, "permanent");
}

#[tokio::test(start_paused = true)]
async fn a_permanent_failure_right_after_a_switch_leaves_that_route_for_good() {
    // The global profile refuses its first submission as invalid: it is out, and Sonnet is next.
    let script = Script::new(vec![unavailable(), permanent(), done("Sonnet.")]);
    let run = drive(script, plan(four()), None).await;
    assert_eq!(
        models_by_prompt(&run),
        [OPUS_US, OPUS_GLOBAL, SONNET_GLOBAL]
    );
    assert_eq!(set_calls(&run), [OPUS_GLOBAL, SONNET_GLOBAL]);
}

#[tokio::test(start_paused = true)]
async fn a_switch_the_engine_refuses_counts_for_nothing_and_the_next_route_is_asked() {
    let mut script = Script::new(vec![unavailable(), done("Sonnet.")]);
    script.setting = Setting::Refuse(vec![OPUS_GLOBAL]);
    let run = drive(script, plan(four()), None).await;
    assert_eq!(
        set_calls(&run),
        [OPUS_GLOBAL, SONNET_GLOBAL],
        "the refused id was asked once, then the next"
    );
    assert_eq!(
        run.end.switches.len(),
        1,
        "only a confirmed switch is listed"
    );
    assert_eq!(run.end.switches[0].to, SONNET_GLOBAL);
    assert_eq!(models_by_prompt(&run), [OPUS_US, SONNET_GLOBAL]);
}

#[tokio::test(start_paused = true)]
async fn an_engine_that_reports_another_model_after_a_switch_has_refused_it() {
    let mut script = Script::new(vec![
        unavailable(),
        unavailable(),
        unavailable(),
        unavailable(),
    ]);
    script.setting = Setting::Mismatch;
    let mut p = plan(four());
    // One pass is enough to see the outcome: the wait after it is long, so cap the run.
    p.timeout = Duration::from_secs(40);
    let run = drive(script, p, None).await;
    assert!(run.end.switches.is_empty(), "no switch was confirmed");
    assert_eq!(set_calls(&run).len(), 3, "each other route was asked once");
}

#[tokio::test(start_paused = true)]
async fn an_engine_that_never_answers_a_switch_is_waited_for_no_longer_than_the_timeout() {
    let mut script = Script::new(vec![unavailable()]);
    script.setting = Setting::Silent;
    let mut p = plan(four());
    p.failover.as_mut().unwrap().switch_timeout = Duration::from_secs(10);
    p.timeout = Duration::from_secs(60);
    let t0 = tokio::time::Instant::now();
    let run = drive(script, p, None).await;
    assert!(t0.elapsed() < Duration::from_secs(120));
    assert!(run.end.switches.is_empty());
}

// ---- the run with one route stays what it was ------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_run_with_no_routes_after_the_model_retries_and_stops_as_in_the_first_slice() {
    let script = Script::new(vec![transient(), transient(), transient(), transient()]);
    let mut p = plan(vec![route(OPUS_US)]);
    p.failover = None;
    let run = drive(script, p, None).await;
    assert_eq!(texts(&run).len(), 4);
    assert!(set_calls(&run).is_empty());
    assert!(
        run.end.switches.is_empty() && run.end.failover.is_none() && run.end.final_model.is_none()
    );
    assert!(run.end.attempts.iter().all(|a| a.model_id.is_none()));
}

// ---- every route down: wait, probe, begin again ----------------------------------------------------------------------------------

/// Every submission on every route fails once; the preferred route answers on the second pass.
fn all_fail_then(n_fail: usize, then: &str) -> Script {
    let mut p = Vec::new();
    for _ in 0..n_fail {
        p.extend(std::iter::once(transient()));
    }
    p.push(done(then));
    Script::new(p)
}

#[tokio::test(start_paused = true)]
async fn when_every_route_has_failed_the_run_waits_and_begins_again_at_the_model_the_user_chose() {
    // 4 submissions on the user's model, then 2 each on the other three routes = 10 failures; the 11th (the next pass) works.
    let script = all_fail_then(10, "Back on the preferred route.");
    let t0 = tokio::time::Instant::now();
    let run = drive(script, plan(four()), None).await;
    assert_eq!(run.end.raw.message, "Back on the preferred route.");
    assert_eq!(
        models_by_prompt(&run).last().map(String::as_str),
        Some(OPUS_US)
    );
    let note = run.end.failover.as_ref().unwrap();
    assert_eq!(note.passes, 1);
    assert!(note.waited_s >= 30, "waited {}", note.waited_s);
    assert!(t0.elapsed() >= Duration::from_secs(30));
    let waits = events_of(&run, "provider-wait");
    assert_eq!(waits.len(), 1);
    assert_eq!(waits[0]["pass"], 1);
    // The way back is a switch to the preferred route, recorded as a return.
    assert_eq!(
        run.end.switches.last().unwrap().kind,
        DirectSwitchKind::Return
    );
    assert_eq!(run.end.final_model.as_deref(), Some(OPUS_US));
}

#[tokio::test(start_paused = true)]
async fn a_long_outage_is_waited_through_without_the_silence_watchdog_ending_the_run() {
    // 2 outages' worth of failures: the waits add up to more than the 900 s silence limit.
    let mut script = all_fail_then(10, "ignored");
    // The second pass fails again on every route too (10 more), then works: waits of 30 and 45 s plus the resumes' own.
    script.prompts.truncate(10);
    for _ in 0..10 {
        script.prompts.push(transient());
    }
    script.prompts.push(done("After a long outage."));
    let mut p = plan(four());
    let f = p.failover.as_mut().unwrap();
    f.wait_base = Duration::from_secs(400);
    f.wait_cap = Duration::from_secs(600);
    f.wait_slice = Duration::from_secs(30);
    let run = drive(script, p, None).await;
    assert_eq!(run.end.raw.message, "After a long outage.");
    assert!(run.end.failover.as_ref().unwrap().waited_s > 900);
    let outage = events_of(&run, "provider-outage");
    assert_eq!(outage.len(), 1, "the alarm is sent once per stretch");
}

#[tokio::test(start_paused = true)]
async fn the_run_deadline_ends_a_wait_with_every_route_down() {
    let script = all_fail_then(100, "never");
    let mut p = plan(four());
    p.timeout = Duration::from_secs(400);
    let run = drive(script, p, None).await;
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedTimeout
    );
}

#[tokio::test(start_paused = true)]
async fn the_users_cancel_ends_a_wait_with_every_route_down() {
    let (controls, _) = TurnControls::new();
    let script = all_fail_then(100, "never");
    let mut p = plan(four());
    p.cancel = Some(controls.subscribe());
    let c = controls.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(100)).await;
        c.deliver(&[PendingControl::Cancel]);
    });
    let run = drive(script, p, Some(controls)).await;
    canceller.await.unwrap();
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::StoppedByUser);
}

// ---- probes ---------------------------------------------------------------------------------------------------------------------

#[derive(Debug)]
struct Probes {
    answers: Mutex<BTreeMap<String, VecDeque<ProbeAnswer>>>,
    asked: Mutex<Vec<String>>,
}

impl Probes {
    fn new(pairs: &[(&str, Vec<ProbeAnswer>)]) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(
                pairs
                    .iter()
                    .map(|(id, a)| (id.to_string(), a.iter().cloned().collect()))
                    .collect(),
            ),
            asked: Mutex::default(),
        })
    }
}

impl RouteProbe for Probes {
    fn probe<'a>(
        &'a self,
        route: &'a Route,
    ) -> Pin<Box<dyn Future<Output = ProbeAnswer> + Send + 'a>> {
        Box::pin(async move {
            self.asked.lock().unwrap().push(route.id.clone());
            let mut all = self.answers.lock().unwrap();
            let q = all.entry(route.id.clone()).or_default();
            if q.len() > 1 {
                q.pop_front().unwrap()
            } else {
                q.front()
                    .cloned()
                    .unwrap_or(ProbeAnswer::Down("no script".into()))
            }
        })
    }
}

#[tokio::test(start_paused = true)]
async fn while_every_route_is_down_small_test_requests_pick_the_route_the_next_pass_begins_on() {
    // The preferred route and the other profile stay down; Sonnet answers the probe on the second round. No prompt reaches a fallback
    // route before its small test request is answered: the switch targets are probed first, found down and
    // skipped, the run waits, and the probe round finds Sonnet.
    let probes = Probes::new(&[
        (OPUS_US, vec![ProbeAnswer::Down("503".into())]),
        (OPUS_GLOBAL, vec![ProbeAnswer::Down("503".into())]),
        (
            SONNET_GLOBAL,
            vec![ProbeAnswer::Down("503".into()), ProbeAnswer::Healthy],
        ),
        (SOL_US, vec![ProbeAnswer::Down("429".into())]),
    ]);
    let script = all_fail_then(4, "On Sonnet after the probes.");
    let mut p = plan(four());
    p.failover = Some(FailoverPlan::new(four()).with_probe(probes.clone()));
    let run = drive(script, p, None).await;
    assert_eq!(run.end.raw.message, "On Sonnet after the probes.");
    assert_eq!(
        models_by_prompt(&run).last().map(String::as_str),
        Some(SONNET_GLOBAL)
    );
    assert_eq!(set_calls(&run), [SONNET_GLOBAL]);
    let note = run.end.failover.as_ref().unwrap();
    assert_eq!(
        *probes.asked.lock().unwrap(),
        [
            OPUS_GLOBAL,
            SONNET_GLOBAL,
            SOL_US,
            OPUS_US,
            OPUS_GLOBAL,
            SONNET_GLOBAL
        ]
        .map(String::from),
        "the three targets before any switch, then the round: three routes, the one that answered ends it"
    );
    assert_eq!(note.probes, 6);
    // No full-context prompt was sent while the routes were down: the four on the user's route and the one that worked.
    assert_eq!(texts(&run).len(), 5);
}

#[tokio::test(start_paused = true)]
async fn a_route_the_probe_finds_refused_is_not_asked_again_and_not_used() {
    let probes = Probes::new(&[
        (OPUS_US, vec![ProbeAnswer::Refused("403".into())]),
        (OPUS_GLOBAL, vec![ProbeAnswer::Healthy]),
        (SONNET_GLOBAL, vec![ProbeAnswer::Healthy]),
        (SOL_US, vec![ProbeAnswer::Healthy]),
    ]);
    let script = all_fail_then(10, "Used the other profile.");
    let mut p = plan(four());
    p.failover = Some(FailoverPlan::new(four()).with_probe(probes.clone()));
    let run = drive(script, p, None).await;
    assert_eq!(
        models_by_prompt(&run).last().map(String::as_str),
        Some(OPUS_GLOBAL)
    );
    let asked = probes.asked.lock().unwrap();
    assert_eq!(
        asked.iter().filter(|id| id.as_str() == OPUS_US).count(),
        1,
        "asked once, found refused, out for the session: {asked:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn when_the_probes_find_every_route_refused_the_run_stops_with_the_provider_error() {
    let refused = |id: &'static str| (id, vec![ProbeAnswer::Refused("403".into())]);
    let probes = Probes::new(&[
        refused(OPUS_US),
        refused(OPUS_GLOBAL),
        refused(SONNET_GLOBAL),
        refused(SOL_US),
    ]);
    let script = all_fail_then(10, "never");
    let mut p = plan(four());
    p.failover = Some(FailoverPlan::new(four()).with_probe(probes));
    let run = drive(script, p, None).await;
    assert_ne!(classify(&run.end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(
        texts(&run).len(),
        4,
        "the three targets were found refused before any prompt, and the user's route was found refused at the end"
    );
    assert!(set_calls(&run).is_empty());
}

// ---- the return to the preferred route -------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_fallback_that_fails_after_the_dwell_sends_the_run_back_to_the_model_the_user_chose() {
    // The preferred route fails 4 times; the other profile works for a long while (a 200 s sleep inside the prompt), then fails; the dwell
    // of 120 s since the preferred route's failure has passed, so the preferred route gets one submission.
    let mut slow_then_fail = vec![Step::Update(
        json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "run", "kind": "execute", "status": "pending"}),
    )];
    slow_then_fail.extend(transient());
    let script = Script::new(vec![
        transient(),
        transient(),
        transient(),
        transient(),                                    // opus us
        slow_then_fail,                                 // opus global fails after the work above
        transient(),                                    // its one resume
        done("Finished on the preferred route again."), // the return trial
    ]);
    let mut p = plan(four());
    p.failover.as_mut().unwrap().return_dwell = Duration::from_secs(1);
    let run = drive(script, p, None).await;
    assert_eq!(
        run.end.raw.message,
        "Finished on the preferred route again."
    );
    let kinds: Vec<DirectSwitchKind> = run.end.switches.iter().map(|s| s.kind).collect();
    assert_eq!(kinds, [DirectSwitchKind::Route, DirectSwitchKind::Return]);
    assert_eq!(run.end.final_model.as_deref(), Some(OPUS_US));
}

// ---- the summary's card sentence and limits --------------------------------------------------------------------------------------

#[test]
fn the_card_says_where_the_run_ran_and_after_how_many_switches() {
    use agentd::direct::summary::ran_on_sentence;
    assert_eq!(
        ran_on_sentence(SONNET_GLOBAL, 1),
        "Ran on global.anthropic.claude-sonnet-5-5 after 1 switch."
    );
    assert_eq!(
        ran_on_sentence(SONNET_GLOBAL, 3),
        "Ran on global.anthropic.claude-sonnet-5-5 after 3 switches."
    );
}

// ---- Cline's own stops are not provider failures -----------------------------------------------------------

/// A completed tool call, as Cline reports it: the call, then its update.
fn completed_call(id: &str, title: &str) -> Vec<Step> {
    vec![
        Step::Update(
            json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": "execute", "status": "pending"}),
        ),
        Step::Update(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed"}),
        ),
    ]
}

fn failed_call(id: &str, title: &str) -> Vec<Step> {
    vec![
        Step::Update(
            json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": "read", "status": "pending"}),
        ),
        Step::Update(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "failed"}),
        ),
    ]
}

/// Cline's loop guard from outside (run A12 of 2026-10-15): `n` completed calls with the same title, then `cancelled`.
fn loop_guard_stop(n: usize) -> Vec<Step> {
    let mut v = vec![Step::Pause(30)];
    for i in 0..n {
        v.extend(completed_call(
            &format!("c{i}"),
            "timeout 29s sh -c 'sleep 25; date -u'",
        ));
        v.push(Step::Pause(30));
    }
    v.push(Step::Cancelled);
    v
}

/// Cline's mistake limit from outside (live row A13-8): `n` failed calls with different titles, then `cancelled`.
fn mistake_stop(n: usize) -> Vec<Step> {
    let mut v = Vec::new();
    for i in 0..n {
        v.extend(failed_call(&format!("m{i}"), &format!("read file-{i}.txt")));
    }
    v.push(Step::Cancelled);
    v
}

fn engine_stop_attempts(run: &Run) -> Vec<(String, Option<String>)> {
    run.end
        .attempts
        .iter()
        .map(|a| (format!("{:?}", a.result), a.retry_class.clone()))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn the_loop_guard_of_cline_is_resumed_once_on_the_model_the_user_chose_and_never_switched() {
    // The A12 shape on a run with eight failover routes: every prompt ends `cancelled` after identical completed calls.
    let script = Script::new((0..6).map(|_| loop_guard_stop(5)).collect());
    let t0 = tokio::time::Instant::now();
    let run = drive(script, plan(four()), None).await;
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedEngineCancelled
    );
    assert_eq!(
        texts(&run),
        [PROMPT.to_string(), RESUME_PROMPT_INTERRUPTED.to_string()],
        "one resume, the sentence that does not name the provider"
    );
    assert_eq!(models_by_prompt(&run), [OPUS_US, OPUS_US]);
    assert!(
        set_calls(&run).is_empty(),
        "the engine was never asked to change model: {:?}",
        set_calls(&run)
    );
    assert!(run.end.switches.is_empty());
    assert_eq!(run.end.final_model, None);
    assert_eq!(
        engine_stop_attempts(&run),
        [
            (
                "EngineCancelled".to_string(),
                Some("engine_stop".to_string())
            ),
            (
                "EngineCancelled".to_string(),
                Some("engine_stop".to_string())
            ),
        ]
    );
    // Two attempts of 5 x 30 s plus one 5 s pause: nothing like the 24 minutes of the nine-model walk.
    assert!(
        t0.elapsed() < Duration::from_secs(2 * 6 * 30 + 20),
        "{:?}",
        t0.elapsed()
    );
    let note = run.end.failover.as_ref().unwrap();
    assert_eq!((note.passes, note.probes, note.waited_s), (0, 0, 0));
}

#[tokio::test(start_paused = true)]
async fn the_engine_stop_is_one_progress_event_with_its_shape_and_the_model_that_stayed() {
    let script = Script::new((0..3).map(|_| loop_guard_stop(5)).collect());
    let run = drive(script, plan(four()), None).await;
    let stops = events_of(&run, "engine-stop");
    assert_eq!(stops.len(), 1, "one resume, one event: {stops:?}");
    assert_eq!(stops[0]["hint"], "loop-guard");
    assert_eq!(stops[0]["attempt"], 1);
    assert_eq!(stops[0]["of"], 1);
    assert_eq!(stops[0]["backoffS"], 5);
    let detail = stops[0]["detail"].as_str().unwrap();
    assert!(
        detail.contains("5 identical tool calls in a row"),
        "{detail}"
    );
    assert!(
        detail.contains(OPUS_US),
        "the model that stayed is named: {detail}"
    );
    assert!(events_of(&run, "provider-switch").is_empty());
    assert!(events_of(&run, "provider-retry").is_empty());
    let a = run.end.attempts[0].detail.as_deref().unwrap();
    assert!(a.contains("no cancel request"), "{a}");
    assert!(a.contains("5 identical tool calls in a row"), "{a}");
    assert!(a.contains("model was not changed"), "{a}");
}

#[tokio::test(start_paused = true)]
async fn the_mistake_limit_of_cline_gets_the_same_treatment() {
    let script = Script::new((0..3).map(|_| mistake_stop(6)).collect());
    let run = drive(script, plan(four()), None).await;
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedEngineCancelled
    );
    assert_eq!(models_by_prompt(&run), [OPUS_US, OPUS_US]);
    assert!(set_calls(&run).is_empty());
    assert!(run.end.switches.is_empty());
    let stops = events_of(&run, "engine-stop");
    assert_eq!(stops[0]["hint"], "mistake-limit");
}

#[tokio::test(start_paused = true)]
async fn the_resume_after_a_stop_cline_chose_can_finish_the_work_on_the_same_model() {
    let mut script = Script::new(vec![loop_guard_stop(5), done("Finished after the nudge.")]);
    script.setting = Setting::Accept;
    let run = drive(script, plan(four()), None).await;
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(run.end.raw.message, "Finished after the nudge.");
    assert_eq!(models_by_prompt(&run), [OPUS_US, OPUS_US]);
    assert!(set_calls(&run).is_empty());
    assert!(run.end.switches.is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_setting_at_zero_ends_the_run_at_the_first_stop_without_any_resume() {
    let script = Script::new(vec![loop_guard_stop(5), done("never")]);
    let mut p = plan(four());
    p.engine_stop_resumes = 0;
    let run = drive(script, p, None).await;
    assert_eq!(texts(&run), [PROMPT.to_string()]);
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedEngineCancelled
    );
    assert!(run.end.attempts.is_empty(), "a run with one try lists none");
    assert!(events_of(&run, "engine-stop").is_empty());
    assert!(set_calls(&run).is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_setting_allows_more_than_one_resume() {
    let script = Script::new((0..6).map(|_| loop_guard_stop(5)).collect());
    let mut p = plan(four());
    p.engine_stop_resumes = 2;
    let run = drive(script, p, None).await;
    assert_eq!(texts(&run).len(), 3);
    assert_eq!(models_by_prompt(&run), [OPUS_US, OPUS_US, OPUS_US]);
    let stops = events_of(&run, "engine-stop");
    assert_eq!(
        stops
            .iter()
            .map(|e| e["attempt"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        stops[1]["backoffS"], 10,
        "the wait doubles as the provider retry's does"
    );
}

#[tokio::test(start_paused = true)]
async fn an_engine_stop_on_a_fallback_route_stays_on_that_route_and_switches_nowhere() {
    // The preferred route fails 4 times (a real provider outage), the other profile takes over and Cline then stops itself there.
    let mut prompts = vec![transient(), transient(), transient(), transient()];
    prompts.push(loop_guard_stop(5));
    prompts.push(loop_guard_stop(5));
    prompts.push(done("never"));
    let run = drive(Script::new(prompts), plan(four()), None).await;
    assert_eq!(
        models_by_prompt(&run),
        [OPUS_US, OPUS_US, OPUS_US, OPUS_US, OPUS_GLOBAL, OPUS_GLOBAL]
    );
    assert_eq!(
        set_calls(&run),
        [OPUS_GLOBAL],
        "the one switch the provider failures earned"
    );
    assert_eq!(run.end.switches.len(), 1);
    assert_eq!(run.end.final_model.as_deref(), Some(OPUS_GLOBAL));
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedEngineCancelled
    );
}

#[tokio::test(start_paused = true)]
async fn an_engine_stop_does_not_use_up_the_provider_budget_of_the_route() {
    // Cline stops itself first; the route then fails like a provider: it still gets its three resumes before any switch.
    let script = Script::new(vec![
        loop_guard_stop(5),
        transient(),
        transient(),
        transient(),
        transient(),
        done("Other profile."),
    ]);
    let run = drive(script, plan(four()), None).await;
    assert_eq!(
        models_by_prompt(&run),
        [OPUS_US, OPUS_US, OPUS_US, OPUS_US, OPUS_US, OPUS_GLOBAL]
    );
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(run.end.switches[0].class, "transient");
}

#[tokio::test(start_paused = true)]
async fn the_users_cancel_during_the_wait_before_the_resume_ends_the_run_as_the_users_stop() {
    let script = Script::new(vec![loop_guard_stop(5), done("never")]);
    let (controls, _) = TurnControls::new();
    let mut p = plan(four());
    p.cancel = Some(controls.subscribe());
    let c = controls.clone();
    let canceller = tokio::spawn(async move {
        // The loop-guard prompt takes 6 x 30 s; the wait before the resume is 5 s after it.
        tokio::time::sleep(Duration::from_secs(183)).await;
        c.deliver(&[PendingControl::Cancel]);
    });
    let run = drive(script, p, Some(controls)).await;
    canceller.await.unwrap();
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::StoppedByUser);
    assert_eq!(texts(&run), [PROMPT.to_string()]);
}

// ---- an unexplained stop with no progress: one probe ------------------------------------------------------------------------------

fn probed(answer: ProbeAnswer) -> (Arc<Probes>, PromptPlan) {
    let probes = Probes::new(&[(OPUS_US, vec![answer])]);
    let mut p = plan(four());
    p.failover = Some(FailoverPlan::new(four()).with_probe(probes.clone()));
    (probes, p)
}

#[tokio::test(start_paused = true)]
async fn an_unexplained_stop_with_no_completed_call_asks_the_route_once_and_a_dead_route_is_a_provider_failure()
 {
    let (probes, p) = probed(ProbeAnswer::Down("503".into()));
    let script = Script::new(vec![vec![Step::Cancelled], done("The route came back.")]);
    let run = drive(script, p, None).await;
    assert_eq!(*probes.asked.lock().unwrap(), [OPUS_US.to_string()]);
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(
        run.end.attempts[0].retry_class.as_deref(),
        Some("transient"),
        "a route that is down answers the stop with the provider path"
    );
    assert_eq!(run.end.failover.as_ref().unwrap().probes, 1);
    assert!(events_of(&run, "engine-stop").is_empty());
    assert_eq!(models_by_prompt(&run), [OPUS_US, OPUS_US]);
}

#[tokio::test(start_paused = true)]
async fn an_unexplained_stop_on_a_route_that_answers_the_probe_is_an_engine_stop() {
    let (probes, p) = probed(ProbeAnswer::Healthy);
    let script = Script::new(vec![vec![Step::Cancelled], done("Resumed.")]);
    let run = drive(script, p, None).await;
    assert_eq!(probes.asked.lock().unwrap().len(), 1);
    assert_eq!(
        run.end.attempts[0].retry_class.as_deref(),
        Some("engine_stop")
    );
    assert_eq!(events_of(&run, "engine-stop")[0]["hint"], "unexplained");
    assert!(set_calls(&run).is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_route_the_probe_finds_refused_is_not_made_a_provider_failure_by_a_stop_cline_chose() {
    let (_, p) = probed(ProbeAnswer::Refused("403".into()));
    let script = Script::new(vec![vec![Step::Cancelled], vec![Step::Cancelled]]);
    let run = drive(script, p, None).await;
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedEngineCancelled
    );
    assert!(set_calls(&run).is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_stop_after_a_completed_call_or_after_two_minutes_or_with_a_shape_asks_no_probe() {
    let after_work = {
        let mut v = completed_call("a", "ls");
        v.push(Step::Cancelled);
        v
    };
    let slow = vec![Step::Pause(120), Step::Cancelled];
    for (name, first) in [
        ("a completed call", after_work),
        ("two minutes", slow),
        ("mistake shape", mistake_stop(6)),
    ] {
        let (probes, p) = probed(ProbeAnswer::Down("503".into()));
        let script = Script::new(vec![first, done("Resumed.")]);
        let run = drive(script, p, None).await;
        assert!(probes.asked.lock().unwrap().is_empty(), "{name}");
        assert_eq!(
            run.end.attempts[0].retry_class.as_deref(),
            Some("engine_stop"),
            "{name}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_run_with_one_route_has_no_probe_and_an_unexplained_stop_is_an_engine_stop() {
    let script = Script::new(vec![
        vec![Step::Cancelled],
        vec![Step::Cancelled],
        done("never"),
    ]);
    let mut p = plan(four());
    p.failover = None;
    let run = drive(script, p, None).await;
    assert_eq!(texts(&run).len(), 2);
    assert_eq!(
        engine_stop_attempts(&run)[0].1.as_deref(),
        Some("engine_stop")
    );
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedEngineCancelled
    );
}

// ---- every other end of the prompt leaves the model alone --------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_ends_that_are_not_provider_failures_never_switch_or_resume() {
    let cases: [(&str, Vec<Step>); 4] = [
        ("max_tokens", vec![say("cut"), Step::Stop("max_tokens")]),
        (
            "max_turn_requests",
            vec![say("cut"), Step::Stop("max_turn_requests")],
        ),
        ("refusal", vec![Step::Stop("refusal")]),
        ("give-up", done("The task cannot be done.")),
    ];
    for (name, first) in cases {
        let run = drive(Script::new(vec![first, done("never")]), plan(four()), None).await;
        assert_eq!(texts(&run).len(), 1, "{name}");
        assert!(set_calls(&run).is_empty(), "{name}");
        assert!(run.end.switches.is_empty(), "{name}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_content_filter_a_permanent_failure_and_a_refused_route_keep_their_slice_two_answers() {
    // Never: the work causes it. Unavailable: left at once. Permanent: one resume, then the switch.
    let filter = vec![Step::Fail(
        "Internal error: The response was blocked by a content filter",
        json!({"statusCode": 400}),
    )];
    let run = drive(Script::new(vec![filter, done("never")]), plan(four()), None).await;
    assert_eq!(texts(&run).len(), 1);
    assert!(set_calls(&run).is_empty());
    let run = drive(
        Script::new(vec![unavailable(), done("Other.")]),
        plan(four()),
        None,
    )
    .await;
    assert_eq!(models_by_prompt(&run), [OPUS_US, OPUS_GLOBAL]);
    let run = drive(
        Script::new(vec![permanent(), permanent(), done("Other.")]),
        plan(four()),
        None,
    )
    .await;
    assert_eq!(models_by_prompt(&run), [OPUS_US, OPUS_US, OPUS_GLOBAL]);
}

#[tokio::test(start_paused = true)]
async fn a_silent_engine_and_a_closed_pipe_end_the_run_without_a_switch() {
    let mut p = plan(four());
    p.silence = Duration::from_secs(60);
    let run = drive(Script::new(vec![vec![Step::Hang]]), p, None).await;
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedSilence
    );
    assert!(set_calls(&run).is_empty());
    let run = drive(Script::new(vec![vec![Step::Close]]), plan(four()), None).await;
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedEngineDied
    );
    assert!(set_calls(&run).is_empty());
    assert!(run.end.switches.is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_provider_failure_slice_two_handles_still_resumes_three_times_then_switches_route_then_model()
 {
    // The regression guard of the whole change: a 503 is a provider failure, not an engine stop.
    let mut prompts: Vec<Vec<Step>> = (0..4).map(|_| transient()).collect();
    prompts.extend((0..2).map(|_| transient()));
    prompts.push(done("Third route."));
    let run = drive(Script::new(prompts), plan(four()), None).await;
    assert_eq!(
        models_by_prompt(&run),
        [
            OPUS_US,
            OPUS_US,
            OPUS_US,
            OPUS_US,
            OPUS_GLOBAL,
            OPUS_GLOBAL,
            SONNET_GLOBAL
        ]
    );
    assert_eq!(
        run.end.switches.iter().map(|s| s.kind).collect::<Vec<_>>(),
        [DirectSwitchKind::Route, DirectSwitchKind::Model]
    );
    assert!(
        run.end
            .attempts
            .iter()
            .all(|a| a.result != DirectAttemptResult::EngineCancelled)
    );
}

// ---- the sanity bound on switching: fruitless switches, then hold on the chosen route ---------------------------------------------

#[tokio::test(start_paused = true)]
async fn switches_that_achieve_nothing_put_the_run_on_hold_on_the_chosen_route_which_keeps_being_probed()
 {
    // Four routes, and every probe passes while every prompt fails. Pass 1: the user's model fails 4 times, then each of the other three
    // fails twice (a switch and one resume): three fruitless switches, every route used, the run waits. The user's route is down at the
    // first probe round, the other profile answers and is tried: a fourth fruitless switch, so the run goes on hold. From then on a failure
    // gets no resume, only the route the user chose is probed, and the engine switches back to it when it answers.
    let probes = Probes::new(&[
        (
            OPUS_US,
            vec![
                ProbeAnswer::Down("503".into()),
                ProbeAnswer::Down("503".into()),
                ProbeAnswer::Healthy,
            ],
        ),
        (OPUS_GLOBAL, vec![ProbeAnswer::Healthy]),
        (SONNET_GLOBAL, vec![ProbeAnswer::Healthy]),
        (SOL_US, vec![ProbeAnswer::Healthy]),
    ]);
    // 10 failures in pass 1, one more on the other profile (the hold begins), then the user's route answers.
    let script = all_fail_then(11, "Back on the chosen route.");
    let mut p = plan(four());
    p.failover = Some(FailoverPlan::new(four()).with_probe(probes.clone()));
    let run = drive(script, p, None).await;
    assert_eq!(run.end.raw.message, "Back on the chosen route.");
    assert_eq!(
        set_calls(&run),
        [OPUS_GLOBAL, SONNET_GLOBAL, SOL_US, OPUS_GLOBAL, OPUS_US],
        "no switch to another fallback once the hold began"
    );
    assert_eq!(
        *probes.asked.lock().unwrap(),
        [
            OPUS_GLOBAL,
            SONNET_GLOBAL,
            SOL_US,
            OPUS_US,
            OPUS_GLOBAL,
            OPUS_US,
            OPUS_US
        ]
        .map(String::from),
        "the three targets before their switches, the pass-2 round, then during the hold only the chosen route"
    );
    assert_eq!(
        models_by_prompt(&run).last().map(String::as_str),
        Some(OPUS_US)
    );
    assert_eq!(
        run.end.switches.last().unwrap().kind,
        DirectSwitchKind::Return
    );
    let waits = events_of(&run, "provider-wait");
    assert!(
        waits
            .iter()
            .any(|w| w["detail"].as_str().unwrap().contains("on hold")),
        "{waits:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn on_hold_the_run_does_not_give_up_while_the_deadline_has_not_come() {
    // Every route 503s for good and the chosen route never answers a probe: the run waits until its deadline and ends as a timeout.
    let probes = Probes::new(&[
        (OPUS_US, vec![ProbeAnswer::Down("503".into())]),
        (OPUS_GLOBAL, vec![ProbeAnswer::Down("503".into())]),
        (SONNET_GLOBAL, vec![ProbeAnswer::Healthy]),
        (SOL_US, vec![ProbeAnswer::Down("503".into())]),
    ]);
    let script = all_fail_then(400, "never");
    let mut p = plan(four());
    p.timeout = Duration::from_secs(3_000);
    p.failover = Some(FailoverPlan::new(four()).with_probe(probes.clone()));
    let run = drive(script, p, None).await;
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedTimeout
    );
    // 3,000 s of waits that grow to 300 s: a few dozen at most, not hundreds.
    assert!(set_calls(&run).len() <= 12, "{:?}", set_calls(&run));
}

// ---- the history the provider refuses is class 4 of the recovery ladder: no resume, no switch, no exclusion ------------------------------

const ASTRA_SHAPE: &str = "Internal error: The format of the value at messages.7.content.0.toolUse.input is invalid. Provide a json object for the field and try again.";

fn history_refused() -> Vec<Step> {
    vec![Step::Fail(
        ASTRA_SHAPE,
        json!({"statusCode": 400, "errorName": "ValidationException", "isRetryable": false}),
    )]
}

#[tokio::test(start_paused = true)]
async fn a_history_the_provider_refuses_ends_the_run_with_no_resume_no_switch_and_no_exclusion() {
    let run = drive(
        Script::new(vec![history_refused(), done("never")]),
        plan(four()),
        None,
    )
    .await;
    assert_eq!(
        texts(&run).len(),
        1,
        "another try would send the same history"
    );
    assert!(set_calls(&run).is_empty());
    assert!(run.end.switches.is_empty());
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedProviderError
    );
    let notices = events_of(&run, "notify");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["reason"], "history_unusable");
    assert!(notices[0]["outageMinutes"].is_null());
    let detail = notices[0]["detail"].as_str().unwrap();
    assert!(detail.contains("messages.7"), "{detail}");
    assert!(
        detail.contains(OPUS_US),
        "the route is named as not at fault: {detail}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_same_history_error_on_a_fallback_route_leaves_that_route_in_place() {
    let mut prompts: Vec<Vec<Step>> = (0..4).map(|_| transient()).collect();
    prompts.push(history_refused());
    prompts.push(done("never"));
    let run = drive(Script::new(prompts), plan(four()), None).await;
    assert_eq!(texts(&run).len(), 5);
    assert_eq!(
        set_calls(&run),
        [OPUS_GLOBAL],
        "only the switch the provider failures earned"
    );
    assert_eq!(events_of(&run, "notify")[0]["reason"], "history_unusable");
}

#[tokio::test(start_paused = true)]
async fn a_plain_validation_error_keeps_the_permanent_treatment() {
    // A 400 that names no message of the history is a request-shape error of the route: one resume, then the switch (slice two).
    let script = Script::new(vec![permanent(), permanent(), done("fine")]);
    let run = drive(script, plan(four()), None).await;
    assert_eq!(models_by_prompt(&run), [OPUS_US, OPUS_US, OPUS_GLOBAL]);
    assert!(events_of(&run, "notify").is_empty());
}

// ---- a small test request before every provider-fault switch ---------------------------------------------------------------------------------

fn probed_four(probes: &Arc<Probes>) -> PromptPlan {
    let mut p = plan(four());
    p.failover = Some(FailoverPlan::new(four()).with_probe(probes.clone()));
    p
}

fn four_failures_then(then: &str) -> Script {
    all_fail_then(4, then)
}

#[tokio::test(start_paused = true)]
async fn a_switch_is_made_only_after_the_target_answered_a_small_test_request() {
    let probes = Probes::new(&[(OPUS_GLOBAL, vec![ProbeAnswer::Healthy])]);
    let run = drive(
        four_failures_then("Other profile."),
        probed_four(&probes),
        None,
    )
    .await;
    assert_eq!(*probes.asked.lock().unwrap(), [OPUS_GLOBAL.to_string()]);
    assert_eq!(set_calls(&run), [OPUS_GLOBAL]);
    assert_eq!(
        models_by_prompt(&run),
        [OPUS_US, OPUS_US, OPUS_US, OPUS_US, OPUS_GLOBAL]
    );
    assert_eq!(run.end.failover.as_ref().unwrap().probes, 1);
}

#[tokio::test(start_paused = true)]
async fn a_target_that_does_not_answer_is_skipped_without_a_prompt_and_the_next_is_asked() {
    let probes = Probes::new(&[
        (OPUS_GLOBAL, vec![ProbeAnswer::Down("503".into())]),
        (SONNET_GLOBAL, vec![ProbeAnswer::Healthy]),
    ]);
    let run = drive(four_failures_then("Sonnet."), probed_four(&probes), None).await;
    assert_eq!(
        *probes.asked.lock().unwrap(),
        [OPUS_GLOBAL, SONNET_GLOBAL].map(String::from)
    );
    assert_eq!(set_calls(&run), [SONNET_GLOBAL]);
    assert_eq!(
        models_by_prompt(&run),
        [OPUS_US, OPUS_US, OPUS_US, OPUS_US, SONNET_GLOBAL],
        "no prompt went to the profile that did not answer"
    );
    assert_eq!(run.end.switches[0].kind, DirectSwitchKind::Model);
}

#[tokio::test(start_paused = true)]
async fn a_target_the_probe_finds_refused_is_out_for_the_session() {
    let probes = Probes::new(&[
        (OPUS_GLOBAL, vec![ProbeAnswer::Refused("403".into())]),
        (SONNET_GLOBAL, vec![ProbeAnswer::Healthy]),
    ]);
    let run = drive(four_failures_then("Sonnet."), probed_four(&probes), None).await;
    assert_eq!(set_calls(&run), [SONNET_GLOBAL]);
    assert_eq!(
        probes
            .asked
            .lock()
            .unwrap()
            .iter()
            .filter(|id| id.as_str() == OPUS_GLOBAL)
            .count(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn when_no_target_answers_the_run_waits_and_no_fallback_sees_a_prompt() {
    let probes = Probes::new(&[
        (OPUS_US, vec![ProbeAnswer::Healthy]),
        (OPUS_GLOBAL, vec![ProbeAnswer::Down("503".into())]),
        (SONNET_GLOBAL, vec![ProbeAnswer::Down("503".into())]),
        (SOL_US, vec![ProbeAnswer::Down("503".into())]),
    ]);
    let run = drive(
        four_failures_then("The user's route came back."),
        probed_four(&probes),
        None,
    )
    .await;
    assert_eq!(texts(&run).len(), 5);
    assert!(set_calls(&run).is_empty());
    assert_eq!(
        *probes.asked.lock().unwrap(),
        [OPUS_GLOBAL, SONNET_GLOBAL, SOL_US, OPUS_US].map(String::from)
    );
    assert_eq!(run.end.failover.as_ref().unwrap().passes, 1);
}

#[tokio::test(start_paused = true)]
async fn a_return_to_the_users_route_is_probed_like_any_other_switch() {
    // The other profile works for a long while then fails; the dwell since the user's route failed has passed, so the user's route is the
    // next target: it is asked first, found down, and the run goes on to the next route instead of sending it the history.
    let long_call = |secs: u64| {
        let mut v = completed_call("t1", "work");
        v.push(Step::Pause(secs));
        v.extend(transient());
        v
    };
    let probes = Probes::new(&[
        (OPUS_GLOBAL, vec![ProbeAnswer::Healthy]),
        (OPUS_US, vec![ProbeAnswer::Down("503".into())]),
        (SONNET_GLOBAL, vec![ProbeAnswer::Healthy]),
    ]);
    let mut prompts: Vec<Vec<Step>> = (0..4).map(|_| transient()).collect();
    prompts.push(long_call(200));
    prompts.push(transient());
    prompts.push(done("Sonnet."));
    let run = drive(Script::new(prompts), probed_four(&probes), None).await;
    assert_eq!(
        *probes.asked.lock().unwrap(),
        [OPUS_GLOBAL, OPUS_US, SONNET_GLOBAL].map(String::from)
    );
    assert_eq!(set_calls(&run), [OPUS_GLOBAL, SONNET_GLOBAL]);
    assert_eq!(run.end.raw.message, "Sonnet.");
}

// ---- the bound on prompts that achieve nothing, and the outage notice ---------------------------------------------------------------------------

/// The nine routes of the five supported models, the user's first.
const NINE: [&str; 9] = [
    OPUS_US,
    OPUS_GLOBAL,
    SONNET_GLOBAL,
    "us.anthropic.claude-sonnet-5-5",
    SOL_US,
    "global.openai.gpt-6.1-sol",
    "us.openai.gpt-6-astra",
    "global.openai.gpt-6-astra",
    "us.zai.glm-5.3",
];

#[tokio::test(start_paused = true)]
async fn prompts_that_achieve_nothing_stop_after_twelve_then_one_per_answered_probe_until_the_deadline()
 {
    // Every probe passes and every prompt fails: the shape that fed a whole history to a failing provider again and again. After 12
    // prompts with no completed tool call the ladder is on hold (nine routes, so the switch bound of nine is not reached first): a failure
    // gets no resume, only the user's route is asked, once per answered probe, and the waits grow to 300 s. The run goes on to its
    // deadline.
    let answers: Vec<(&str, Vec<ProbeAnswer>)> = NINE
        .iter()
        .map(|id| (*id, vec![ProbeAnswer::Healthy]))
        .collect();
    let probes = Probes::new(&answers);
    let routes: Vec<Route> = NINE.iter().map(|id| route(id)).collect();
    let mut p = plan(routes.clone());
    p.failover = Some(FailoverPlan::new(routes).with_probe(probes.clone()));
    p.timeout = Duration::from_secs(3_000);
    let run = drive(all_fail_then(400, "never"), p, None).await;
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedTimeout
    );
    let models = models_by_prompt(&run);
    assert!(
        (13..=30).contains(&models.len()),
        "12 prompts, then one per cycle of 45 to 300 s: {}",
        models.len()
    );
    // The first 12 prompts took the ladder: 4 on the user's route, then a switch and one resume on each of the next four.
    assert_eq!(&models[..4], [OPUS_US; 4]);
    assert_eq!(models[4], models[5]);
    assert_eq!(models[10], models[11]);
    assert_ne!(models[11], OPUS_US);
    // After the twelfth no fallback sees a prompt: the hold probes only the user's route, which is where the engine goes back to.
    assert!(
        models[12..].iter().all(|m| m == OPUS_US),
        "{:?}",
        &models[12..]
    );
    assert_eq!(
        set_calls(&run).len(),
        5,
        "four switches on the way, one return: {:?}",
        set_calls(&run)
    );
}

#[tokio::test(start_paused = true)]
async fn every_route_down_for_thirty_minutes_raises_one_notice_and_the_run_goes_on_waiting() {
    let probes = Probes::new(&[
        (OPUS_US, vec![ProbeAnswer::Down("503".into())]),
        (OPUS_GLOBAL, vec![ProbeAnswer::Down("503".into())]),
        (SONNET_GLOBAL, vec![ProbeAnswer::Down("503".into())]),
        (SOL_US, vec![ProbeAnswer::Down("503".into())]),
    ]);
    let mut p = probed_four(&probes);
    p.timeout = Duration::from_secs(4_000);
    let run = drive(all_fail_then(400, "never"), p, None).await;
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedTimeout
    );
    // The notice is the non-blocking `needs-input` (reason provider_outage), not the stand-in `notify`.
    let notices: Vec<Value> = events_of(&run, "needs-input");
    assert_eq!(notices.len(), 1, "once per stretch: {notices:?}");
    assert_eq!(notices[0]["reason"], "provider_outage");
    assert_eq!(notices[0]["blocking"], false);
    let minutes = minutes_in(&notices[0]);
    assert!((30..40).contains(&minutes), "{minutes}");
    assert!(
        notices[0]["text"]
            .as_str()
            .unwrap()
            .contains("keeps trying"),
        "{notices:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_notice_after_is_a_plan_setting_and_the_outage_event_carries_the_minutes() {
    let probes = Probes::new(&[
        (OPUS_US, vec![ProbeAnswer::Down("503".into())]),
        (OPUS_GLOBAL, vec![ProbeAnswer::Down("503".into())]),
        (SONNET_GLOBAL, vec![ProbeAnswer::Down("503".into())]),
        (SOL_US, vec![ProbeAnswer::Down("503".into())]),
    ]);
    let mut f = FailoverPlan::new(four()).with_probe(probes);
    assert_eq!(f.notice_after, Duration::from_secs(OUTAGE_NOTICE_AFTER_S));
    assert_eq!(OUTAGE_NOTICE_AFTER_S, 1_800);
    f.notice_after = Duration::from_secs(300);
    let mut p = plan(four());
    p.failover = Some(f);
    p.timeout = Duration::from_secs(2_000);
    let run = drive(all_fail_then(400, "never"), p, None).await;
    let rec = run.rec.drain();
    let find = |kind: &str| -> Vec<Value> {
        rec.iter()
            .filter_map(|e| match &e.payload {
                harness_protocol::envelope::Payload::Inline { value } if value["kind"] == kind => {
                    Some(value.clone())
                }
                _ => None,
            })
            .collect()
    };
    let notices = find("needs-input");
    assert_eq!(notices.len(), 1);
    assert!((5..10).contains(&minutes_in(&notices[0])));
    let alarms = find("provider-outage");
    assert_eq!(alarms.len(), 1, "the 900 s alarm is unchanged");
    assert!(alarms[0]["outageMinutes"].as_u64().unwrap() >= 15);
}

// ---- what the user-correction path needs from the recovery loop ---------------------------------------------------------------------

/// Drives `prompt_with_retry` twice on one engine session and one list of attempts, as a stage loop does.
async fn two_calls(
    script: Script,
    plan: &PromptPlan,
) -> (
    Vec<harness_protocol::direct::DirectAttempt>,
    FailoverRecord,
    Vec<String>,
) {
    let (mut client, _wire, prompts) = engine(script, OPUS_US);
    let activity = Activity::new();
    client = client.with_activity(activity.clone());
    let rec = EventRecorder::new("run1", "direct");
    let mut collector = DirectCollector::new(rec);
    client
        .request("initialize", json!({"protocolVersion": 1}), &mut collector)
        .await
        .unwrap();
    client
        .request(
            "session/new",
            json!({"cwd": TOP, "mcpServers": []}),
            &mut collector,
        )
        .await
        .unwrap();
    let mut attempts = Vec::new();
    let mut record = FailoverRecord::default();
    let first = prompt_with_retry(
        &mut client,
        &mut collector,
        &activity,
        "s-1",
        PROMPT,
        plan,
        &mut attempts,
        &mut record,
    )
    .await;
    assert!(first.is_ok());
    let second = prompt_with_retry_from(
        &mut client,
        &mut collector,
        &activity,
        "s-1",
        "The user's new instruction",
        plan,
        &mut attempts,
        &mut record,
        DirectAttemptKind::Prompt,
    )
    .await;
    assert!(second.is_ok());
    let sent = prompts
        .lock()
        .unwrap()
        .iter()
        .map(|(t, _)| t.clone())
        .collect();
    (attempts, record, sent)
}

#[tokio::test(start_paused = true)]
async fn a_second_call_continues_the_attempt_numbers_and_its_first_send_is_a_prompt() {
    let script = Script::new(vec![transient(), done("one"), transient(), done("two")]);
    let (attempts, _, sent) = two_calls(script, &plan(four())).await;
    let seen: Vec<(u32, DirectAttemptKind)> = attempts.iter().map(|a| (a.n, a.kind)).collect();
    assert_eq!(
        seen,
        [
            (1, DirectAttemptKind::Prompt),
            (2, DirectAttemptKind::Resume),
            (3, DirectAttemptKind::Prompt),
            (4, DirectAttemptKind::Resume)
        ]
    );
    assert_eq!(
        sent,
        [
            PROMPT.to_string(),
            RESUME_PROMPT.to_string(),
            "The user's new instruction".to_string(),
            RESUME_PROMPT.to_string()
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn each_call_has_its_own_engine_stop_resume_and_the_stage_facts_start_over() {
    // The user's instruction after a stop gets its own budget: the engine-stop resume is counted per call (per stage).
    let script = Script::new(vec![
        loop_guard_stop(5),
        loop_guard_stop(5),
        loop_guard_stop(5),
        done("fine"),
    ]);
    let (_, record, sent) = two_calls(script, &plan(four())).await;
    assert_eq!(
        sent.len(),
        4,
        "prompt + one resume, instruction + one resume"
    );
    assert_eq!(record.stage.stage_engine_stops, 3);
    assert_eq!(record.stage.identical_calls, Some(5));
    assert_eq!(record.stage.failed_turns, Some(0));
    assert_eq!(
        record.stage.last_shape,
        Some(agentd::direct::engine_stop::Shape::LoopGuard)
    );
    assert!(!record.stage.cap_hit);
    let mut record = record;
    record.begin_stage();
    assert_eq!(
        record.stage,
        agentd::direct::engine_stop::StageFacts::default()
    );
    assert_eq!(record.stage.stage_engine_stops, 0);
    assert_eq!(record.stage.identical_calls, None);
}

#[tokio::test(start_paused = true)]
async fn the_stage_facts_say_when_the_ladder_stopped_on_its_prompt_bound() {
    let answers: Vec<(&str, Vec<ProbeAnswer>)> = NINE
        .iter()
        .map(|id| (*id, vec![ProbeAnswer::Healthy]))
        .collect();
    let probes = Probes::new(&answers);
    let routes: Vec<Route> = NINE.iter().map(|id| route(id)).collect();
    let mut p = plan(routes.clone());
    p.failover = Some(FailoverPlan::new(routes).with_probe(probes));
    p.timeout = Duration::from_secs(3_000);
    // The facts live in the record the caller owns; drive the loop directly so the test can read it when the deadline ends the call.
    let (mut client, _, _) = engine(all_fail_then(400, "never"), OPUS_US);
    let activity = Activity::new();
    client = client.with_activity(activity.clone());
    let rec = EventRecorder::new("run1", "direct");
    let mut collector = DirectCollector::new(rec);
    client
        .request("initialize", json!({"protocolVersion": 1}), &mut collector)
        .await
        .unwrap();
    client
        .request(
            "session/new",
            json!({"cwd": TOP, "mcpServers": []}),
            &mut collector,
        )
        .await
        .unwrap();
    let mut attempts = Vec::new();
    let mut record = FailoverRecord::default();
    let _ = tokio::time::timeout(
        Duration::from_secs(600),
        prompt_with_retry(
            &mut client,
            &mut collector,
            &activity,
            "s-1",
            PROMPT,
            &p,
            &mut attempts,
            &mut record,
        ),
    )
    .await;
    assert!(record.stage.cap_hit, "{:?}", record.stage);
    assert_eq!(record.stage.stage_engine_stops, 0);
}

#[test]
fn a_plan_can_start_its_ladder_on_the_route_the_engine_is_on() {
    let mut f = FailoverPlan::new(four());
    assert_eq!(f.start, None);
    assert_eq!(
        f.ladder(RetryPolicy::new(3, Duration::from_secs(5)))
            .current(),
        0
    );
    f.start = Some(2);
    assert_eq!(
        f.ladder(RetryPolicy::new(3, Duration::from_secs(5)))
            .current(),
        2
    );
    // Seeded from the model the record says the run ended on.
    let f = FailoverPlan::new(four()).starting_on(Some(SONNET_GLOBAL));
    assert_eq!(f.start, Some(2));
    assert_eq!(
        FailoverPlan::new(four()).starting_on(Some(OPUS_US)).start,
        Some(0)
    );
    assert_eq!(
        FailoverPlan::new(four())
            .starting_on(Some("us.nobody.nothing"))
            .start,
        None
    );
    assert_eq!(FailoverPlan::new(four()).starting_on(None).start, None);
}

#[tokio::test(start_paused = true)]
async fn the_outage_notice_is_one_call_of_the_collector() {
    let rec = EventRecorder::new("run1", "direct");
    let c = DirectCollector::new(rec.clone());
    c.record_outage_notice(1_860);
    let events: Vec<Value> = rec
        .drain()
        .into_iter()
        .filter_map(|e| match e.payload {
            harness_protocol::envelope::Payload::Inline { value } => Some(value),
            _ => None,
        })
        .collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["kind"], "needs-input");
    assert_eq!(events[0]["reason"], "provider_outage");
    assert_eq!(events[0]["blocking"], false);
    assert_eq!(minutes_in(&events[0]), 31);
}

// ---- the answer of a try the loop drops -------------------------------------------------------------------------------------

/// What a module that counts the usage of a run is told about the prompts: when each went out and was answered, and when the loop dropped
/// an answered try for another (so that run's tokens are counted from the log, not from the answer).
#[derive(Default)]
struct Trace(Mutex<Vec<String>>);

impl DirectHook for Trace {
    fn name(&self) -> &'static str {
        "trace"
    }

    fn on_event(&self, event: &Event<'_>) {
        let line = match event {
            Event::PromptSent(p) => format!("sent {}", p.no),
            Event::PromptReplied(r) => format!("replied {}", r.no),
            Event::Ladder(LadderEvent::AnswerDropped) => "dropped".to_string(),
            _ => return,
        };
        self.0.lock().unwrap().push(line);
    }
}

async fn traced(
    script: Script,
    plan: PromptPlan,
    controls: Option<Arc<TurnControls>>,
) -> Vec<String> {
    let trace = Arc::new(Trace::default());
    let hooks = Hooks::new(vec![trace.clone() as Arc<dyn DirectHook>]);
    drive_with(script, plan, controls, hooks).await;
    trace.0.lock().unwrap().clone()
}

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(ToString::to_string).collect()
}

#[tokio::test(start_paused = true)]
async fn b824_an_answered_try_dropped_after_the_probe_found_the_route_down_is_reported_dropped() {
    // The engine ended the prompt `cancelled` with no cancel request, completed no call, and the route does not answer a small test
    // request: the class is a provider failure, not an engine stop, and the answer is dropped all the same.
    let (probes, p) = probed(ProbeAnswer::Down("503".into()));
    let script = Script::new(vec![vec![Step::Cancelled], done("The route came back.")]);
    let seen = traced(script, p, None).await;
    assert_eq!(probes.asked.lock().unwrap().len(), 1);
    assert_eq!(
        seen,
        words(&["sent 1", "replied 1", "dropped", "sent 2", "replied 2"])
    );
}

#[tokio::test(start_paused = true)]
async fn b824_an_answered_try_dropped_for_an_engine_stop_is_reported_dropped_once() {
    let (_, p) = probed(ProbeAnswer::Healthy);
    let script = Script::new(vec![vec![Step::Cancelled], done("Resumed.")]);
    let seen = traced(script, p, None).await;
    assert_eq!(
        seen,
        words(&["sent 1", "replied 1", "dropped", "sent 2", "replied 2"])
    );
}

#[tokio::test(start_paused = true)]
async fn b824_an_answer_the_loop_returns_is_never_reported_dropped() {
    // An answer that ends the prompt.
    let seen = traced(Script::new(vec![done("Done.")]), plan(four()), None).await;
    assert_eq!(seen, words(&["sent 1", "replied 1"]));
    // A provider error is no answer: its try is resumed and there is nothing to take back.
    let script = Script::new(vec![transient(), done("Back.")]);
    let seen = traced(script, plan(four()), None).await;
    assert_eq!(seen, words(&["sent 1", "replied 1", "sent 2", "replied 2"]));
    // The engine's own stop with no resume left: the loop returns the answer, so the driver counts it.
    let mut p = plan(four());
    p.engine_stop_resumes = 0;
    let seen = traced(Script::new(vec![vec![Step::Cancelled]]), p, None).await;
    assert_eq!(seen, words(&["sent 1", "replied 1"]));
    // The first stop is resumed and dropped; the second is returned.
    let script = Script::new(vec![vec![Step::Cancelled], vec![Step::Cancelled]]);
    let seen = traced(script, plan(four()), None).await;
    assert_eq!(
        seen,
        words(&["sent 1", "replied 1", "dropped", "sent 2", "replied 2"])
    );
}

#[tokio::test(start_paused = true)]
async fn b824_a_user_cancel_while_the_loop_waits_after_dropping_an_answer_reports_it_dropped() {
    // The loop returns the user's `cancelled`, not the answer it held, so the held answer's usage is not the driver's.
    let script = Script::new(vec![loop_guard_stop(5), done("never")]);
    let (controls, _) = TurnControls::new();
    let mut p = plan(four());
    p.cancel = Some(controls.subscribe());
    let c = controls.clone();
    let canceller = tokio::spawn(async move {
        // The loop-guard prompt takes 6 x 30 s; the wait before the resume is 5 s after it.
        tokio::time::sleep(Duration::from_secs(183)).await;
        c.deliver(&[PendingControl::Cancel]);
    });
    let seen = traced(script, p, Some(controls)).await;
    canceller.await.unwrap();
    assert_eq!(seen, words(&["sent 1", "replied 1", "dropped"]));
}
