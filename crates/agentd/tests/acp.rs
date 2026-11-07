//! ACP client: handshake, prompt, updates, usage names, engine errors, unknown agent requests and cancel,
//! against an in-process scripted engine. The sequence a run sends and the model check are `direct::driver`'s, under
//! `tests/direct_driver.rs`; the exchange below is `initialize`, `session/new`, one `session/prompt`.
#![allow(clippy::unwrap_used)]

use agentd::acp::{AcpClient, AcpError, read_usage};
use agentd::direct::collector::DirectCollector;
use agentd::events::EventRecorder;
use harness_protocol::common::Usage;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Default)]
struct Engine {
    fail_new: bool,
    close_after_init: bool,
    unknown_request: bool,
    /// Holds `session/prompt` until `session/cancel`, then answers `cancelled`.
    hold_prompt: bool,
    /// With `hold_prompt`: never answers, even after `session/cancel`.
    ignore_cancel: bool,
    /// Answers `session/prompt` with `stopReason` only, as Cline 3.0.65 does (its ACP layer drops
    /// usage events).
    no_usage: bool,
    /// Answers `session/prompt` with ACP `PromptResponse.usage` (`cachedReadTokens`,
    /// `cachedWriteTokens`, `totalTokens`), as fork patch 0008 does.
    acp_usage: bool,
}

type Seen = Arc<Mutex<Vec<Value>>>;
type Client = AcpClient<tokio::io::DuplexStream, tokio::io::DuplexStream>;

/// Spawns a scripted engine; returns the client and every message the engine received.
fn start(engine: Engine) -> (Client, Seen) {
    let (client_w, engine_r) = tokio::io::duplex(1 << 16);
    let (engine_w, client_r) = tokio::io::duplex(1 << 16);
    let seen: Seen = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(engine_r).lines();
        let mut out = engine_w;
        let mut held: Option<Value> = None;
        while let Ok(Some(line)) = lines.next_line().await {
            let msg: Value = serde_json::from_str(&line).unwrap();
            log.lock().unwrap().push(msg.clone());
            let id = msg.get("id").cloned();
            let method = msg
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mut replies: Vec<Value> = Vec::new();
            if method == "session/cancel" && !engine.ignore_cancel {
                if let Some(held) = held.take() {
                    replies.push(json!({"jsonrpc": "2.0", "id": held, "result": {"stopReason": "cancelled"}}));
                }
            } else if method == "session/prompt" && engine.hold_prompt {
                held = id.clone();
            } else {
                match method.as_str() {
                    "initialize" => {
                        replies.push(json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": 1, "agentInfo": {"name": "scripted", "version": "9.9.9"}}}));
                    }
                    "session/new" if engine.fail_new => {
                        replies.push(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "auth required"}}));
                    }
                    "session/new" => {
                        replies.push(json!({"jsonrpc": "2.0", "id": id, "result": {"sessionId": "s-1", "configOptions": [{"id": "model", "category": "model", "currentValue": "default-model", "options": []}]}}));
                    }
                    "session/prompt" => {
                        let up = |u: Value| json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s-1", "update": u}});
                        replies.push(up(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Working. "}})));
                        replies.push(up(json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Edit src/calc.py", "kind": "edit", "status": "pending"})));
                        replies.push(json!({"jsonrpc": "2.0", "id": 900, "method": "session/request_permission", "params": {
                            "sessionId": "s-1",
                            "toolCall": {"toolCallId": "p900", "title": "Edit", "kind": "edit", "locations": [{"path": "/w/src/calc.py"}]},
                            "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}, {"optionId": "deny", "name": "Reject", "kind": "reject_once"}]}}));
                        if engine.unknown_request {
                            replies.push(json!({"jsonrpc": "2.0", "id": 902, "method": "fs/read_text_file", "params": {"path": "x"}}));
                        }
                        replies.push(up(json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed"})));
                        replies.push(up(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Done."}})));
                        if engine.no_usage {
                            replies.push(json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}}));
                        } else if engine.acp_usage {
                            replies.push(json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn", "_meta": {}, "usage": {"inputTokens": 100, "outputTokens": 40, "cachedReadTokens": 1000, "cachedWriteTokens": 200, "totalTokens": 1340}}}));
                        } else {
                            replies.push(json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn", "_meta": {"usage": {"inputTokens": 120, "outputTokens": 30, "cacheReadInputTokens": 7}}}}));
                        }
                    }
                    _ => {}
                }
            }
            for r in replies {
                out.write_all(format!("{r}\n").as_bytes()).await.unwrap();
            }
            out.flush().await.unwrap();
            if method == "initialize" && engine.close_after_init {
                return;
            }
        }
    });
    (AcpClient::new(client_r, client_w), seen)
}

fn collector() -> DirectCollector {
    DirectCollector::new(EventRecorder::new("run1", "a1"))
}

/// What the exchange ended with.
#[derive(Debug)]
struct Done {
    stop_reason: String,
    usage: Option<Usage>,
}

/// `initialize`, `session/new`, one `session/prompt`, as `direct::driver` sends them (without its model check).
async fn exchange(client: &mut Client, collector: &mut DirectCollector) -> Result<Done, AcpError> {
    client
        .request("initialize", json!({"protocolVersion": 1}), collector)
        .await?;
    let new = client
        .request(
            "session/new",
            json!({"cwd": "/w", "mcpServers": []}),
            collector,
        )
        .await?;
    let sid = new["sessionId"].as_str().unwrap().to_string();
    let p = client
        .request(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type": "text", "text": "Add percent_change"}]}),
            collector,
        )
        .await?;
    Ok(Done {
        stop_reason: p["stopReason"].as_str().unwrap_or("unknown").to_string(),
        usage: read_usage(&p),
    })
}

/// One more round trip: the engine answers only after reading everything the client wrote before.
async fn sync(client: &mut Client) {
    client
        .request(
            "initialize",
            json!({"protocolVersion": 1}),
            &mut collector(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_prompt_collects_the_message_answers_permissions_and_reads_the_usage() {
    let (mut client, seen) = start(Engine::default());
    let mut c = collector();
    let done = exchange(&mut client, &mut c).await.unwrap();
    sync(&mut client).await;
    assert_eq!(done.stop_reason, "end_turn");
    assert_eq!(c.final_message(), "Done.");
    assert_eq!(c.tool_calls(), 1);
    let usage = done.usage.unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_input_tokens
        ),
        (120, 30, 7)
    );
    let seen = seen.lock().unwrap();
    let answer = seen.iter().find(|v| v["id"] == 900).unwrap();
    assert_eq!(answer["result"]["outcome"]["optionId"], "allow");
    let new = seen.iter().find(|v| v["method"] == "session/new").unwrap();
    assert_eq!(new["params"]["cwd"], "/w");
    assert_eq!(new["params"]["mcpServers"], json!([]));
}

#[tokio::test]
async fn acp_usage_names_carry_the_cache_tokens() {
    // ACP `PromptResponse.usage` names cache tokens `cachedReadTokens` and `cachedWriteTokens`
    // (fork patch 0008); input excludes them.
    let (mut client, _) = start(Engine {
        acp_usage: true,
        ..Engine::default()
    });
    let done = exchange(&mut client, &mut collector()).await.unwrap();
    let usage = done.usage.unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_input_tokens,
            usage.cache_write_input_tokens
        ),
        (100, 40, 1000, 200)
    );
}

#[tokio::test]
async fn an_engine_that_reports_no_usage_gives_none_never_zero() {
    // An absent measurement stays absent (L33): token accounting then comes from invocation logs.
    let (mut client, _) = start(Engine {
        no_usage: true,
        ..Engine::default()
    });
    let mut c = collector();
    let done = exchange(&mut client, &mut c).await.unwrap();
    assert_eq!(done.stop_reason, "end_turn");
    assert_eq!(done.usage, None);
    assert_eq!(c.usage(), None);
}

#[tokio::test]
async fn engine_errors_and_a_closed_stream_surface_as_errors() {
    let (mut client, _) = start(Engine {
        fail_new: true,
        ..Engine::default()
    });
    let err = exchange(&mut client, &mut collector()).await.unwrap_err();
    assert_eq!(
        err,
        AcpError::Rpc {
            code: -32000,
            message: "auth required".into(),
            data: None,
        }
    );
    let (mut client, _) = start(Engine {
        close_after_init: true,
        ..Engine::default()
    });
    let err = exchange(&mut client, &mut collector()).await.unwrap_err();
    assert_eq!(err, AcpError::Closed);
}

#[tokio::test]
async fn unknown_agent_requests_get_method_not_found_and_the_turn_continues() {
    let (mut client, seen) = start(Engine {
        unknown_request: true,
        ..Engine::default()
    });
    let done = exchange(&mut client, &mut collector()).await.unwrap();
    sync(&mut client).await;
    assert_eq!(done.stop_reason, "end_turn");
    let seen = seen.lock().unwrap();
    let e = seen.iter().find(|v| v["id"] == 902).unwrap();
    assert_eq!(e["error"]["code"], -32601);
}

#[tokio::test]
async fn notify_writes_a_message_without_an_id() {
    let (mut client, seen) = start(Engine::default());
    client
        .notify("session/cancel", json!({"sessionId": "s-1"}))
        .await
        .unwrap();
    client
        .request(
            "initialize",
            json!({"protocolVersion": 1}),
            &mut collector(),
        )
        .await
        .unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0]["method"], "session/cancel");
    assert!(seen[0].get("id").is_none());
}

#[tokio::test]
async fn a_cancel_signal_mid_prompt_sends_session_cancel_and_returns_cancelled() {
    // The harness cancels a running turn; the engine answers cancelled.
    let (tx, rx) = tokio::sync::watch::channel(false);
    let (client, seen) = start(Engine {
        hold_prompt: true,
        ..Engine::default()
    });
    let mut client = client.with_cancel(rx, Duration::from_secs(10));
    let turn = tokio::spawn(async move { exchange(&mut client, &mut collector()).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    tx.send(true).unwrap();
    let done = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(done.stop_reason, "cancelled");
    let cancels: Vec<Value> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m["method"] == "session/cancel")
        .cloned()
        .collect();
    assert_eq!(cancels.len(), 1, "one session/cancel");
    assert_eq!(cancels[0]["params"]["sessionId"], "s-1");
    assert!(
        cancels[0].get("id").is_none(),
        "session/cancel is a notification"
    );
}

#[tokio::test]
async fn an_engine_that_ignores_cancel_fails_the_prompt_after_the_grace() {
    let (tx, rx) = tokio::sync::watch::channel(false);
    let (client, _) = start(Engine {
        hold_prompt: true,
        ignore_cancel: true,
        ..Engine::default()
    });
    let mut client = client.with_cancel(rx, Duration::from_millis(300));
    let turn = tokio::spawn(async move { exchange(&mut client, &mut collector()).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    tx.send(true).unwrap();
    let e = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(e, AcpError::CancelIgnored), "{e:?}");
}

// ---- `pump_idle`: serving the engine's pipe while a person decides -----------------------------------------------

use agentd::acp::{AcpHandler, Activity};
use tokio::io::DuplexStream;

/// Records what the pump handed to the handler; answers every engine request with `{"handled": true}`.
#[derive(Default)]
struct Rec {
    notes: Vec<(String, Value)>,
    requests: Vec<(String, Value)>,
}

impl AcpHandler for Rec {
    fn on_notification(&mut self, method: &str, params: &Value) {
        self.notes.push((method.to_string(), params.clone()));
    }
    fn on_request(&mut self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        self.requests.push((method.to_string(), params.clone()));
        Ok(json!({"handled": true}))
    }
}

/// A client with no scripted engine: the test writes the engine's lines itself and reads what the client writes.
fn raw() -> (Client, DuplexStream, DuplexStream, Arc<Activity>) {
    let (client_w, engine_r) = tokio::io::duplex(1 << 16);
    let (engine_w, client_r) = tokio::io::duplex(1 << 16);
    let activity = Activity::new();
    let client = AcpClient::new(client_r, client_w).with_activity(activity.clone());
    (client, engine_w, engine_r, activity)
}

fn update_line(update: Value) -> String {
    format!(
        "{}\n",
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s-1", "update": update}})
    )
}

#[tokio::test]
async fn pump_idle_hands_a_late_notification_to_the_handler_and_the_activity() {
    let (mut client, mut engine, _from_client, activity) = raw();
    activity.prompt_sent();
    let running = json!({"sessionUpdate": "tool_call", "toolCallId": "late-1", "title": "run_commands: sleep 5", "kind": "execute", "status": "in_progress"});
    engine
        .write_all(update_line(running).as_bytes())
        .await
        .unwrap();
    let mut rec = Rec::default();
    client.pump_idle(&mut rec).await.unwrap();
    assert_eq!(rec.notes.len(), 1, "the notification reached the handler");
    assert_eq!(rec.notes[0].0, "session/update");
    assert_eq!(rec.notes[0].1["update"]["toolCallId"], "late-1");
    assert_eq!(
        activity.tools_in_flight(),
        1,
        "the activity saw the tool call, as it does for a line read during a request"
    );
}

#[tokio::test]
async fn pump_idle_answers_an_engine_request_so_the_engine_is_not_left_waiting() {
    let (mut client, mut engine, from_client, _activity) = raw();
    let req = json!({"jsonrpc": "2.0", "id": 41, "method": "fs/read_text_file", "params": {"path": "notes.md"}});
    engine
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let mut rec = Rec::default();
    client.pump_idle(&mut rec).await.unwrap();
    assert_eq!(rec.requests.len(), 1);
    assert_eq!(rec.requests[0].0, "fs/read_text_file");
    let mut lines = BufReader::new(from_client).lines();
    let reply: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(reply["id"], 41);
    assert_eq!(reply["result"], json!({"handled": true}));
}

#[tokio::test]
async fn pump_idle_reports_the_engine_closing_its_output() {
    let (mut client, engine, _from_client, _activity) = raw();
    drop(engine);
    let mut rec = Rec::default();
    let err = client.pump_idle(&mut rec).await.unwrap_err();
    assert_eq!(err, AcpError::Closed);
}

#[tokio::test]
async fn pump_idle_reads_one_line_per_call_and_drops_chatter_and_stray_responses() {
    let (mut client, mut engine, _from_client, _activity) = raw();
    let mut text = String::from("engine banner, not JSON-RPC\n");
    text.push_str(&format!(
        "{}\n",
        json!({"jsonrpc": "2.0", "id": 99, "result": {}})
    ));
    text.push_str(&update_line(
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "still here"}}),
    ));
    engine.write_all(text.as_bytes()).await.unwrap();
    let mut rec = Rec::default();
    client.pump_idle(&mut rec).await.unwrap();
    client.pump_idle(&mut rec).await.unwrap();
    assert!(
        rec.notes.is_empty(),
        "chatter and a response nobody awaits are consumed without a handler call"
    );
    client.pump_idle(&mut rec).await.unwrap();
    assert_eq!(rec.notes.len(), 1, "the third line is the notification");
}

#[tokio::test(start_paused = true)]
async fn pump_idle_is_cancel_safe_a_dropped_wait_and_a_partial_line_lose_nothing() {
    let (mut client, mut engine, _from_client, _activity) = raw();
    let mut rec = Rec::default();
    // Nothing to read: the wait must really wait (a select! branch that lost is dropped here).
    let waited = tokio::time::timeout(Duration::from_secs(5), client.pump_idle(&mut rec)).await;
    assert!(
        waited.is_err(),
        "with no line to read pump_idle waits, it does not return"
    );
    // Half a line arrives, the wait is dropped again, the rest arrives.
    let full = update_line(
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "split"}}),
    );
    let (head, tail) = full.split_at(full.len() / 2);
    engine.write_all(head.as_bytes()).await.unwrap();
    let waited = tokio::time::timeout(Duration::from_secs(5), client.pump_idle(&mut rec)).await;
    assert!(waited.is_err(), "a partial line is not a line");
    engine.write_all(tail.as_bytes()).await.unwrap();
    client.pump_idle(&mut rec).await.unwrap();
    assert_eq!(
        rec.notes.len(),
        1,
        "the whole line arrived once, none of it was lost to the dropped waits"
    );
    assert_eq!(rec.notes[0].1["update"]["content"]["text"], "split");
}

#[tokio::test(start_paused = true)]
async fn pump_idle_does_not_move_the_silence_clock_into_a_held_wait() {
    // The pump feeds the activity (line_read) like a request does; during a hold that must not count the human's wait.
    let (mut client, mut engine, _from_client, activity) = raw();
    activity.prompt_sent();
    activity.hold();
    tokio::time::advance(Duration::from_secs(31 * 60)).await;
    engine
        .write_all(
            update_line(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "late"}}))
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut rec = Rec::default();
    client.pump_idle(&mut rec).await.unwrap();
    assert_eq!(rec.notes.len(), 1);
    assert_eq!(activity.max_silent_gap(), Duration::ZERO);
}

// ---- an abandoned session's traffic ----------------------------------------------------------
//
// After a stage is abandoned (a stalled prompt future is dropped and `session/cancel` is sent) the OLD session keeps running in the
// same engine process: its notifications keep arriving tagged with its old session id and its late `session/prompt` response with
// its old request id, while a fresh session runs beside it.

fn update_for(session: &str, update: Value) -> String {
    format!(
        "{}\n",
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": update}})
    )
}

fn tool_running_for(session: &str, id: &str) -> String {
    update_for(
        session,
        json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": "run_commands: sleep 25", "kind": "execute", "status": "in_progress"}),
    )
}

fn response(id: i64, result: Value) -> String {
    format!(
        "{}\n",
        json!({"jsonrpc": "2.0", "id": id, "result": result})
    )
}

#[tokio::test(start_paused = true)]
async fn a_late_response_of_a_dropped_prompt_is_not_the_answer_of_the_next_prompt() {
    let (mut client, mut engine, _from_client, _activity) = raw();
    let mut rec = Rec::default();
    // Stage 1: the prompt (request 1) is abandoned: its future is dropped.
    let abandoned = tokio::time::timeout(
        Duration::from_secs(5),
        client.request(
            "session/prompt",
            json!({"sessionId": "s-1", "prompt": []}),
            &mut rec,
        ),
    )
    .await;
    assert!(abandoned.is_err());
    // The old prompt answers late, then the new session's prompt (request 2) answers.
    engine
        .write_all(response(1, json!({"stopReason": "end_turn", "tag": "OLD"})).as_bytes())
        .await
        .unwrap();
    engine
        .write_all(response(2, json!({"stopReason": "end_turn", "tag": "NEW"})).as_bytes())
        .await
        .unwrap();
    client.set_session("s-2");
    let out = client
        .request(
            "session/prompt",
            json!({"sessionId": "s-2", "prompt": []}),
            &mut rec,
        )
        .await
        .unwrap();
    assert_eq!(
        out["tag"], "NEW",
        "the late response of request 1 is not the answer of request 2"
    );
}

#[tokio::test]
async fn an_abandoned_sessions_notifications_do_not_reach_the_handler_once_the_current_session_is_named()
 {
    let (mut client, mut engine, _from_client, _activity) = raw();
    client.set_session("s-2");
    let mut text = tool_running_for("s-1", "old-1");
    text.push_str(&update_for("s-1", json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "OLD SESSION TALKING"}})));
    text.push_str(&update_for("s-2", json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "fresh"}})));
    text.push_str(&response(1, json!({"stopReason": "end_turn"})));
    engine.write_all(text.as_bytes()).await.unwrap();
    let mut rec = Rec::default();
    client
        .request(
            "session/prompt",
            json!({"sessionId": "s-2", "prompt": []}),
            &mut rec,
        )
        .await
        .unwrap();
    assert_eq!(
        rec.notes.len(),
        1,
        "only the current session's notification reached the handler: {:?}",
        rec.notes
    );
    assert_eq!(rec.notes[0].1["sessionId"], "s-2");
    assert_eq!(client.stale_lines(), 2);
}

#[tokio::test(start_paused = true)]
async fn an_abandoned_sessions_traffic_does_not_feed_or_reset_the_silence_clock() {
    let (mut client, mut engine, _from_client, activity) = raw();
    client.set_session("s-2");
    activity.reset_for_stage();
    activity.prompt_sent();
    tokio::time::advance(Duration::from_secs(100)).await;
    // The old session's tool call is still streaming: it is not the new stage's activity, and it must not switch its watchdog off.
    engine
        .write_all(tool_running_for("s-1", "old-1").as_bytes())
        .await
        .unwrap();
    let mut rec = Rec::default();
    client.pump_idle(&mut rec).await.unwrap();
    assert!(rec.notes.is_empty());
    assert_eq!(
        activity.tools_in_flight(),
        0,
        "the old session's tool call is not tracked"
    );
    assert_eq!(
        activity.silent_for(),
        Duration::from_secs(100),
        "a line of an abandoned session is not engine activity of the current stage"
    );
    // A line of the current session is.
    engine.write_all(update_for("s-2", json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hi"}})).as_bytes()).await.unwrap();
    client.pump_idle(&mut rec).await.unwrap();
    assert_eq!(rec.notes.len(), 1);
    assert_eq!(activity.silent_for(), Duration::ZERO);
}

#[tokio::test]
async fn an_engine_request_from_an_abandoned_session_is_refused_not_served() {
    let (mut client, mut engine, from_client, _activity) = raw();
    client.set_session("s-2");
    let req = json!({"jsonrpc": "2.0", "id": 77, "method": "session/request_permission", "params": {"sessionId": "s-1", "toolCall": {"toolCallId": "old-9"}}});
    engine
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let mut rec = Rec::default();
    client.pump_idle(&mut rec).await.unwrap();
    assert!(
        rec.requests.is_empty(),
        "the handler never saw the abandoned session's request"
    );
    let mut lines = BufReader::new(from_client).lines();
    let reply: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(reply["id"], 77, "the engine is not left waiting");
    assert!(reply.get("result").is_none(), "{reply}");
    assert!(reply["error"]["code"].is_i64(), "{reply}");
}

#[tokio::test]
async fn nothing_is_filtered_until_a_session_is_named_and_a_line_without_a_session_id_always_passes()
 {
    let (mut client, mut engine, _from_client, _activity) = raw();
    engine.write_all(update_for("s-1", json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "a"}})).as_bytes()).await.unwrap();
    let mut rec = Rec::default();
    client.pump_idle(&mut rec).await.unwrap();
    assert_eq!(
        rec.notes.len(),
        1,
        "no session named: the existing behaviour"
    );
    client.set_session("s-2");
    engine
        .write_all(
            format!(
                "{}\n",
                json!({"jsonrpc": "2.0", "method": "cline/status", "params": {"state": "idle"}})
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    client.pump_idle(&mut rec).await.unwrap();
    assert_eq!(
        rec.notes.len(),
        2,
        "a notification with no session id passes"
    );
    assert_eq!(client.stale_lines(), 0);
    // The session can change: after a second fresh session the first one's lines are stale too.
    client.set_session("s-3");
    engine.write_all(update_for("s-2", json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "b"}})).as_bytes()).await.unwrap();
    client.pump_idle(&mut rec).await.unwrap();
    assert_eq!(rec.notes.len(), 2);
    assert_eq!(client.stale_lines(), 1);
}
