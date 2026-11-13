//! The direct driver's ACP sequence and watchdogs over in-memory streams with a scripted
//! engine and a paused clock: 900 s of silence takes no wall time. What goes on the wire, how each way the one prompt can end
//! is classified, and when the optional nudge is sent.
#![allow(clippy::unwrap_used)]

use agentd::acp::{AcpClient, Activity};
use agentd::direct::classify::{classify, stop_reason};
use agentd::direct::collector::DirectCollector;
use agentd::direct::driver::{Nudge, PromptPlan, TreeProbe, run_direct_prompt, says_done};
use agentd::direct::retry::{RESUME_PROMPT, RESUME_PROMPT_INTERRUPTED, RetryPolicy};
use agentd::events::{EventRecorder, TurnControls};
use harness_protocol::api::internal::PendingControl;
use harness_protocol::direct::DirectOutcome;
use harness_protocol::direct::{DirectAttemptKind, DirectAttemptResult};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const MODEL: &str = "us.anthropic.claude-opus-5-5";
const TOP: &str = "/mnt/workspace/run1/direct/repo";
const PROMPT: &str = "  Add percent_change\n\twith tests  \n";

#[derive(Clone, Debug)]
enum Step {
    Update(Value),
    Sleep(u64),
    /// The prompt's response with this stop reason, optionally with usage.
    Finish(&'static str, Option<Value>),
    /// The prompt's response is a JSON-RPC error.
    Error(i64, &'static str),
    /// The prompt's response is a JSON-RPC error with `data` (the fork's patch 0011 sends the structure of a provider failure).
    ErrorWith(i64, &'static str, Value),
    /// Wait for `session/cancel`, then answer `cancelled`.
    AwaitCancel,
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

fn tool_done(id: &str) -> Step {
    Step::Update(
        json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed"}),
    )
}

fn frame(v: Value) -> String {
    format!("{v}\n")
}

struct Script {
    /// `currentValue` of the model option at `session/new`; `None` leaves the option out.
    model: Option<&'static str>,
    /// One list of steps per `session/prompt`, in order.
    prompts: Vec<Vec<Step>>,
    hang_initialize: bool,
    new_error: bool,
    /// Never answer `session/cancel`.
    ignore_cancel: bool,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            model: Some(MODEL),
            prompts: vec![vec![chunk("Done."), Step::Finish("end_turn", None)]],
            hang_initialize: false,
            new_error: false,
            ignore_cancel: false,
        }
    }
}

type Client = AcpClient<tokio::io::DuplexStream, tokio::io::DuplexStream>;
type Received = Arc<Mutex<Vec<Value>>>;

/// A client wired to an in-process engine, and what the engine received.
fn engine(script: Script) -> (Client, Received) {
    let (client_w, engine_r) = tokio::io::duplex(1 << 16);
    let (mut out, client_r) = tokio::io::duplex(1 << 16);
    let received: Received = Arc::default();
    let seen = received.clone();
    // A reader records every line the moment it arrives, even while the script is busy or silent.
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
            if script.hang_initialize {
                continue;
            }
            let id = msg.get("id").cloned();
            let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
            match msg["method"].as_str().unwrap_or_default() {
                "initialize" => {
                    out.write_all(frame(reply(json!({"protocolVersion": 1, "agentInfo": {"name": "scripted", "version": "9.9.9"}}))).as_bytes())
                        .await
                        .unwrap();
                }
                "session/new" if script.new_error => {
                    out.write_all(frame(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "bad cwd"}})).as_bytes())
                        .await
                        .unwrap();
                }
                "session/new" => {
                    let options = match script.model {
                        Some(m) => json!([{"id": "model", "currentValue": m, "options": []}]),
                        None => json!([]),
                    };
                    out.write_all(
                        frame(reply(json!({"sessionId": "s-1", "configOptions": options})))
                            .as_bytes(),
                    )
                    .await
                    .unwrap();
                }
                "session/prompt" => {
                    let steps = script.prompts.get(prompt_no).cloned().unwrap_or_default();
                    prompt_no += 1;
                    for step in steps {
                        match step {
                            Step::Update(u) => out
                                .write_all(frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s-1", "update": u}})).as_bytes())
                                .await
                                .unwrap(),
                            Step::Sleep(s) => tokio::time::sleep(Duration::from_secs(s)).await,
                            Step::Finish(reason, usage) => {
                                let mut result = json!({"stopReason": reason});
                                if let Some(u) = usage {
                                    result["_meta"] = json!({"usage": u});
                                }
                                out.write_all(frame(reply(result)).as_bytes()).await.unwrap();
                            }
                            Step::Error(code, message) => out
                                .write_all(frame(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})).as_bytes())
                                .await
                                .unwrap(),
                            Step::ErrorWith(code, message, data) => out
                                .write_all(frame(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message, "data": data}})).as_bytes())
                                .await
                                .unwrap(),
                            Step::AwaitCancel => {
                                while let Some(m) = rx.recv().await {
                                    if m["method"] == "session/cancel" {
                                        break;
                                    }
                                }
                                if !script.ignore_cancel {
                                    out.write_all(frame(reply(json!({"stopReason": "cancelled"}))).as_bytes()).await.unwrap();
                                } else {
                                    std::future::pending::<()>().await;
                                }
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
    (AcpClient::new(client_r, client_w), received)
}

fn plan(timeout_s: u64, silence_s: u64) -> PromptPlan {
    PromptPlan {
        cwd: TOP.into(),
        engine: "cline".into(),
        model: MODEL.into(),
        prompt: PROMPT.into(),
        nudge: None,
        cancel: None,
        timeout: Duration::from_secs(timeout_s),
        silence: Duration::from_secs(silence_s),
        // The tests before the retry slice send one prompt and keep doing so; the retry tests set their own policy.
        retry: RetryPolicy::off(),
        failover: None,
        engine_stop_resumes: 0,
    }
}

/// A probe that returns the given fingerprints in turn, then repeats the last.
struct Seq(Mutex<VecDeque<String>>, Mutex<String>);

impl Seq {
    fn new(v: &[&str]) -> Self {
        Self(
            Mutex::new(v.iter().map(|s| s.to_string()).collect()),
            Mutex::default(),
        )
    }
}

impl TreeProbe for Seq {
    fn fingerprint(&self) -> Pin<Box<dyn Future<Output = String> + Send + '_>> {
        Box::pin(async move {
            let mut q = self.0.lock().unwrap();
            let mut last = self.1.lock().unwrap();
            if let Some(next) = q.pop_front() {
                *last = next;
            }
            last.clone()
        })
    }
}

struct Run {
    end: agentd::direct::driver::DriverEnd,
    received: Received,
    collector: DirectCollector,
    rec: Arc<EventRecorder>,
}

async fn drive(
    script: Script,
    plan: PromptPlan,
    probe: &dyn TreeProbe,
    controls: Option<Arc<TurnControls>>,
) -> Run {
    let (client, received) = engine(script);
    let activity = Activity::new();
    let mut client = client.with_activity(activity.clone());
    if let Some(c) = &controls {
        client = client.with_cancel(c.subscribe(), Duration::from_secs(10));
    }
    let rec = EventRecorder::new("run1", "direct");
    let mut collector = DirectCollector::new(rec.clone());
    let end = run_direct_prompt(&mut client, &mut collector, &activity, &plan, probe).await;
    Run {
        end,
        received,
        collector,
        rec,
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

// ---- the wire ------------------------------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_wire_sequence_is_initialize_new_one_prompt_and_nothing_else() {
    let run = drive(
        Script::default(),
        plan(1000, 900),
        &agentd::direct::driver::NoProbe,
        None,
    )
    .await;
    assert_eq!(
        methods(&run.received),
        ["initialize", "session/new", "session/prompt"]
    );
    let r = run.received.lock().unwrap().clone();
    assert_eq!(r[0]["params"]["protocolVersion"], 1);
    assert_eq!(
        r[0]["params"]["clientCapabilities"],
        json!({"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false})
    );
    // session/new: the repository root, no servers, no `_meta`.
    assert_eq!(r[1]["params"], json!({"cwd": TOP, "mcpServers": []}));
    // session/prompt: the session and ONE text block holding the user's text byte for byte.
    assert_eq!(
        r[2]["params"],
        json!({"sessionId": "s-1", "prompt": [{"type": "text", "text": PROMPT}]})
    );
    assert_eq!(run.end.pinned.as_deref(), Some(MODEL));
    assert_eq!(run.end.session_id.as_deref(), Some("s-1"));
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(stop_reason(&run.end.raw).as_deref(), Some("end_turn"));
    assert_eq!(run.end.raw.message, "Done.");
    assert!(run.end.raw.setup_error.is_none());
    assert_eq!(run.end.nudges_sent, 0);
}

#[tokio::test(start_paused = true)]
async fn no_set_config_option_is_sent_the_launch_pinned_the_model() {
    let run = drive(
        Script::default(),
        plan(1000, 900),
        &agentd::direct::driver::NoProbe,
        None,
    )
    .await;
    assert!(
        !methods(&run.received)
            .iter()
            .any(|m| m == "session/set_config_option")
    );
}

#[tokio::test(start_paused = true)]
async fn usage_a_response_reports_is_kept_and_none_stays_none() {
    let none = drive(
        Script::default(),
        plan(1000, 900),
        &agentd::direct::driver::NoProbe,
        None,
    )
    .await;
    assert_eq!(none.end.usage, None);
    let script = Script {
        prompts: vec![vec![
            chunk("ok"),
            Step::Finish(
                "end_turn",
                Some(
                    json!({"inputTokens": 100, "outputTokens": 20, "cachedReadTokens": 7, "cachedWriteTokens": 3}),
                ),
            ),
        ]],
        ..Script::default()
    };
    let some = drive(
        script,
        plan(1000, 900),
        &agentd::direct::driver::NoProbe,
        None,
    )
    .await;
    let u = some.end.usage.unwrap();
    assert_eq!(
        (
            u.input_tokens,
            u.output_tokens,
            u.cache_read_input_tokens,
            u.cache_write_input_tokens
        ),
        (100, 20, 7, 3)
    );
}

// ---- failures before the prompt -----------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_model_the_engine_does_not_report_back_never_gets_the_prompt() {
    for model in [Some("some.other-model"), None] {
        let run = drive(
            Script {
                model,
                ..Script::default()
            },
            plan(1000, 900),
            &agentd::direct::driver::NoProbe,
            None,
        )
        .await;
        assert!(
            !methods(&run.received).iter().any(|m| m == "session/prompt"),
            "{model:?}"
        );
        let c = classify(&run.end.raw);
        assert_eq!(c.outcome, DirectOutcome::StoppedSetupFailed);
        assert!(
            run.end
                .raw
                .setup_error
                .as_deref()
                .unwrap()
                .starts_with("model pin:")
        );
        assert_eq!(run.end.pinned.as_deref(), model);
    }
}

#[tokio::test(start_paused = true)]
async fn an_error_answering_session_new_is_a_setup_failure() {
    let run = drive(
        Script {
            new_error: true,
            ..Script::default()
        },
        plan(1000, 900),
        &agentd::direct::driver::NoProbe,
        None,
    )
    .await;
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedSetupFailed
    );
    assert!(
        run.end
            .raw
            .setup_error
            .as_deref()
            .unwrap()
            .starts_with("session/new:")
    );
    assert!(!methods(&run.received).iter().any(|m| m == "session/prompt"));
}

#[tokio::test(start_paused = true)]
async fn an_engine_that_never_answers_initialize_fails_setup_after_120_seconds_not_the_deadline() {
    let t0 = tokio::time::Instant::now();
    let run = drive(
        Script {
            hang_initialize: true,
            ..Script::default()
        },
        plan(27_900, 900),
        &agentd::direct::driver::NoProbe,
        None,
    )
    .await;
    let took = t0.elapsed();
    assert!((120..130).contains(&took.as_secs()), "{took:?}");
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedSetupFailed
    );
    assert!(
        run.end
            .raw
            .setup_error
            .as_deref()
            .unwrap()
            .starts_with("initialize:")
    );
}

// ---- the watchdogs ---------------------------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn silence_for_the_limit_stops_the_prompt_and_writes_a_cancel() {
    let script = Script {
        prompts: vec![vec![chunk("thinking"), Step::Silent]],
        ..Script::default()
    };
    let t0 = tokio::time::Instant::now();
    let run = drive(
        script,
        plan(27_900, 900),
        &agentd::direct::driver::NoProbe,
        None,
    )
    .await;
    assert!(
        (900..915).contains(&t0.elapsed().as_secs()),
        "{:?}",
        t0.elapsed()
    );
    let c = classify(&run.end.raw);
    assert_eq!(c.outcome, DirectOutcome::StoppedSilence);
    assert!(c.detail.unwrap().contains("90"));
    assert!(run.end.max_silent_gap_s.unwrap() >= 900);
    // The cancel the driver writes is the engine's chance to stop its tools; it is not the user's cancel.
    tokio::task::yield_now().await;
    assert!(methods(&run.received).iter().any(|m| m == "session/cancel"));
    assert!(!run.end.raw.cancel_sent);
}

#[tokio::test(start_paused = true)]
async fn a_long_tool_call_is_not_silence() {
    let script = Script {
        prompts: vec![vec![
            tool("c1", "in_progress"),
            Step::Sleep(3000),
            tool_done("c1"),
            chunk("All green."),
            Step::Finish("end_turn", None),
        ]],
        ..Script::default()
    };
    let run = drive(
        script,
        plan(27_900, 900),
        &agentd::direct::driver::NoProbe,
        None,
    )
    .await;
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(run.collector.tool_calls(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_deadline_ends_the_prompt_even_with_a_tool_running_and_writes_a_cancel() {
    let script = Script {
        prompts: vec![vec![tool("c1", "in_progress"), Step::Silent]],
        ..Script::default()
    };
    let t0 = tokio::time::Instant::now();
    let run = drive(
        script,
        plan(2000, 900),
        &agentd::direct::driver::NoProbe,
        None,
    )
    .await;
    assert!(
        (2000..2010).contains(&t0.elapsed().as_secs()),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedTimeout
    );
    tokio::task::yield_now().await;
    assert!(methods(&run.received).iter().any(|m| m == "session/cancel"));
}

#[tokio::test(start_paused = true)]
async fn a_zero_time_left_ends_at_once_as_the_deadline() {
    let run = drive(
        Script::default(),
        plan(0, 900),
        &agentd::direct::driver::NoProbe,
        None,
    )
    .await;
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedTimeout
    );
}

// ---- the user's cancel ---------------------------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_cancel_control_writes_session_cancel_and_the_stop_is_the_users() {
    let script = Script {
        prompts: vec![vec![chunk("working"), Step::AwaitCancel]],
        ..Script::default()
    };
    let (controls, _) = TurnControls::new();
    let c = controls.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        c.deliver(&[PendingControl::Cancel]);
    });
    let run = drive(
        script,
        plan(27_900, 900),
        &agentd::direct::driver::NoProbe,
        Some(controls),
    )
    .await;
    let classified = classify(&run.end.raw);
    assert_eq!(classified.outcome, DirectOutcome::StoppedByUser);
    assert!(run.end.raw.cancel_sent);
    assert_eq!(stop_reason(&run.end.raw).as_deref(), Some("cancelled"));
    assert_eq!(
        methods(&run.received)
            .iter()
            .filter(|m| *m == "session/cancel")
            .count(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn an_engine_that_ignores_the_cancel_is_given_ten_seconds_and_the_stop_is_still_the_users() {
    let script = Script {
        prompts: vec![vec![Step::AwaitCancel]],
        ignore_cancel: true,
        ..Script::default()
    };
    let (controls, _) = TurnControls::new();
    let c = controls.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        c.deliver(&[PendingControl::Cancel]);
    });
    let t0 = tokio::time::Instant::now();
    let run = drive(
        script,
        plan(27_900, 900),
        &agentd::direct::driver::NoProbe,
        Some(controls),
    )
    .await;
    assert!(
        (15..20).contains(&t0.elapsed().as_secs()),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::StoppedByUser);
}

#[tokio::test(start_paused = true)]
async fn a_prompt_the_engine_ends_cancelled_by_itself_is_not_read_as_a_user_stop() {
    let script = Script {
        prompts: vec![vec![
            chunk("six calls failed"),
            Step::Finish("cancelled", None),
        ]],
        ..Script::default()
    };
    let run = drive(
        script,
        plan(1000, 900),
        &agentd::direct::driver::NoProbe,
        None,
    )
    .await;
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedEngineCancelled
    );
}

// ---- errors during the prompt --------------------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn an_error_answering_the_prompt_is_the_provider_failing_and_not_a_setup_failure() {
    let script = Script {
        prompts: vec![vec![
            chunk("part"),
            Step::Error(-32603, "Internal error: boom"),
        ]],
        ..Script::default()
    };
    let run = drive(
        script,
        plan(1000, 900),
        &agentd::direct::driver::NoProbe,
        None,
    )
    .await;
    let c = classify(&run.end.raw);
    assert_eq!(c.outcome, DirectOutcome::StoppedProviderError);
    assert!(run.end.raw.setup_error.is_none());
    assert!(c.detail.unwrap().contains("boom"));
    assert_eq!(
        run.end.raw.message, "part",
        "what was said before the error is kept"
    );
}

// ---- the nudge ---------------------------------------------------------------------------------------------------------------------

fn nudge(max: u32) -> Option<Nudge> {
    Some(Nudge {
        text: "Re-read my request. Reply DONE if it is all done.".into(),
        max,
    })
}

fn prompts(received: &Received) -> Vec<String> {
    received
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
        .collect()
}

fn reply(text: &str) -> Vec<Step> {
    vec![
        chunk(text),
        Step::Finish(
            "end_turn",
            Some(json!({"inputTokens": 10, "outputTokens": 5})),
        ),
    ]
}

#[test]
fn done_is_a_word_in_capitals_not_a_part_of_one() {
    assert!(says_done("DONE"));
    assert!(says_done("All set. DONE: tests pass."));
    assert!(says_done("(DONE)"));
    assert!(!says_done("done"));
    assert!(!says_done("UNDONE"));
    assert!(!says_done("ABANDONED"));
    assert!(!says_done(""));
}

#[tokio::test(start_paused = true)]
async fn without_a_nudge_the_probe_is_never_asked_and_one_prompt_is_sent() {
    struct Boom;
    impl TreeProbe for Boom {
        fn fingerprint(&self) -> Pin<Box<dyn Future<Output = String> + Send + '_>> {
            panic!("the probe is for the nudge only");
        }
    }
    let run = drive(Script::default(), plan(1000, 900), &Boom, None).await;
    assert_eq!(prompts(&run.received).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_nudge_goes_out_after_an_end_turn_that_did_not_say_done_and_stops_when_it_does() {
    let script = Script {
        prompts: vec![reply("Made a start."), reply("DONE. Everything passes.")],
        ..Script::default()
    };
    let mut p = plan(1000, 900);
    p.nudge = nudge(3);
    let probe = Seq::new(&["t0", "t1", "t2"]);
    let run = drive(script, p, &probe, None).await;
    assert_eq!(
        prompts(&run.received),
        vec![
            PROMPT.to_string(),
            "Re-read my request. Reply DONE if it is all done.".to_string()
        ]
    );
    assert_eq!(run.end.nudges_sent, 1);
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(run.end.raw.message, "DONE. Everything passes.");
    // Usage adds up across the turn's prompts.
    let u = run.end.usage.unwrap();
    assert_eq!((u.input_tokens, u.output_tokens), (20, 10));
}

#[tokio::test(start_paused = true)]
async fn the_first_reply_that_says_done_needs_no_nudge() {
    let script = Script {
        prompts: vec![reply("DONE.")],
        ..Script::default()
    };
    let mut p = plan(1000, 900);
    p.nudge = nudge(3);
    let run = drive(script, p, &Seq::new(&["t0", "t1"]), None).await;
    assert_eq!(prompts(&run.received).len(), 1);
    assert_eq!(run.end.nudges_sent, 0);
}

#[tokio::test(start_paused = true)]
async fn the_nudge_stops_at_its_count() {
    let script = Script {
        prompts: vec![reply("a"), reply("b"), reply("c"), reply("d")],
        ..Script::default()
    };
    let mut p = plan(1000, 900);
    p.nudge = nudge(2);
    // The tree changes after every prompt.
    let run = drive(script, p, &Seq::new(&["t0", "t1", "t2", "t3", "t4"]), None).await;
    assert_eq!(prompts(&run.received).len(), 3);
    assert_eq!(run.end.nudges_sent, 2);
    assert_eq!(run.end.raw.message, "c");
}

#[tokio::test(start_paused = true)]
async fn the_nudge_stops_when_two_prompts_in_a_row_changed_nothing() {
    let script = Script {
        prompts: vec![reply("a"), reply("b"), reply("c"), reply("d")],
        ..Script::default()
    };
    let mut p = plan(1000, 900);
    p.nudge = nudge(9);
    let run = drive(script, p, &Seq::new(&["same"]), None).await;
    assert_eq!(
        prompts(&run.received).len(),
        2,
        "the first prompt and one nudge, both without a change"
    );
}

#[tokio::test(start_paused = true)]
async fn one_changeless_prompt_followed_by_a_change_does_not_stop_the_nudge() {
    let script = Script {
        prompts: vec![reply("a"), reply("b"), reply("c")],
        ..Script::default()
    };
    let mut p = plan(1000, 900);
    p.nudge = nudge(2);
    // before-first t0; after first t0 (unchanged); after second t1 (changed); after third t1 (unchanged, but the count is spent).
    let run = drive(script, p, &Seq::new(&["t0", "t0", "t1", "t1"]), None).await;
    assert_eq!(prompts(&run.received).len(), 3);
}

#[tokio::test(start_paused = true)]
async fn no_nudge_follows_a_stop_other_than_end_turn() {
    for reason in ["max_tokens", "refusal", "max_turn_requests", "cancelled"] {
        let script = Script {
            prompts: vec![vec![chunk("x"), Step::Finish(reason, None)], reply("never")],
            ..Script::default()
        };
        let mut p = plan(1000, 900);
        p.nudge = nudge(3);
        let run = drive(script, p, &Seq::new(&["t0", "t1", "t2"]), None).await;
        assert_eq!(prompts(&run.received).len(), 1, "{reason}");
    }
}

#[tokio::test(start_paused = true)]
async fn no_nudge_follows_a_cancel_from_the_user() {
    let script = Script {
        prompts: vec![vec![Step::AwaitCancel], reply("never")],
        ..Script::default()
    };
    let mut p = plan(27_900, 900);
    p.nudge = nudge(3);
    let (controls, _) = TurnControls::new();
    let c = controls.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        c.deliver(&[PendingControl::Cancel]);
    });
    let run = drive(script, p, &Seq::new(&["t0", "t1", "t2"]), Some(controls)).await;
    assert_eq!(prompts(&run.received).len(), 1);
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::StoppedByUser);
}

#[tokio::test(start_paused = true)]
async fn an_error_answering_a_nudge_ends_the_turn_as_a_provider_failure_and_keeps_the_count() {
    let script = Script {
        prompts: vec![reply("a"), vec![Step::Error(-32603, "boom")]],
        ..Script::default()
    };
    let mut p = plan(1000, 900);
    p.nudge = nudge(3);
    let run = drive(script, p, &Seq::new(&["t0", "t1", "t2"]), None).await;
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedProviderError
    );
    assert_eq!(run.end.nudges_sent, 1);
}

// ---- review hardening (2026-10-13) ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_nudge_reply_with_no_text_is_no_message_even_though_the_first_prompt_had_text() {
    // The label belongs to the last prompt: the earlier prompt's words are not borrowed.
    let script = Script {
        prompts: vec![
            reply("I made a start."),
            vec![Step::Finish("end_turn", None)],
        ],
        ..Script::default()
    };
    let mut p = plan(1000, 900);
    p.nudge = nudge(1);
    let run = drive(script, p, &Seq::new(&["t0", "t1", "t2"]), None).await;
    assert_eq!(prompts(&run.received).len(), 2);
    assert_eq!(run.end.raw.message, "");
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedNoMessage
    );
}

#[tokio::test(start_paused = true)]
async fn a_cancel_that_arrived_between_two_prompts_stops_the_nudge_before_it_is_sent() {
    let script = Script {
        prompts: vec![reply("a"), reply("never")],
        ..Script::default()
    };
    let (controls, _) = TurnControls::new();
    let mut p = plan(1000, 900);
    p.nudge = nudge(3);
    p.cancel = Some(controls.subscribe());
    // The control lands while the probe runs after the first reply (its second call; the first runs before prompt 1), when
    // activity.cancel_sent() cannot know it yet: only the `plan.cancel` clause of the nudge's stop test can end the nudge.
    struct CancelAtProbe(Arc<TurnControls>, Seq, std::sync::atomic::AtomicU32);
    impl TreeProbe for CancelAtProbe {
        fn fingerprint(&self) -> Pin<Box<dyn Future<Output = String> + Send + '_>> {
            Box::pin(async move {
                let call = self.2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call == 1 {
                    self.0.deliver(&[PendingControl::Cancel]);
                }
                self.1.fingerprint().await
            })
        }
    }
    let probe = CancelAtProbe(
        controls.clone(),
        Seq::new(&["t0", "t1", "t2"]),
        std::sync::atomic::AtomicU32::new(0),
    );
    let run = drive(script, p, &probe, Some(controls)).await;
    assert_eq!(
        prompts(&run.received).len(),
        1,
        "no nudge follows a cancel the user already asked for"
    );
}

// ---- the provider retry ---------------------------------------------------------------

const TIMEOUT_TEXT: &str = "Internal error: The operation timed out.";

fn retrying(max: u32) -> PromptPlan {
    let mut p = plan(27_900, 900);
    p.retry = RetryPolicy::new(max, Duration::from_secs(5));
    p.engine_stop_resumes = 1;
    p
}

fn prompt_texts(run: &Run) -> Vec<String> {
    prompts(&run.received)
}

#[tokio::test(start_paused = true)]
async fn a_timeout_ending_the_prompt_is_retried_on_the_same_session_with_the_resume_sentence() {
    // The run of 2026-10-14: the prompt ended `-32603 The operation timed out.` after 88 tool calls and a final message.
    let script = Script {
        prompts: vec![
            vec![chunk("partial"), Step::Error(-32603, TIMEOUT_TEXT)],
            reply("Done."),
        ],
        ..Script::default()
    };
    let t0 = tokio::time::Instant::now();
    let run = drive(script, retrying(3), &agentd::direct::driver::NoProbe, None).await;
    assert_eq!(
        methods(&run.received),
        [
            "initialize",
            "session/new",
            "session/prompt",
            "session/prompt"
        ],
        "the engine is not restarted and no second session is made"
    );
    assert_eq!(
        prompt_texts(&run),
        [PROMPT.to_string(), RESUME_PROMPT.to_string()],
        "the user's text byte for byte first, the one fixed sentence second"
    );
    assert!(
        t0.elapsed() >= Duration::from_secs(5),
        "the first wait is the base"
    );
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(
        run.end.raw.message, "Done.",
        "the final message belongs to the last try"
    );
    let u = run.end.usage.unwrap();
    assert_eq!((u.input_tokens, u.output_tokens), (10, 5));
    let a = &run.end.attempts;
    assert_eq!(a.len(), 2);
    assert_eq!(
        (a[0].n, a[0].kind, a[0].result),
        (
            1,
            DirectAttemptKind::Prompt,
            DirectAttemptResult::ProviderError
        )
    );
    assert_eq!(a[0].retry_class.as_deref(), Some("transient"));
    assert_eq!(a[0].backoff_s, Some(5));
    assert!(a[0].detail.as_deref().unwrap().contains("timed out"));
    assert_eq!(
        (a[1].n, a[1].kind, a[1].result),
        (2, DirectAttemptKind::Resume, DirectAttemptResult::Ok)
    );
    assert_eq!(a[1].backoff_s, None);
}

#[tokio::test(start_paused = true)]
async fn each_retry_is_one_turn_progress_event_and_the_first_prompt_sends_none() {
    let script = Script {
        prompts: vec![vec![Step::Error(-32603, TIMEOUT_TEXT)], reply("Done.")],
        ..Script::default()
    };
    let run = drive(script, retrying(3), &agentd::direct::driver::NoProbe, None).await;
    let retries: Vec<Value> = run
        .rec
        .drain()
        .into_iter()
        .filter_map(|e| match e.payload {
            harness_protocol::envelope::Payload::Inline { value }
                if value["kind"] == "provider-retry" =>
            {
                Some(value)
            }
            _ => None,
        })
        .collect();
    assert_eq!(retries.len(), 1);
    let r = &retries[0];
    assert_eq!(
        (
            r["attempt"].as_u64(),
            r["of"].as_u64(),
            r["backoffS"].as_u64()
        ),
        (Some(2), Some(4), Some(5))
    );
    assert_eq!(r["class"], "transient");
    assert!(r["detail"].as_str().unwrap().contains("timed out"));
    let clean = drive(
        Script::default(),
        retrying(3),
        &agentd::direct::driver::NoProbe,
        None,
    )
    .await;
    assert!(
        clean
            .rec
            .drain()
            .iter()
            .all(|e| !format!("{:?}", e.payload).contains("provider-retry"))
    );
    assert!(
        clean.end.attempts.len() <= 1,
        "a run with one try records no list"
    );
}

#[tokio::test(start_paused = true)]
async fn the_tries_stop_at_the_cap_and_the_waits_double() {
    let fail = || vec![Step::Error(-32603, TIMEOUT_TEXT)];
    let script = Script {
        prompts: vec![fail(), fail(), fail(), fail(), reply("never")],
        ..Script::default()
    };
    let t0 = tokio::time::Instant::now();
    let run = drive(script, retrying(3), &agentd::direct::driver::NoProbe, None).await;
    assert_eq!(
        prompt_texts(&run).len(),
        4,
        "the user's prompt and three resumes, then no more"
    );
    let c = classify(&run.end.raw);
    assert_eq!(c.outcome, DirectOutcome::StoppedProviderError);
    assert!(c.detail.unwrap().contains("timed out"));
    assert_eq!(t0.elapsed().as_secs(), 5 + 10 + 20);
    let waits: Vec<Option<u64>> = run.end.attempts.iter().map(|a| a.backoff_s).collect();
    assert_eq!(waits, [Some(5), Some(10), Some(20), None]);
    assert!(
        run.end
            .attempts
            .iter()
            .all(|a| a.result == DirectAttemptResult::ProviderError)
    );
}

#[tokio::test(start_paused = true)]
async fn an_off_policy_sends_one_prompt_as_before() {
    let script = Script {
        prompts: vec![vec![Step::Error(-32603, TIMEOUT_TEXT)], reply("never")],
        ..Script::default()
    };
    let run = drive(
        script,
        plan(1000, 900),
        &agentd::direct::driver::NoProbe,
        None,
    )
    .await;
    assert_eq!(prompt_texts(&run).len(), 1);
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedProviderError
    );
    assert!(run.end.attempts.len() <= 1);
}

#[tokio::test(start_paused = true)]
async fn a_block_by_the_content_filter_is_not_retried() {
    let script = Script {
        prompts: vec![
            vec![Step::Error(
                -32603,
                "Internal error: Model returned no content because the response was blocked by a content filter. Retrying is unlikely to help — try rephrasing the request.",
            )],
            reply("never"),
        ],
        ..Script::default()
    };
    let run = drive(script, retrying(3), &agentd::direct::driver::NoProbe, None).await;
    assert_eq!(prompt_texts(&run).len(), 1);
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedProviderError
    );
}

#[tokio::test(start_paused = true)]
async fn a_request_the_provider_refused_gets_exactly_one_more_try() {
    let refused = || {
        vec![Step::ErrorWith(
            -32603,
            "Internal error: The provided request is not valid.",
            json!({"message": "The provided request is not valid.", "errorName": "ValidationException", "statusCode": 400, "isRetryable": false, "phase": "request"}),
        )]
    };
    let script = Script {
        prompts: vec![refused(), refused(), reply("never")],
        ..Script::default()
    };
    let run = drive(script, retrying(3), &agentd::direct::driver::NoProbe, None).await;
    assert_eq!(prompt_texts(&run).len(), 2);
    assert_eq!(
        run.end.attempts[0].retry_class.as_deref(),
        Some("permanent")
    );
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedProviderError
    );
}

#[tokio::test(start_paused = true)]
async fn a_failure_of_a_helper_agent_that_the_main_agent_outlived_is_absorbed_by_the_one_more_try()
{
    // Measured on the 3.0.71 binary against a fake Bedrock (2026-10-14): the main agent called spawn_agent, the sub-agent's request got
    // a 400, the main agent finished and said so, and the prompt still ended in -32603 with the sub-agent's error. One resume ends it.
    let refused = vec![
        chunk("All done: the helper failed but I finished."),
        Step::ErrorWith(
            -32603,
            "Internal error: The provided request is not valid.",
            json!({"errorName": "ValidationException", "statusCode": 400, "isRetryable": false}),
        ),
    ];
    let script = Script {
        prompts: vec![refused, reply("Checked the tree; nothing to change.")],
        ..Script::default()
    };
    let run = drive(script, retrying(3), &agentd::direct::driver::NoProbe, None).await;
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(run.end.raw.message, "Checked the tree; nothing to change.");
    assert_eq!(run.end.attempts.len(), 2);
}

#[tokio::test(start_paused = true)]
async fn the_providers_retry_after_sets_the_wait() {
    let script = Script {
        prompts: vec![
            vec![Step::ErrorWith(
                -32603,
                "Internal error: Too many requests",
                json!({"errorName": "ThrottlingException", "statusCode": 429, "retryAfterS": 30}),
            )],
            reply("Done."),
        ],
        ..Script::default()
    };
    let t0 = tokio::time::Instant::now();
    let run = drive(script, retrying(3), &agentd::direct::driver::NoProbe, None).await;
    assert_eq!(t0.elapsed().as_secs(), 30);
    assert_eq!(run.end.attempts[0].backoff_s, Some(30));
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::ReportedDone);
}

#[tokio::test(start_paused = true)]
async fn a_cancel_during_the_wait_ends_the_run_as_the_users_and_no_resume_is_sent() {
    let script = Script {
        prompts: vec![vec![Step::Error(-32603, TIMEOUT_TEXT)], reply("never")],
        ..Script::default()
    };
    let (controls, _) = TurnControls::new();
    let c = controls.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        c.deliver(&[PendingControl::Cancel]);
    });
    let mut p = retrying(3);
    p.cancel = Some(controls.subscribe());
    let t0 = tokio::time::Instant::now();
    let run = drive(script, p, &agentd::direct::driver::NoProbe, Some(controls)).await;
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "the wait is cut short"
    );
    assert_eq!(prompt_texts(&run).len(), 1);
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::StoppedByUser);
    assert!(run.end.raw.cancel_sent);
}

#[tokio::test(start_paused = true)]
async fn the_deadline_still_ends_a_run_that_keeps_failing_and_waiting() {
    let fail = || vec![Step::Error(-32603, TIMEOUT_TEXT)];
    let script = Script {
        prompts: vec![fail(), fail(), fail(), fail()],
        ..Script::default()
    };
    // 12 s in all: the first wait (5 s) fits, the second (10 s) does not.
    let mut p = retrying(3);
    p.timeout = Duration::from_secs(12);
    let run = drive(script, p, &agentd::direct::driver::NoProbe, None).await;
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedTimeout
    );
}

#[tokio::test(start_paused = true)]
async fn a_nudge_after_a_retried_prompt_follows_the_try_that_answered() {
    let script = Script {
        prompts: vec![
            vec![Step::Error(-32603, TIMEOUT_TEXT)],
            reply("a"),
            reply("b DONE"),
        ],
        ..Script::default()
    };
    let mut p = retrying(3);
    p.nudge = nudge(3);
    let run = drive(script, p, &Seq::new(&["t0", "t1", "t2", "t3"]), None).await;
    assert_eq!(prompt_texts(&run).len(), 3);
    assert_eq!(run.end.nudges_sent, 1);
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::ReportedDone);
}

#[tokio::test(start_paused = true)]
async fn a_prompt_the_engine_ends_cancelled_by_itself_is_resumed_once() {
    // L1's keep-alive runs of 2026-10-14: five model calls that all completed, then `cancelled` with nobody having asked (Cline's mistake
    // limit, a stop from a hook, or a stream it lost). One resume on the same session; the model decides what the tree needs.
    let script = Script {
        prompts: vec![
            vec![
                chunk("Run 4 timed out. This is run 5 of 6."),
                Step::Finish("cancelled", None),
            ],
            reply("Done."),
        ],
        ..Script::default()
    };
    let run = drive(script, retrying(3), &agentd::direct::driver::NoProbe, None).await;
    assert_eq!(
        prompt_texts(&run),
        [PROMPT.to_string(), RESUME_PROMPT_INTERRUPTED.to_string()],
        "the cause is not known to be the provider, so the sentence does not name it"
    );
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(run.end.raw.message, "Done.");
    let a = &run.end.attempts;
    assert_eq!(a.len(), 2);
    assert_eq!(a[0].result, DirectAttemptResult::EngineCancelled);
    assert_eq!(
        a[0].retry_class.as_deref(),
        Some("engine_stop"),
        "Cline's own stop is not a provider failure and has its own class"
    );
    assert_eq!(a[0].backoff_s, Some(5));
    assert!(
        a[0].detail
            .as_deref()
            .unwrap()
            .contains("no cancel request")
    );
    assert_eq!(a[1].result, DirectAttemptResult::Ok);
}

#[tokio::test(start_paused = true)]
async fn a_second_cancel_nobody_asked_for_ends_the_run_as_before_with_both_tries_listed() {
    let script = Script {
        prompts: vec![
            vec![Step::Finish("cancelled", None)],
            vec![Step::Finish("cancelled", None)],
            reply("never"),
        ],
        ..Script::default()
    };
    let run = drive(script, retrying(3), &agentd::direct::driver::NoProbe, None).await;
    assert_eq!(
        prompt_texts(&run).len(),
        2,
        "one resume, the engine-stop setting"
    );
    assert_eq!(
        classify(&run.end.raw).outcome,
        DirectOutcome::StoppedEngineCancelled
    );
    assert!(!run.end.raw.cancel_sent);
    let results: Vec<_> = run.end.attempts.iter().map(|a| a.result).collect();
    assert_eq!(
        results,
        [
            DirectAttemptResult::EngineCancelled,
            DirectAttemptResult::EngineCancelled
        ]
    );
    assert_eq!(run.end.attempts[1].backoff_s, None);
}

#[tokio::test(start_paused = true)]
async fn a_cancel_the_user_asked_for_is_never_resumed_even_with_the_retry_on() {
    let script = Script {
        prompts: vec![vec![chunk("working"), Step::AwaitCancel], reply("never")],
        ..Script::default()
    };
    let (controls, _) = TurnControls::new();
    let c = controls.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        c.deliver(&[PendingControl::Cancel]);
    });
    let mut p = retrying(3);
    p.cancel = Some(controls.subscribe());
    let run = drive(script, p, &agentd::direct::driver::NoProbe, Some(controls)).await;
    assert_eq!(prompt_texts(&run).len(), 1);
    assert_eq!(classify(&run.end.raw).outcome, DirectOutcome::StoppedByUser);
    assert!(run.end.attempts.len() <= 1);
}
