//! The value an engine's ACP `model` option takes for a catalog model id: the pin check at `session/new`, the pin check of a fresh
//! session and the switch to another route (`session/set_config_option`) all speak the engine's own form, while the summary keeps the catalog id.
//! Cline takes the id as it is; OpenCode takes `amazon-bedrock/<id>`.
#![allow(clippy::unwrap_used)]

use agentd::acp::{AcpClient, Activity};
use agentd::direct::classify::classify;
use agentd::direct::collector::DirectCollector;
use agentd::direct::driver::{DriverEnd, NoProbe, PromptPlan, run_direct_prompt};
use agentd::direct::failover::{FailoverPlan, Route};
use agentd::direct::retry::RetryPolicy;
use agentd::engine::config::{acp_model_value, catalog_model_id};
use agentd::events::EventRecorder;
use harness_protocol::direct::DirectOutcome;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const OPUS_US: &str = "us.anthropic.claude-opus-5-5";
const OPUS_GLOBAL: &str = "global.anthropic.claude-opus-5-5";
const TOP: &str = "/mnt/workspace/run1/direct/repo";

#[test]
fn b789_cline_and_unknown_engines_take_the_catalog_id_as_it_is() {
    for engine in ["cline", "fake-acp", "some-later-engine"] {
        assert_eq!(acp_model_value(engine, OPUS_US), OPUS_US, "{engine}");
        assert_eq!(catalog_model_id(engine, OPUS_US), OPUS_US, "{engine}");
    }
}

#[test]
fn b789_opencode_takes_the_amazon_bedrock_form_of_the_catalog_id() {
    assert_eq!(
        acp_model_value("opencode", OPUS_US),
        format!("amazon-bedrock/{OPUS_US}")
    );
    assert_eq!(
        acp_model_value("opencode", "aws-mantle/qwen.qwen3-coder-next"),
        "amazon-bedrock/aws-mantle/qwen.qwen3-coder-next"
    );
}

#[test]
fn b789_the_catalog_id_comes_back_from_the_engines_form_and_a_foreign_value_is_left_alone() {
    let value = acp_model_value("opencode", OPUS_US);
    assert_eq!(catalog_model_id("opencode", &value), OPUS_US);
    // A value in another form is not a catalog id in disguise: it is returned as it came, so a mismatch still names what the engine said.
    assert_eq!(
        catalog_model_id("opencode", "opencode/big-pickle"),
        "opencode/big-pickle"
    );
    assert_eq!(catalog_model_id("opencode", OPUS_US), OPUS_US);
}

// ---- over the wire -----------------------------------------------------------------------------------------------------------

type Client = AcpClient<tokio::io::DuplexStream, tokio::io::DuplexStream>;
type Wire = Arc<Mutex<Vec<(String, Value)>>>;

fn frame(v: Value) -> String {
    format!("{v}\n")
}

/// An engine that reports `first` as its model at `session/new`, takes every `session/set_config_option` value as it is, fails its first
/// prompt `fail_first` times with a 503 and then ends the turn.
fn engine(first: String, fail_first: usize) -> (Client, Wire) {
    let (client_w, engine_r) = tokio::io::duplex(1 << 16);
    let (mut out, client_r) = tokio::io::duplex(1 << 16);
    let wire: Wire = Arc::default();
    let seen = wire.clone();
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
        let mut current = first;
        let mut prompts = 0usize;
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
                    current = msg["params"]["value"].as_str().unwrap_or_default().to_string();
                    out.write_all(frame(reply(json!({"configOptions": options(&current)}))).as_bytes())
                        .await
                        .unwrap();
                }
                "session/prompt" => {
                    prompts += 1;
                    if prompts <= fail_first {
                        out.write_all(frame(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": "Internal error: Service unavailable",
                            "data": {"isRetryable": true, "statusCode": 503, "errorName": "ServiceUnavailableException"}}})).as_bytes())
                            .await
                            .unwrap();
                    } else {
                        out.write_all(frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s-1",
                            "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Done."}}}})).as_bytes())
                            .await
                            .unwrap();
                        out.write_all(frame(reply(json!({"stopReason": "end_turn"}))).as_bytes())
                            .await
                            .unwrap();
                    }
                }
                _ => {}
            }
            out.flush().await.unwrap();
        }
    });
    (AcpClient::new(client_r, client_w), wire)
}

fn route(id: &str) -> Route {
    Route {
        id: id.into(),
        region: "us-east-1".into(),
        context_tokens: 1_000_000,
        max_output_tokens: 128_000,
    }
}

fn plan(engine: &str, failover: Option<FailoverPlan>) -> PromptPlan {
    PromptPlan {
        cwd: TOP.into(),
        engine: engine.into(),
        model: OPUS_US.into(),
        prompt: "Add percent_change with tests".into(),
        nudge: None,
        cancel: None,
        timeout: Duration::from_secs(27_900),
        silence: Duration::from_secs(900),
        retry: if failover.is_some() {
            RetryPolicy::new(1, Duration::from_secs(5))
        } else {
            RetryPolicy::off()
        },
        failover,
        engine_stop_resumes: 0,
    }
}

async fn drive(first_value: &str, plan: PromptPlan, fail_first: usize) -> (DriverEnd, Wire) {
    let (client, wire) = engine(first_value.to_string(), fail_first);
    let activity = Activity::new();
    let mut client = client.with_activity(activity.clone());
    let rec = EventRecorder::new("run1", "direct");
    let mut collector = DirectCollector::new(rec);
    let end = run_direct_prompt(&mut client, &mut collector, &activity, &plan, &NoProbe).await;
    (end, wire)
}

fn methods(wire: &Wire) -> Vec<String> {
    wire.lock()
        .unwrap()
        .iter()
        .map(|(m, _)| m.clone())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn b789_an_opencode_session_that_reports_its_own_form_passes_the_pin_and_the_summary_keeps_the_catalog_id()
 {
    let (end, wire) = drive(
        &format!("amazon-bedrock/{OPUS_US}"),
        plan("opencode", None),
        0,
    )
    .await;
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
    assert!(methods(&wire).iter().any(|m| m == "session/prompt"));
    assert_eq!(
        end.pinned.as_deref(),
        Some(OPUS_US),
        "the summary records the catalog id, not the engine's form"
    );
}

#[tokio::test(start_paused = true)]
async fn b789_an_opencode_session_that_reports_the_bare_catalog_id_never_gets_the_prompt() {
    let (end, wire) = drive(OPUS_US, plan("opencode", None), 0).await;
    assert!(!methods(&wire).iter().any(|m| m == "session/prompt"));
    assert_eq!(
        classify(&end.raw).outcome,
        DirectOutcome::StoppedSetupFailed
    );
    let why = end.raw.setup_error.as_deref().unwrap();
    assert!(why.starts_with("model pin:"), "{why}");
    assert!(
        why.contains(&format!("amazon-bedrock/{OPUS_US}")),
        "the error names the value the engine had to report: {why}"
    );
}

#[tokio::test(start_paused = true)]
async fn b789_a_cline_session_that_reports_the_prefixed_form_never_gets_the_prompt() {
    let (end, wire) = drive(&format!("amazon-bedrock/{OPUS_US}"), plan("cline", None), 0).await;
    assert!(!methods(&wire).iter().any(|m| m == "session/prompt"));
    assert_eq!(
        classify(&end.raw).outcome,
        DirectOutcome::StoppedSetupFailed
    );
}

#[tokio::test(start_paused = true)]
async fn b789_the_switch_to_another_route_sends_the_engines_form_and_the_summary_keeps_the_catalog_id()
 {
    let failover = FailoverPlan::new(vec![route(OPUS_US), route(OPUS_GLOBAL)]);
    // The first prompt and its one resume fail; the next submission runs on the other route.
    let (end, wire) = drive(
        &format!("amazon-bedrock/{OPUS_US}"),
        plan("opencode", Some(failover)),
        2,
    )
    .await;
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
    let sent: Vec<String> = wire
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| m == "session/set_config_option")
        .map(|(_, p)| p["value"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(sent, [format!("amazon-bedrock/{OPUS_GLOBAL}")]);
    assert_eq!(
        end.final_model.as_deref(),
        Some(OPUS_GLOBAL),
        "the run ended on the catalog id of the other route"
    );
    assert_eq!(end.switches.len(), 1);
    assert_eq!(end.switches[0].to, OPUS_GLOBAL);
}

#[tokio::test(start_paused = true)]
async fn b789_the_switch_on_cline_still_sends_the_bare_id() {
    let failover = FailoverPlan::new(vec![route(OPUS_US), route(OPUS_GLOBAL)]);
    let (end, wire) = drive(OPUS_US, plan("cline", Some(failover)), 2).await;
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
    let sent: Vec<String> = wire
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| m == "session/set_config_option")
        .map(|(_, p)| p["value"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(sent, [OPUS_GLOBAL]);
}
