//! Permission asks of OpenCode: every `session/request_permission` is answered at once, in the parent session and in the
//! middle of a turn, and nothing else the engine asks of the client is left open.
#![allow(clippy::unwrap_used)]

mod b790_support;

use agentd::acp::Activity;
use agentd::direct::classify::classify;
use agentd::direct::collector::{DirectCollector, allow_once};
use agentd::direct::driver::{NoProbe, PromptPlan, run_direct_prompt};
use agentd::direct::retry::RetryPolicy;
use agentd::events::EventRecorder;
use b790_support::{
    MODEL, SESSION, Script, TOP, ask, chunk, engine, opencode_ask, tool, tool_done,
};
use harness_protocol::direct::DirectOutcome;
use serde_json::json;
use std::time::Duration;

fn plan() -> PromptPlan {
    PromptPlan {
        cwd: TOP.into(),
        model: MODEL.into(),
        prompt: "Add percent_change".into(),
        nudge: None,
        cancel: None,
        timeout: Duration::from_secs(1000),
        silence: Duration::from_secs(900),
        retry: RetryPolicy::off(),
        failover: None,
        engine_stop_resumes: 0,
        engine: "opencode".into(),
    }
}

#[test]
fn b790_an_ask_with_the_once_option_is_answered_with_that_option() {
    let answer = allow_once(&opencode_ask("call_1"));
    assert_eq!(
        answer,
        json!({"outcome": {"outcome": "selected", "optionId": "once"}})
    );
}

#[test]
fn b790_without_a_once_option_the_always_option_answers_and_never_reject() {
    let mut p = opencode_ask("call_1");
    p["options"].as_array_mut().unwrap().remove(0);
    let answer = allow_once(&p);
    assert_eq!(answer["outcome"]["optionId"], "always");
}

#[test]
fn b790_an_ask_that_offers_only_rejection_is_cancelled_and_not_left_open() {
    let mut p = opencode_ask("call_1");
    p["options"] = json!([{"optionId": "reject", "kind": "reject_once", "name": "Reject"}]);
    assert_eq!(allow_once(&p), json!({"outcome": {"outcome": "cancelled"}}));
}

async fn run_with(
    steps: Vec<b790_support::Step>,
) -> (
    agentd::direct::driver::DriverEnd,
    b790_support::Received,
    b790_support::Received,
) {
    let (client, received, answers) = engine(Script {
        prompts: vec![steps],
        ..Script::default()
    });
    let activity = Activity::new();
    let mut client = client.with_activity(activity.clone());
    let rec = EventRecorder::new("run1", "direct");
    let mut collector = DirectCollector::new(rec);
    let end = run_direct_prompt(&mut client, &mut collector, &activity, &plan(), &NoProbe).await;
    (end, received, answers)
}

#[tokio::test(start_paused = true)]
async fn b790_a_turn_that_waits_for_an_ask_gets_its_answer_and_ends_done() {
    let (end, _, answers) = run_with(vec![
        tool("call_1", "bash", "pending"),
        ask(opencode_ask("call_1")),
        tool_done("call_1"),
        chunk("Done."),
        b790_support::Step::Finish("end_turn"),
    ])
    .await;
    let got = answers.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "the engine got its answer");
    assert_eq!(got[0]["result"]["outcome"]["optionId"], "once");
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
}

#[tokio::test(start_paused = true)]
async fn b790_five_asks_in_one_turn_are_five_answers() {
    let mut steps = Vec::new();
    for i in 0..5 {
        steps.push(ask(opencode_ask(&format!("call_{i}"))));
    }
    steps.push(chunk("Done."));
    steps.push(b790_support::Step::Finish("end_turn"));
    let (end, _, answers) = run_with(steps).await;
    let got = answers.lock().unwrap().clone();
    assert_eq!(got.len(), 5);
    assert!(
        got.iter()
            .all(|a| a["result"]["outcome"]["outcome"] == "selected")
    );
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
}

#[tokio::test(start_paused = true)]
async fn b790_a_file_request_the_client_did_not_offer_is_refused_and_does_not_hang() {
    // OpenCode asks the client to write a file only when the client declared the capability, and agentd declares none; a request that comes
    // anyway gets a method-not-found error at once.
    let (end, _, answers) = run_with(vec![
        b790_support::Step::Request(
            "fs/write_text_file",
            json!({"sessionId": SESSION, "path": "/x", "content": "y"}),
        ),
        chunk("Done."),
        b790_support::Step::Finish("end_turn"),
    ])
    .await;
    let got = answers.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["error"]["code"], -32601);
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
}
